/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! What adaptive dark mode remembers of a page while it paints it, to tell content that
//! lies over a picture from content that lies over the page.
//!
//! An opaque image that keeps its pixels is the backdrop of what is painted over it later:
//! a label or an icon positioned over a photo was drawn for the photo, not for the page
//! behind both. A background color painted over the image hides it again.
//!
//! Every text run, image and background of a page asks about these areas, so they are
//! indexed by horizontal bands: a lookup compares the areas near it, not all of a long
//! page's.

use rustc_hash::FxHashMap;
use servo_base::id::ScrollTreeNodeId;
use webrender_api::units::{LayoutPoint, LayoutRect};

/// The height of a band, in CSS pixels.
const BAND_HEIGHT: f32 = 256.0;

/// An area that spans more bands than this is listed apart, where every lookup reads it.
/// Few areas are this tall, and listing one in each of its bands would cost more.
const MAX_BANDS: i32 = 32;

struct PaintedArea {
    spatial_id: ScrollTreeNodeId,
    rect: LayoutRect,
    is_kept_image: bool,
}

/// The areas of a display list that decide what later content is painted over, in paint
/// order. Only areas that scroll together are compared; gradients and rounded corners are
/// not considered.
#[derive(Default)]
pub(crate) struct PaintedAreas {
    areas: Vec<PaintedArea>,
    /// For each scroll node (by its index) and band, the areas in that band as indices
    /// into `areas`, ascending.
    bands: FxHashMap<(usize, i32), Vec<usize>>,
    /// The areas that span more than [`MAX_BANDS`], as indices into `areas`, ascending.
    tall: Vec<usize>,
}

impl PaintedAreas {
    /// Remembers that an opaque image kept its pixels in `rect`.
    pub(crate) fn note_kept_image(&mut self, spatial_id: ScrollTreeNodeId, rect: LayoutRect) {
        self.push(PaintedArea {
            spatial_id,
            rect,
            is_kept_image: true,
        });
    }

    /// Remembers that a background that hides what is under it was painted in `rect`, if
    /// that is over an image that kept its pixels. Other backgrounds change nothing.
    pub(crate) fn note_covering_background(
        &mut self,
        spatial_id: ScrollTreeNodeId,
        rect: LayoutRect,
    ) {
        if self.is_over_any_kept_image(spatial_id, &rect) {
            self.push(PaintedArea {
                spatial_id,
                rect,
                is_kept_image: false,
            });
        }
    }

    /// Whether what was painted last at `point` is an image that kept its pixels.
    pub(crate) fn is_over_kept_image(
        &self,
        spatial_id: ScrollTreeNodeId,
        point: LayoutPoint,
    ) -> bool {
        let contains = |index: &&usize| {
            let area = &self.areas[**index];
            area.spatial_id == spatial_id && area.rect.contains(point)
        };
        let in_band = self
            .bands
            .get(&(spatial_id.index, band_of(point.y)))
            .and_then(|indices| indices.iter().rev().find(contains));
        let tall = self.tall.iter().rev().find(contains);
        // The later of the two was painted over the other.
        in_band
            .max(tall)
            .is_some_and(|index| self.areas[*index].is_kept_image)
    }

    fn is_over_any_kept_image(&self, spatial_id: ScrollTreeNodeId, rect: &LayoutRect) -> bool {
        if self.areas.is_empty() || rect.is_empty() {
            return false;
        }
        let is_kept_image_under = |index: &usize| {
            let area = &self.areas[*index];
            area.is_kept_image && area.spatial_id == spatial_id && area.rect.intersects(rect)
        };
        if self.tall.iter().any(is_kept_image_under) {
            return true;
        }
        match bands_of(rect) {
            Some(bands) => bands.into_iter().any(|band| {
                self.bands
                    .get(&(spatial_id.index, band))
                    .is_some_and(|indices| indices.iter().any(is_kept_image_under))
            }),
            // Taller than the bands are meant for, such as a page's own background.
            None => (0..self.areas.len()).any(|index| is_kept_image_under(&index)),
        }
    }

    fn push(&mut self, area: PaintedArea) {
        if area.rect.is_empty() {
            return;
        }
        let index = self.areas.len();
        match bands_of(&area.rect) {
            Some(bands) => {
                for band in bands {
                    self.bands
                        .entry((area.spatial_id.index, band))
                        .or_default()
                        .push(index);
                }
            },
            None => self.tall.push(index),
        }
        self.areas.push(area);
    }
}

/// The band that the vertical position `y` lies in.
fn band_of(y: f32) -> i32 {
    // The cast saturates, and yields 0 for a position that is not a number.
    (y / BAND_HEIGHT).floor() as i32
}

/// The bands that `rect` touches, or `None` if those are more than [`MAX_BANDS`].
fn bands_of(rect: &LayoutRect) -> Option<std::ops::RangeInclusive<i32>> {
    let (first, last) = (band_of(rect.min.y), band_of(rect.max.y));
    (last.saturating_sub(first) < MAX_BANDS).then_some(first..=last)
}

#[cfg(test)]
mod tests {
    use webrender_api::units::LayoutSize;

    use super::*;

    const NODE: ScrollTreeNodeId = ScrollTreeNodeId { index: 1 };
    const OTHER_NODE: ScrollTreeNodeId = ScrollTreeNodeId { index: 2 };

    fn rect(x: f32, y: f32, width: f32, height: f32) -> LayoutRect {
        LayoutRect::from_origin_and_size(LayoutPoint::new(x, y), LayoutSize::new(width, height))
    }

    fn point(x: f32, y: f32) -> LayoutPoint {
        LayoutPoint::new(x, y)
    }

