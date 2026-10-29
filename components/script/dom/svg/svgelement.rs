/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::codegen::GenericBindings::ElementBinding::ScrollLogicalPosition;
use script_bindings::codegen::GenericBindings::WindowBinding::ScrollBehavior;
use script_bindings::str::DOMString;
use style::attr::AttrValue;
use style::parser::ParserContext;
use style::properties::{LonghandId, PropertyDeclaration, PropertyId, SourcePropertyDeclaration};
use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
use style::values::generics::NonNegative;
use style::values::specified;
use style_traits::ParsingMode;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::HTMLOrSVGElementBinding::FocusOptions;
use crate::dom::bindings::codegen::Bindings::SVGElementBinding::SVGElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot, LayoutDom, MutNullableDom};
use crate::dom::css::cssstyledeclaration::{
    CSSModificationAccess, CSSStyleDeclaration, CSSStyleOwner,
};
use crate::dom::document::Document;
use crate::dom::document::focus::FocusableArea;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::node::focus::FocusTrigger;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::svg::svgcircleelement::SVGCircleElement;
use crate::dom::svg::svgellipseelement::SVGEllipseElement;
use crate::dom::svg::svgimageelement::SVGImageElement;
use crate::dom::svg::svgpathelement::SVGPathElement;
use crate::dom::svg::svgrectelement::SVGRectElement;
use crate::dom::svg::svgsvgelement::SVGSVGElement;
use crate::dom::window::scrolling_box::{ScrollAxisState, ScrollRequirement};

#[dom_struct]
pub(crate) struct SVGElement {
    element: Element,
    style_decl: MutNullableDom<CSSStyleDeclaration>,
}

/// The SVG 2 presentation attributes that map to CSS properties of the same
/// name (SVG 2 §Styling). `transform`, `overflow`, `filter`, `mask`,
/// `clip-path`, `marker-*`, `font-variant` and `text-decoration` are missing
/// here: their attributes still reach the SVG rasterizer as attribute text,
/// and document CSS for them is not resolved.
const SVG_PRESENTATION_ATTRIBUTES: &[(&str, LonghandId)] = &[
    ("fill", LonghandId::Fill),
    ("fill-opacity", LonghandId::FillOpacity),
    ("fill-rule", LonghandId::FillRule),
    ("stroke", LonghandId::Stroke),
    ("stroke-width", LonghandId::StrokeWidth),
    ("stroke-linecap", LonghandId::StrokeLinecap),
    ("stroke-linejoin", LonghandId::StrokeLinejoin),
    ("stroke-dasharray", LonghandId::StrokeDasharray),
    ("stroke-dashoffset", LonghandId::StrokeDashoffset),
    ("stroke-miterlimit", LonghandId::StrokeMiterlimit),
    ("stroke-opacity", LonghandId::StrokeOpacity),
    ("opacity", LonghandId::Opacity),
    ("display", LonghandId::Display),
    ("visibility", LonghandId::Visibility),
    ("color", LonghandId::Color),
    ("font-family", LonghandId::FontFamily),
    ("font-size", LonghandId::FontSize),
    ("font-style", LonghandId::FontStyle),
    ("font-weight", LonghandId::FontWeight),
    ("letter-spacing", LonghandId::LetterSpacing),
    ("word-spacing", LonghandId::WordSpacing),
    ("alignment-baseline", LonghandId::AlignmentBaseline),
    ("baseline-shift", LonghandId::BaselineShift),
    ("direction", LonghandId::Direction),
];

