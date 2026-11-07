/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `Selection.modify()`, which moves the focus of a selection by a character or a word.
//!
//! Browsers move the focus visually, through layout. This moves it through the DOM instead:
//! by grapheme cluster or word inside a text node and, past its edge, to the nearest visible
//! text node in tree order. Crossing a line break, a `<br>` or the boundary between two
//! blocks, counts as one character. That is enough for rich text editors, which extend the
//! selection by a character or a word to learn what Backspace or Delete removes. Moves by
//! line, sentence, paragraph or to a boundary need layout and leave the selection alone, as
//! do right-to-left specifics: "left" and "right" are taken as "backward" and "forward".

use cssparser::match_ignore_ascii_case;
use js::context::{JSContext, NoGC};
use servo_base::text::Utf16CodeUnits;
use servo_base::{Rope, RopeIndex, RopeMovement};
use style::values::specified::box_::DisplayOutside;

use crate::dom::CharacterData;
use crate::dom::bindings::codegen::Bindings::CharacterDataBinding::CharacterDataMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::{GetRootNodeOptions, NodeMethods};
use crate::dom::bindings::codegen::Bindings::SelectionBinding::SelectionMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::element::Element;
use crate::dom::html::htmlbrelement::HTMLBRElement;
use crate::dom::html::htmlimageelement::HTMLImageElement;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::Node;
use crate::dom::selection::Selection;
use crate::dom::text::Text;
use crate::dom::types::{
    HTMLButtonElement, HTMLCanvasElement, HTMLEmbedElement, HTMLHRElement, HTMLIFrameElement,
    HTMLInputElement, HTMLMediaElement, HTMLObjectElement, HTMLSelectElement, HTMLTextAreaElement,
    SVGSVGElement,
};

/// Whether a modification moves the whole selection or only its focus.
pub(crate) enum Alter {
    /// Collapse the selection at the new focus.
    Move,
    /// Keep the anchor and move the focus.
    Extend,
}

#[derive(Clone, Copy)]
pub(crate) enum Direction {
    Backward,
    Forward,
}

/// How far a modification moves the focus.
#[derive(Clone, Copy)]
pub(crate) enum Granularity {
    /// One grapheme cluster, or one line break.
    Character,
    /// To the start of the previous word, or the end of the next one.
    Word,
}

impl Selection {
    /// Moves the focus of this selection by `granularity` in `direction`, as `modify()` does,
    /// and collapses the selection there for [`Alter::Move`].
    pub(crate) fn modify_focus(
        &self,
        cx: &mut JSContext,
        alter: Alter,
        direction: Direction,
        granularity: Granularity,
    ) {
        // If this selection is empty, abort these steps.
        let Some((mut focus_node, mut focus_offset)) = self.composed_focus_position() else {
            return;
        };
        // Like browsers, moving a selection that is not collapsed starts at its edge in the
        // direction of the move; by a character, it only collapses there.
        if matches!(alter, Alter::Move) && !self.collapsed() {
            let Some((edge_node, edge_offset)) = self.composed_edge(direction) else {
                return;
            };
            if matches!(granularity, Granularity::Character) {
                let _ = self.Collapse(cx, Some(&edge_node), edge_offset);
                return;
            }
            (focus_node, focus_offset) = (edge_node, edge_offset);
        }
        // Modify this selection as the user agent would if the user moved the focus.
        let Some((node, offset)) = moved_position(
            cx.no_gc(),
            &focus_node,
            focus_offset,
            direction,
            granularity,
        ) else {
            return;
        };
        let _ = match alter {
            Alter::Move => self.Collapse(cx, Some(&node), offset),
            Alter::Extend => self.Extend(cx, &node, offset),
        };
    }

    /// The start of this selection for [`Direction::Backward`] or its end for
    /// [`Direction::Forward`], which may be inside a shadow tree.
    fn composed_edge(&self, direction: Direction) -> Option<(DomRoot<Node>, u32)> {
        let range = self.range.borrow();
        let range = range.as_ref()?;
        let boundary = match direction {
            Direction::Backward => &range.start,
            Direction::Forward => &range.end,
        };
        Some((boundary.container.as_rooted(), boundary.offset))
    }
}

