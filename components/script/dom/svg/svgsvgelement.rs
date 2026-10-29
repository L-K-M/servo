/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use atomic_refcell::AtomicRefCell;
use base64::Engine as _;
use cssparser::Parser;
use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::{JSContext, NoGC};
use js::rust::HandleObject;
use layout_api::{SVGElementData, SvgElementStyle, SvgTextTransform};
use servo_arc::Arc as ServoArc;
use servo_url::ServoUrl;
use style::attr::AttrValue;
use style::parser::ParserContext;
use style::properties::parse_style_attribute;
use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
use style::values::specified::LengthPercentage;
use style_traits::ParsingMode;
use uuid::Uuid;
use xml5ever::serialize::TraversalScope;

use crate::dom::bindings::codegen::Bindings::CharacterDataBinding::CharacterDataMethods;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, LayoutDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::characterdata::CharacterData;
use crate::dom::document::Document;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{
    ChildrenMutation, CloneChildrenFlag, Node, NodeDamage, NodeTraits, UnbindContext,
};
use crate::dom::svg::svgcircleelement::SVGCircleElement;
use crate::dom::svg::svgellipseelement::SVGEllipseElement;
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;
use crate::dom::svg::svgimageelement::SVGImageElement;
use crate::dom::svg::svgrectelement::SVGRectElement;
use crate::dom::text::Text;

#[dom_struct]
pub(crate) struct SVGSVGElement {
    svggraphicselement: SVGGraphicsElement,
    #[no_trace]
    uuid: Uuid,
    // The XML source of subtree rooted at this SVG element, serialized into
    // a base64 encoded `data:` url. This is cached to avoid recomputation
    // on each layout and must be invalidated when the subtree changes.
    #[no_trace]
    cached_serialized_data_url: AtomicRefCell<Option<Result<ServoUrl, ()>>>,
}

