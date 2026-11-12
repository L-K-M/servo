/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
mod layout;
mod stylo_taffy;
pub(crate) mod subgrid;
use std::fmt;

use app_units::Au;
use malloc_size_of_derive::MallocSizeOf;
use script::layout_dom::ServoLayoutNode;
use servo_arc::Arc;
use style::Atom;
use style::context::SharedStyleContext;
use style::properties::ComputedValues;
pub(crate) use stylo_taffy::TaffyStyloStyle;
pub(crate) use subgrid::{SubgridAxisTemplate, SubgridTemplates};
use taffy::GridItemStyle;

use crate::cell::ArcRefCell;
use crate::construct_modern::{ModernContainerBuilder, ModernItemKind};
use crate::context::LayoutContext;
use crate::dom::{LayoutBox, WeakLayoutBox};
use crate::dom_traversal::{NodeAndStyleInfo, NonReplacedContents};
use crate::formatting_contexts::IndependentFormattingContext;
use crate::fragment_tree::Fragment;
use crate::geom::{PhysicalPoint, PhysicalRect, PhysicalSides, PhysicalSize};
use crate::layout_box_base::LayoutBoxBase;
use crate::positioned::{AbsolutelyPositionedBox, PositioningContext};
use crate::{DefiniteContainingBlock, PropagatedBoxTreeData};

#[derive(Debug, MallocSizeOf)]
pub(crate) struct TaffyContainer {
    children: Vec<ArcRefCell<TaffyItemBox>>,
    style: Arc<ComputedValues>,
    /// Approximated `subgrid` template, resolved by this container's parent
    /// grid at construction time. See [`subgrid`].
    subgrid_templates: Option<SubgridTemplates>,
}

impl TaffyContainer {
    pub fn construct(
        context: &LayoutContext,
        info: &NodeAndStyleInfo,
        contents: NonReplacedContents,
        propagated_data: PropagatedBoxTreeData,
    ) -> Self {
        let mut builder = ModernContainerBuilder::new(context, info, propagated_data);
        contents.traverse(context, info, &mut builder);
        let items = builder.finish();

        let children = items
            .into_iter()
            .map(|item| {
                let taffy_item_box = match item.kind {
                    ModernItemKind::InFlow(independent_formatting_context) => {
                        ArcRefCell::new(TaffyItemBox::new(TaffyItemBoxInner::InFlowBox(
                            independent_formatting_context,
                        )))
                    },
                    ModernItemKind::OutOfFlow(independent_formatting_context) => {
                        let abs_pos_box = ArcRefCell::new(AbsolutelyPositionedBox::new(
                            independent_formatting_context,
                        ));
                        ArcRefCell::new(TaffyItemBox::new(
                            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(abs_pos_box),
                        ))
                    },
                    ModernItemKind::ReusedBox(layout_box) => match layout_box {
                        LayoutBox::TaffyItemBox(taffy_item_box) => taffy_item_box,
                        _ => unreachable!("Undamaged taffy level element should be associated with taffy level box"),
                    },
                };

                item.box_slot.set(LayoutBox::TaffyItemBox(taffy_item_box.clone()));
                taffy_item_box
            })
            .collect();

        let container = Self {
            children,
            style: info.style.clone(),
            subgrid_templates: None,
        };
        container.re_resolve_children();
        container
    }

    pub(crate) fn repair_style(&mut self, new_style: &Arc<ComputedValues>) {
        self.style = new_style.clone();
        // Re-resolve our own subgrid template against the parent data it was
        // resolved from, then the children's from our new style: a restyle
        // must not keep tracks from superseded styles.
        self.subgrid_templates = self
            .subgrid_templates
            .as_ref()
            .and_then(|templates| templates.re_resolve(new_style));
        self.re_resolve_children();
    }

    /// Set the `subgrid` fallback template resolved by this container's
    /// parent grid, and re-resolve the children against it: subgrids nested
    /// inside this one resolve from its resolved tracks.
    pub(crate) fn set_subgrid_templates(&mut self, templates: Option<SubgridTemplates>) {
        if self.subgrid_templates == templates {
            // Child resolution depends only on our style and templates; if
            // nothing changed, the cascade below would recompute the same
            // results for the whole subtree.
            return;
        }
        self.subgrid_templates = templates;
        self.re_resolve_children();
    }

    /// The track and name data this grid exposes to its items.
    fn axis_sources(
        &self,
    ) -> (Option<Arc<SubgridAxisTemplate>>, Option<Arc<SubgridAxisTemplate>>) {
        let position = self.style.get_position();
        let own = self.subgrid_templates.as_ref();
        (
            subgrid::axis_source(
                &position.grid_template_columns,
                own.and_then(|templates| templates.columns.as_ref()),
            ),
            subgrid::axis_source(
                &position.grid_template_rows,
                own.and_then(|templates| templates.rows.as_ref()),
            ),
        )
    }

    fn re_resolve_children(&self) {
        let (columns, rows) = self.axis_sources();
        for child in &self.children {
            let templates =
                subgrid::resolve_for_item(columns.as_ref(), rows.as_ref(), &child.borrow().style);
            child.borrow_mut().set_subgrid_templates(templates);
        }
    }
}

#[derive(MallocSizeOf)]
pub(crate) struct TaffyItemBox {
    pub(crate) taffy_layout: taffy::Layout,
    pub(crate) taffy_baselines: taffy::Baselines,
    pub(crate) child_fragments: Vec<Fragment>,
    pub(crate) positioning_context: PositioningContext,
    pub(crate) style: Arc<ComputedValues>,
    pub(crate) taffy_level_box: TaffyItemBoxInner,
}

