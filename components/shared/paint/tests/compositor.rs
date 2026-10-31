/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use euclid::Size2D;
use paint_api::display_list::{
    AxesScrollSensitivity, ReferenceFrameNodeInfo, ScrollTree, ScrollType, ScrollableNodeInfo,
    SpatialTreeNodeInfo, TouchAction,
};
use servo_base::id::ScrollTreeNodeId;
use servo_geometry::FastLayoutTransform;
use webrender_api::units::LayoutVector2D;
use webrender_api::{
    ExternalScrollId, PipelineId, ReferenceFrameKind, ScrollLocation, TransformStyle,
};

fn add_mock_scroll_node(tree: &mut ScrollTree) -> (ScrollTreeNodeId, ExternalScrollId) {
    let pipeline_id = PipelineId(0, 0);
    let num_nodes = tree.nodes.len();
    let parent = if num_nodes > 0 {
        Some(ScrollTreeNodeId {
            index: num_nodes - 1,
        })
    } else {
        None
    };

    let external_id = ExternalScrollId(num_nodes as u64, pipeline_id);
    let scroll_node_id = tree.add_scroll_tree_node(
        parent,
        SpatialTreeNodeInfo::Scroll(ScrollableNodeInfo {
            external_id,
            content_rect: Size2D::new(200.0, 200.0).into(),
            clip_rect: Size2D::new(100.0, 100.0).into(),
            scroll_sensitivity: AxesScrollSensitivity {
                x: ScrollType::Script | ScrollType::InputEvents,
                y: ScrollType::Script | ScrollType::InputEvents,
            },
            touch_action: TouchAction::Auto,
            offset: LayoutVector2D::zero(),
            offset_changed: Cell::new(false),
        }),
    );
    (scroll_node_id, external_id)
}

#[test]
fn test_scroll_tree_simple_scroll() {
    let mut scroll_tree = ScrollTree::default();
    let (id, external_id) = add_mock_scroll_node(&mut scroll_tree);

    let (scrolled_id, offset) = scroll_tree
        .scroll_node_or_ancestor(
            external_id,
            ScrollLocation::Delta(LayoutVector2D::new(20.0, 40.0)),
            ScrollType::Script,
        )
        .unwrap();
    let expected_offset = LayoutVector2D::new(20.0, 40.0);
    assert_eq!(scrolled_id, external_id);
    assert_eq!(offset, expected_offset);
    assert_eq!(scroll_tree.get_node(id).offset(), Some(expected_offset));

    let (scrolled_id, offset) = scroll_tree
        .scroll_node_or_ancestor(
            external_id,
            ScrollLocation::Delta(LayoutVector2D::new(-20.0, -40.0)),
            ScrollType::Script,
        )
        .unwrap();
    let expected_offset = LayoutVector2D::new(0.0, 0.0);
    assert_eq!(scrolled_id, external_id);
    assert_eq!(offset, expected_offset);
    assert_eq!(scroll_tree.get_node(id).offset(), Some(expected_offset));

    // Scroll offsets must be positive.
    let result = scroll_tree.scroll_node_or_ancestor(
        external_id,
        ScrollLocation::Delta(LayoutVector2D::new(-20.0, -40.0)),
        ScrollType::Script,
    );
    assert!(result.is_none());
    assert_eq!(
        scroll_tree.get_node(id).offset(),
        Some(LayoutVector2D::new(0.0, 0.0))
    );
}

