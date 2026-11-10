/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Adaptive dark mode: when the embedder asks for it, a light page is painted with dark
//! colors while its styles stay as authored.
//!
//! Only colors on their way into the display list change. Scripts keep reading the
//! authored colors from computed styles; photos, video and canvas keep their pixels.
//! Pages and elements that declare dark support (`color-scheme: dark` or `light dark`) or
//! forbid any change (`color-scheme: light only`) are painted as authored: for a page, its
//! root or body declares it. An element that declares it keeps its backgrounds, and its
//! foregrounds follow what is painted behind them like any other: if it paints no
//! background of its own over a darkened one, its text still gets lighter, to stay readable.
//!
//! Colors change by role:
//!
//! - Backgrounds (background colors, gradient stops, including those of a `border-image`,
//!   shadows, the selection highlight): a light color gets darker, keeping its hue, so
//!   white becomes a dark gray and pale tints become deep ones. Mid and dark colors stay as
//!   they are. Lighter colors end darker, so a slightly raised panel stays slightly
//!   distinct.
//! - Foregrounds (text, its decorations, the caret, borders and outlines) follow what is
//!   painted behind them, the backdrop: the background colors below them, and the average
//!   color stop of gradients that fill a background. A border is over its box's own
//!   background unless `background-clip` stops that short of it. The backdrop follows
//!   ancestors only: a positioned element over a sibling's background is judged against
//!   what is behind both. Where the backdrop kept its color,
//!   so does the foreground. Where it changed, the foreground keeps its hue and gets the
//!   lightness that restores its authored contrast against the new backdrop, so black
//!   body text becomes near-white and gray secondary text a dimmer gray, as written. Text
//!   and carets get at least 4.5:1 (WCAG AA), outlines 3:1.
//! - Areas painted with a repeated or covering image (a texture, a photo) keep their
//!   authored colors, and so does the text over them: the image's lightness is unknown.
//! - An inline `<svg>` is rasterized with its colors, so a dark icon drawn in the text
//!   color would vanish on a darkened backdrop. One whose `color` is darker than the
//!   authored backdrop gets its lightness inverted, keeping hue, like the text around it.
//! - An image (in an `<img>`, or a background image that does not fill its area, such as an
//!   icon) is judged by its pixels, see [`ImageTone`]: a dark logo or icon on a
//!   transparent background and a black and white logo on a light one get their lightness
//!   inverted where the backdrop got dark. Photos, colorful artwork, video and `<canvas>`
//!   keep their pixels.
//!
//! Lightness is OKLab's: perceptually even, with hue kept in its `a` and `b` axes.

use std::cell::RefCell;

use rustc_hash::FxHashMap;
use style::color::{AbsoluteColor, ColorSpace};
use style::computed_values::background_clip::single_value::T as BackgroundClip;
use style::properties::ComputedValues;
use style::values::computed::Color;
use style::values::computed::background::BackgroundSize;
use style::values::computed::image::{Gradient, Image};
use style::values::generics::image::GradientItem;
use style::values::specified::background::BackgroundRepeatKeyword;
use style::values::specified::color::ColorSchemeFlags;
use webrender_api::{ColorF, FilterOp, GradientStop};

use super::background::get_cyclic;

mod image_tone;

pub(crate) use image_tone::{ImageTone, ImageTones};

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

/// Text with less contrast than this against its authored backdrop could not be read
/// there. It was drawn for something the backdrop does not know about, such as a picture
/// positioned behind it, and keeps its color.
const UNREADABLE_CONTRAST: f32 = 1.25;

/// Borders keep their authored contrast, however low.
pub(crate) const MIN_BORDER_CONTRAST: f32 = 1.0;

/// The canvas color of a page that paints no background of its own.
const DEFAULT_CANVAS: Rgb = Rgb([1.0, 1.0, 1.0]);

/// Colors this close (in sRGB components) count as the same, so a backdrop that a
/// darkening left (nearly) unchanged leaves its foregrounds alone.
const SAME_COLOR_TOLERANCE: f32 = 1.0 / 255.0;

