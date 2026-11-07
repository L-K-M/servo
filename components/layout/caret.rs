/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Caret geometry uses shaped fragments, including element boundaries with no text run.

use std::sync::Arc;

use app_units::Au;
use euclid::{Point2D, Rect, Vector2D};
use fonts::FontContext;
use layout_api::{CaretMovement, CaretPosition, ElementCaret, LayoutNode};
use script::layout_dom::ServoLayoutNode;
use servo_base::text::{RangeAny, Utf32CodeUnits};
use style::Zero;
use style::computed_values::direction::T as Direction;
use style::dom::OpaqueNode;
use style::values::generics::font::LineHeight;
use style_traits::CSSPixel;

use crate::dom::NodeExt;
use crate::fragment_tree::{
    BoxFragment, ContainingBlockCalculation, Fragment, FragmentFlags, TextFragment,
};

/// Tags originate in DOM nodes kept alive throughout layout; no script or GC can run here.
#[expect(unsafe_code)]
fn tagged_node(fragment: &BoxFragment) -> Option<ServoLayoutNode<'_>> {
    let tag = fragment.base.tag.as_ref()?;
    if !tag.pseudo_element_chain.is_empty() {
        return None;
    }
    Some(unsafe { ServoLayoutNode::from_opaque(tag.node) })
}

fn text_end_advance(text: &TextFragment) -> Au {
    text.glyphs.iter().fold(Au::zero(), |advance, store| {
        advance
            + store.total_advance()
            + text.justification_adjustment * store.total_word_separators() as i32
    })
}

fn neighbor_rect(
    fragments: &[Fragment],
    node: OpaqueNode,
    end: bool,
    origin: Point2D<Au, CSSPixel>,
    height: Au,
) -> Option<Rect<Au, CSSPixel>> {
    let mut found = None;
    for fragment in fragments {
        let offset = fragment
            .base()
            .map(|base| base.rect().origin.to_vector())
            .unwrap_or_else(Vector2D::zero);
        let origin = origin + offset;
        let matches = fragment
            .base()
            .and_then(|base| {
                base.tag
                    .as_ref()
                    .map(|tag| tag.node == node && tag.pseudo_element_chain.is_empty())
            })
            .unwrap_or(false);
        let current = match fragment {
            Fragment::Text(text) if matches => Some(Rect::new(
                Point2D::new(
                    origin.x
                        + if end {
                            text_end_advance(text)
                        } else {
                            Au::zero()
                        },
                    origin.y,
                ),
                euclid::Size2D::new(Au::zero(), text.font_metrics.line_gap),
            )),
            Fragment::Box(box_fragment) if matches => {
                let rect = box_fragment.border_rect();
                let rect = rect
                    .translate(origin.to_vector() - box_fragment.base.rect().origin.to_vector());
                Some(Rect::new(
                    Point2D::new(
                        if end { rect.max_x() } else { rect.min_x() },
                        rect.max_y() - height,
                    ),
                    euclid::Size2D::new(Au::zero(), height),
                ))
            },
            _ => fragment
                .children()
                .and_then(|children| neighbor_rect(&children, node, end, origin, height)),
        };
        if current.is_some() {
            found = current;
            if !end {
                break;
            }
        }
    }
    found
}

fn boundary_rect(
    fragment: &BoxFragment,
    caret: &ElementCaret,
    font_context: &FontContext,
) -> Option<Rect<Au, CSSPixel>> {
    let style = fragment.style();
    let font = style.get_font();
    let line_gap = font_context
        .font_group(style.clone_font())
        .first(font_context)?
        .metrics()
        .line_gap;
    let height: Au = line_gap;
    let line_height: Au = match font.line_height {
        LineHeight::Normal => line_gap,
        LineHeight::Number(number) => (font.font_size.computed_size() * number.0).into(),
        LineHeight::Length(length) => length.0.into(),
    };
    let start_x = if style.get_inherited_box().direction == Direction::Rtl {
        fragment.content_rect().width()
    } else {
        Au::zero()
    };
    let empty = || {
        Rect::new(
            Point2D::new(start_x, (line_height - height).scale_by(0.5)),
            euclid::Size2D::new(Au::zero(), height),
        )
    };
    if let Some(before) = caret
        .before
        .and_then(|node| neighbor_rect(&fragment.children, node, true, Point2D::origin(), height))
    {
        let mut rect = before;
        if caret.breaks_before > 0 {
            rect.origin.x = start_x;
        }
        rect.origin.y += line_height * caret.breaks_before as i32;
        return Some(rect);
    }
    if let Some(after) = caret
        .after
        .and_then(|node| neighbor_rect(&fragment.children, node, false, Point2D::origin(), height))
    {
        let mut rect = after;
        if caret.breaks_after > 0 {
            rect.origin.x = start_x;
        }
        rect.origin.y -= line_height * caret.breaks_after as i32;
        return Some(rect);
    }
    let mut rect = empty();
    rect.origin.y += line_height * caret.breaks_before as i32;
    Some(rect)
}