impl SVGSVGElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGSVGElement {
        SVGSVGElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            uuid: Uuid::new_v4(),
            cached_serialized_data_url: Default::default(),
        }
    }

    #[cfg_attr(crown, allow(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGSVGElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGSVGElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    pub(crate) fn serialize_and_cache_subtree(&self, cx: &mut js::context::JSContext) {
        let document_fragment = self.owner_document().CreateDocumentFragment(cx);
        let cloned_node = Node::clone(
            cx,
            self.upcast(),
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        if document_fragment
            .upcast::<Node>()
            .AppendChild(cx, &cloned_node)
            .is_err()
        {
            error!("Unable to clone SVG tree");
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        }

        // The rasterizer only sees the serialized markup, so resolve the
        // document's class rules and var() references into the clone first.
        // Baking walks the clone in lockstep with the original subtree, so it
        // must run before `use` expansion appends extra nodes; expansion
        // clones bake their own source subtree in `process_single_use_element`.
        self.bake_resolved_styles(cx, self.upcast::<Node>(), &cloned_node);

        self.process_use_elements(cx, &cloned_node);

        let Ok(xml_source) = cloned_node.xml_serialize(TraversalScope::IncludeNode) else {
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        };

        let xml_source: String = xml_source.into();
        let base64_encoded_source = base64::engine::general_purpose::STANDARD.encode(xml_source);
        let data_url = format!("data:image/svg+xml;base64,{base64_encoded_source}");
        match ServoUrl::parse(&data_url) {
            Ok(url) => *self.cached_serialized_data_url.borrow_mut() = Some(Ok(url)),
            Err(error) => error!("Unable to parse serialized SVG data url: {error}"),
        };
    }

    fn process_use_elements(&self, cx: &mut JSContext, root_node: &Node) {
        for node in root_node.traverse_preorder(ShadowIncluding::No) {
            if let Some(element) = node.downcast::<Element>() &&
                element.local_name() == &local_name!("use")
            {
                self.process_single_use_element(cx, element, root_node)
            }
        }
    }

    fn process_single_use_element(
        &self,
        cx: &mut JSContext,
        use_element: &Element,
        root_node: &Node,
    ) {
        let href = use_element.get_string_attribute(&local_name!("href"));
        let Some(id_string) = href.str().strip_prefix("#").map(DOMString::from) else {
            return;
        };

        let document = self.upcast::<Node>().owner_doc();
        let Some(referenced_element) = document.GetElementById(cx, id_string) else {
            return;
        };
        let referenced_node = referenced_element.upcast::<Node>();

        // Don't use this node if it doesn't have an `<svg>` ancestor.
        if !referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| ancestor.is::<SVGSVGElement>())
        {
            return;
        };

        // Don't use this node if it already exists within the same `<svg>` element.
        if referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| *ancestor == self.upcast())
        {
            return;
        };

        let cloned_node = Node::clone(
            cx,
            referenced_node,
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        self.bake_resolved_styles(cx, referenced_node.upcast::<Node>(), &cloned_node);
        let _ = root_node.AppendChild(cx, &cloned_node);
    }

    /// Serialize the resolved presentation styles of `original`'s elements
    /// into the matching elements of `cloned`, which must be structurally
    /// identical to `original`.
    ///
    /// Servo rasterizes SVG subtrees as images from their serialized XML,
    /// and that rasterizer only understands presentation attributes,
    /// `style` attributes and stylesheets inside the subtree. Resolving the
    /// document's full CSS cascade here makes class rules, custom properties
    /// and `var()` references survive serialization.
    fn bake_resolved_styles(&self, cx: &mut JSContext, original: &Node, cloned: &Node) {
        let document = self.owner_document();
        let styles = self.owner_window().layout().query_svg_presentation_styles(
            original.to_trusted_node_address(),
            document.animation_manager().sets(),
            document.current_animation_timeline_value(),
        );

        let original_elements: Vec<DomRoot<Element>> = original
            .traverse_preorder(ShadowIncluding::No)
            .filter_map(|node| node.downcast::<Element>().map(DomRoot::from_ref))
            .collect();
        let cloned_elements: Vec<DomRoot<Element>> = cloned
            .traverse_preorder(ShadowIncluding::No)
            .filter_map(|node| node.downcast::<Element>().map(DomRoot::from_ref))
            .collect();
        if styles.len() != original_elements.len() ||
            original_elements.len() != cloned_elements.len()
        {
            // The layout walk and the clone walk must see the same elements
            // in the same order; rather than assign styles to the wrong
            // element, serialize the subtree without resolved styles.
            error!(
                "SVG style bake skipped: {} styles for {} original and {} cloned elements",
                styles.len(),
                original_elements.len(),
                cloned_elements.len(),
            );
            return;
        }

        for ((style, original_element), cloned_element) in styles
            .iter()
            .zip(original_elements.iter())
            .zip(cloned_elements.iter())
        {
            self.bake_element_style(cx, style, original_element, cloned_element);
        }
    }

    /// Write one element's resolved presentation styles into its clone.
    fn bake_element_style(
        &self,
        cx: &mut JSContext,
        style: &SvgElementStyle,
        original_element: &Element,
        cloned_element: &Element,
    ) {
        let declarations = self.declarations_for_raster(style, original_element);

        // Append the resolved declarations after any existing style
        // attribute: the rasterizer resolves the attribute last, so the
        // resolved values win over leftovers it cannot resolve (var()
        // references).
        if !declarations.is_empty() {
            let existing = cloned_element.get_string_attribute(&local_name!("style"));
            let merged = if existing.is_empty() {
                declarations
            } else {
                format!("{existing};{declarations}")
            };
            self.set_style_attribute_for_serialization(cx, cloned_element, merged);
        }
        self.move_geometry_to_attributes(cx, cloned_element, style);

        // The rasterizer has no text-transform; apply it to the cloned text.
        if style.text_transform != SvgTextTransform::None {
            self.apply_text_transform(cx, style.text_transform, cloned_element);
        }
    }

    /// The declarations to serialize for one element.
    fn declarations_for_raster(
        &self,
        style: &SvgElementStyle,
        original_element: &Element,
    ) -> String {
        let parent_is_svg = original_element
            .upcast::<Node>()
            .GetParentNode()
            .is_some_and(|parent| parent.is::<SVGSVGElement>());
        if !parent_is_svg {
            return style.declarations.clone();
        }

        // The user agent hides SVG children from CSS layout (`svg > *` in
        // servo.css), which is not their rendered state: the rasterizer draws
        // them. A computed `display: none` on a direct child of an `<svg>`
        // comes from that rule unless the element carries its own `display`:
        // a presentation attribute keeps working in the rasterizer, and a
        // `display` in the element's own style attribute is author intent,
        // so both keep the computed value.
        let author_hides = original_element.has_attribute(&local_name!("display")) ||
            original_element
                .get_string_attribute(&local_name!("style"))
                .contains("display");
        if author_hides {
            return style.declarations.clone();
        }

        style.declarations.replace("display:none;", "")
    }

    /// Set the style attribute on a serialization clone without running the
    /// attribute-mutation pipeline: no style-attribute parsing and no
    /// Content Security Policy check, which would report violations for
    /// this engine-internal write.
    fn set_style_attribute_for_serialization(
        &self,
        cx: &mut JSContext,
        cloned_element: &Element,
        value: String,
    ) {
        let document = self.owner_document();
        let lock = document.style_shared_author_lock();
        let block = parse_style_attribute(
            &value,
            &UrlExtraData(document.base_url().get_arc()),
            None,
            document.quirks_mode(),
            CssRuleType::Style,
        );
        cloned_element.set_attribute(
            cx,
            &local_name!("style"),
            AttrValue::Declaration {
                block: ServoArc::new(lock.wrap(block)),
                lock: lock.clone(),
                serialization: std::sync::OnceLock::new(),
            },
        );
    }

    /// Move the resolved geometry declarations onto the clone's attributes.
    /// The rasterizer resolves geometry only from presentation attributes,
    /// never from a style attribute, and a raw attribute value cannot be
    /// parsed after `var()` substitution re-parses it strictly. Only the
    /// element kinds whose geometry attributes map to CSS properties carry
    /// resolved values (SVG 2 §Geometry): text positioning attributes do
    /// not, and the root `<svg>` keeps its own width and height, which
    /// select the raster's natural size.
    fn move_geometry_to_attributes(
        &self,
        cx: &mut JSContext,
        cloned_element: &Element,
        style: &SvgElementStyle,
    ) {
        let is_rect = cloned_element.is::<SVGRectElement>();
        let is_image = cloned_element.is::<SVGImageElement>();
        let is_circle = cloned_element.is::<SVGCircleElement>();
        let is_ellipse = cloned_element.is::<SVGEllipseElement>();
        if !(is_rect || is_image || is_circle || is_ellipse) {
            return;
        }

        for declaration in style.declarations.split(';') {
            let Some((name, value)) = declaration.split_once(':') else {
                continue;
            };
            let maps = match name {
                "x" | "y" | "width" | "height" => is_rect || is_image,
                "rx" | "ry" => is_rect || is_ellipse,
                "cx" | "cy" => is_circle || is_ellipse,
                "r" => is_circle,
                _ => false,
            };
            if !maps || value == "auto" {
                continue;
            }
            cloned_element.set_attribute(
                cx,
                &LocalName::from(name),
                AttrValue::String(value.into()),
            );
        }
    }

    /// Apply `text_transform` to the text nodes directly below
    /// `cloned_element` (CSS Text 4 §text-transform). Descendant elements
    /// apply their own computed transform to their own text; text that is
    /// never rendered keeps its spelling.
    fn apply_text_transform(
        &self,
        cx: &mut JSContext,
        text_transform: SvgTextTransform,
        cloned_element: &Element,
    ) {
        if matches!(
            &**cloned_element.local_name(),
            "style" | "script" | "title" | "desc"
        ) {
            return;
        }

        for node in cloned_element.upcast::<Node>().children() {
            let Some(text) = node.downcast::<Text>() else {
                continue;
            };
            let transformed = match text_transform {
                SvgTextTransform::Uppercase => text.data().to_uppercase(),
                SvgTextTransform::Lowercase => text.data().to_lowercase(),
                SvgTextTransform::Capitalize => capitalize_words(&text.data()),
                SvgTextTransform::None | SvgTextTransform::Unsupported => continue,
            };
            text.upcast::<CharacterData>()
                .SetData(cx, DOMString::from(transformed));
        }
    }

    fn invalidate_cached_serialized_subtree_and_rasterization_result(&self, no_gc: &NoGC) {
        let owner_window = self.owner_window();
        owner_window
            .image_cache()
            .evict_rasterized_image(&self.uuid);
        if let Some(Ok(url)) = &*self.cached_serialized_data_url.borrow() {
            owner_window.layout_mut().remove_cached_image(url);
            owner_window.image_cache().evict_completed_image(
                url,
                owner_window.origin().immutable(),
                &None,
            );
        }

        *self.cached_serialized_data_url.borrow_mut() = None;
        self.upcast::<Node>().dirty(no_gc, NodeDamage::Other);
    }
}