/// A backdrop with a WCAG luminance below this is dark: light artwork shows better on it
/// than dark artwork does. White and black contrast equally with a luminance of 0.179.
const DARK_BACKDROP_LUMINANCE: f32 = 0.18;

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
/// authored contrast, at least `min_contrast`, as far as `painted` allows. Text that had
/// next to no contrast keeps its color, see [`UNREADABLE_CONTRAST`].
fn adapt_foreground(color: Rgb, authored: Rgb, painted: Rgb, min_contrast: f32) -> Rgb {
    let authored_contrast = contrast(color, authored);
    if min_contrast >= MIN_TEXT_CONTRAST && authored_contrast < UNREADABLE_CONTRAST {
        return color;
    }

    let target = authored_contrast.max(min_contrast);
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
    /// the propagated body's background) over the darkened default canvas. At the top level,
    /// [`AdaptiveDarkPainter::canvas_underlay`] paints that default canvas where needed.
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

    /// The backdrop of a border of a box with `style`, painted over `self`: the box's
    /// background when it extends under the border (`background-clip: border-box`, the
    /// default), otherwise what is behind the box.
    pub(crate) fn behind_border(&self, style: &ComputedValues) -> Self {
        let background = style.get_background();
        // The background color is clipped like the bottom layer.
        let bottom_layer = background.background_image.0.len().saturating_sub(1);
        match get_cyclic(&background.background_clip.0, bottom_layer) {
            BackgroundClip::BorderBox | BackgroundClip::BorderArea => self.behind(style),
            BackgroundClip::PaddingBox | BackgroundClip::ContentBox => *self,
        }
    }

    /// The backdrop of the content of a box with `style`, painted over `self`: its
    /// background color, then the gradients that fill its background.
    pub(crate) fn behind(&self, style: &ComputedValues) -> Self {
        if has_covering_image(style) {
            return Self {
                over_image: true,
                ..*self
            };
        }

        let background = style.get_background();
        let keeps_colors = opts_out(style);
        let mut backdrop = self.with_layer(
            rgba(style.resolve_color(&background.background_color)),
            keeps_colors,
        );
        // The first layer is painted last, on top.
        for (index, image) in background.background_image.0.iter().enumerate().rev() {
            if let Image::Gradient(gradient) = image &&
                fills_background_area(style, index, image)
            {
                backdrop = backdrop.with_gradient(style, gradient, keeps_colors);
            }
        }
        backdrop
    }

    /// This backdrop with a layer of `color` painted over it, as authored if `keeps_colors`.
    fn with_layer(&self, color: ColorF, keeps_colors: bool) -> Self {
        if color.a < MIN_BACKDROP_ALPHA {
            return *self;
        }

        let authored_color = Rgb::from_color_f(color);
        let painted_color = if keeps_colors {
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

    /// This backdrop with `gradient` painted over it. What is behind a piece of text
    /// depends on where it sits in the gradient, so this takes the average over the color
    /// stops.
    fn with_gradient(
        &self,
        style: &ComputedValues,
        gradient: &Gradient,
        keeps_colors: bool,
    ) -> Self {
        match gradient {
            Gradient::Linear { items, .. } | Gradient::Radial { items, .. } => {
                self.with_stops(style, items, keeps_colors)
            },
            Gradient::Conic { items, .. } => self.with_stops(style, items, keeps_colors),
        }
    }

    fn with_stops<T>(
        &self,
        style: &ComputedValues,
        items: &[GradientItem<Color, T>],
        keeps_colors: bool,
    ) -> Self {
        let layers: Vec<Self> = items
            .iter()
            .filter_map(|item| match item {
                GradientItem::SimpleColorStop(color) |
                GradientItem::ComplexColorStop { color, .. } => {
                    Some(self.with_layer(rgba(style.resolve_color(color)), keeps_colors))
                },
                GradientItem::InterpolationHint(_) => None,
            })
            .collect();
        if layers.is_empty() {
            return *self;
        }

        let average = |color: fn(&Self) -> Rgb| {
            let mut sum = [0.0; 3];
            for layer in &layers {
                for (total, component) in sum.iter_mut().zip(color(layer).0) {
                    *total += component;
                }
            }
            Rgb(sum.map(|total| total / layers.len() as f32))
        };
        Self {
            authored: average(|layer| layer.authored),
            painted: average(|layer| layer.painted),
            over_image: layers.iter().any(|layer| layer.over_image),
        }
    }

    /// This backdrop with an image painted over it that kept its pixels.
    pub(crate) fn under_image(self) -> Self {
        Self {
            over_image: true,
            ..self
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
    style
        .get_background()
        .background_image
        .0
        .iter()
        .enumerate()
        .any(|(index, image)| {
            matches!(image, Image::Url(..) | Image::ImageSet(..)) &&
                fills_background_area(style, index, image)
        })
}

/// Whether `image`, the background layer `index` of `style`, fills the background area: it
/// covers it or repeats in both directions. A gradient has no size of its own, so `auto` and
/// `contain` stretch it over the area too.
fn fills_background_area(style: &ComputedValues, index: usize, image: &Image) -> bool {
    let background = style.get_background();
    let size = &background.background_size.0[index % background.background_size.0.len()];
    let stretched = match size {
        BackgroundSize::Cover => true,
        BackgroundSize::Contain => matches!(image, Image::Gradient(..)),
        BackgroundSize::ExplicitSize { width, height } => {
            matches!(image, Image::Gradient(..)) && width.is_auto() && height.is_auto()
        },
    };
    let repeat = &background.background_repeat.0[index % background.background_repeat.0.len()];
    stretched ||
        (repeat.0 != BackgroundRepeatKeyword::NoRepeat &&
            repeat.1 != BackgroundRepeatKeyword::NoRepeat)
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

    /// Adapts the color `stops` of a gradient of an element with `style`. Under a repeated
    /// or covering image they stay as authored, like the background color.
    pub(crate) fn gradient_stops(&self, style: &ComputedValues, stops: &mut [GradientStop]) {
        if has_covering_image(style) {
            return;
        }
        for stop in stops {
            stop.color = self.background_rgba(style, stop.color);
        }
    }

    /// The color to paint under a top-level canvas painted from `canvas_style`, if any: the
    /// darkened default canvas, so that the white the embedder clears to does not show
    /// through a canvas color that is not opaque. An image that covers the canvas keeps the
    /// authored white, like the text over it.
    pub(crate) fn canvas_underlay(&self, canvas_style: &ComputedValues) -> Option<ColorF> {
        let color =
            rgba(canvas_style.resolve_color(&canvas_style.get_background().background_color));
        if color.a >= 1.0 || has_covering_image(canvas_style) {
            return None;
        }
        Some(Backdrop::blank_canvas_color())
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
        if backdrop.keeps_foregrounds() {
            return false;
        }
        let color = Rgb::from_color_f(rgba(style.get_inherited_text().clone_color()));
        color.luminance() < backdrop.authored.luminance()
    }

    /// Whether to paint an image over `backdrop` with [`inverted_lightness_filter`]: its
    /// `tone` says that it is drawn for a light backdrop, which got dark. `tone` reads the
    /// image's pixels, so it is asked for last.
    pub(crate) fn inverts_image(
        &self,
        backdrop: &Backdrop,
        tone: impl FnOnce() -> ImageTone,
    ) -> bool {
        !backdrop.keeps_foregrounds() &&
            backdrop.painted.luminance() < DARK_BACKDROP_LUMINANCE &&
            tone() != ImageTone::Keep
    }

    /// Like [`Self::inverts_image`], for the image of background layer `index` of an
    /// element with `style`, where `backdrop` is behind the element. An image that fills
    /// its area keeps its pixels, as the text over it keeps its colors.
    pub(crate) fn inverts_background_image(
        &self,
        style: &ComputedValues,
        index: usize,
        backdrop: &Backdrop,
        tone: impl FnOnce() -> ImageTone,
    ) -> bool {
        let fills = style
            .get_background()
            .background_image
            .0
            .get(index)
            .is_none_or(|image| fills_background_area(style, index, image));
        // The image is painted over the element's own background color.
        !fills && self.inverts_image(&backdrop.behind(style), tone)
    }

    /// The color to paint for a foreground `color` over `backdrop`. This does not depend on
    /// whether the element opts out: one that paints its own background keeps its colors
    /// through the backdrop, and one that does not needs readable text over a darkened one.
    pub(crate) fn foreground(
        &self,
        color: AbsoluteColor,
        backdrop: &Backdrop,
        min_contrast: f32,
    ) -> ColorF {
        let color = rgba(color);
        if backdrop.keeps_foregrounds() || color.a < 1.0 / 255.0 {
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
    fn text_that_was_unreadable_on_its_backdrop_keeps_its_color() {
        let white = hex(0xffffff);
        let painted = darken_background(white);
        // White text on a white page sits on a picture the backdrop does not know about.
        for value in [0xffffff, 0xf4f4f4] {
            let text = adapt_foreground(hex(value), white, painted, MIN_TEXT_CONTRAST);
            assert_eq!(text, hex(value), "{value:06x}");
        }

        // Faint text that could be read is still dimmed, to stay faint.
        let faint = adapt_foreground(hex(0xbbbbbb), white, painted, MIN_TEXT_CONTRAST);
        assert!(lightness(faint) < lightness(hex(0xbbbbbb)), "{faint:?}");

        // A border in its background's color stays hidden.
        let border = adapt_foreground(white, white, painted, MIN_BORDER_CONTRAST);
        assert!(contrast(border, painted) < 1.1, "{border:?}");
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
            min_contrast_tenths: (MIN_TEXT_CONTRAST * 10.0).round() as u8,
        };
        let result = adapt_foreground_key(key);
        assert_eq!(quantize(result.a), 138);

        let painted = Rgb(key.painted.map(dequantize));
        let seen = Rgb([result.r, result.g, result.b]).over(result.a, painted);
        assert!(contrast(seen, painted) >= MIN_TEXT_CONTRAST - 0.1);
        assert!(lightness(seen) > lightness(painted));
    }
}
