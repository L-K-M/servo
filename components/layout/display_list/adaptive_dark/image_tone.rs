/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Which images adaptive dark mode paints with their lightness inverted.
//!
//! A logo or an icon drawn in dark colors for a light page would vanish on the page's
//! darkened background, and the white box around a black and white logo would glare on
//! it. Photos, illustrations and colorful artwork must keep their pixels. The difference
//! is read from a sample of the image's pixels:
//!
//! - how much of the image is transparent,
//! - how much of its opaque part is dark or light,
//! - whether that part uses only a few flat colors, as drawn artwork does and photos do
//!   not,
//! - whether its dark marks may be a code for a scanner.
//!
//! Anything unclear is kept: a dark logo that stays dark is a smaller problem than a photo
//! shown as its negative or a code that no longer scans. An animated image is kept too:
//! its later frames may show something else than its first.

use parking_lot::RwLock;
use pixels::{PixelFormat, RasterImage};
use rustc_hash::FxHashMap;
use webrender_api::ImageKey;

use super::Rgb;

/// What adaptive dark mode does with an image, judged from its pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImageTone {
    /// Painted as it is: a photo, light or colorful artwork, or anything unclear.
    Keep,
    /// Flat dark artwork on a transparent background, such as a logo or an icon drawn for
    /// a light page. On a darkened backdrop it is painted with its lightness inverted.
    DarkOnClear,
    /// A few flat dark marks on an opaque light background, such as a black and white
    /// logo. On a darkened backdrop it is painted with its lightness inverted, which
    /// turns its background into the backdrop's dark.
    DarkOnLight,
}

/// The tones of a document's images, by image key. Reading an image's pixels takes a
/// while and a display list is built often, so each image is read once.
#[derive(Default)]
pub(crate) struct ImageTones(RwLock<FxHashMap<ImageKey, ImageTone>>);

impl ImageTones {
    /// Forgetting every tone now and then keeps a long-lived document's map small.
    const MAX_IMAGES: usize = 4096;

    /// The tone of `image`, which WebRender knows as `image_key`.
    pub(crate) fn tone(&self, image_key: ImageKey, image: &RasterImage) -> ImageTone {
        if let Some(tone) = self.0.read().get(&image_key) {
            return *tone;
        }

        let tone = ImageTone::of(image);
        let mut tones = self.0.write();
        if tones.len() >= Self::MAX_IMAGES {
            tones.clear();
        }
        tones.insert(image_key, tone);
        tone
    }
}

/// Samples are taken on a grid of at most this many columns and rows.
const GRID: u32 = 32;

/// A sample with less alpha than this (of 255) counts as transparent.
const TRANSPARENT_ALPHA: u8 = 26;

/// A sample with at least this alpha (of 255) counts as fully opaque.
const OPAQUE_ALPHA: u8 = 250;

/// A sample with an OKLab lightness below this is dark.
const DARK_LIGHTNESS: f32 = 0.5;

/// A sample with an OKLab lightness above this is light.
const LIGHT_LIGHTNESS: f32 = 0.8;

/// A sample with an OKLab chroma above this has a color of its own; below, it is a gray.
const COLORFUL_CHROMA: f32 = 0.05;

/// Flat artwork needs at most this many colors (of 4096, four bits a channel) to cover
/// [`FLAT_COVERAGE`] of its opaque part. A photo needs many more.
const MAX_FLAT_COLORS: usize = 8;
const FLAT_COVERAGE: f32 = 0.9;

/// [`ImageTone::DarkOnClear`]: at least this share of the samples is transparent. Artwork
/// that fills its box, such as a dark panel with rounded corners, is not a glyph.
const MIN_CLEAR_SHARE: f32 = 0.4;

/// [`ImageTone::DarkOnClear`]: at least this share of the opaque part is dark.
const MIN_DARK_SHARE: f32 = 0.5;

/// [`ImageTone::DarkOnClear`]: at most this share of the opaque part is light. Artwork
/// with light parts of its own, such as a white outline, shows on any backdrop.
const MAX_LIGHT_SHARE: f32 = 0.1;

/// [`ImageTone::DarkOnLight`]: at most this share of the samples is transparent.
const MAX_CLEAR_SHARE: f32 = 0.02;

/// [`ImageTone::DarkOnLight`]: at least this share of the image is its light background.
const MIN_LIGHT_GROUND_SHARE: f32 = 0.6;