#[test]
fn test_scroll_tree_simple_scroll_chaining() {
    let mut scroll_tree = ScrollTree::default();

    let pipeline_id = PipelineId(0, 0);
    let (parent_id, parent_external_id) = add_mock_scroll_node(&mut scroll_tree);

    let unscrollable_external_id = ExternalScrollId(100 as u64, pipeline_id);
    let unscrollable_child_id = scroll_tree.add_scroll_tree_node(
        Some(parent_id),
        SpatialTreeNodeInfo::Scroll(ScrollableNodeInfo {
            external_id: unscrollable_external_id,
            content_rect: Size2D::new(100.0, 100.0).into(),
            clip_rect: Size2D::new(100.0, 100.0).into(),
            scroll_sensitivity: AxesScrollSensitivity {
                x: ScrollType::Script | ScrollType::InputEvents,
                y: ScrollType::Script | ScrollType::InputEvents,
            },
            touch_action: TouchAction::Auto,
            offset: LayoutVector2D::zero(),
            offset_changed: Cell::new(false),
        }),
    );

    let (scrolled_id, offset) = scroll_tree
        .scroll_node_or_ancestor(
            unscrollable_external_id,
            ScrollLocation::Delta(LayoutVector2D::new(20.0, 40.0)),
            ScrollType::Script,
        )
        .unwrap();
    let expected_offset = LayoutVector2D::new(20.0, 40.0);
    assert_eq!(scrolled_id, parent_external_id);
    assert_eq!(offset, expected_offset);
    assert_eq!(
        scroll_tree.get_node(parent_id).offset(),
        Some(expected_offset)
    );

    let (scrolled_id, offset) = scroll_tree
        .scroll_node_or_ancestor(
            unscrollable_external_id,
            ScrollLocation::Delta(LayoutVector2D::new(10.0, 15.0)),
            ScrollType::Script,
        )
        .unwrap();
    let expected_offset = LayoutVector2D::new(30.0, 55.0);
    assert_eq!(scrolled_id, parent_external_id);
    assert_eq!(offset, expected_offset);
    assert_eq!(
        scroll_tree.get_node(parent_id).offset(),
        Some(expected_offset)
    );
    assert_eq!(
        scroll_tree.get_node(unscrollable_child_id).offset(),
        Some(LayoutVector2D::zero())
    );
}

#[test]
fn test_scroll_tree_chain_when_at_extent() {
    let mut scroll_tree = ScrollTree::default();

    let (parent_id, parent_external_id) = add_mock_scroll_node(&mut scroll_tree);
    let (child_id, child_external_id) = add_mock_scroll_node(&mut scroll_tree);

    let (scrolled_id, offset) = scroll_tree
        .scroll_node_or_ancestor(child_external_id, ScrollLocation::End, ScrollType::Script)
        .unwrap();

    let expected_offset = LayoutVector2D::new(0.0, 100.0);
    assert_eq!(scrolled_id, child_external_id);
    assert_eq!(offset, expected_offset);
    assert_eq!(
        scroll_tree.get_node(child_id).offset(),
        Some(expected_offset)
    );

    // The parent will have scrolled because the child is already at the extent
    // of its scroll area in the y axis.
    let (scrolled_id, offset) = scroll_tree
        .scroll_node_or_ancestor(
            child_external_id,
            ScrollLocation::Delta(LayoutVector2D::new(0.0, 10.0)),
            ScrollType::Script,
        )
        .unwrap();
    let expected_offset = LayoutVector2D::new(0.0, 10.0);
    assert_eq!(scrolled_id, parent_external_id);
    assert_eq!(offset, expected_offset);
    assert_eq!(
        scroll_tree.get_node(parent_id).offset(),
        Some(expected_offset)
    );
}

#[test]
fn test_scroll_tree_chain_through_overflow_hidden() {
    let mut scroll_tree = ScrollTree::default();

    // Create a tree with a scrollable leaf, but make its `scroll_sensitivity`
    // reflect `overflow: hidden` ie not responsive to non-script scroll events.
    let (parent_id, parent_external_id) = add_mock_scroll_node(&mut scroll_tree);
    let (overflow_hidden_id, overflow_hidden_external_id) = add_mock_scroll_node(&mut scroll_tree);
    let node = scroll_tree.get_node_mut(overflow_hidden_id);

    if let SpatialTreeNodeInfo::Scroll(ref mut scroll_node_info) = node.info {
        scroll_node_info.scroll_sensitivity = AxesScrollSensitivity {
            x: ScrollType::Script,
            y: ScrollType::Script,
        };
    }

    let (scrolled_id, offset) = scroll_tree
        .scroll_node_or_ancestor(
            overflow_hidden_external_id,
            ScrollLocation::Delta(LayoutVector2D::new(20.0, 40.0)),
            ScrollType::InputEvents,
        )
        .unwrap();
    let expected_offset = LayoutVector2D::new(20.0, 40.0);
    assert_eq!(scrolled_id, parent_external_id);
    assert_eq!(offset, expected_offset);
    assert_eq!(
        scroll_tree.get_node(parent_id).offset(),
        Some(expected_offset)
    );
    assert_eq!(
        scroll_tree.get_node(overflow_hidden_id).offset(),
        Some(LayoutVector2D::new(0.0, 0.0))
    );
}