impl SVGElement {
    fn new_inherited(
        tag_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGElement {
        SVGElement::new_inherited_with_state(ElementState::empty(), tag_name, prefix, document)
    }

    pub(crate) fn new_inherited_with_state(
        state: ElementState,
        tag_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGElement {
        SVGElement {
            element: Element::new_inherited_with_state(state, tag_name, ns!(svg), prefix, document),
            style_decl: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        tag_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGElement::new_inherited(tag_name, prefix, document)),
            document,
            proto,
        )
    }

    fn as_element(&self) -> &Element {
        self.upcast::<Element>()
    }
}

impl VirtualMethods for SVGElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.as_element() as &dyn VirtualMethods)
    }

    fn attribute_mutated(
        &self,
        cx: &mut js::context::JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);
        let element = self.as_element();
        if let (&local_name!("nonce"), mutation) = (attr.local_name(), mutation) {
            match mutation {
                AttributeMutation::Set(_) => {
                    let nonce = &**attr.value();
                    element.update_nonce_internal_slot(nonce.to_owned(), cx.no_gc());
                },
                AttributeMutation::Removed => {
                    element.update_nonce_internal_slot(String::new(), cx.no_gc());
                },
            }
        }
    }

    fn attribute_affects_presentational_hints(&self, attr: AttrRef<'_>) -> bool {
        let name = &**attr.local_name();
        SVG_PRESENTATION_ATTRIBUTES
            .iter()
            .any(|(attr_name, _)| name == *attr_name) ||
            matches!(
                name,
                "cx" | "cy" | "r" | "rx" | "ry" | "x" | "y" | "width" | "height" | "d"
            ) ||
            self.super_type()
                .unwrap()
                .attribute_affects_presentational_hints(attr)
    }
}

impl SVGElementMethods<crate::DomTypeHolder> for SVGElement {
    /// <https://html.spec.whatwg.org/multipage/#the-style-attribute>
    fn Style(&self, cx: &mut JSContext) -> DomRoot<CSSStyleDeclaration> {
        self.style_decl.or_init(|| {
            let global = self.owner_window();
            CSSStyleDeclaration::new(
                cx,
                &global,
                CSSStyleOwner::Element(Dom::from_ref(self.upcast())),
                None,
                CSSModificationAccess::ReadWrite,
            )
        })
    }

    // https://html.spec.whatwg.org/multipage/#globaleventhandlers
    global_event_handlers!();

