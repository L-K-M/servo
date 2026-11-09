/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Adaptive dark mode: when the embedder asks for it, a light page is painted with dark
//! colors while its styles stay as authored.
//!
//! Only colors on their way into the display list change. Scripts keep reading the
//! authored colors from computed styles; images, video, canvas and inline SVG keep theirs.
//! Pages and elements that declare dark support (`color-scheme: dark` or `light dark`) or
//! forbid any change (`color-scheme: light only`) are painted as authored.
//!
//! Colors change by role:
//!
//! - Backgrounds (background colors, gradient stops, shadows, the selection highlight):
//!   a light color gets darker, keeping its hue, so white becomes a dark gray and pale
//!   tints become deep ones. Mid and dark colors stay as they are. Lighter colors end
//!   darker, so a slightly raised panel stays slightly distinct.
//! - Foregrounds (text, its decorations, the caret, borders and outlines) follow what is
//!   painted behind them, the backdrop. Where the backdrop kept its color, so does the
//!   foreground. Where it changed, the foreground keeps its hue and gets the lightness
//!   that restores its authored contrast against the new backdrop, so black body text
//!   becomes near-white and gray secondary text a dimmer gray, as written. Text and
//!   carets get at least 4.5:1 (WCAG AA), outlines 3:1.
//! - Areas painted with a repeated or covering image (a texture, a photo) keep their
//!   authored colors, and so does the text over them: the image's lightness is unknown.
//! - An inline `<svg>` is rasterized with its colors, so a dark icon drawn in the text
//!   color would vanish on a darkened backdrop. One whose `color` is darker than the
//!   authored backdrop gets its lightness inverted, keeping hue, like the text around it.
//!   Images, video and `<canvas>` keep their pixels.
//!
//! Lightness is OKLab's: perceptually even, with hue kept in its `a` and `b` axes.

use std::cell::RefCell;

use rustc_hash::FxHashMap;
use style::color::{AbsoluteColor, ColorSpace};
use style::properties::ComputedValues;
use style::values::computed::background::BackgroundSize;
use style::values::computed::image::Image;
use style::values::specified::background::BackgroundRepeatKeyword;
use style::values::specified::color::ColorSchemeFlags;
use webrender_api::{ColorF, FilterOp};

/// The OKLab lightness that white backgrounds become. It matches the dark surface of a
/// typical dark theme without being black.
const DARKEST_BACKGROUND_LIGHTNESS: f32 = 0.21;

/// Backgrounds lighter than this get darker; darker ones keep their color.
const BACKGROUND_LIGHTNESS_THRESHOLD: f32 = 0.6;

/// The lightest a foreground gets on a dark backdrop: near white, which glares less than
/// white on a dark page.
const LIGHTEST_FOREGROUND_LIGHTNESS: f32 = 0.93;

/// The minimum contrast of text and carets against their backdrop (WCAG 2 AA).
pub(crate) const MIN_TEXT_CONTRAST: f32 = 4.5;

/// The minimum contrast of focus outlines (WCAG 2 non-text contrast).
pub(crate) const MIN_OUTLINE_CONTRAST: f32 = 3.0;

/// Borders keep their authored contrast, however low.
pub(crate) const MIN_BORDER_CONTRAST: f32 = 1.0;

/// The canvas color of a page that paints no background of its own.
const DEFAULT_CANVAS: Rgb = Rgb([1.0, 1.0, 1.0]);

/// Colors this close (in sRGB components) count as the same, so a backdrop that a
/// darkening left (nearly) unchanged leaves its foregrounds alone.
const SAME_COLOR_TOLERANCE: f32 = 1.0 / 255.0;

/// A layer more transparent than this does not change the backdrop.
const MIN_BACKDROP_ALPHA: f32 = 0.05;