#[expect(unsafe_code)]
pub(crate) fn prefers_upstream(node: OpaqueNode) -> bool {
    // Fragment identities belong to this layout's live, frozen DOM.
    unsafe { ServoLayoutNode::from_opaque(node) }.caret_prefers_upstream()
}

/// The current element caret relative to the box's content origin. It is painted separately
/// from text, so empty editors do not need synthetic DOM text or layout-affecting glyphs.
pub(crate) fn element_caret_rect(
    fragment: &BoxFragment,
    font_context: &FontContext,
) -> Option<Rect<Au, CSSPixel>> {
    let caret = tagged_node(fragment)?.element_caret()?;
    boundary_rect(fragment, &caret, font_context)
}

enum PositionKind {
    Text(Arc<TextFragment>),
    Element(Utf32CodeUnits),
}

struct VisualPosition {
    node: OpaqueNode,
    kind: PositionKind,
    rect: Rect<Au, CSSPixel>,
}

impl VisualPosition {
    fn caret_x(&self, offset: Utf32CodeUnits) -> Option<Au> {
        let PositionKind::Text(text) = &self.kind else {
            return Some(self.rect.origin.x);
        };
        let offset = text
            .run_data
            .map_dom_range_to_transformed_range(RangeAny::new(Some(offset), Some(offset)))
            .start;
        let characters = &text.character_range_in_dom_node;
        if offset < characters.start || offset > characters.end {
            return None;
        }
        let mut index = characters.start;
        let mut advance = Au::zero();
        for store in &text.glyphs {
            for glyph in store.glyphs() {
                if index >= offset {
                    return Some(self.rect.origin.x + advance);
                }
                index += glyph.character_count();
                advance += glyph.advance();
                if glyph.char_is_word_separator() {
                    advance += text.justification_adjustment;
                }
            }
        }
        Some(self.rect.origin.x + advance)
    }

    fn at_x(&self, x: Au) -> Option<(OpaqueNode, Utf32CodeUnits)> {
        let offset = match &self.kind {
            PositionKind::Text(text) => {
                text.character_offset(Point2D::new(x - self.rect.origin.x, Au::zero()))?
            },
            PositionKind::Element(offset) => *offset,
        };
        Some((self.node, offset))
    }
}

fn has_text(fragment: &Fragment) -> bool {
    matches!(fragment, Fragment::Text(_))
        || fragment
            .children()
            .is_some_and(|children| children.iter().any(has_text))
}

