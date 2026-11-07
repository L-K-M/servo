/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `Selection.modify()`, which moves the focus of a selection by a character or a word.
//!
//! Browsers move the focus visually, through layout. This moves it through the DOM instead:
//! by grapheme cluster or word inside a text node and, past its edge, to the nearest visible
//! text node in tree order. Crossing a line break, a `<br>` or the boundary between two
//! blocks, counts as one character. That is enough for rich text editors, which extend the
//! selection by a character or a word to learn what Backspace or Delete removes.
//!
//! Line and line-boundary moves query the shaped fragment tree, retaining the desired
//! inline position across shorter lines and distinguishing the two sides of a soft wrap.
//! Document-boundary moves remain within the editing host. Sentence, paragraph and
//! right-to-left specifics are not implemented: "left" and "right" are taken as
//! "backward" and "forward".

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
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::htmlbrelement::HTMLBRElement;
use crate::dom::html::htmlimageelement::HTMLImageElement;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::selection::Selection;
use crate::dom::text::Text;
use crate::dom::types::{
    HTMLButtonElement, HTMLCanvasElement, HTMLEmbedElement, HTMLHRElement, HTMLIFrameElement,
    HTMLInputElement, HTMLMediaElement, HTMLMeterElement, HTMLObjectElement, HTMLProgressElement,
    HTMLSelectElement, HTMLTextAreaElement, SVGSVGElement,
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
    /// To the adjacent shaped line, retaining the desired inline coordinate.
    Line,
    /// To the start or end of the current shaped line.
    LineBoundary,
    /// To the start or end of the editing host.
    DocumentBoundary,
}