/// [`ImageTone::DarkOnLight`]: the dark marks take this share of the image. More is no
/// longer a few marks.
const MARKS_SHARE: std::ops::RangeInclusive<f32> = 0.005..=0.2;

/// [`ImageTone::DarkOnLight`]: marks may be a code for a scanner, which may not read it
/// inverted, when at least this share of the box around them is dark and at least
/// [`CODE_BUSYNESS`] of the neighboring samples along its rows or along its columns
/// differ. A bar code or a QR code is about half dark and changes at every other module;
/// its quiet zone, however large, is outside that box. Simple shapes have few such
/// changes and thin strokes little ink. Bold lettering can pass for a code and is kept.
const CODE_DENSITY: f32 = 0.25;
const CODE_BUSYNESS: f32 = 0.15;

/// [`ImageTone::DarkOnLight`]: at least this share of the image is either light or dark;
/// the rest is the blur between them.
const MIN_TWO_TONE_SHARE: f32 = 0.85;

/// [`ImageTone::DarkOnLight`]: at most this share of the image has a color of its own.
const MAX_COLORFUL_SHARE: f32 = 0.1;

/// [`ImageTone::DarkOnLight`]: at least this share of the samples on the image's edge is
/// opaque and light, so the image sits in a light box.
const MIN_LIGHT_EDGE_SHARE: f32 = 0.9;

/// [`ImageTone::DarkOnLight`]: larger images are scans, drawings and diagrams, not logos.
const MAX_LOGO_PIXELS: u64 = 512 * 512;

impl ImageTone {
    /// The tone of `image`.
    pub(crate) fn of(image: &RasterImage) -> Self {
        if image.should_animate() {
            return Self::Keep;
        }
        let Some(frame) = image.frame(0) else {
            return Self::Keep;
        };
        let bytes_per_pixel = match image.format {
            PixelFormat::K8 => 1,
            PixelFormat::KA8 => 2,
            PixelFormat::RGB8 => 3,
            PixelFormat::RGBA8 | PixelFormat::BGRA8 => 4,
        };
        let (width, height) = (frame.width, frame.height);
        let has_all_pixels = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
            .is_some_and(|length| length <= frame.bytes.len());
        if !has_all_pixels {
            return Self::Keep;
        }

        let format = image.format;
        classify(width, height, |x, y| {
            let offset = (y as usize * width as usize + x as usize) * bytes_per_pixel;
            let pixel = &frame.bytes[offset..offset + bytes_per_pixel];
            // Decoded and rasterized images have premultiplied alpha.
            match format {
                PixelFormat::K8 => [pixel[0], pixel[0], pixel[0], u8::MAX],
                PixelFormat::KA8 => unpremultiply([pixel[0], pixel[0], pixel[0], pixel[1]]),
                PixelFormat::RGB8 => [pixel[0], pixel[1], pixel[2], u8::MAX],
                PixelFormat::RGBA8 => unpremultiply([pixel[0], pixel[1], pixel[2], pixel[3]]),
                PixelFormat::BGRA8 => unpremultiply([pixel[2], pixel[1], pixel[0], pixel[3]]),
            }
        })
    }
}