#[expect(clippy::large_enum_variant)]
#[derive(Debug, MallocSizeOf)]
pub(crate) enum TaffyItemBoxInner {
    InFlowBox(IndependentFormattingContext),
    OutOfFlowAbsolutelyPositionedBox(ArcRefCell<AbsolutelyPositionedBox>),
}

impl fmt::Debug for TaffyItemBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaffyItemBox")
            .field("taffy_layout", &self.taffy_layout)
            .field("taffy_baselines", &self.taffy_baselines)
            .field("child_fragments", &self.child_fragments.len())
            .field("style", &self.style)
            .field("taffy_level_box", &self.taffy_level_box)
            .finish()
    }
}

impl TaffyItemBox {
    fn new(inner: TaffyItemBoxInner) -> Self {
        let style: Arc<ComputedValues> = match &inner {
            TaffyItemBoxInner::InFlowBox(item) => item.style().clone(),
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(absbox) => {
                (*absbox).borrow().context.style().clone()
            },
        };

        Self {
            taffy_layout: Default::default(),
            taffy_baselines: taffy::Baselines::NONE,
            child_fragments: Vec::new(),
            positioning_context: PositioningContext::default(),
            style,
            taffy_level_box: inner,
        }
    }

    /// Route the parent grid's `subgrid` fallback template to the item's
    /// grid container, if it is one. `None` clears a stale fallback.
    pub(crate) fn set_subgrid_templates(&mut self, templates: Option<SubgridTemplates>) {
        if let TaffyItemBoxInner::InFlowBox(context) = &mut self.taffy_level_box {
            context.set_subgrid_templates(templates);
        }
    }

    pub(crate) fn with_base<T>(&self, callback: impl FnOnce(&LayoutBoxBase) -> T) -> T {
        match self.taffy_level_box {
            TaffyItemBoxInner::InFlowBox(ref independent_formatting_context) => {
                callback(&independent_formatting_context.base)
            },
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(ref positioned_box) => {
                callback(&positioned_box.borrow().context.base)
            },
        }
    }

    pub(crate) fn with_base_mut<T>(&mut self, callback: impl FnOnce(&mut LayoutBoxBase) -> T) -> T {
        match &mut self.taffy_level_box {
            TaffyItemBoxInner::InFlowBox(independent_formatting_context) => {
                callback(&mut independent_formatting_context.base)
            },
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(positioned_box) => {
                callback(&mut positioned_box.borrow_mut().context.base)
            },
        }
    }

    pub(crate) fn repair_style(
        &mut self,
        context: &SharedStyleContext,
        node: &ServoLayoutNode,
        new_style: &Arc<ComputedValues>,
    ) {
        self.style = new_style.clone();
        match &mut self.taffy_level_box {
            TaffyItemBoxInner::InFlowBox(independent_formatting_context) => {
                independent_formatting_context.repair_style(context, node, new_style)
            },
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(positioned_box) => positioned_box
                .borrow_mut()
                .context
                .repair_style(context, node, new_style),
        }
    }

    fn is_in_flow_replaced(&self) -> bool {
        match &self.taffy_level_box {
            TaffyItemBoxInner::InFlowBox(fc) => fc.is_replaced(),
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(_) => false,
        }
    }

    pub(crate) fn attached_to_tree(&self, layout_box: WeakLayoutBox) {
        match &self.taffy_level_box {
            TaffyItemBoxInner::InFlowBox(formatting_context) => {
                formatting_context.attached_to_tree(layout_box)
            },
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(positioned_box) => positioned_box
                .borrow_mut()
                .context
                .attached_to_tree(layout_box),
        }
    }
}

/// Details from Taffy grid layout that will be stored
#[derive(Clone, Debug, MallocSizeOf)]
pub(crate) struct SpecificTaffyGridInfo {
    pub info: taffy::DetailedGridInfo<Atom>,
}

impl SpecificTaffyGridInfo {
    fn from_detailed_grid_layout(grid_info: taffy::DetailedGridInfo<Atom>) -> Self {
        Self { info: grid_info }
    }

    pub(crate) fn resolve_grid_area(
        &self,
        item_style: &ComputedValues,
        containing_block: &DefiniteContainingBlock,
        containing_block_border: PhysicalSides<Au>,
    ) -> PhysicalRect<Au> {
        let item_style = TaffyStyloStyle::new(item_style, false);
        let writing_mode = containing_block.style.writing_mode;
        let physical_containing_block_size = containing_block.size.to_physical_size(writing_mode);

        // Convert direction to Taffy type
        let direction = if writing_mode.is_bidi_ltr() {
            taffy::Direction::Ltr
        } else {
            taffy::Direction::Rtl
        };

        // Convert padding box to Taffy type
        let border_left = containing_block_border.left.to_f32_px();
        let border_top = containing_block_border.top.to_f32_px();

        let padding_box = taffy::Rect {
            left: border_left,
            right: border_left + physical_containing_block_size.width.to_f32_px(),
            top: border_top,
            bottom: border_top + physical_containing_block_size.height.to_f32_px(),
        };

        // Call into Taffy to resolve grid area
        let area = self.info.resolve_absolute_grid_area(
            item_style.grid_row(),
            item_style.grid_column(),
            direction,
            padding_box,
        );

        // Convert grid area into a PhysicalRect, and adjust it to be relative to the padding box
        PhysicalRect::new(
            PhysicalPoint::new(
                Au::from_f32_px(area.left) - containing_block_border.left,
                Au::from_f32_px(area.top) - containing_block_border.top,
            ),
            PhysicalSize::new(
                Au::from_f32_px(area.right - area.left),
                Au::from_f32_px(area.bottom - area.top),
            ),
        )
    }
}