#[test]
fn test_scroll_tree_chain_from_fixed_drawer_to_viewport() {
    check_fixed_drawer_chaining(ScrollType::InputEvents);
}

#[test]
fn test_touch_scroll_tree_chain_from_fixed_drawer_to_viewport() {
    check_fixed_drawer_chaining(ScrollType::Touch);
}

fn check_fixed_drawer_chaining(context: ScrollType) {
    // Fixed descendants are siblings of the viewport scroll node, not its children.
    let mut tree = ScrollTree::default();
    let pipeline = PipelineId(0, 0);
    let reference = tree.add_scroll_tree_node(
        None,
        SpatialTreeNodeInfo::ReferenceFrame(ReferenceFrameNodeInfo {
            origin: Default::default(),
            frame_origin_for_query: Default::default(),
            transform_style: TransformStyle::Flat,
            transform: FastLayoutTransform::identity(),
            kind: ReferenceFrameKind::default(),
        }),
    );
    let add_sibling = |tree: &mut ScrollTree, external_id, height| {
        tree.add_scroll_tree_node(
            Some(reference),
            SpatialTreeNodeInfo::Scroll(ScrollableNodeInfo {
                external_id,
                content_rect: Size2D::new(100.0, height).into(),
                clip_rect: Size2D::new(100.0, 100.0).into(),
                scroll_sensitivity: AxesScrollSensitivity {
                    x: ScrollType::Script | ScrollType::InputEvents | ScrollType::Touch,
                    y: ScrollType::Script | ScrollType::InputEvents | ScrollType::Touch,
                },
                touch_action: TouchAction::Auto,
                offset: LayoutVector2D::zero(),
                offset_changed: Cell::new(false),
            }),
        )
    };
    let viewport = ExternalScrollId(0, pipeline);
    let viewport_node = add_sibling(&mut tree, viewport, 300.0);
    let drawer = ExternalScrollId(1, pipeline);
    let drawer_node = add_sibling(&mut tree, drawer, 100.0);
    let delta = ScrollLocation::Delta(LayoutVector2D::new(0.0, 40.0));

    // A script scrolling an empty drawer must not move the page.
    assert!(
        tree.scroll_node_or_ancestor(drawer, delta, ScrollType::Script)
            .is_none()
    );
    assert_eq!(
        tree.scroll_node_or_ancestor(drawer, delta, context),
        Some((viewport, LayoutVector2D::new(0.0, 40.0)))
    );
    assert_eq!(
        tree.get_node(drawer_node).offset(),
        Some(LayoutVector2D::zero())
    );

    // At the page edge, an unconsumed swipe stays unconsumed.
    tree.scroll_node_or_ancestor(viewport, ScrollLocation::End, ScrollType::Script);
    assert!(
        tree.scroll_node_or_ancestor(drawer, delta, context)
            .is_none()
    );
    tree.scroll_node_or_ancestor(viewport, ScrollLocation::Start, ScrollType::Script);

    // A scrollable drawer consumes input itself until it reaches its own edge.
    if let SpatialTreeNodeInfo::Scroll(info) = &mut tree.get_node_mut(drawer_node).info {
        info.content_rect = Size2D::new(100.0, 300.0).into();
    }
    assert_eq!(
        tree.scroll_node_or_ancestor(drawer, delta, context),
        Some((drawer, LayoutVector2D::new(0.0, 40.0)))
    );
    assert_eq!(
        tree.get_node(viewport_node).offset(),
        Some(LayoutVector2D::zero())
    );
    tree.scroll_node_or_ancestor(drawer, ScrollLocation::End, ScrollType::Script);
    assert_eq!(
        tree.scroll_node_or_ancestor(drawer, delta, context),
        Some((viewport, LayoutVector2D::new(0.0, 40.0)))
    );

    // Root overflow:hidden still forbids input scrolling.
    if let SpatialTreeNodeInfo::Scroll(info) = &mut tree.get_node_mut(viewport_node).info {
        info.scroll_sensitivity.y = ScrollType::Script;
    }
    assert!(
        tree.scroll_node_or_ancestor(drawer, delta, context)
            .is_none()
    );

    // A drawer in another document must never borrow this document's viewport.
    if let SpatialTreeNodeInfo::Scroll(info) = &mut tree.get_node_mut(viewport_node).info {
        info.scroll_sensitivity.y |= context;
    }
    let other_drawer = ExternalScrollId(1, PipelineId(0, 1));
    if let SpatialTreeNodeInfo::Scroll(info) = &mut tree.get_node_mut(drawer_node).info {
        info.external_id = other_drawer;
    }
    assert!(
        tree.scroll_node_or_ancestor(other_drawer, delta, context)
            .is_none()
    );
}