/// Whether `style` asks for its colors to be painted as authored: it declares a dark color
/// scheme (so it styles itself for one) or the `only` keyword.
pub(crate) fn opts_out(style: &ComputedValues) -> bool {
    style
        .get_inherited_ui()
        .color_scheme
        .bits
        .intersects(ColorSchemeFlags::DARK | ColorSchemeFlags::ONLY)
}

/// An opaque sRGB color, components in `[0, 1]`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rgb([f32; 3]);

impl Rgb {
    fn from_color_f(color: ColorF) -> Self {
        Self([color.r, color.g, color.b])
    }

    /// `self` with `alpha` painted over `below`, blended as browsers do, in sRGB.
    fn over(self, alpha: f32, below: Rgb) -> Rgb {
        let alpha = alpha.clamp(0.0, 1.0);
        let mut result = [0.0; 3];
        for (index, component) in result.iter_mut().enumerate() {
            *component = self.0[index] * alpha + below.0[index] * (1.0 - alpha);
        }
        Rgb(result)
    }

    fn is_close_to(self, other: Rgb) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .all(|(a, b)| (a - b).abs() <= SAME_COLOR_TOLERANCE)
    }

    /// WCAG 2 relative luminance.
    fn luminance(self) -> f32 {
        let [r, g, b] = self.0.map(srgb_to_linear);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn to_oklab(self) -> Oklab {
        let [r, g, b] = self.0.map(srgb_to_linear);
        let l = 0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b;
        let m = 0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b;
        let s = 0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b;
        let (l, m, s) = (l.cbrt(), m.cbrt(), s.cbrt());
        Oklab {
            lightness: 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
            a: 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
            b: 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
        }
    }
}

/// WCAG 2 contrast ratio between two opaque colors, from 1 to 21.
fn contrast(first: Rgb, second: Rgb) -> f32 {
    contrast_of_luminances(first.luminance(), second.luminance())
}