impl<'dom> LayoutDom<'dom, SVGSVGElement> {
    pub(crate) fn data(self) -> SVGElementData<'dom> {
        let svg_id = self.unsafe_get().uuid;
        let element = self.upcast::<Element>();
        let width = element.get_attr_for_layout(&ns!(), &local_name!("width"));
        let height = element.get_attr_for_layout(&ns!(), &local_name!("height"));
        let view_box = element.get_attr_for_layout(&ns!(), &local_name!("viewBox"));
        SVGElementData {
            source: self
                .unsafe_get()
                .cached_serialized_data_url
                .borrow()
                .clone(),
            width,
            height,
            view_box,
            svg_id,
        }
    }
}

impl VirtualMethods for SVGSVGElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
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

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn attribute_affects_presentational_hints(&self, attr: AttrRef<'_>) -> bool {
        match attr.local_name() {
            &local_name!("width") | &local_name!("height") => true,
            _ => self
                .super_type()
                .unwrap()
                .attribute_affects_presentational_hints(attr),
        }
    }

    fn parse_plain_attribute(&self, name: &LocalName, value: DOMString) -> AttrValue {
        match *name {
            local_name!("width") | local_name!("height") => {
                let value = &value.str();
                let parser = &mut Parser::new(value);
                let doc = self.owner_document();
                let url = doc.url().into_url().into();
                let context = ParserContext::new(
                    Origin::Author,
                    &url,
                    None,
                    ParsingMode::ALLOW_UNITLESS_LENGTH,
                    doc.quirks_mode(),
                    /* namespaces = */ Default::default(),
                    None,
                    None,
                    /* attr_taint = */ Default::default(),
                );
                let val = LengthPercentage::parse_quirky(
                    &context,
                    parser,
                    style::values::specified::AllowQuirks::Always,
                );
                AttrValue::LengthPercentage(value.to_string(), val.ok())
            },
            _ => self
                .super_type()
                .unwrap()
                .parse_plain_attribute(name, value),
        }
    }

    fn children_changed(&self, cx: &mut JSContext, mutation: &ChildrenMutation) {
        if let Some(super_type) = self.super_type() {
            super_type.children_changed(cx, mutation);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn unbind_from_tree(&self, cx: &mut js::context::JSContext, context: &UnbindContext<'_>) {
        if let Some(s) = self.super_type() {
            s.unbind_from_tree(cx, context);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }
}

/// Uppercase the first typographic letter of every word. Approximates CSS
/// `text-transform: capitalize` (CSS Text 4): a word boundary is a run of
/// non-alphanumeric characters, except that apostrophes between letters
/// ("don't") do not start a new word, and digits do not consume the word
/// start ("2abc" becomes "2Abc").
fn capitalize_words(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut at_word_start = true;
    let mut previous_is_letter = false;

    for character in text.chars() {
        if character.is_alphabetic() {
            if at_word_start {
                result.extend(character.to_uppercase());
                at_word_start = false;
                previous_is_letter = true;
                continue;
            }
            previous_is_letter = true;
        } else if matches!(character, '\'' | '\u{2019}') && previous_is_letter {
            // A word-internal apostrophe: keep the word going.
        } else if !character.is_numeric() {
            at_word_start = true;
            previous_is_letter = false;
        }
        result.push(character);
    }

    result
}