/// <https://w3c.github.io/selection-api/#dom-selection-modify>
pub(super) fn modify(
    cx: &mut JSContext,
    selection: &Selection,
    alter: &str,
    direction: &str,
    granularity: &str,
) {
    // Step 1. If alter is not ASCII case-insensitive match with "extend" or "move", abort
    // these steps.
    let alter = match_ignore_ascii_case! { alter,
        "move" => Alter::Move,
        "extend" => Alter::Extend,
        _ => return,
    };
    // Step 2. If direction is not ASCII case-insensitive match with "forward", "backward",
    // "left", or "right", abort these steps.
    let direction = match_ignore_ascii_case! { direction,
        "forward" | "right" => Direction::Forward,
        "backward" | "left" => Direction::Backward,
        _ => return,
    };
    // Step 3. If granularity is not ASCII case-insensitive match with "character", "word",
    // "sentence", "line", "paragraph", "lineboundary", "sentenceboundary",
    // "paragraphboundary", "documentboundary", abort these steps.
    //
    // Only moves by character and word are implemented (see the module documentation).
    let granularity = match_ignore_ascii_case! { granularity,
        "character" => Granularity::Character,
        "word" => Granularity::Word,
        _ => return,
    };
    // Steps 4 to 6.
    selection.modify_focus(cx, alter, direction, granularity);
}

/// Where the focus at (`node`, `offset`) moves to, or `None` if it cannot move.
fn moved_position(
    no_gc: &NoGC,
    node: &Node,
    offset: u32,
    direction: Direction,
    granularity: Granularity,
) -> Option<(DomRoot<Node>, u32)> {
    if let Some(text) = node.downcast::<Text>() &&
        let Some(moved) = move_within_text(text, offset, direction, granularity)
    {
        return Some((DomRoot::from_ref(node), moved));
    }

    // Inside an editing host, the focus does not leave it.
    let editing_host = node.editing_host_of();
    let in_editing_host = editing_host.is_some();
    let scope =
        editing_host.unwrap_or_else(|| node.GetRootNode(&GetRootNodeOptions { composed: false }));
    let origin_block = node.block_node_of();
    // The position beside the first line break crossed, where the move ends unless text in
    // the same block follows.
    let mut beside_line_break: Option<(Option<DomRoot<Node>>, (DomRoot<Node>, u32))> = None;
    for candidate in nodes_beyond(node, offset, &scope, direction) {
        if candidate == scope {
            break;
        }
        // An image, a form control or, inside an editing host, content that is not editable
        // counts as one character: the focus stops beside it, never inside.
        if let Some(unit) = atomic_unit(&candidate, &scope, in_editing_host) {
            if !unit.is_visible(no_gc) {
                continue;
            }
            // The line a unit sits on: a block unit, such as a non-editable `<div>`, sits on
            // a line of its parent's block.
            let unit_block = if unit.is_block_node() {
                unit.GetParentNode()
                    .and_then(|parent| parent.block_node_of())
            } else {
                unit.block_node_of()
            };
            let crossed_line = match &beside_line_break {
                Some((line_break_block, position)) => {
                    if unit_block != *line_break_block {
                        return Some(position.clone());
                    }
                    true
                },
                None => unit_block != origin_block,
            };
            // Past a line break, the move ends on the near side of the unit; otherwise it
            // moves past the unit.
            let near_side = match direction {
                Direction::Forward => Direction::Backward,
                Direction::Backward => Direction::Forward,
            };
            if crossed_line {
                let position = position_beside(&unit, near_side)?;
                // Right beside the start, the near side is no move at all.
                if *position.0 != *node || position.1 != offset {
                    return Some(position);
                }
            }
            return position_beside(&unit, direction);
        }
        if candidate.is::<HTMLBRElement>() {
            if !breaks_a_line(&candidate, no_gc) {
                continue;
            }
            let parent = candidate.GetParentNode()?;
            let index = candidate.index();
            if let Some((_, position)) = beside_line_break {
                // A second line break: stop on the line between the two.
                return Some(position);
            }
            if candidate.block_node_of() != origin_block {
                // Entering another block, which starts or ends with this line break.
                return Some((parent, index));
            }
            let position = match direction {
                Direction::Backward => (parent, index),
                Direction::Forward => (parent, index + 1),
            };
            beside_line_break = Some((candidate.block_node_of(), position));
            continue;
        }

        let Some(text) = candidate.downcast::<Text>() else {
            continue;
        };
        let length = text.upcast::<CharacterData>().Length();
        if length == 0 || !candidate.is_visible(no_gc) {
            continue;
        }
        let near_edge = match direction {
            Direction::Backward => length,
            Direction::Forward => 0,
        };
        if let Some((line_break_block, position)) = beside_line_break {
            if candidate.block_node_of() != line_break_block {
                return Some(position);
            }
            return Some((candidate, near_edge));
        }
        if candidate.block_node_of() != origin_block {
            // Moving into another block counts as one character, like a line break.
            return Some((candidate, near_edge));
        }
        // Same line: the move continues inside this text node. A word move goes on past
        // text without a word in it.
        if let Some(moved) = move_within_text(text, near_edge, direction, granularity) {
            return Some((candidate, moved));
        }
    }
    beside_line_break.map(|(_, position)| position)
}

