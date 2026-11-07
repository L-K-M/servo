/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use app_units::Au;
use js::context::JSContext;
use layout_api::CaretMovement;

use super::Selection;
use crate::dom::bindings::root::DomRoot;
use crate::dom::node::{Node, NodeTraits};

impl Selection {
    /// Query before changing the selection: a user extension can then dispatch selectstart
    /// and revalidate its original context before applying the returned DOM position.
    pub(super) fn layout_caret_position(
        &self,
        _cx: &mut JSContext,
        node: &Node,
        offset: u32,
        movement: CaretMovement,
    ) -> Option<(DomRoot<Node>, u32, Au, bool)> {
        let (node, offset) = Self::caret_position_for_rendering(node, offset);
        let host = node.editing_host_of()?;
        let vertical = matches!(movement, CaretMovement::Up | CaretMovement::Down);
        let preferred = vertical.then(|| self.preferred_caret_x.get()).flatten();
        let (target, offset, x, prefer_upstream) = node
            .owner_window()
            .caret_movement_query(&host, &node, offset, movement, preferred)?;
        if target.editing_host_of().as_ref() != Some(&host) || !target.is_editable_or_editing_host()
        {
            return None;
        }
        Some((target, offset, x, prefer_upstream))
    }
}