fn unpremultiply([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    if a == 0 || a == u8::MAX {
        return [r, g, b, a];
    }
    let straight = |component: u8| (u32::from(component) * 255 / u32::from(a)).min(255) as u8;
    [straight(r), straight(g), straight(b), a]
}

/// The tone of an image of `width` by `height` pixels whose `pixel(x, y)` is a color with
/// straight alpha, as `[red, green, blue, alpha]`.
fn classify(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> ImageTone {
    if width == 0 || height == 0 {
        return ImageTone::Keep;
    }

    let (columns, rows) = (width.min(GRID), height.min(GRID));

    let mut clear = 0_u32;
    // The grid cells that hold the dark samples: first and last column, first and last row.
    let mut marks: Option<[u32; 4]> = None;
    // Shares of the opaque part, each sample weighted by its alpha.
    let (mut opaque, mut dark, mut light, mut colorful) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
    let mut colors: FxHashMap<u16, f32> = FxHashMap::default();
    let (mut edge, mut light_edge) = (0_u32, 0_u32);

    for row in 0..rows {
        let y = position(row, rows, height);
        for column in 0..columns {
            let x = position(column, columns, width);
            let [r, g, b, a] = pixel(x, y);
            let on_edge = row == 0 || column == 0 || row == rows - 1 || column == columns - 1;
            edge += u32::from(on_edge);
            if a < TRANSPARENT_ALPHA {
                clear += 1;
                continue;
            }

            let weight = f32::from(a) / 255.0;
            let lab = Rgb([r, g, b].map(|component| f32::from(component) / 255.0)).to_oklab();
            opaque += weight;
            if lab.lightness < DARK_LIGHTNESS {
                dark += weight;
                let [left, right, top, bottom] = marks.unwrap_or([column, column, row, row]);
                marks = Some([
                    left.min(column),
                    right.max(column),
                    top.min(row),
                    bottom.max(row),
                ]);
            }
            if lab.lightness > LIGHT_LIGHTNESS {
                light += weight;
                light_edge += u32::from(on_edge && a >= OPAQUE_ALPHA);
            }
            if lab.a.hypot(lab.b) > COLORFUL_CHROMA {
                colorful += weight;
            }
            let color = (u16::from(r >> 4) << 8) | (u16::from(g >> 4) << 4) | u16::from(b >> 4);
            *colors.entry(color).or_default() += weight;
        }
    }

    if opaque == 0.0 {
        return ImageTone::Keep;
    }
    let samples = (columns * rows) as f32;
    let clear_share = clear as f32 / samples;
    let (dark_share, light_share) = (dark / opaque, light / opaque);

    let mut weights: Vec<f32> = colors.into_values().collect();
    weights.sort_unstable_by(|a, b| b.total_cmp(a));
    let mut covered = 0.0;
    let flat_colors = weights
        .iter()
        .take_while(|weight| {
            let needs_more = covered < FLAT_COVERAGE * opaque;
            covered += **weight;
            needs_more
        })
        .count();
    if flat_colors > MAX_FLAT_COLORS {
        return ImageTone::Keep;
    }

    if clear_share >= MIN_CLEAR_SHARE &&
        dark_share >= MIN_DARK_SHARE &&
        light_share <= MAX_LIGHT_SHARE
    {
        return ImageTone::DarkOnClear;
    }

    let is_logo_sized = u64::from(width) * u64::from(height) <= MAX_LOGO_PIXELS;
    if is_logo_sized &&
        clear_share <= MAX_CLEAR_SHARE &&
        light_share >= MIN_LIGHT_GROUND_SHARE &&
        MARKS_SHARE.contains(&dark_share) &&
        dark_share + light_share >= MIN_TWO_TONE_SHARE &&
        colorful / opaque <= MAX_COLORFUL_SHARE &&
        light_edge as f32 >= MIN_LIGHT_EDGE_SHARE * edge as f32
    {
        // The pixels of the grid cells that hold the marks.
        let may_be_code = marks.is_some_and(|[left, right, top, bottom]| {
            let pixels = |cell: u32, cells: u32, length: u32| {
                (u64::from(cell) * u64::from(length) / u64::from(cells)) as u32
            };
            let (x, y) = (pixels(left, columns, width), pixels(top, rows, height));
            let marks_width = pixels(right + 1, columns, width) - x;
            let marks_height = pixels(bottom + 1, rows, height) - y;
            may_be_code(marks_width, marks_height, |column, row| {
                pixel(x + column, y + row)
            })
        });
        if !may_be_code {
            return ImageTone::DarkOnLight;
        }
    }

    ImageTone::Keep
}

/// The pixel in the middle of grid cell `index` of `cells` along `length` pixels.
fn position(index: u32, cells: u32, length: u32) -> u32 {
    ((2 * u64::from(index) + 1) * u64::from(length) / (2 * u64::from(cells))) as u32
}

/// Whether the dark marks in an area of `width` by `height` pixels that they span may be
/// a code for a scanner: see [`CODE_DENSITY`]. The area is sampled on a grid of its own,
/// so that a code shows its modules whatever part of the image it takes.
fn may_be_code(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> bool {
    if width == 0 || height == 0 {
        return false;
    }

    let (columns, rows) = (width.min(GRID), height.min(GRID));
    let mut dark = 0_u32;
    let (mut row_changes, mut column_changes) = (0_u32, 0_u32);
    // Whether the sample above the current one was dark.
    let mut above = [false; GRID as usize];
    for row in 0..rows {
        let y = position(row, rows, height);
        let mut before = false;
        for column in 0..columns {
            let [r, g, b, a] = pixel(position(column, columns, width), y);
            let color = Rgb([r, g, b].map(|component| f32::from(component) / 255.0));
            let is_dark = a >= TRANSPARENT_ALPHA && color.to_oklab().lightness < DARK_LIGHTNESS;
            dark += u32::from(is_dark);
            row_changes += u32::from(column > 0 && is_dark != before);
            column_changes += u32::from(row > 0 && is_dark != above[column as usize]);
            before = is_dark;
            above[column as usize] = is_dark;
        }
    }

    let share = |part: u32, whole: u32| part as f32 / whole.max(1) as f32;
    let density = share(dark, columns * rows);
    let busyness =
        share(row_changes, rows * (columns - 1)).max(share(column_changes, columns * (rows - 1)));
    density >= CODE_DENSITY && busyness >= CODE_BUSYNESS
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAR: [u8; 4] = [0, 0, 0, 0];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];

    /// A wordmark: bars of `ink` that cover `ink_share` of the width, on `ground`.
    fn bars(ink: [u8; 4], ground: [u8; 4], ink_share: f32) -> impl Fn(u32, u32) -> [u8; 4] {
        move |x, y| {
            let in_bar = (x % 20) as f32 / 20.0 < ink_share;
            // A margin of ground on every side, as around a logo.
            let in_margin = !(10..230).contains(&x) || !(10..62).contains(&y);
            if in_bar && !in_margin { ink } else { ground }
        }
    }

    fn tone(pixel: impl Fn(u32, u32) -> [u8; 4]) -> ImageTone {
        classify(240, 72, pixel)
    }

    #[test]
    fn dark_artwork_on_a_transparent_background_is_dark_on_clear() {
        assert_eq!(
            tone(bars([17, 17, 17, 255], CLEAR, 0.5)),
            ImageTone::DarkOnClear
        );
        // Dark colors count as well as grays: a navy logo is as hard to see.
        assert_eq!(
            tone(bars([16, 42, 92, 255], CLEAR, 0.5)),
            ImageTone::DarkOnClear
        );
        // A thin icon is mostly transparent.
        assert_eq!(tone(bars(BLACK, CLEAR, 0.1)), ImageTone::DarkOnClear);
    }

    #[test]
    fn soft_edges_do_not_change_the_tone() {
        // Half-transparent ink between solid ink and nothing, as anti-aliasing leaves.
        let soft = |x: u32, y: u32| match (x % 20, (10..62).contains(&y)) {
            (0..=7, true) => [20, 20, 20, 255],
            (8..=9, true) => [20, 20, 20, 128],
            _ => CLEAR,
        };
        assert_eq!(tone(soft), ImageTone::DarkOnClear);
    }

    #[test]
    fn light_and_colorful_artwork_is_kept() {
        // A white logo is drawn for a dark backdrop.
        assert_eq!(tone(bars(WHITE, CLEAR, 0.5)), ImageTone::Keep);
        // A brand color of middle lightness shows on light and dark backdrops.
        assert_eq!(tone(bars([214, 51, 108, 255], CLEAR, 0.5)), ImageTone::Keep);
        // Dark artwork with a white outline shows on any backdrop.
        let outlined = |x: u32, y: u32| match (x % 20, (10..62).contains(&y)) {
            (0..=5, true) => BLACK,
            (6..=8, true) => WHITE,
            _ => CLEAR,
        };
        assert_eq!(tone(outlined), ImageTone::Keep);
    }

    #[test]
    fn dark_artwork_that_fills_its_box_is_kept() {
        // A dark panel with transparent corners carries its own light content.
        let panel = |x: u32, y: u32| {
            let in_corner = (x < 12 || x >= 228) && (y < 12 || y >= 60);
            if in_corner { CLEAR } else { [32, 33, 36, 255] }
        };
        assert_eq!(tone(panel), ImageTone::Keep);
        assert_eq!(tone(|_, _| BLACK), ImageTone::Keep);
    }

    #[test]
    fn an_empty_or_transparent_image_is_kept() {
        assert_eq!(classify(0, 0, |_, _| BLACK), ImageTone::Keep);
        assert_eq!(tone(|_, _| CLEAR), ImageTone::Keep);
        // Ink too faint to count is transparent too.
        assert_eq!(tone(|_, _| [0, 0, 0, 10]), ImageTone::Keep);
    }

    /// A stand-in for a photo: smooth, with many colors.
    fn photo(x: u32, y: u32) -> [u8; 4] {
        let (u, v) = (x as f32 / 240.0, y as f32 / 72.0);
        let wave = |phase: f32| (127.5 + 127.5 * (phase * 6.3).sin()) as u8;
        [wave(u + 0.2), wave(v + 0.5 * u), wave(u + v), 255]
    }

    #[test]
    fn photos_are_kept() {
        assert_eq!(tone(photo), ImageTone::Keep);
        // A dark photo cut out of its background has many dark colors, not a few.
        let cutout = |x: u32, y: u32| {
            if (x / 4 + y / 4) % 2 == 0 {
                return CLEAR;
            }
            let [r, g, b, a] = photo(x, y);
            [r / 3, g / 3, b / 3, a]
        };
        assert_eq!(tone(cutout), ImageTone::Keep);
        // A gray photo on a light background has more than two tones.
        let gray = |x: u32, y: u32| {
            let level = if x < 60 { 250 } else { (x + y) as u8 };
            [level, level, level, 255]
        };
        assert_eq!(tone(gray), ImageTone::Keep);
    }

    #[test]
    fn a_few_dark_marks_on_a_light_background_are_dark_on_light() {
        assert_eq!(tone(bars(BLACK, WHITE, 0.15)), ImageTone::DarkOnLight);
        // Not quite black on not quite white, as a compressed image has.
        assert_eq!(
            tone(bars([24, 22, 20, 255], [250, 251, 249, 255], 0.15)),
            ImageTone::DarkOnLight
        );
    }

    #[test]
    fn other_opaque_images_are_kept() {
        // A light texture has no marks.
        let texture = |x: u32, y: u32| {
            if (x + y) % 8 == 0 {
                [228, 222, 208, 255]
            } else {
                WHITE
            }
        };
        assert_eq!(tone(texture), ImageTone::Keep);
        // Half dark, as a bar code or a QR code is: a scanner may not read it inverted.
        assert_eq!(tone(bars(BLACK, WHITE, 0.5)), ImageTone::Keep);
        // Colored marks keep their meaning only with their colors.
        assert_eq!(tone(bars([200, 30, 30, 255], WHITE, 0.15)), ImageTone::Keep);
        // Marks that run to the edge are part of a larger drawing.
        let dark_edge = |x: u32, _y: u32| if x < 30 { BLACK } else { WHITE };
        assert_eq!(tone(dark_edge), ImageTone::Keep);
        // A large image is a scan or a drawing, not a logo.
        let logo = bars(BLACK, WHITE, 0.15);
        assert_eq!(
            classify(1200, 360, |x, y| logo(x / 5, y / 5)),
            ImageTone::Keep
        );
        assert_eq!(
            classify(480, 144, |x, y| logo(x / 2, y / 2)),
            ImageTone::DarkOnLight
        );
    }

    /// Whether module `(x, y)` of a code is dark: about every other one, without a pattern.
    fn module_is_dark(x: u32, y: u32) -> bool {
        let mut hash = x.wrapping_mul(0x9e37_79b1) ^ y.wrapping_mul(0x85eb_ca6b);
        hash = (hash ^ (hash >> 16)).wrapping_mul(0x7feb_352d);
        hash = (hash ^ (hash >> 15)).wrapping_mul(0x846c_a68b);
        (hash ^ (hash >> 16)) & 1 == 1
    }

    /// A matrix code such as a QR code, `modules` modules a side, in the middle of a white
    /// image `size` pixels a side. It takes `fill` of the side; the rest is its quiet zone.
    fn matrix_code(size: u32, modules: u32, fill: f32) -> impl Fn(u32, u32) -> [u8; 4] {
        let side = (size as f32 * fill) as u32;
        let start = (size - side) / 2;
        move |x, y| {
            let in_code = (start..start + side).contains(&x) && (start..start + side).contains(&y);
            let module = |position: u32| (position - start) * modules / side;
            if in_code && module_is_dark(module(x), module(y)) {
                BLACK
            } else {
                WHITE
            }
        }
    }

    /// A bar code of `modules` modules across the middle of a white image of `width` by
    /// `height` pixels. Its bars take `fill` of the width and half of the height.
    fn bar_code(width: u32, height: u32, modules: u32, fill: f32) -> impl Fn(u32, u32) -> [u8; 4] {
        let length = (width as f32 * fill) as u32;
        let start = (width - length) / 2;
        move |x, y| {
            let in_code =
                (start..start + length).contains(&x) && (height / 4..height * 3 / 4).contains(&y);
            if in_code && module_is_dark((x - start) * modules / length, 0) {
                BLACK
            } else {
                WHITE
            }
        }
    }

    #[test]
    fn codes_for_a_scanner_are_kept() {
        // Whatever its quiet zone leaves of the image, and however many modules it has.
        for fill in [0.35, 0.49, 0.59, 0.8, 1.0] {
            for modules in [21, 33, 57, 97] {
                for size in [160, 300, 512] {
                    assert_eq!(
                        classify(size, size, matrix_code(size, modules, fill)),
                        ImageTone::Keep,
                        "matrix code of {modules} modules, {fill} of {size} pixels"
                    );
                }
            }
            for modules in [46, 95, 143] {
                for (width, height) in [(300, 120), (512, 300), (240, 240)] {
                    assert_eq!(
                        classify(width, height, bar_code(width, height, modules, fill)),
                        ImageTone::Keep,
                        "bar code of {modules} modules, {fill} of {width} by {height} pixels"
                    );
                }
            }
        }
        // On its side.
        let upright = bar_code(512, 300, 95, 0.6);
        assert_eq!(classify(300, 512, |x, y| upright(y, x)), ImageTone::Keep);
    }

    #[test]
    fn simple_marks_are_not_taken_for_a_code() {
        // A ring and a bar, as a logo: dense where they are, but with few edges.
        let logo = |x: u32, y: u32| {
            let (dx, dy) = (x as f32 - 60.0, y as f32 - 60.0);
            let in_ring = (30.0..42.0).contains(&dx.hypot(dy));
            let in_bar = (120..210).contains(&x) && (48..72).contains(&y);
            if in_ring || in_bar { BLACK } else { WHITE }
        };
        assert_eq!(classify(240, 120, logo), ImageTone::DarkOnLight);
        // Thin strokes, as handwriting or a line drawing has: many edges, but little ink.
        let strokes = |x: u32, y: u32| {
            let in_margin = !(20..220).contains(&x) || !(20..100).contains(&y);
            let on_stroke = (x + 2 * y) % 24 < 3 || (3 * x + 200 - y) % 40 < 3;
            if on_stroke && !in_margin {
                BLACK
            } else {
                WHITE
            }
        };
        assert_eq!(classify(240, 120, strokes), ImageTone::DarkOnLight);
    }

    #[test]
    fn premultiplied_colors_are_read_as_straight_ones() {
        assert_eq!(unpremultiply([64, 32, 0, 128]), [127, 63, 0, 128]);
        assert_eq!(unpremultiply([0, 0, 0, 0]), [0, 0, 0, 0]);
        assert_eq!(unpremultiply([10, 20, 30, 255]), [10, 20, 30, 255]);
    }

    /// An image of 8 by 8 pixels with `frames` equal frames: black on the left half,
    /// transparent on the right.
    fn half_black_image(frames: usize) -> RasterImage {
        let frame: Vec<u8> = (0..64)
            .flat_map(|index| if index % 8 < 4 { BLACK } else { CLEAR })
            .collect();
        RasterImage {
            metadata: pixels::ImageMetadata {
                width: 8,
                height: 8,
            },
            format: PixelFormat::RGBA8,
            id: None,
            cors_status: pixels::CorsStatus::Safe,
            bytes: std::sync::Arc::new(frame.repeat(frames)),
            frames: (0..frames)
                .map(|index| pixels::ImageFrame {
                    delay: None,
                    byte_range: index * frame.len()..(index + 1) * frame.len(),
                    width: 8,
                    height: 8,
                })
                .collect(),
            is_opaque: false,
            loop_count: None,
        }
    }

    #[test]
    fn an_animated_image_is_kept() {
        assert_eq!(ImageTone::of(&half_black_image(1)), ImageTone::DarkOnClear);
        // Its other frames may show something else, and it has one tone for all of them.
        assert_eq!(ImageTone::of(&half_black_image(2)), ImageTone::Keep);
    }

    #[test]
    fn small_images_are_sampled_whole() {
        // Fewer pixels than the grid has cells: each is read once.
        let icon = |x: u32, y: u32| if x == y { BLACK } else { CLEAR };
        assert_eq!(classify(8, 8, icon), ImageTone::DarkOnClear);
        assert_eq!(classify(1, 1, |_, _| BLACK), ImageTone::Keep);
    }
}