/// The outermost unit that `node` is, or is part of, inside `scope` and that the focus moves
/// over in one step: a replaced element or form control, or, inside an editing host, content
/// that is not editable.
fn atomic_unit(node: &Node, scope: &Node, in_editing_host: bool) -> Option<DomRoot<Node>> {
    let mut unit = None;
    for ancestor in node.inclusive_ancestors(ShadowIncluding::No) {
        if &*ancestor == scope {
            break;
        }
        if is_replaced_or_control(&ancestor) ||
            (in_editing_host && !ancestor.is_editable_or_editing_host())
        {
            unit = Some(ancestor);
        }
    }
    unit
}

/// Elements that render as one box with no caret positions inside.
fn is_replaced_or_control(node: &Node) -> bool {
    node.is::<HTMLImageElement>() ||
        node.is::<HTMLInputElement>() ||
        node.is::<HTMLTextAreaElement>() ||
        node.is::<HTMLSelectElement>() ||
        node.is::<HTMLButtonElement>() ||
        node.is::<HTMLMediaElement>() ||
        node.is::<HTMLIFrameElement>() ||
        node.is::<HTMLCanvasElement>() ||
        node.is::<HTMLObjectElement>() ||
        node.is::<HTMLEmbedElement>() ||
        node.is::<HTMLHRElement>() ||
        node.is::<SVGSVGElement>()
}

/// The position just before (`side` backward) or just after (`side` forward) `node`, in the
/// adjacent text node if there is one, as browsers report it.
fn position_beside(node: &Node, side: Direction) -> Option<(DomRoot<Node>, u32)> {
    match side {
        Direction::Backward => {
            if let Some(text) = node
                .GetPreviousSibling()
                .filter(|sibling| sibling.is::<Text>())
            {
                let length = text.len();
                return Some((text, length));
            }
            Some((node.GetParentNode()?, node.index()))
        },
        Direction::Forward => {
            if let Some(text) = node.GetNextSibling().filter(|sibling| sibling.is::<Text>()) {
                return Some((text, 0));
            }
            Some((node.GetParentNode()?, node.index() + 1))
        },
    }
}

/// Whether the `<br>` element `br` breaks a line. One that ends a block after other content
/// in it has no visual effect.
fn breaks_a_line(br: &Node, no_gc: &NoGC) -> bool {
    let Some(block) = br.block_node_of() else {
        return true;
    };
    // Not `is_visible()` for a `<br>`: it takes every `<br>` in an inline formatting context for
    // an extraneous line break.
    let is_displayed = |node: &DomRoot<Node>| {
        !node
            .inclusive_ancestors(ShadowIncluding::No)
            .any(|ancestor| {
                ancestor.downcast::<Element>().is_some_and(|element| {
                    element.resolved_display_value() == Some(DisplayOutside::None)
                })
            })
    };
    let is_content_of_block = |node: &DomRoot<Node>| {
        node.block_node_of().as_ref() == Some(&block) &&
            match node.downcast::<Text>() {
                Some(text) => text.upcast::<CharacterData>().Length() > 0 && node.is_visible(no_gc),
                None => {
                    (node.is::<HTMLBRElement>() || node.is::<HTMLImageElement>()) &&
                        is_displayed(node)
                },
            }
    };
    br.following_nodes(&block, ShadowIncluding::No)
        .any(|node| is_content_of_block(&node)) ||
        !br.preceding_nodes(&block)
            .any(|node| is_content_of_block(&node))
}