fn allow_touch_input(tree: &mut ScrollTree, node_id: ScrollTreeNodeId) {
    if let SpatialTreeNodeInfo::Scroll(info) = &mut tree.get_node_mut(node_id).info {
        info.scroll_sensitivity.x |= ScrollType::Touch;
        info.scroll_sensitivity.y |= ScrollType::Touch;
    }
}

fn scroll_to(tree: &mut ScrollTree, external_id: ExternalScrollId, y: f32) {
    tree.set_scroll_offset_for_node_with_external_scroll_id(
        external_id,
        LayoutVector2D::new(0.0, y),
        ScrollType::Script,
    );
}

#[test]
fn test_scroll_tree_can_scroll_up_reports_none_for_missing_node() {
    let mut scroll_tree = ScrollTree::default();
    assert_eq!(
        scroll_tree.node_or_ancestors_can_scroll_up(
            ExternalScrollId(0, PipelineId(0, 0)),
            ScrollType::Touch,
        ),
        None
    );

    add_mock_scroll_node(&mut scroll_tree);
    assert_eq!(
        scroll_tree.node_or_ancestors_can_scroll_up(
            ExternalScrollId(42, PipelineId(0, 0)),
            ScrollType::Touch,
        ),
        None
    );
}

#[test]
fn test_scroll_tree_can_scroll_up_checks_node_and_ancestors() {
    let mut scroll_tree = ScrollTree::default();
    let (viewport_id, viewport_external_id) = add_mock_scroll_node(&mut scroll_tree);
    let (child_id, child_external_id) = add_mock_scroll_node(&mut scroll_tree);
    allow_touch_input(&mut scroll_tree, viewport_id);
    allow_touch_input(&mut scroll_tree, child_id);

    // Everything at the top edge: there is no upward scroll to consume.
    assert_eq!(
        scroll_tree.node_or_ancestors_can_scroll_up(child_external_id, ScrollType::Touch),
        Some(false)
    );

    // The touched node itself is scrolled: it can scroll up.
    scroll_to(&mut scroll_tree, child_external_id, 40.0);
    assert_eq!(
        scroll_tree.node_or_ancestors_can_scroll_up(child_external_id, ScrollType::Touch),
        Some(true)
    );

    // The touched node is at its top edge, but an ancestor is scrolled.
    scroll_to(&mut scroll_tree, child_external_id, 0.0);
    scroll_to(&mut scroll_tree, viewport_external_id, 30.0);
    assert_eq!(
        scroll_tree.node_or_ancestors_can_scroll_up(child_external_id, ScrollType::Touch),
        Some(true)
    );
}