fn collect_positions(
    fragment: &Fragment,
    origin: Point2D<Au, CSSPixel>,
    out: &mut Vec<VisualPosition>,
    font_context: &FontContext,
) {
    if let Fragment::Text(text) = fragment {
        if let Some(tag) = text
            .base
            .tag
            .as_ref()
            .filter(|tag| tag.pseudo_element_chain.is_empty())
        {
            out.push(VisualPosition {
                node: tag.node,
                kind: PositionKind::Text(text.clone()),
                rect: Rect::new(origin, text.base.rect().size),
            });
        }
    }
    if let Fragment::Box(box_fragment) = fragment {
        if let Some(node) = tagged_node(box_fragment) {
            let current = node.element_caret();
            let empty = !box_fragment.base.flags.contains(FragmentFlags::IS_REPLACED)
                && !box_fragment.with_style().is_inline_box()
                && node.text_node_paints_caret()
                && !box_fragment.children.iter().any(has_text)
                && !box_fragment
                    .children
                    .iter()
                    .any(|child| matches!(child, Fragment::Box(_) | Fragment::Float(_)));
            // The break's own box sits at the end of its visual line, including a
            // completely empty line. Its pseudo text is not a DOM text position.
            let line_break = box_fragment
                .base
                .flags
                .contains(FragmentFlags::IS_BR_ELEMENT)
                && node.text_node_paints_caret();
            if current.is_some() || empty || line_break {
                let caret = current.unwrap_or(ElementCaret {
                    offset: 0,
                    before: None,
                    after: None,
                    breaks_before: 0,
                    breaks_after: 0,
                });
                if let Some(rect) = boundary_rect(box_fragment, &caret, font_context) {
                    out.push(VisualPosition {
                        node: node.opaque(),
                        kind: PositionKind::Element(Utf32CodeUnits(caret.offset)),
                        rect: rect.translate(origin.to_vector()),
                    });
                }
            }
        }
    }
    if let Some(children) = fragment.children() {
        for child in children.iter() {
            let offset = child
                .base()
                .map(|base| base.rect().origin.to_vector())
                .unwrap_or_else(Vector2D::zero);
            collect_positions(child, origin + offset, out, font_context);
        }
    }
}

pub(crate) fn move_caret(
    scope: ServoLayoutNode<'_>,
    focus: OpaqueNode,
    offset: Utf32CodeUnits,
    movement: CaretMovement,
    preferred_x: Option<Au>,
    font_context: &FontContext,
) -> Option<CaretPosition> {
    let mut positions = Vec::new();
    for fragment in scope.fragments_for_pseudo(None) {
        let origin = fragment
            .retrieve_box_fragment()
            .map(|fragment| {
                fragment
                    .cumulative_content_box_rect(
                        ContainingBlockCalculation::AlreadyDoneWithStackingContextTree,
                    )
                    .origin
            })
            .unwrap_or_else(Point2D::origin);
        collect_positions(&fragment, origin, &mut positions, font_context);
    }
    let mut matching = positions
        .iter()
        .filter(|position| position.node == focus)
        .filter_map(|position| position.caret_x(offset).map(|x| (position, x)));
    let (origin, x) = if prefers_upstream(focus) {
        matching.next()
    } else {
        matching.last()
    }?;
    let middle = origin.rect.min_y() + origin.rect.height() / 2;
    let same_line = |position: &&VisualPosition| {
        position.rect.min_y() <= middle && position.rect.max_y() > middle
    };
    let x = preferred_x.unwrap_or(x);
    let target = match movement {
        CaretMovement::LineStart => positions
            .iter()
            .filter(same_line)
            .min_by_key(|position| position.rect.min_x()),
        CaretMovement::LineEnd => positions
            .iter()
            .filter(same_line)
            // A break box can share the text's endpoint. Keep the DOM caret in
            // the text when it exists, using the break only for an empty line.
            .max_by_key(|position| {
                (
                    position.rect.max_x(),
                    matches!(&position.kind, PositionKind::Text(_)),
                )
            }),
        CaretMovement::Up | CaretMovement::Down => {
            let target_y = positions
                .iter()
                .filter(|position| !same_line(position))
                .map(|position| position.rect.min_y() + position.rect.height() / 2)
                .filter(|y| match movement {
                    CaretMovement::Up => *y < middle,
                    CaretMovement::Down => *y > middle,
                    _ => unreachable!(),
                })
                .min_by_key(|y| (*y - middle).abs())?;
            positions
                .iter()
                .filter(|position| {
                    position.rect.min_y() <= target_y && position.rect.max_y() > target_y
                })
                .min_by_key(|position| {
                    (position.rect.min_x() - x)
                        .max(Au::zero())
                        .max(x - position.rect.max_x())
                })
        },
    }?;
    let target_x = match movement {
        CaretMovement::LineStart => target.rect.min_x(),
        CaretMovement::LineEnd => target.rect.max_x(),
        _ => x,
    };
    let (node, offset) = target.at_x(target_x)?;
    Some(CaretPosition {
        node,
        offset,
        preferred_x: x,
        prefer_upstream: target_x >= target.rect.max_x(),
    })
}