fn contrast_of_luminances(first: f32, second: f32) -> f32 {
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

fn srgb_to_linear(component: f32) -> f32 {
    if component <= 0.040_45 {
        component / 12.92
    } else {
        ((component + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(component: f32) -> f32 {
    if component <= 0.003_130_8 {
        component * 12.92
    } else {
        1.055 * component.powf(1.0 / 2.4) - 0.055
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Oklab {
    lightness: f32,
    a: f32,
    b: f32,
}

impl Oklab {
    fn to_linear_srgb(self) -> [f32; 3] {
        let l = self.lightness + 0.396_337_78 * self.a + 0.215_803_76 * self.b;
        let m = self.lightness - 0.105_561_346 * self.a - 0.063_854_17 * self.b;
        let s = self.lightness - 0.089_484_18 * self.a - 1.291_485_5 * self.b;
        let (l, m, s) = (l * l * l, m * m * m, s * s * s);
        [
            4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
            -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
            -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
        ]
    }

    /// The sRGB color with this lightness and hue. A color outside sRGB loses chroma
    /// until it fits, which keeps its lightness and hue.
    fn to_rgb(self) -> Rgb {
        const EPSILON: f32 = 1e-4;
        let in_gamut = |linear: [f32; 3]| {
            linear
                .iter()
                .all(|component| (-EPSILON..=1.0 + EPSILON).contains(component))
        };

        let mut linear = self.to_linear_srgb();
        if !in_gamut(linear) {
            let (mut fits, mut overflows) = (0.0_f32, 1.0_f32);
            for _ in 0..12 {
                let scale = (fits + overflows) / 2.0;
                let candidate = Oklab {
                    a: self.a * scale,
                    b: self.b * scale,
                    ..self
                };
                if in_gamut(candidate.to_linear_srgb()) {
                    fits = scale;
                } else {
                    overflows = scale;
                }
            }
            linear = Oklab {
                a: self.a * fits,
                b: self.b * fits,
                ..self
            }
            .to_linear_srgb();
        }
        Rgb(linear.map(|component| linear_to_srgb(component.clamp(0.0, 1.0)).clamp(0.0, 1.0)))
    }

    fn with_lightness(self, lightness: f32) -> Self {
        Self { lightness, ..self }
    }
}

/// The darker color that adaptive dark mode paints instead of a light background.
fn darken_background(color: Rgb) -> Rgb {
    let lab = color.to_oklab();
    if lab.lightness <= BACKGROUND_LIGHTNESS_THRESHOLD {
        return color;
    }

    // Map (threshold, 1] onto [darkest, threshold) in reverse, continuous at the threshold.
    let excess = (lab.lightness.min(1.0) - BACKGROUND_LIGHTNESS_THRESHOLD) /
        (1.0 - BACKGROUND_LIGHTNESS_THRESHOLD);
    let lightness = BACKGROUND_LIGHTNESS_THRESHOLD -
        excess * (BACKGROUND_LIGHTNESS_THRESHOLD - DARKEST_BACKGROUND_LIGHTNESS);
    lab.with_lightness(lightness).to_rgb()
}

/// The color to paint for an opaque foreground `color` that was authored over `authored`
/// and is now painted over `painted`: the same hue, at the lightness that restores its
/// authored contrast, at least `min_contrast`, as far as `painted` allows.
fn adapt_foreground(color: Rgb, authored: Rgb, painted: Rgb, min_contrast: f32) -> Rgb {
    let target = contrast(color, authored).max(min_contrast);
    let painted_luminance = painted.luminance();
    let lab = color.to_oklab();

    let lighter = lab.with_lightness(LIGHTEST_FOREGROUND_LIGHTNESS).to_rgb();
    let darker = lab.with_lightness(0.0).to_rgb();
    let lighter_reach = contrast_of_luminances(lighter.luminance(), painted_luminance);
    let darker_reach = contrast_of_luminances(darker.luminance(), painted_luminance);

    // Keep the authored polarity unless the other side reaches clearly more of the target:
    // dark text on a backdrop that stayed light stays dark.
    let authored_was_lighter = color.luminance() >= authored.luminance();
    let (same_reach, other_reach) = if authored_was_lighter {
        (lighter_reach, darker_reach)
    } else {
        (darker_reach, lighter_reach)
    };
    let go_lighter = if same_reach.min(target) >= other_reach.min(target) * 0.9 {
        authored_was_lighter
    } else {
        !authored_was_lighter
    };

    let painted_lightness = painted.to_oklab().lightness;
    let (mut near, mut far, extreme) = if go_lighter {
        (painted_lightness, LIGHTEST_FOREGROUND_LIGHTNESS, lighter)
    } else {
        (painted_lightness, 0.0, darker)
    };
    if contrast_of_luminances(extreme.luminance(), painted_luminance) <= target {
        return extreme;
    }

    // Contrast grows monotonically from the backdrop's lightness to the extreme.
    for _ in 0..16 {
        let middle = (near + far) / 2.0;
        let candidate = lab.with_lightness(middle).to_rgb();
        if contrast_of_luminances(candidate.luminance(), painted_luminance) < target {
            near = middle;
        } else {
            far = middle;
        }
    }
    lab.with_lightness(far).to_rgb()
}

/// What is painted behind a box's content, as authored and as adaptive dark mode paints
/// it. Both are opaque.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Backdrop {
    authored: Rgb,
    painted: Rgb,
    /// A repeated or covering image paints over these colors, so foregrounds keep their
    /// authored colors.
    over_image: bool,
}

impl Backdrop {
    /// The backdrop of a document whose canvas is painted from `canvas_style` (the root's or
    /// the propagated body's background), or `None` if the document paints as authored.
    pub(crate) fn for_canvas(canvas_style: &ComputedValues) -> Self {
        let blank = Self {
            authored: DEFAULT_CANVAS,
            painted: darken_background(DEFAULT_CANVAS),
            over_image: false,
        };
        blank.behind(canvas_style)
    }

    /// The color to paint a canvas with no background of its own.
    pub(crate) fn blank_canvas_color() -> ColorF {
        let Rgb([r, g, b]) = darken_background(DEFAULT_CANVAS);
        ColorF::new(r, g, b, 1.0)
    }

    /// The backdrop of the content of a box with `style`, painted over `self`.
    pub(crate) fn behind(&self, style: &ComputedValues) -> Self {
        let background = style.get_background();
        if has_covering_image(style) {
            return Self {
                over_image: true,
                ..*self
            };
        }

        let color = rgba(style.resolve_color(&background.background_color));
        if color.a < MIN_BACKDROP_ALPHA {
            return *self;
        }

        let authored_color = Rgb::from_color_f(color);
        let painted_color = if opts_out(style) {
            authored_color
        } else {
            darken_background(authored_color)
        };
        Self {
            authored: authored_color.over(color.a, self.authored),
            painted: painted_color.over(color.a, self.painted),
            // An opaque color hides an image painted further down.
            over_image: self.over_image && color.a < 1.0,
        }
    }

    /// Whether foregrounds over this backdrop keep their authored colors.
    fn keeps_foregrounds(&self) -> bool {
        self.over_image || self.painted.is_close_to(self.authored)
    }
}

/// Whether one of `style`'s background layers is an image that repeats in both directions
/// or covers its area, such as a texture or a photo.
fn has_covering_image(style: &ComputedValues) -> bool {
    let background = style.get_background();
    background
        .background_image
        .0
        .iter()
        .enumerate()
        .any(|(index, image)| {
            if !matches!(image, Image::Url(..) | Image::ImageSet(..)) {
                return false;
            }
            let size = &background.background_size.0[index % background.background_size.0.len()];
            if matches!(size, BackgroundSize::Cover) {
                return true;
            }
            let repeat =
                &background.background_repeat.0[index % background.background_repeat.0.len()];
            repeat.0 != BackgroundRepeatKeyword::NoRepeat &&
                repeat.1 != BackgroundRepeatKeyword::NoRepeat
        })
}

fn rgba(color: AbsoluteColor) -> ColorF {
    let rgba = color.to_color_space(ColorSpace::Srgb);
    ColorF::new(
        rgba.components.0.clamp(0.0, 1.0),
        rgba.components.1.clamp(0.0, 1.0),
        rgba.components.2.clamp(0.0, 1.0),
        rgba.alpha.clamp(0.0, 1.0),
    )
}

/// The cache key of an adapted foreground: the color, its backdrop and minimum contrast.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct ForegroundKey {
    color: [u8; 4],
    authored: [u8; 3],
    painted: [u8; 3],
    min_contrast_tenths: u8,
}

fn quantize(component: f32) -> u8 {
    (component.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn dequantize(component: u8) -> f32 {
    f32::from(component) / 255.0
}

/// The colors of one display list in adaptive dark mode. Adapting a foreground searches
/// for a lightness, so results are cached: a page reuses few color and backdrop pairs.
#[derive(Default)]
pub(crate) struct AdaptiveDarkPainter {
    foregrounds: RefCell<FxHashMap<ForegroundKey, ColorF>>,
}

impl AdaptiveDarkPainter {
    /// The color to paint for the background-like `color` of an element with `style`.
    pub(crate) fn background(&self, style: &ComputedValues, color: AbsoluteColor) -> ColorF {
        self.background_rgba(style, rgba(color))
    }

    /// The color to paint for the `background-color` of an element with `style`. Under a
    /// repeated or covering image it stays as authored, like the text over the image.
    pub(crate) fn background_layer(&self, style: &ComputedValues, color: AbsoluteColor) -> ColorF {
        if has_covering_image(style) {
            return rgba(color);
        }
        self.background(style, color)
    }

    /// Like [`Self::background`], for a color already converted for WebRender.
    pub(crate) fn background_rgba(&self, style: &ComputedValues, color: ColorF) -> ColorF {
        if opts_out(style) || color.a == 0.0 {
            return color;
        }
        let Rgb([r, g, b]) = darken_background(Rgb::from_color_f(color));
        ColorF::new(r, g, b, color.a)
    }

    /// Whether to paint an inline `<svg>` of an element with `style` over `backdrop` with
    /// [`inverted_lightness_filter`]: it is drawn for a light backdrop that got darker.
    pub(crate) fn inverts_inline_svg(&self, style: &ComputedValues, backdrop: &Backdrop) -> bool {
        if opts_out(style) || backdrop.keeps_foregrounds() {
            return false;
        }
        let color = Rgb::from_color_f(rgba(style.get_inherited_text().clone_color()));
        color.luminance() < backdrop.authored.luminance()
    }

    /// The color to paint for the foreground `color` of an element with `style` over
    /// `backdrop`.
    pub(crate) fn foreground(
        &self,
        style: &ComputedValues,
        color: AbsoluteColor,
        backdrop: &Backdrop,
        min_contrast: f32,
    ) -> ColorF {
        let color = rgba(color);
        if opts_out(style) || backdrop.keeps_foregrounds() || color.a < 1.0 / 255.0 {
            return color;
        }

        let key = ForegroundKey {
            color: [color.r, color.g, color.b, color.a].map(quantize),
            authored: backdrop.authored.0.map(quantize),
            painted: backdrop.painted.0.map(quantize),
            min_contrast_tenths: (min_contrast * 10.0).round() as u8,
        };
        *self
            .foregrounds
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| adapt_foreground_key(key))
    }
}

fn adapt_foreground_key(key: ForegroundKey) -> ColorF {
    let [r, g, b, a] = key.color.map(dequantize);
    let authored = Rgb(key.authored.map(dequantize));
    let painted = Rgb(key.painted.map(dequantize));
    let min_contrast = f32::from(key.min_contrast_tenths) / 10.0;

    // A translucent foreground is judged by what it looks like over its backdrop, then
    // painted with the same alpha, so that it looks like the result over the new one.
    let seen = Rgb([r, g, b]).over(a, authored);
    let Rgb(target) = adapt_foreground(seen, authored, painted, min_contrast);
    let mut result = [0.0; 3];
    for (index, component) in result.iter_mut().enumerate() {
        *component = ((target[index] - painted.0[index] * (1.0 - a)) / a).clamp(0.0, 1.0);
    }
    ColorF::new(result[0], result[1], result[2], a)
}

/// A filter that inverts lightness and keeps hue (an inversion followed by a half turn of
/// hue), scaled so that white becomes the darkened canvas and black the lightest
/// foreground, as for page colors. Like CSS filters, it works on sRGB components.
pub(crate) fn inverted_lightness_filter() -> FilterOp {
    // The CSS `hue-rotate(180deg)` matrix, by rows.
    const HALF_TURN: [[f32; 3]; 3] = [
        [-0.574, 1.430, 0.144],
        [0.426, 0.430, 0.144],
        [0.426, 1.430, -0.856],
    ];
    let darkest = darken_background(DEFAULT_CANVAS).0[0];
    let lightest = Oklab {
        lightness: LIGHTEST_FOREGROUND_LIGHTNESS,
        a: 0.0,
        b: 0.0,
    }
    .to_rgb()
    .0[0];

    // out = lightest - (lightest - darkest) * HALF_TURN * in. WebRender reads the 4x4 part
    // column by column, then the offset.
    let mut matrix = [0.0; 20];
    for (output, row) in HALF_TURN.iter().enumerate() {
        for (input, coefficient) in row.iter().enumerate() {
            matrix[input * 4 + output] = -(lightest - darkest) * coefficient;
        }
        matrix[16 + output] = lightest;
    }
    matrix[15] = 1.0;
    FilterOp::ColorMatrix(matrix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(value: u32) -> Rgb {
        Rgb([
            ((value >> 16) & 0xff) as f32 / 255.0,
            ((value >> 8) & 0xff) as f32 / 255.0,
            (value & 0xff) as f32 / 255.0,
        ])
    }

    fn lightness(color: Rgb) -> f32 {
        color.to_oklab().lightness
    }

    fn hue(color: Rgb) -> f32 {
        let lab = color.to_oklab();
        lab.b.atan2(lab.a)
    }

    #[test]
    fn oklab_round_trips_srgb() {
        for value in [0x000000, 0xffffff, 0x0000ee, 0xffeb3b, 0x1a73e8, 0x777777] {
            let color = hex(value);
            let round_trip = color.to_oklab().to_rgb();
            assert!(color.is_close_to(round_trip), "{value:06x}: {round_trip:?}");
        }
    }

    #[test]
    fn white_becomes_a_dark_neutral_gray() {
        let painted = darken_background(hex(0xffffff));
        assert!((lightness(painted) - DARKEST_BACKGROUND_LIGHTNESS).abs() < 0.01);
        assert!((painted.0[0] - painted.0[1]).abs() < 0.01);
        assert!((painted.0[1] - painted.0[2]).abs() < 0.01);
    }

    #[test]
    fn darker_backgrounds_keep_their_color() {
        for value in [0x000000, 0x333333, 0x1a73e8, 0x7a7a7a] {
            assert_eq!(darken_background(hex(value)), hex(value), "{value:06x}");
        }
    }

    #[test]
    fn lighter_backgrounds_end_darker_and_keep_their_hue() {
        let panels = [0xffffff, 0xf5f5f5, 0xeeeeee, 0xdddddd, 0xcccccc];
        let painted: Vec<f32> = panels
            .iter()
            .map(|value| lightness(darken_background(hex(*value))))
            .collect();
        assert!(
            painted.windows(2).all(|pair| pair[0] < pair[1]),
            "{painted:?}"
        );

        let tint = hex(0xe3f2fd);
        let dark_tint = darken_background(tint);
        assert!(lightness(dark_tint) < 0.35);
        assert!((hue(dark_tint) - hue(tint)).abs() < 0.1);
    }

    #[test]
    fn darkening_is_continuous_at_the_threshold() {
        let lab = Oklab {
            lightness: BACKGROUND_LIGHTNESS_THRESHOLD + 0.005,
            a: 0.0,
            b: 0.0,
        };
        let color = lab.to_rgb();
        assert!((lightness(darken_background(color)) - lightness(color)).abs() < 0.02);
    }

    #[test]
    fn black_text_on_white_becomes_near_white_text() {
        let white = hex(0xffffff);
        let painted = darken_background(white);
        let text = adapt_foreground(hex(0x000000), white, painted, MIN_TEXT_CONTRAST);
        assert!((lightness(text) - LIGHTEST_FOREGROUND_LIGHTNESS).abs() < 0.01);
        assert!(contrast(text, painted) > 12.0);
    }

    #[test]
    fn secondary_text_stays_dimmer_than_primary_text() {
        let white = hex(0xffffff);
        let painted = darken_background(white);
        let primary = adapt_foreground(hex(0x222222), white, painted, MIN_TEXT_CONTRAST);
        let secondary = adapt_foreground(hex(0x666666), white, painted, MIN_TEXT_CONTRAST);
        assert!(contrast(secondary, painted) < contrast(primary, painted));
        // Authored at 5.7:1, the secondary text keeps about that.
        assert!((contrast(secondary, painted) - contrast(hex(0x666666), white)).abs() < 0.3);
    }

    #[test]
    fn low_contrast_text_reaches_the_minimum() {
        let white = hex(0xffffff);
        let painted = darken_background(white);
        let text = adapt_foreground(hex(0xaaaaaa), white, painted, MIN_TEXT_CONTRAST);
        assert!(contrast(text, painted) >= MIN_TEXT_CONTRAST - 0.05);
    }

    #[test]
    fn link_blue_stays_blue_and_readable() {
        let white = hex(0xffffff);
        let painted = darken_background(white);
        let link = adapt_foreground(hex(0x0000ee), white, painted, MIN_TEXT_CONTRAST);
        assert!(link.0[2] > link.0[0] && link.0[2] > link.0[1], "{link:?}");
        assert!(contrast(link, painted) >= contrast(hex(0x0000ee), white) - 0.3);
    }

    #[test]
    fn subtle_borders_stay_subtle() {
        let white = hex(0xffffff);
        let painted = darken_background(white);
        let border = adapt_foreground(hex(0xdddddd), white, painted, MIN_BORDER_CONTRAST);
        let authored = contrast(hex(0xdddddd), white);
        assert!(
            (contrast(border, painted) - authored).abs() < 0.1,
            "{border:?}"
        );
        assert!(lightness(border) > lightness(painted));
    }

    #[test]
    fn dark_text_on_a_backdrop_that_stays_light_stays_dark() {
        // A mid-light backdrop just above the threshold barely darkens.
        let authored = Oklab {
            lightness: BACKGROUND_LIGHTNESS_THRESHOLD + 0.02,
            a: 0.0,
            b: 0.0,
        }
        .to_rgb();
        let painted = darken_background(authored);
        let text = adapt_foreground(hex(0x111111), authored, painted, MIN_TEXT_CONTRAST);
        assert!(lightness(text) < lightness(painted));
    }

    #[test]
    fn foregrounds_over_an_unchanged_backdrop_keep_their_color() {
        let dark = hex(0x202124);
        let backdrop = Backdrop {
            authored: dark,
            painted: darken_background(dark),
            over_image: false,
        };
        assert!(backdrop.keeps_foregrounds());

        let image = Backdrop {
            authored: DEFAULT_CANVAS,
            painted: darken_background(DEFAULT_CANVAS),
            over_image: true,
        };
        assert!(image.keeps_foregrounds());
    }

    /// Applies a WebRender color matrix to an opaque color.
    fn apply(filter: FilterOp, color: Rgb) -> Rgb {
        let FilterOp::ColorMatrix(matrix) = filter else {
            unreachable!()
        };
        let mut result = [0.0; 3];
        for (output, component) in result.iter_mut().enumerate() {
            *component = matrix[16 + output] +
                (0..3)
                    .map(|input| matrix[input * 4 + output] * color.0[input])
                    .sum::<f32>();
        }
        Rgb(result.map(|component| component.clamp(0.0, 1.0)))
    }

    #[test]
    fn inverted_lightness_filter_matches_page_colors() {
        let filter = inverted_lightness_filter();
        let white = apply(filter, hex(0xffffff));
        assert!(
            white.is_close_to(darken_background(DEFAULT_CANVAS)),
            "{white:?}"
        );
        let black = apply(filter, hex(0x000000));
        assert!((lightness(black) - LIGHTEST_FOREGROUND_LIGHTNESS).abs() < 0.01);

        // A dark blue icon becomes a light blue one.
        let blue = apply(filter, hex(0x1a237e));
        assert!(lightness(blue) > 0.6, "{blue:?}");
        assert!(blue.0[2] > blue.0[0], "{blue:?}");
    }

    #[test]
    fn translucent_text_keeps_its_alpha_and_contrast() {
        let key = ForegroundKey {
            color: [0, 0, 0, 138], // rgba(0, 0, 0, 0.54)
            authored: [255, 255, 255],
            painted: darken_background(DEFAULT_CANVAS).0.map(quantize),
            min_contrast_tenths: 45,
        };
        let result = adapt_foreground_key(key);
        assert_eq!(quantize(result.a), 138);

        let painted = Rgb(key.painted.map(dequantize));
        let seen = Rgb([result.r, result.g, result.b]).over(result.a, painted);
        assert!(contrast(seen, painted) >= MIN_TEXT_CONTRAST - 0.1);
        assert!(lightness(seen) > lightness(painted));
    }
}