enum ChangeSource {
    Script,
    User,
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
        self.modify_focus_from(cx, alter, direction, granularity, ChangeSource::Script);
    }

    fn modify_focus_from(
        &self,
        cx: &mut JSContext,
        alter: Alter,
        direction: Direction,
        granularity: Granularity,
        source: ChangeSource,
    ) {
        let original_focus = self.composed_focus_position();
        let original_anchor = self.composed_anchor_position();
        let Some((mut focus_node, mut focus_offset)) = original_focus.clone() else {
            return;
        };
        let was_collapsed = self.collapsed();
        let mut only_collapse = false;
        if matches!(alter, Alter::Move) && !was_collapsed {
            let Some((node, offset)) = self.composed_edge(direction) else {
                return;
            };
            (focus_node, focus_offset) = (node, offset);
            only_collapse = matches!(granularity, Granularity::Character);
        }
        let position = if only_collapse {
            Some((focus_node.clone(), focus_offset, None, false))
        } else if matches!(granularity, Granularity::Line | Granularity::LineBoundary) {
            let movement = match (granularity, direction) {
                (Granularity::Line, Direction::Backward) => layout_api::CaretMovement::Up,
                (Granularity::Line, Direction::Forward) => layout_api::CaretMovement::Down,
                (_, Direction::Backward) => layout_api::CaretMovement::LineStart,
                (_, Direction::Forward) => layout_api::CaretMovement::LineEnd,
            };
            self.layout_caret_position(cx, &focus_node, focus_offset, movement)
                .map(|(node, offset, x, upstream)| (node, offset, Some(x), upstream))
        } else {
            moved_position(
                cx.no_gc(),
                &focus_node,
                focus_offset,
                direction,
                granularity,
            )
            .map(|(node, offset)| (node, offset, None, false))
        };
        let Some((node, offset, x, prefer_upstream)) = position else {
            return;
        };
        if matches!(source, ChangeSource::User)
            && matches!(alter, Alter::Extend)
            && was_collapsed
            && (node.clone(), offset) != (focus_node.clone(), focus_offset)
        {
            let target = match direction {
                Direction::Backward => &node,
                Direction::Forward => &focus_node,
            };
            if !fire_selectstart(cx, target) {
                return;
            }
            // Listeners can move focus, change selection or remove the original editor.
            if self.composed_focus_position() != original_focus
                || self.composed_anchor_position() != original_anchor
                || !node.is_connected()
                || node
                    .editing_host_of()
                    .as_ref()
                    .is_none_or(|host| !self.document.focus_allows_user_editing_of(host))
            {
                return;
            }
        }
        let result = match alter {
            Alter::Move => self.Collapse(cx, Some(&node), offset),
            Alter::Extend => self.Extend(cx, &node, offset),
        };
        if result.is_ok() && self.caret_prefers_upstream.replace(prefer_upstream) != prefer_upstream
        {
            self.set_visible_selection_dirty();
        }
        if result.is_ok() && matches!(granularity, Granularity::Line) {
            self.preferred_caret_x.set(x);
        } else {
            self.preferred_caret_x.set(None);
        }
    }

    /// Moves the focus of this selection as a user action, such as an arrow key. Extending a
    /// collapsed selection first fires a cancelable `selectstart` event at the node of the new
    /// range's start, as the selection API requires; a canceled event leaves the selection alone.
    pub(crate) fn modify_focus_for_user(
        &self,
        cx: &mut JSContext,
        direction: Direction,
        granularity: Granularity,
    ) {
        self.modify_focus_from(
            cx,
            Alter::Extend,
            direction,
            granularity,
            ChangeSource::User,
        );
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
    // Sentence and paragraph granularities are not implemented (see the module documentation).
    let granularity = match_ignore_ascii_case! { granularity,
        "character" => Granularity::Character,
        "word" => Granularity::Word,
        "line" => Granularity::Line,
        "lineboundary" => Granularity::LineBoundary,
        "documentboundary" => Granularity::DocumentBoundary,
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
    // Inside an editing host, the focus does not leave it.
    let editing_host = node.editing_host_of();
    let in_editing_host = editing_host.is_some();
    let scope =
        editing_host.unwrap_or_else(|| node.GetRootNode(&GetRootNodeOptions { composed: false }));

    if matches!(granularity, Granularity::DocumentBoundary) {
        return document_boundary_position(no_gc, &scope, direction, in_editing_host);
    }

    if let Some(text) = node.downcast::<Text>()
        && let Some(moved) = move_within_text(text, offset, direction, granularity)
    {
        return Some((DomRoot::from_ref(node), moved));
    }

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
            if !atomic_is_visible(&unit, no_gc) {
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

/// Resolve the host's edges to visible, editable positions without entering atomic units.
fn document_boundary_position(
    no_gc: &NoGC,
    scope: &Node,
    direction: Direction,
    in_editing_host: bool,
) -> Option<(DomRoot<Node>, u32)> {
    match direction {
        Direction::Backward => first_caret_position_from(no_gc, scope, 0, scope, in_editing_host),
        Direction::Forward => {
            last_caret_position_before(no_gc, scope, scope.children_count(), scope, in_editing_host)
        },
    }
}

/// The first caret position at or after (`node`, `offset`) inside `scope`.
fn first_caret_position_from(
    no_gc: &NoGC,
    node: &Node,
    offset: u32,
    scope: &Node,
    in_editing_host: bool,
) -> Option<(DomRoot<Node>, u32)> {
    if let Some(text) = node.downcast::<Text>() {
        if offset < text.upcast::<CharacterData>().Length() && node.is_visible(no_gc) {
            return Some((DomRoot::from_ref(node), offset));
        }
    }
    if !node.is::<CharacterData>() && offset < node.children_count() {
        let child = node.children().nth(offset as usize)?;
        if let Some(position) = caret_position_before_child(&child, scope, in_editing_host, no_gc) {
            return Some(position);
        }
    }
    for candidate in nodes_beyond(node, offset, scope, Direction::Forward) {
        if &*candidate == scope {
            break;
        }
        if let Some(text) = candidate.downcast::<Text>() {
            if text.upcast::<CharacterData>().Length() > 0 && candidate.is_visible(no_gc) {
                return Some((candidate, 0));
            }
            continue;
        }
        if let Some(unit) = atomic_unit(&candidate, scope, in_editing_host) {
            if atomic_is_visible(&unit, no_gc) {
                return position_beside(&unit, Direction::Backward);
            }
            continue;
        }
        if candidate.is::<HTMLBRElement>() && breaks_a_line(&candidate, no_gc) {
            return Some((candidate.GetParentNode()?, candidate.index()));
        }
    }
    Some((DomRoot::from_ref(node), offset))
}

/// The last caret position at or before (`node`, `offset`) inside `scope`.
fn last_caret_position_before(
    no_gc: &NoGC,
    node: &Node,
    offset: u32,
    scope: &Node,
    in_editing_host: bool,
) -> Option<(DomRoot<Node>, u32)> {
    if node.is::<Text>() {
        if offset > 0 && node.is_visible(no_gc) {
            return Some((DomRoot::from_ref(node), offset));
        }
    }
    if !node.is::<CharacterData>() && offset > 0 {
        let child = node.children().nth(offset as usize - 1)?;
        if let Some(position) = caret_position_after_child(&child, scope, in_editing_host, no_gc) {
            return Some(position);
        }
        // The last caret position of an element is inside it, at its end.
        if !child.is::<CharacterData>() && child.is_visible(no_gc) {
            if let Some(position) = last_caret_position_before(
                no_gc,
                &child,
                child.children_count(),
                scope,
                in_editing_host,
            ) {
                return Some(position);
            }
        }
    }
    for candidate in nodes_beyond(node, offset, scope, Direction::Backward) {
        if &*candidate == scope {
            break;
        }
        if let Some(text) = candidate.downcast::<Text>() {
            let length = text.upcast::<CharacterData>().Length();
            if length > 0
                && candidate.is_visible(no_gc)
                && atomic_unit(&candidate, scope, in_editing_host).is_none()
            {
                return Some((candidate, length));
            }
            continue;
        }
        if let Some(unit) = atomic_unit(&candidate, scope, in_editing_host) {
            if atomic_is_visible(&unit, no_gc) {
                return position_beside(&unit, Direction::Forward);
            }
            continue;
        }
        if candidate.is::<HTMLBRElement>() && breaks_a_line(&candidate, no_gc) {
            return Some((candidate.GetParentNode()?, candidate.index() + 1));
        }
    }
    Some((DomRoot::from_ref(node), offset))
}

/// The caret position at the start of `child`, if it has one before any nested content.
fn caret_position_before_child(
    child: &Node,
    scope: &Node,
    in_editing_host: bool,
    no_gc: &NoGC,
) -> Option<(DomRoot<Node>, u32)> {
    if let Some(text) = child.downcast::<Text>() {
        if text.upcast::<CharacterData>().Length() > 0 && child.is_visible(no_gc) {
            return Some((DomRoot::from_ref(child), 0));
        }
        return None;
    }
    if let Some(unit) = atomic_unit(child, scope, in_editing_host) {
        if atomic_is_visible(&unit, no_gc) {
            return position_beside(&unit, Direction::Backward);
        }
        return None;
    }
    if child.is::<HTMLBRElement>() && breaks_a_line(child, no_gc) {
        return Some((child.GetParentNode()?, child.index()));
    }
    None
}

/// The caret position at the end of `child`, if it has one after any nested content.
fn caret_position_after_child(
    child: &Node,
    scope: &Node,
    in_editing_host: bool,
    no_gc: &NoGC,
) -> Option<(DomRoot<Node>, u32)> {
    if let Some(text) = child.downcast::<Text>() {
        if text.upcast::<CharacterData>().Length() > 0 && child.is_visible(no_gc) {
            return Some((
                DomRoot::from_ref(child),
                text.upcast::<CharacterData>().Length(),
            ));
        }
        return None;
    }
    if let Some(unit) = atomic_unit(child, scope, in_editing_host) {
        if atomic_is_visible(&unit, no_gc) {
            return position_beside(&unit, Direction::Forward);
        }
        return None;
    }
    if child.is::<HTMLBRElement>() && breaks_a_line(child, no_gc) {
        return Some((child.GetParentNode()?, child.index() + 1));
    }
    None
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

/// Replaced controls are visible boxes even when they have no DOM text descendants.
fn atomic_is_visible(node: &Node, no_gc: &NoGC) -> bool {
    if !is_replaced_or_control(node) {
        return node.is_visible(no_gc);
    }
    !node
        .inclusive_ancestors(ShadowIncluding::No)
        .any(|ancestor| {
            ancestor
                .downcast::<Element>()
                .and_then(Element::resolved_display_value)
                == Some(DisplayOutside::None)
        })
        && node
            .downcast::<Element>()
            .and_then(Element::style)
            .is_none_or(|style| {
                style.get_inherited_box().visibility
                    == style::computed_values::visibility::T::Visible
            })
}

/// Elements that render as one box with no caret positions inside.
pub(super) fn is_replaced_or_control(node: &Node) -> bool {
    node.is::<HTMLImageElement>()
        || node.is::<HTMLInputElement>()
        || node.is::<HTMLTextAreaElement>()
        || node.is::<HTMLSelectElement>()
        || node.is::<HTMLButtonElement>()
        || node.is::<HTMLMediaElement>()
        || node.is::<HTMLMeterElement>()
        || node.is::<HTMLProgressElement>()
        || node.is::<HTMLIFrameElement>()
        || node.is::<HTMLCanvasElement>()
        || node.is::<HTMLObjectElement>()
        || node.is::<HTMLEmbedElement>()
        || node.is::<HTMLHRElement>()
        || node.is::<SVGSVGElement>()
}

/// Fires a cancelable `selectstart` event at `target`, returning whether it was not canceled.
fn fire_selectstart(cx: &mut JSContext, target: &Node) -> bool {
    let global = target.owner_global();
    let event = Event::new(
        cx,
        &global,
        atom!("selectstart"),
        EventBubbles::Bubbles,
        EventCancelable::Cancelable,
    );
    event.fire(cx, target.upcast::<EventTarget>())
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
        Granularity::Line | Granularity::LineBoundary | Granularity::DocumentBoundary => {
            return None;
        },
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