#[test]
fn test_scroll_tree_can_scroll_up_respects_input_sensitivity() {
    let mut scroll_tree = ScrollTree::default();
    let (_, viewport_external_id) = add_mock_scroll_node(&mut scroll_tree);
    let (child_id, child_external_id) = add_mock_scroll_node(&mut scroll_tree);

    // A scroll container that only script can scroll (e.g. `overflow: hidden`)
    // cannot consume a touch pan, even while scrolled; the scroll chains onward.
    if let SpatialTreeNodeInfo::Scroll(info) = &mut scroll_tree.get_node_mut(child_id).info {
        info.scroll_sensitivity.x = ScrollType::Script;
        info.scroll_sensitivity.y = ScrollType::Script;
    }
    scroll_to(&mut scroll_tree, child_external_id, 40.0);

    assert_eq!(
        scroll_tree.node_or_ancestors_can_scroll_up(child_external_id, ScrollType::Touch),
        Some(false)
    );
    assert_eq!(
        scroll_tree.node_or_ancestors_can_scroll_up(child_external_id, ScrollType::Script),
        Some(true)
    );

    // The ancestor viewport still answers for its own axis: scrolled, it can
    // scroll up for input types it is sensitive to.
    scroll_to(&mut scroll_tree, viewport_external_id, 30.0);
    assert_eq!(
        scroll_tree.node_or_ancestors_can_scroll_up(child_external_id, ScrollType::Touch),
        Some(false)
    );
    assert_eq!(
        scroll_tree.node_or_ancestors_can_scroll_up(child_external_id, ScrollType::InputEvents),
        Some(true)
    );
}

#[test]
fn test_scroll_tree_can_scroll_up_chains_from_fixed_drawer_to_viewport() {
    // Fixed descendants are siblings of the viewport in the spatial tree, so
    // their ancestor walk never reaches it; input scrolls chain to the
    // document's viewport explicitly.
    let mut tree = ScrollTree::default();
    let pipeline = PipelineId(0, 0);
    let reference = tree.add_scroll_tree_node(
        None,
        SpatialTreeNodeInfo::ReferenceFrame(ReferenceFrameNodeInfo {
            origin: Default::default(),
            frame_origin_for_query: Default::default(),
            transform_style: TransformStyle::Flat,
            transform: FastLayoutTransform::identity(),
            kind: ReferenceFrameKind::default(),
        }),
    );
    let add_sibling = |tree: &mut ScrollTree, external_id, height| {
        tree.add_scroll_tree_node(
            Some(reference),
            SpatialTreeNodeInfo::Scroll(ScrollableNodeInfo {
                external_id,
                content_rect: Size2D::new(100.0, height).into(),
                clip_rect: Size2D::new(100.0, 100.0).into(),
                scroll_sensitivity: AxesScrollSensitivity {
                    x: ScrollType::Script | ScrollType::InputEvents | ScrollType::Touch,
                    y: ScrollType::Script | ScrollType::InputEvents | ScrollType::Touch,
                },
                touch_action: TouchAction::Auto,
                offset: LayoutVector2D::zero(),
                offset_changed: Cell::new(false),
            }),
        )
    };
    let viewport = ExternalScrollId(0, pipeline);
    let viewport_node = add_sibling(&mut tree, viewport, 300.0);
    let drawer = ExternalScrollId(1, pipeline);
    add_sibling(&mut tree, drawer, 100.0);

    // The viewport is at its top edge: nothing consumes an upward pan.
    assert_eq!(
        tree.node_or_ancestors_can_scroll_up(drawer, ScrollType::Touch),
        Some(false)
    );

    // Page scrolled below its top edge: the gesture chains to the viewport.
    scroll_to(&mut tree, viewport, 50.0);
    assert_eq!(
        tree.node_or_ancestors_can_scroll_up(drawer, ScrollType::Touch),
        Some(true)
    );

    // A script-only viewport cannot be scrolled up by touch, though it can by
    // script itself.
    if let SpatialTreeNodeInfo::Scroll(info) = &mut tree.get_node_mut(viewport_node).info {
        info.scroll_sensitivity.y = ScrollType::Script;
    }
    assert_eq!(
        tree.node_or_ancestors_can_scroll_up(drawer, ScrollType::Touch),
        Some(false)
    );
    assert_eq!(
        tree.node_or_ancestors_can_scroll_up(viewport, ScrollType::Script),
        Some(true)
    );
}
