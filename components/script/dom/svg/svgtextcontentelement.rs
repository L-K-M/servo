/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGTextContentElementBinding::SVGTextContentElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::DomRoot;
use crate::dom::characterdata::CharacterData;
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::svg::svggraphicselement::{
    SVGGraphicsElement, is_display_none, is_rendered, length_attribute,
};
use crate::dom::text::Text;

#[dom_struct]
pub(crate) struct SVGTextContentElement {
    svggraphicselement: SVGGraphicsElement,
}

impl SVGTextContentElement {
    pub(crate) fn new_inherited(
        tag_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGTextContentElement {
        SVGTextContentElement::new_inherited_with_state(
            ElementState::empty(),
            tag_name,
            prefix,
            document,
        )
    }

    pub(crate) fn new_inherited_with_state(
        state: ElementState,
        tag_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGTextContentElement {
        SVGTextContentElement {
            svggraphicselement: SVGGraphicsElement::new_inherited_with_state(
                state, tag_name, prefix, document,
            ),
        }
    }

    /// The text that this element renders, one string per text node with
    /// white space processed, as part of its `text` element. Empty strings
    /// are left out, and so is everything when the element does not render.
    fn rendered_text(&self) -> Vec<RenderedText> {
        let element = self.upcast::<Element>();
        if !is_rendered(element) {
            return Vec::new();
        }
        let Some(text_element) = element
            .upcast::<Node>()
            .inclusive_ancestors(ShadowIncluding::No)
            .filter_map(DomRoot::downcast::<Element>)
            .find(|ancestor| *ancestor.local_name() == local_name!("text"))
        else {
            return Vec::new();
        };

        // White space collapses across the whole `text` element, so collect
        // all of it and keep this element's part.
        let mut texts = Vec::new();
        collect_rendered_text(text_element, element, &mut texts);
        process_white_space(&mut texts);
        texts.retain(|rendered| rendered.in_element && !rendered.text.is_empty());
        texts
    }
}

impl SVGTextContentElementMethods<crate::DomTypeHolder> for SVGTextContentElement {
    /// <https://svgwg.org/svg2-draft/text.html#__svg__SVGTextContentElement__getNumberOfChars>
    fn GetNumberOfChars(&self) -> i32 {
        // The DOM counts UTF-16 code units as characters.
        let count: usize = self
            .rendered_text()
            .iter()
            .map(|rendered| rendered.text.encode_utf16().count())
            .sum();
        count.try_into().unwrap_or(i32::MAX)
    }

    /// <https://svgwg.org/svg2-draft/text.html#__svg__SVGTextContentElement__getComputedTextLength>
    fn GetComputedTextLength(&self) -> Finite<f32> {
        let runs: Vec<_> = self
            .rendered_text()
            .into_iter()
            .map(|rendered| {
                (
                    rendered.parent.upcast::<Node>().to_trusted_node_address(),
                    rendered.text,
                )
            })
            .collect();
        if runs.is_empty() {
            return Finite::wrap(0.);
        }
        let advances = self
            .owner_window()
            .text_advances_query(&runs)
            .into_iter()
            .fold(0., |total, advance| total + advance);

        // `lengthAdjust="spacingAndGlyphs"` scales the glyphs to fill
        // `textLength`, and so their advances. The default, `spacing`, only
        // widens the gaps between them, which are not part of any advance.
        let element = self.upcast::<Element>();
        let scales_glyphs = element
            .get_attribute_string_value_with_namespace(&ns!(), &local_name!("lengthAdjust"))
            .is_some_and(|value| value == "spacingAndGlyphs");
        let length = match length_attribute(element, &local_name!("textLength")) {
            Some(text_length) if scales_glyphs && text_length >= 0. && advances > 0. => {
                text_length as f32
            },
            _ => advances,
        };
        Finite::wrap(length.min(f32::MAX))
    }
}

/// How `xml:space` treats white space in SVG text. The rasterizer follows it
/// rather than CSS `white-space`.
#[derive(Clone, Copy, PartialEq)]
enum XmlSpace {
    /// A run of white space collapses to one space, and the `text`
    /// element's leading and trailing spaces go.
    Default,
    /// Each white space character becomes a space.
    Preserve,
}

impl XmlSpace {
    fn from_attribute(element: &Element) -> Option<XmlSpace> {
        let value =
            element.get_attribute_string_value_with_namespace(&ns!(xml), &local_name!("space"))?;
        Some(if value == "preserve" {
            XmlSpace::Preserve
        } else {
            XmlSpace::Default
        })
    }
}

/// The `xml:space` that applies to an element: its own, else that of its
/// nearest ancestor that sets one.
fn inherited_xml_space(element: &Element) -> XmlSpace {
    element
        .upcast::<Node>()
        .inclusive_ancestors(ShadowIncluding::No)
        .filter_map(DomRoot::downcast::<Element>)
        .find_map(|ancestor| XmlSpace::from_attribute(&ancestor))
        .unwrap_or(XmlSpace::Default)
}

/// A text node that a `text` element renders.
struct RenderedText {
    /// The element whose style the text takes.
    parent: DomRoot<Element>,
    text: String,
    space: XmlSpace,
    /// Whether the text is inside the element being asked about.
    in_element: bool,
}

/// Collects the text nodes below `parent` that render, in document order:
/// those in `a` and `tspan` elements and in a `textPath` element directly
/// inside the `text` element, as the rasterizer reads them, but not in a
/// `display: none` element.
fn collect_rendered_text(
    root: DomRoot<Element>,
    measured: &Element,
    texts: &mut Vec<RenderedText>,
) {
    let root_space = inherited_xml_space(&root);
    let in_element = std::ptr::eq(&*root, measured);
    let root_children = root.upcast::<Node>().children();
    let mut stack = vec![(root, root_children, root_space, in_element)];
    while let Some((parent, children, space, in_element)) = stack.last_mut() {
        let Some(child) = children.next() else {
            stack.pop();
            continue;
        };
        if let Some(text) = child.downcast::<Text>() {
            let text = text.upcast::<CharacterData>().data();
            if let Some(previous) = texts.last_mut() &&
                std::ptr::eq(&*previous.parent, &**parent)
            {
                previous.text.push_str(&text);
            } else {
                texts.push(RenderedText {
                    parent: parent.clone(),
                    text: text.clone(),
                    space: *space,
                    in_element: *in_element,
                });
            }
            continue;
        }
        let Some(child) = DomRoot::downcast::<Element>(child) else {
            continue;
        };
        let renders_text = *child.namespace() == ns!(svg) &&
            match *child.local_name() {
                local_name!("a") | local_name!("tspan") => true,
                local_name!("textPath") => *parent.local_name() == local_name!("text"),
                _ => false,
            };
        if !renders_text || is_display_none(&child) {
            continue;
        }
        let children = child.upcast::<Node>().children();
        let space = XmlSpace::from_attribute(&child).unwrap_or(*space);
        let in_element = *in_element || std::ptr::eq(&*child, measured);
        stack.push((child, children, space, in_element));
    }
}

/// Processes white space across a `text` element's text nodes by the SVG 1.1
/// rules that the rasterizer follows: tabs and line breaks become spaces,
/// and where `xml:space` is not `preserve`, a space after another space or
/// at the start goes, and so does a final space.
fn process_white_space(texts: &mut [RenderedText]) {
    let mut after_space = true;
    for rendered in texts.iter_mut() {
        let mut processed = String::with_capacity(rendered.text.len());
        for character in rendered.text.chars() {
            let character = match character {
                '\r' | '\n' | '\t' => ' ',
                character => character,
            };
            if character == ' ' && after_space && rendered.space == XmlSpace::Default {
                continue;
            }
            after_space = character == ' ';
            processed.push(character);
        }
        rendered.text = processed;
    }

    if let Some(last) = texts
        .iter_mut()
        .rev()
        .find(|rendered| !rendered.text.is_empty()) &&
        last.space == XmlSpace::Default &&
        last.text.ends_with(' ')
    {
        last.text.pop();
    }
}

impl VirtualMethods for SVGTextContentElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }
}
