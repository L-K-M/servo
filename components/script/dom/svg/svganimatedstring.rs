/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, ns};
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::SVGAnimatedStringBinding::SVGAnimatedStringMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;

/// An `SVGAnimatedString` reflecting a content attribute of an SVG element.
/// Servo does not animate attributes, so `animVal` always equals `baseVal`.
#[dom_struct]
pub(crate) struct SVGAnimatedString {
    reflector_: Reflector,
    element: Dom<Element>,
    #[no_trace]
    local_name: LocalName,
}

impl SVGAnimatedString {
    fn new_inherited(element: &Element, local_name: LocalName) -> SVGAnimatedString {
        SVGAnimatedString {
            reflector_: Reflector::new(),
            element: Dom::from_ref(element),
            local_name,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        element: &Element,
        local_name: LocalName,
    ) -> DomRoot<SVGAnimatedString> {
        reflect_dom_object(
            cx,
            Box::new(SVGAnimatedString::new_inherited(element, local_name)),
            &*element.owner_window(),
        )
    }
}

impl SVGAnimatedStringMethods<crate::DomTypeHolder> for SVGAnimatedString {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__baseVal>
    fn BaseVal(&self) -> DOMString {
        // The attributes reflected so far all have the empty string as their
        // initial value.
        self.element.get_string_attribute(&self.local_name)
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__baseVal>
    fn SetBaseVal(&self, cx: &mut JSContext, value: DOMString) {
        let value = self
            .element
            .parse_attribute(&ns!(), &self.local_name, value);
        // Set the attribute in no namespace, which `baseVal` reads, even when
        // an attribute in another namespace has the same local name.
        self.element.set_attribute_with_namespace(
            cx,
            self.local_name.clone(),
            value,
            self.local_name.clone(),
            ns!(),
            None,
        );
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__animVal>
    fn AnimVal(&self) -> DOMString {
        self.BaseVal()
    }
}