/// Nodes past the boundary point (`node`, `offset`) in `direction`, in tree order, inside
/// `scope`.
fn nodes_beyond(
    node: &Node,
    offset: u32,
    scope: &Node,
    direction: Direction,
) -> Box<dyn Iterator<Item = DomRoot<Node>>> {
    let is_text = node.is::<CharacterData>();
    match direction {
        Direction::Backward => {
            let child_before = (!is_text && offset > 0)
                .then(|| node.children().nth(offset as usize - 1))
                .flatten();
            match child_before {
                Some(child) => {
                    let last = child.descending_last_children().last().unwrap_or(child);
                    let preceding = last.preceding_nodes(scope);
                    Box::new(std::iter::once(last).chain(preceding))
                },
                None => Box::new(node.preceding_nodes(scope)),
            }
        },
        Direction::Forward => {
            let child_after = (!is_text)
                .then(|| node.children().nth(offset as usize))
                .flatten();
            match child_after {
                Some(child) => {
                    let following = child.following_nodes(scope, ShadowIncluding::No);
                    Box::new(std::iter::once(child).chain(following))
                },
                None => {
                    // Past the end of `node`: start after its last descendant.
                    let last = node
                        .descending_last_children()
                        .last()
                        .unwrap_or_else(|| DomRoot::from_ref(node));
                    Box::new(last.following_nodes(scope, ShadowIncluding::No))
                },
            }
        },
    }
}

/// The offset a move by `granularity` from `offset` reaches inside `text`, or `None` if it
/// would leave the text node.
fn move_within_text(
    text: &Text,
    offset: u32,
    direction: Direction,
    granularity: Granularity,
) -> Option<u32> {
    let rope = Rope::new(text.upcast::<CharacterData>().data().as_str());
    let origin =
        rope.utf8_offset_to_rope_index(rope.utf16_offset_to_utf8_offset(Utf16CodeUnits(offset)));
    let movement = match granularity {
        Granularity::Character => RopeMovement::Grapheme,
        Granularity::Word => RopeMovement::Word,
    };
    let amount = match direction {
        Direction::Backward => -1,
        Direction::Forward => 1,
    };
    let moved = rope
        .index_to_utf16_offset(rope.move_by(origin, movement, amount))
        .0;
    let moved_toward_direction = match direction {
        Direction::Backward => moved < offset,
        Direction::Forward => moved > offset,
    };
    // Without a word before or after the offset, a word move stops at the edge of the
    // text; past it is where the next word may be.
    let reached_edge_without_word = matches!(granularity, Granularity::Word) &&
        match direction {
            Direction::Backward => moved == 0 && !text_has_word_before(&rope, origin),
            Direction::Forward => {
                moved == text.upcast::<CharacterData>().Length() &&
                    !text_has_word_after(&rope, origin)
            },
        };
    (moved_toward_direction && !reached_edge_without_word).then_some(moved)
}

/// Whether the text before `index` has a word in it: Unicode word segmentation counts as
/// words the segments with a letter or a digit.
fn text_has_word_before(rope: &Rope, index: RopeIndex) -> bool {
    rope.slice(None, Some(index))
        .chars()
        .any(char::is_alphanumeric)
}

/// Whether the text after `index` has a word in it (see [`text_has_word_before`]).
fn text_has_word_after(rope: &Rope, index: RopeIndex) -> bool {
    rope.slice(Some(index), None)
        .chars()
        .any(char::is_alphanumeric)
}