    /// <https://html.spec.whatwg.org/multipage/#dom-noncedelement-nonce>
    fn Nonce(&self) -> DOMString {
        self.as_element().nonce_value().into()
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-noncedelement-nonce>
    fn SetNonce(&self, cx: &mut JSContext, value: DOMString) {
        self.as_element()
            .update_nonce_internal_slot(String::from(value), cx.no_gc())
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-fe-autofocus>
    fn Autofocus(&self) -> bool {
        self.element.has_attribute(&local_name!("autofocus"))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-fe-autofocus>
    fn SetAutofocus(&self, cx: &mut JSContext, autofocus: bool) {
        self.element
            .set_bool_attribute(cx, &local_name!("autofocus"), autofocus);
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-focus>
    fn Focus(&self, cx: &mut js::context::JSContext, options: &FocusOptions) {
        // 1. If the allow focus steps given this's node document return false, then return.
        // TODO: Implement this.

        // 2. Run the focusing steps for this.
        if !self
            .upcast::<Node>()
            .run_the_focusing_steps(cx, None, FocusTrigger::Other)
        {
            // The specification seems to imply we should scroll into view even if this element
            // is not a focusable area. No browser does this, so we return early in that case.
            // See https://github.com/whatwg/html/issues/12231.
            return;
        }

        // > 3. If options["focusVisible"] is true, or does not exist but in an
        // >    implementation-defined  way the user agent determines it would be best to do so,
        // >    then indicate focus. TODO: Implement this.
        // TODO: Implement this.

        // > 4. If options["preventScroll"] is false, then scroll a target into view given this,
        // >    "auto", "center", and "center".
        if !options.preventScroll {
            let scroll_axis = ScrollAxisState {
                position: ScrollLogicalPosition::Center,
                requirement: ScrollRequirement::IfNotVisible,
            };
            self.upcast::<Element>().scroll_into_view_with_options(
                cx,
                ScrollBehavior::Smooth,
                scroll_axis,
                scroll_axis,
                None,
                None,
            );
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-blur>
    fn Blur(&self, cx: &mut js::context::JSContext) {
        // TODO: Run the unfocusing steps. Focus the top-level document, not
        //       the current document.
        if !self.as_element().focus_state() {
            return;
        }
        // <https://html.spec.whatwg.org/multipage/#unfocusing-steps>
        self.owner_document()
            .focus_handler()
            .focus(cx, &FocusableArea::Viewport);
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-tabindex>
    fn TabIndex(&self) -> i32 {
        self.element.tab_index()
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-tabindex>
    fn SetTabIndex(&self, cx: &mut JSContext, tab_index: i32) {
        self.element
            .set_attribute(cx, &local_name!("tabindex"), tab_index.into());
    }
}

impl<'dom> LayoutDom<'dom, SVGElement> {
    pub(crate) fn synthesize_presentational_hints(
        self,
        document: LayoutDom<'dom, Document>,
        push: &mut impl FnMut(PropertyDeclaration),
    ) {
        let element = self.upcast::<Element>();

        if element.is::<SVGSVGElement>() {
            if let Some(width) = element
                .get_attr_for_layout(&ns!(), &local_name!("width"))
                .and_then(AttrValue::as_length_percentage)
            {
                push(PropertyDeclaration::Width(
                    specified::Size::LengthPercentage(NonNegative(width.clone())),
                ));
            }
            if let Some(height) = element
                .get_attr_for_layout(&ns!(), &local_name!("height"))
                .and_then(AttrValue::as_length_percentage)
            {
                push(PropertyDeclaration::Height(
                    specified::Size::LengthPercentage(NonNegative(height.clone())),
                ));
            }
        }
        let url_data = UrlExtraData(document.url_for_layout().get_arc());
        let parsing_mode =
            ParsingMode::ALLOW_UNITLESS_LENGTH | ParsingMode::ALLOW_ALL_NUMERIC_VALUES;
        let parser_context = ParserContext::new(
            Origin::Author,
            &url_data,
            Some(CssRuleType::Style),
            parsing_mode,
            document.quirks_mode(),
            Default::default(),
            None,
            None,
            Default::default(),
        );

        // SVG 2 §Styling maps these attributes to CSS properties of the same
        // name. Layout rasterizes SVG subtrees as images from serialized
        // styles (layout_api::SVG_RASTER_STYLE_LONGHANDS); an attribute that
        // is not mapped here keeps reaching that rasterizer only as raw
        // attribute text. `text-anchor`, `dominant-baseline`, `clip-rule`,
        // `stop-color`, `stop-opacity` and `paint-order` are missing because
        // Servo's style system does not implement those properties yet.
        for (attr_name, longhand_id) in SVG_PRESENTATION_ATTRIBUTES {
            self.parse_svg_attribute(
                &parser_context,
                attr_name,
                PropertyId::NonCustom((*longhand_id).into()),
                push,
            );
        }

        // Parse geometry attributes based on element type
        // <circle>: https://svgwg.org/svg2-draft/shapes.html#CircleElement
        if element.downcast::<SVGCircleElement>().is_some() {
            self.parse_svg_attribute(
                &parser_context,
                "cx",
                PropertyId::NonCustom(LonghandId::Cx.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "cy",
                PropertyId::NonCustom(LonghandId::Cy.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "r",
                PropertyId::NonCustom(LonghandId::R.into()),
                push,
            );
        }
        // <ellipse>: https://svgwg.org/svg2-draft/shapes.html#EllipseElement
        if element.downcast::<SVGEllipseElement>().is_some() {
            self.parse_svg_attribute(
                &parser_context,
                "cx",
                PropertyId::NonCustom(LonghandId::Cx.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "cy",
                PropertyId::NonCustom(LonghandId::Cy.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "rx",
                PropertyId::NonCustom(LonghandId::Rx.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "ry",
                PropertyId::NonCustom(LonghandId::Ry.into()),
                push,
            );
        }
        // <rect>: https://svgwg.org/svg2-draft/shapes.html#RectElement
        if element.downcast::<SVGRectElement>().is_some() {
            self.parse_svg_attribute(
                &parser_context,
                "x",
                PropertyId::NonCustom(LonghandId::X.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "y",
                PropertyId::NonCustom(LonghandId::Y.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "width",
                PropertyId::NonCustom(LonghandId::Width.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "height",
                PropertyId::NonCustom(LonghandId::Height.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "rx",
                PropertyId::NonCustom(LonghandId::Rx.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "ry",
                PropertyId::NonCustom(LonghandId::Ry.into()),
                push,
            );
        }
        // <image>: https://svgwg.org/svg2-draft/embedded.html#ImageElement
        if element.downcast::<SVGImageElement>().is_some() {
            self.parse_svg_attribute(
                &parser_context,
                "x",
                PropertyId::NonCustom(LonghandId::X.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "y",
                PropertyId::NonCustom(LonghandId::Y.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "width",
                PropertyId::NonCustom(LonghandId::Width.into()),
                push,
            );
            self.parse_svg_attribute(
                &parser_context,
                "height",
                PropertyId::NonCustom(LonghandId::Height.into()),
                push,
            );
        }
        // <path>: https://svgwg.org/svg2-draft/paths.html#PathElement
        if element.downcast::<SVGPathElement>().is_some() {
            // The d CSS property only accepts `none` or `path(<string>)`,
            // but the SVG presentation attribute uses raw path data (e.g. "M0,0 L1,1").
            // Wrap the raw path data in `path("...")` so the CSS parser can handle it.
            if let Some(value) = element.get_attr_val_for_layout(&ns!(), &local_name!("d")) {
                if value.eq_ignore_ascii_case("none") {
                    self.parse_svg_attribute(
                        &parser_context,
                        "d",
                        PropertyId::NonCustom(LonghandId::D.into()),
                        push,
                    );
                } else {
                    // Path data treats whitespace alike, and a literal
                    // newline would break the CSS string below.
                    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
                    let wrapped = format!("path(\"{normalized}\")");
                    self.parse_svg_value(
                        &wrapped,
                        PropertyId::NonCustom(LonghandId::D.into()),
                        &parser_context,
                        push,
                    );
                }
            }
        }
    }

    /// Parse one presentation attribute value as a declaration of
    /// `property_id` and push the result.
    ///
    /// Presentation attributes are CSS declarations (SVG 2 §Styling), so
    /// values may carry `var()` references and CSS-wide keywords. The
    /// declaration parser accepts those where the property's value parser
    /// does not.
    fn parse_svg_attribute(
        self,
        parser_context: &ParserContext,
        attr_name: &str,
        property_id: PropertyId,
        push: &mut impl FnMut(PropertyDeclaration),
    ) {
        let element = self.upcast::<Element>();
        let Some(value) = element.get_attr_val_for_layout(&ns!(), &LocalName::from(attr_name))
        else {
            return;
        };
        self.parse_svg_value(value, property_id, parser_context, push);
    }

    /// Parse `value` as a declaration of `property_id` and push the result.
    fn parse_svg_value(
        self,
        value: &str,
        property_id: PropertyId,
        parser_context: &ParserContext,
        push: &mut impl FnMut(PropertyDeclaration),
    ) {
        let mut parser = cssparser::Parser::new(value);
        let mut declarations = SourcePropertyDeclaration::default();
        let parsed = PropertyDeclaration::parse_into(
            &mut declarations,
            property_id,
            parser_context,
            &mut parser,
        );
        if parsed.is_ok() {
            // Single longhand parses never set the `all` shorthand, so the
            // drain only carries longhand declarations.
            for declaration in declarations.drain().declarations {
                push(declaration);
            }
        }
    }
}
