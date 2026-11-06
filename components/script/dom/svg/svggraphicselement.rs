/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashMap;

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::JSContext;
use kurbo::{Affine, Point, Rect};
use style::properties::{LonghandId, NonCustomPropertyId};
use stylo_atoms::Atom;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::DocumentFragmentBinding::DocumentFragmentMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::codegen::Bindings::SVGGraphicsElementBinding::SVGGraphicsElementMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::document::documentfragment::DocumentFragment;
use crate::dom::element::Element;
use crate::dom::geometry::domrect::DOMRect;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svggeometryelement::SVGGeometryElement;
use crate::dom::svg::svgsvgelement::{SVGSVGElement, sets_own_display};

/// How many elements one bounding box may visit, counting each use of a
/// referenced element again, and how deeply it may nest. Beyond either,
/// `getBBox()` throws `NotSupportedError` instead of hanging on symbols
/// that reference each other several times over, or overflowing the stack.
const MAX_BOUNDING_BOX_VISITS: usize = 100_000;
const MAX_BOUNDING_BOX_DEPTH: usize = 256;

#[dom_struct]
pub(crate) struct SVGGraphicsElement {
    svgelement: SVGElement,
}

impl SVGGraphicsElement {
    pub(crate) fn new_inherited(
        tag_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGGraphicsElement {
        SVGGraphicsElement::new_inherited_with_state(
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
    ) -> SVGGraphicsElement {
        SVGGraphicsElement {
            svgelement: SVGElement::new_inherited_with_state(state, tag_name, prefix, document),
        }
    }
}

impl SVGGraphicsElementMethods<crate::DomTypeHolder> for SVGGraphicsElement {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGGraphicsElement__getBBox>
    ///
    /// The fill bounding box in the element's user space. An element that
    /// is not rendered (detached, `display: none`, or inside `defs`, a
    /// `symbol` or another element that is never rendered) gets an empty
    /// box at the origin, as the SVG WG resolved in
    /// <https://github.com/w3c/svgwg/issues/1018>. The geometry comes from
    /// the elements' attributes, as for `getTotalLength()`. Text needs a
    /// text layout that script cannot reach, so the boxes of text and of
    /// containers holding rendered text throw `NotSupportedError`, as do
    /// nested viewports with a `viewBox`.
    fn GetBBox(&self, cx: &mut JSContext) -> Fallible<DomRoot<DOMRect>> {
        let element = self.upcast::<Element>();
        let bounds = if is_rendered(element) {
            element_bounds(cx, element, &mut BoundsWalk::default())
                .map_err(|UnsupportedGeometry| {
                    Error::NotSupported(Some(
                        "getBBox() does not compute text, viewports with a viewBox, use of an svg \
                         element, or geometry beyond its limits"
                            .into(),
                    ))
                })?
                .unwrap_or(Rect::ZERO)
        } else {
            Rect::ZERO
        };
        Ok(DOMRect::new(
            cx,
            &self.global(),
            bounds.x0,
            bounds.y0,
            bounds.width(),
            bounds.height(),
        ))
    }
}

/// The geometry of an element that script cannot compute: text, a viewport
/// with a `viewBox`, or more than the limits above allow.
struct UnsupportedGeometry;

/// The state of one bounding box computation.
#[derive(Default)]
struct BoundsWalk {
    /// The `use` targets being expanded, innermost last. A `use` element
    /// whose target is among them is a circular reference, an error that
    /// renders nothing.
    use_targets: Vec<DomRoot<Element>>,
    /// Whether `display: none` applies, by element: a style query each
    /// time would make the elements a `use` repeats slow to visit.
    display_none: HashMap<*const Element, bool>,
    visits: usize,
    depth: usize,
}

impl BoundsWalk {
    fn visit(&mut self) -> Result<(), UnsupportedGeometry> {
        self.visits += 1;
        if self.visits > MAX_BOUNDING_BOX_VISITS || self.depth > MAX_BOUNDING_BOX_DEPTH {
            return Err(UnsupportedGeometry);
        }
        Ok(())
    }

    fn is_display_none(&mut self, element: &Element) -> bool {
        *self
            .display_none
            .entry(element as *const Element)
            .or_insert_with(|| is_display_none(element))
    }

    /// Runs `f` one level deeper.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }
}

/// The computed value of a property, as `getComputedStyle()` returns it.
/// Unlike the element's own style data, this also resolves the style of an
/// element that layout left unstyled, such as one below a `display: none`
/// element; the descendants of an `svg` element's children are such
/// elements (see [`is_display_none`]). Empty for a disconnected element.
pub(crate) fn computed_value(element: &Element, longhand: LonghandId) -> String {
    let node = element.upcast::<Node>();
    if !node.is_connected() {
        return String::new();
    }
    node.owner_window()
        .resolved_style_query(
            node.to_trusted_node_address(),
            None,
            NonCustomPropertyId::from_longhand(longhand).to_property_id(),
        )
        .into()
}

/// Whether `display: none` applies to an SVG element. The user agent style
/// sheet hides the children of an `svg` element from CSS layout (`svg > *`
/// in servo.css), which is not their rendered state; as when serializing
/// for the rasterizer, a computed `none` on such a child counts only if the
/// element sets `display` itself.
fn is_display_none(element: &Element) -> bool {
    if computed_value(element, LonghandId::Display) != "none" {
        return false;
    }
    let parent_is_svg = element
        .upcast::<Node>()
        .GetParentNode()
        .is_some_and(|parent| parent.is::<SVGSVGElement>());
    !parent_is_svg || sets_own_display(element)
}

/// Graphics and container elements: what renders, itself or through its
/// children, and what a `switch` chooses among. Never-rendered elements
/// (`defs`, `symbol`, `clipPath` and the like), `use` targets in place and
/// unknown elements render nothing where they are.
fn is_rendering_element(element: &Element) -> bool {
    *element.namespace() == ns!(svg) &&
        matches!(
            *element.local_name(),
            local_name!("a") |
                local_name!("circle") |
                local_name!("ellipse") |
                local_name!("foreignObject") |
                local_name!("g") |
                local_name!("image") |
                local_name!("line") |
                local_name!("path") |
                local_name!("polygon") |
                local_name!("polyline") |
                local_name!("rect") |
                local_name!("svg") |
                local_name!("switch") |
                local_name!("text") |
                local_name!("use")
        )
}

/// Elements whose children render where they are. The children of `use`,
/// of never-rendered elements and of unknown elements do not.
fn renders_children(element: &Element) -> bool {
    matches!(
        *element.local_name(),
        local_name!("a") |
            local_name!("g") |
            local_name!("svg") |
            local_name!("switch") |
            local_name!("text") |
            local_name!("textPath") |
            local_name!("tspan")
    )
}

/// The child a `switch` renders: the first graphics or container element,
/// as the conditional attributes are not evaluated. It renders nothing if
/// it is `display: none`; the others never render.
fn switch_child(switch: &Element) -> Option<DomRoot<Element>> {
    switch
        .upcast::<Node>()
        .children()
        .filter_map(DomRoot::downcast::<Element>)
        .find(|child| is_rendering_element(child))
}

/// Whether the element takes part in rendering: it is a connected graphics
/// or container element inside an `svg` element, it and its ancestors
/// render their children and are not `display: none`, each `switch` on the
/// way chose it, and CSS lays out its outermost `svg` element.
fn is_rendered(element: &Element) -> bool {
    let node = element.upcast::<Node>();
    if !node.is_connected() || !is_rendering_element(element) || is_display_none(element) {
        return false;
    }
    let mut outermost_svg =
        (*element.local_name() == local_name!("svg")).then(|| DomRoot::from_ref(element));
    let mut child = DomRoot::from_ref(element);
    for ancestor in node.ancestors().filter_map(DomRoot::downcast::<Element>) {
        if *ancestor.namespace() != ns!(svg) {
            break;
        }
        if !renders_children(&ancestor) || is_display_none(&ancestor) {
            return false;
        }
        if *ancestor.local_name() == local_name!("switch") &&
            !switch_child(&ancestor).is_some_and(|selected| std::ptr::eq(&*selected, &*child))
        {
            return false;
        }
        if *ancestor.local_name() == local_name!("svg") {
            outermost_svg = Some(ancestor.clone());
        }
        child = ancestor;
    }
    // Layout styles the outermost `svg` element unless it is inside a
    // `display: none` element.
    outermost_svg.is_some_and(|svg| {
        svg.style()
            .is_some_and(|style| !style.get_box().display.is_none())
    })
}

/// The bounding box of a rendered element in its own user space. `None`
/// when it has no geometry, which adds nothing to a container's box.
fn element_bounds(
    cx: &JSContext,
    element: &Element,
    walk: &mut BoundsWalk,
) -> Result<Option<Rect>, UnsupportedGeometry> {
    if *element.namespace() != ns!(svg) {
        return Ok(None);
    }
    walk.visit()?;
    if let Some(geometry) = element.downcast::<SVGGeometryElement>() {
        return Ok(geometry.fill_bounds());
    }
    match *element.local_name() {
        local_name!("svg") | local_name!("g") | local_name!("a") => {
            children_bounds(cx, element, walk)
        },
        local_name!("switch") => match switch_child(element) {
            Some(child) => child_bounds(cx, &child, walk),
            None => Ok(None),
        },
        local_name!("use") => use_bounds(cx, element, walk),
        local_name!("image") | local_name!("foreignObject") => Ok(viewport_bounds(element)),
        local_name!("text") | local_name!("tspan") | local_name!("textPath") => {
            Err(UnsupportedGeometry)
        },
        _ => Ok(None),
    }
}

/// The union of the boxes of a container's children, each mapped into the
/// container's user space.
fn children_bounds(
    cx: &JSContext,
    container: &Element,
    walk: &mut BoundsWalk,
) -> Result<Option<Rect>, UnsupportedGeometry> {
    let mut bounds: Option<Rect> = None;
    for child in container
        .upcast::<Node>()
        .children()
        .filter_map(DomRoot::downcast::<Element>)
    {
        if let Some(child_bounds) = child_bounds(cx, &child, walk)? {
            bounds = Some(bounds.map_or(child_bounds, |bounds| bounds.union(child_bounds)));
        }
    }
    Ok(bounds)
}

/// A child's box in its parent's user space. `None` when it renders
/// nothing.
fn child_bounds(
    cx: &JSContext,
    child: &Element,
    walk: &mut BoundsWalk,
) -> Result<Option<Rect>, UnsupportedGeometry> {
    if !is_rendering_element(child) || walk.is_display_none(child) {
        return Ok(None);
    }
    let Some(bounds) = walk.nested(|walk| element_bounds(cx, child, walk))? else {
        return Ok(None);
    };
    Ok(Some(to_parent_space(child)?.transform_rect_bbox(bounds)))
}

/// The transform from an element's user space to its parent's: its
/// `transform` attribute and, for an `svg` or `symbol` element, which
/// establish a viewport, the viewport's offset.
fn to_parent_space(element: &Element) -> Result<Affine, UnsupportedGeometry> {
    let transform = transform_attribute(element);
    if !matches!(
        *element.local_name(),
        local_name!("svg") | local_name!("symbol")
    ) {
        return Ok(transform);
    }
    if has_view_box(element) {
        return Err(UnsupportedGeometry);
    }
    let offset = (
        length_attribute(element, &local_name!("x")).unwrap_or(0.),
        length_attribute(element, &local_name!("y")).unwrap_or(0.),
    );
    Ok(transform * Affine::translate(offset))
}

/// Whether the element has a `viewBox` attribute, whose case-sensitive
/// name the lowercase attribute lookup does not take.
fn has_view_box(element: &Element) -> bool {
    element
        .get_attribute_string_value_with_namespace(&ns!(), &local_name!("viewBox"))
        .is_some()
}

/// <https://svgwg.org/svg2-draft/struct.html#UseElement>: the referenced
/// element's box, with its own `transform` (and a `symbol`'s offset),
/// offset by `x` and `y`. A reference to a missing or hidden element, to
/// another document or to the `use` element's own ancestor renders nothing.
fn use_bounds(
    cx: &JSContext,
    use_element: &Element,
    walk: &mut BoundsWalk,
) -> Result<Option<Rect>, UnsupportedGeometry> {
    let href = use_element
        .get_attribute_string_value(&local_name!("href"))
        .or_else(|| {
            use_element.get_attribute_string_value_with_namespace(&ns!(xlink), &local_name!("href"))
        });
    let Some(id) = href
        .as_deref()
        .and_then(|href| href.trim().strip_prefix('#'))
    else {
        return Ok(None);
    };
    // The reference resolves in the `use` element's own tree.
    let target = match use_element.upcast::<Node>().containing_shadow_root() {
        Some(shadow_root) => shadow_root
            .upcast::<DocumentFragment>()
            .GetElementById(cx, DOMString::from(id)),
        None => use_element
            .owner_document()
            .get_element_by_id(cx.no_gc(), &Atom::from(id)),
    };
    let Some(target) = target else {
        return Ok(None);
    };
    let is_circular = target
        .upcast::<Node>()
        .is_inclusive_ancestor_of(use_element.upcast::<Node>()) ||
        walk.use_targets
            .iter()
            .any(|expanded| std::ptr::eq(&**expanded, &*target));
    if is_circular {
        return Ok(None);
    }
    // A target that is `display: none` renders nothing in the `use` either.
    if walk.is_display_none(&target) {
        return Ok(None);
    }
    walk.use_targets.push(target.clone());
    let target_bounds = walk.nested(|walk| target_bounds(cx, &target, walk));
    walk.use_targets.pop();
    let target_bounds = target_bounds?;
    let offset = (
        length_attribute(use_element, &local_name!("x")).unwrap_or(0.),
        length_attribute(use_element, &local_name!("y")).unwrap_or(0.),
    );
    Ok(target_bounds.map(|bounds| Affine::translate(offset).transform_rect_bbox(bounds)))
}

/// The box of what a `use` element renders of its target, in the `use`
/// element's user space before its `x` and `y`.
fn target_bounds(
    cx: &JSContext,
    target: &Element,
    walk: &mut BoundsWalk,
) -> Result<Option<Rect>, UnsupportedGeometry> {
    match *target.local_name() {
        // A referenced `svg` element takes the `use` element's size for its
        // viewport, which is not computed here.
        local_name!("svg") => Err(UnsupportedGeometry),
        local_name!("symbol") => match children_bounds(cx, target, walk)? {
            Some(bounds) => Ok(Some(to_parent_space(target)?.transform_rect_bbox(bounds))),
            None => Ok(None),
        },
        _ if is_rendering_element(target) => Ok(element_bounds(cx, target, walk)?
            .map(|bounds| transform_attribute(target).transform_rect_bbox(bounds))),
        _ => Ok(None),
    }
}

/// The viewport of an `image` or `foreignObject` element. A missing or
/// non-positive size renders nothing; an image's intrinsic size, which
/// `auto` would use, is not available here.
fn viewport_bounds(element: &Element) -> Option<Rect> {
    let width = length_attribute(element, &local_name!("width")).filter(|width| *width > 0.)?;
    let height = length_attribute(element, &local_name!("height")).filter(|height| *height > 0.)?;
    let x = length_attribute(element, &local_name!("x")).unwrap_or(0.);
    let y = length_attribute(element, &local_name!("y")).unwrap_or(0.);
    Some(Rect::new(x, y, x + width, y + height))
}

/// A length attribute in user units: a number, optionally in `px`. Other
/// units and percentages count as missing, as for the geometry attributes
/// of shapes.
fn length_attribute(element: &Element, name: &LocalName) -> Option<f64> {
    let value = element.get_attribute_string_value(name)?;
    let value = value.trim();
    parse_number(value.strip_suffix("px").unwrap_or(value))
}

/// An SVG number: digits with an optional sign, fraction and exponent. Not
/// `inf` or `NaN`, which Rust's parser also accepts.
fn parse_number(value: &str) -> Option<f64> {
    if !value.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

/// The `transform` attribute: <https://www.w3.org/TR/SVG11/coords.html#TransformAttribute>.
/// A missing or invalid list is the identity, as an invalid attribute is
/// ignored.
fn transform_attribute(element: &Element) -> Affine {
    element
        .get_attribute_string_value(&local_name!("transform"))
        .and_then(|list| parse_transform_list(&list))
        .unwrap_or(Affine::IDENTITY)
}

/// Parses a transform list such as `translate(10, 20) rotate(45 5 5)`: the
/// product of its transforms in order, so the last one applies first.
fn parse_transform_list(list: &str) -> Option<Affine> {
    let mut rest = list.trim_start_matches(is_wsp);
    let mut result = Affine::IDENTITY;
    while !rest.is_empty() {
        let name_end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let (name, after_name) = rest.split_at(name_end);
        let arguments_start = after_name.trim_start_matches(is_wsp).strip_prefix('(')?;
        let arguments_end = arguments_start.find(')')?;
        let arguments = parse_number_list(&arguments_start[..arguments_end])?;
        result *= transform_function(name, &arguments)?;
        // Transforms are separated by whitespace and at most one comma.
        rest = arguments_start[arguments_end + 1..].trim_start_matches(is_wsp);
        if let Some(after_comma) = rest.strip_prefix(',') {
            rest = after_comma.trim_start_matches(is_wsp);
            if rest.is_empty() {
                return None;
            }
        }
    }
    Some(result)
}

/// One transform function with its arguments; `None` for an unknown name
/// or a wrong number of arguments.
fn transform_function(name: &str, arguments: &[f64]) -> Option<Affine> {
    let transform = match (name, arguments) {
        ("matrix", &[a, b, c, d, e, f]) => Affine::new([a, b, c, d, e, f]),
        ("translate", &[tx]) => Affine::translate((tx, 0.)),
        ("translate", &[tx, ty]) => Affine::translate((tx, ty)),
        ("scale", &[s]) => Affine::scale(s),
        ("scale", &[sx, sy]) => Affine::scale_non_uniform(sx, sy),
        ("rotate", &[angle]) => Affine::rotate(angle.to_radians()),
        ("rotate", &[angle, cx, cy]) => {
            Affine::rotate_about(angle.to_radians(), Point::new(cx, cy))
        },
        ("skewX", &[angle]) => Affine::new([1., 0., angle.to_radians().tan(), 1., 0., 0.]),
        ("skewY", &[angle]) => Affine::new([1., angle.to_radians().tan(), 0., 1., 0., 0.]),
        _ => return None,
    };
    Some(transform)
}

/// Numbers separated by whitespace or a comma, as in a transform's
/// arguments. A number's sign or a second decimal point also ends the
/// previous number, as in `1-2` or `.5.5`.
fn parse_number_list(list: &str) -> Option<Vec<f64>> {
    let mut numbers = Vec::new();
    let mut rest = list.trim_matches(is_wsp);
    while !rest.is_empty() {
        let length = number_length(rest)?;
        numbers.push(parse_number(&rest[..length])?);
        rest = rest[length..].trim_start_matches(is_wsp);
        if let Some(after_comma) = rest.strip_prefix(',') {
            rest = after_comma.trim_start_matches(is_wsp);
            if rest.is_empty() {
                return None;
            }
        }
    }
    Some(numbers)
}

/// The length of the SVG number at the start of `text`, if any.
fn number_length(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        index += 1;
    }
    let integer_digits = bytes[index..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    index += integer_digits;
    let mut fraction_digits = 0;
    if bytes.get(index) == Some(&b'.') {
        fraction_digits = bytes[index + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        // `1.` and `.5` are numbers, a lone `.` is not.
        if integer_digits > 0 || fraction_digits > 0 {
            index += 1 + fraction_digits;
        }
    }
    if integer_digits == 0 && fraction_digits == 0 {
        return None;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        let mut exponent = index + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let exponent_digits = bytes[exponent..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if exponent_digits > 0 {
            index = exponent + exponent_digits;
        }
    }
    Some(index)
}

/// SVG's whitespace: <https://www.w3.org/TR/SVG11/types.html#BasicDataTypes>.
fn is_wsp(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

impl VirtualMethods for SVGGraphicsElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGElement>() as &dyn VirtualMethods)
    }
}