    #[test]
    fn content_over_a_kept_image_is_over_it() {
        let mut areas = PaintedAreas::default();
        assert!(!areas.is_over_kept_image(NODE, point(50.0, 50.0)));

        areas.note_kept_image(NODE, rect(0.0, 0.0, 100.0, 100.0));
        assert!(areas.is_over_kept_image(NODE, point(50.0, 50.0)));
        assert!(!areas.is_over_kept_image(NODE, point(150.0, 50.0)));
        assert!(!areas.is_over_kept_image(NODE, point(50.0, 150.0)));
        // Another scroll node moves on its own: its coordinates are not comparable.
        assert!(!areas.is_over_kept_image(OTHER_NODE, point(50.0, 50.0)));
    }

    #[test]
    fn a_background_painted_over_an_image_hides_it() {
        let mut areas = PaintedAreas::default();
        areas.note_kept_image(NODE, rect(0.0, 0.0, 100.0, 100.0));
        areas.note_covering_background(NODE, rect(20.0, 20.0, 40.0, 40.0));

        assert!(!areas.is_over_kept_image(NODE, point(30.0, 30.0)));
        assert!(areas.is_over_kept_image(NODE, point(80.0, 80.0)));

        // An image painted over that background shows again.
        areas.note_kept_image(NODE, rect(25.0, 25.0, 10.0, 10.0));
        assert!(areas.is_over_kept_image(NODE, point(30.0, 30.0)));
    }

    #[test]
    fn a_background_that_is_not_over_an_image_is_not_remembered() {
        let mut areas = PaintedAreas::default();
        areas.note_covering_background(NODE, rect(0.0, 0.0, 100.0, 100.0));
        areas.note_kept_image(NODE, rect(200.0, 0.0, 100.0, 100.0));
        areas.note_covering_background(NODE, rect(0.0, 0.0, 100.0, 100.0));
        areas.note_covering_background(OTHER_NODE, rect(200.0, 0.0, 100.0, 100.0));
        areas.note_covering_background(NODE, rect(0.0, 0.0, 0.0, 100.0));

        assert_eq!(areas.areas.len(), 1);
        assert!(areas.is_over_kept_image(NODE, point(250.0, 50.0)));
    }

    #[test]
    fn areas_of_any_height_and_position_are_found() {
        let mut areas = PaintedAreas::default();
        // Across a band's edge, above the origin, and taller than the bands hold.
        areas.note_kept_image(NODE, rect(0.0, BAND_HEIGHT - 10.0, 100.0, 20.0));
        areas.note_kept_image(NODE, rect(0.0, -1000.0, 100.0, 100.0));
        let page_height = BAND_HEIGHT * (MAX_BANDS as f32) * 4.0;
        areas.note_kept_image(NODE, rect(500.0, 0.0, 100.0, page_height));
        assert_eq!(areas.tall.len(), 1);

        assert!(areas.is_over_kept_image(NODE, point(50.0, BAND_HEIGHT - 5.0)));
        assert!(areas.is_over_kept_image(NODE, point(50.0, BAND_HEIGHT + 5.0)));
        assert!(areas.is_over_kept_image(NODE, point(50.0, -950.0)));
        assert!(areas.is_over_kept_image(NODE, point(550.0, page_height - 1.0)));
        assert!(!areas.is_over_kept_image(NODE, point(550.0, page_height + 1.0)));

        // A background as tall as the page, over the tall image and over the others.
        areas.note_covering_background(NODE, rect(0.0, -2000.0, 1000.0, page_height + 4000.0));
        assert!(!areas.is_over_kept_image(NODE, point(50.0, BAND_HEIGHT - 5.0)));
        assert!(!areas.is_over_kept_image(NODE, point(550.0, 10.0)));
    }

    /// The index gives the answers of comparing every area, for areas and points spread
    /// over a long page.
    #[test]
    fn the_bands_agree_with_comparing_every_area() {
        // A small generator of the same numbers on every run.
        let mut seed = 0x2545_f491_u32;
        let mut next = |below: f32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed % 10_000) as f32 / 10_000.0 * below
        };

        let mut areas = PaintedAreas::default();
        let mut every_area: Vec<(ScrollTreeNodeId, LayoutRect, bool)> = Vec::new();
        for step in 0..600 {
            let node = if next(1.0) < 0.8 { NODE } else { OTHER_NODE };
            let height = if step % 50 == 0 {
                BAND_HEIGHT * (MAX_BANDS as f32) * 2.0
            } else {
                next(700.0)
            };
            let area = rect(next(900.0), next(40_000.0) - 2_000.0, next(500.0), height);
            if next(1.0) < 0.6 {
                areas.note_kept_image(node, area);
                if !area.is_empty() {
                    every_area.push((node, area, true));
                }
            } else {
                areas.note_covering_background(node, area);
                let is_over_image = every_area.iter().any(|(other_node, other, is_image)| {
                    *is_image && *other_node == node && other.intersects(&area)
                });
                if is_over_image && !area.is_empty() {
                    every_area.push((node, area, false));
                }
            }
        }
        assert_eq!(areas.areas.len(), every_area.len());

        let mut over_images = 0;
        for _ in 0..4000 {
            let node = if next(1.0) < 0.8 { NODE } else { OTHER_NODE };
            let middle = point(next(1400.0), next(44_000.0) - 3_000.0);
            let expected = every_area
                .iter()
                .rev()
                .find(|(other_node, area, _)| *other_node == node && area.contains(middle))
                .is_some_and(|(_, _, is_image)| *is_image);
            assert_eq!(
                areas.is_over_kept_image(node, middle),
                expected,
                "at {middle:?}"
            );
            over_images += u32::from(expected);
        }
        // The comparison means something only if both answers occur.
        assert!((400..3600).contains(&over_images), "{over_images}");
    }
}
