/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::f64::consts::{FRAC_PI_2, PI};

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use kurbo::{
    Arc, BezPath, CubicBez, Line, ParamCurve, ParamCurveArclen, ParamCurveExtrema,
    ParamCurveNearest, PathEl, PathSeg, Point, QuadBez, Rect, Shape, SvgArc, Vec2,
};
use style::properties::LonghandId;
use style::values::generics::Optional;
use style::values::generics::basic_shape::{ArcSize, ArcSweep};
use style::values::specified::svg_path::{CoordPair, PathCommand, SVGPathData};
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::DOMPointBinding::DOMPointInit;
use crate::dom::bindings::codegen::Bindings::SVGGeometryElementBinding::SVGGeometryElementMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::geometry::dompoint::DOMPoint;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::svg::svggraphicselement::{SVGGraphicsElement, computed_value};

/// Accuracy, in user units, of the arc lengths of curved path segments.
const ARC_LENGTH_ACCURACY: f64 = 1e-4;

/// Tolerance, in user units, of the Bézier curves that approximate
/// elliptical arcs, unless the arc is large enough for
/// `RELATIVE_ARC_TOLERANCE`.
const ARC_TOLERANCE: f64 = 1e-4;

/// Tolerance relative to an arc's larger radius. It bounds the number of
/// Bézier curves for huge radii; the results are `float`s anyway.
const RELATIVE_ARC_TOLERANCE: f64 = 1e-6;

/// How closely the Bézier curves that stand in for elliptical arcs follow
/// them: the larger of an absolute tolerance in user units and one relative
/// to the arc's larger radius.
#[derive(Clone, Copy)]
struct ArcTolerance {
    absolute: f64,
    relative: f64,
}

/// For lengths and points along the path.
const LENGTH_ARC_TOLERANCE: ArcTolerance = ArcTolerance {
    absolute: ARC_TOLERANCE,
    relative: RELATIVE_ARC_TOLERANCE,
};

/// For bounding boxes and hit testing, whose results must match the arc's
/// own extremes and outline to `float` precision. An arc then takes a few
/// dozen curves instead of a few.
const PRECISE_ARC_TOLERANCE: ArcTolerance = ArcTolerance {
    absolute: 1e-9,
    relative: 1e-12,
};

/// How far, relative to the point's larger coordinate (and at least in
/// user units), a point may lie from the outline and still count as on it.
const OUTLINE_TOLERANCE: f64 = 1e-9;

/// Accuracy of the distance from a point to a curve.
const NEAREST_ACCURACY: f64 = 1e-12;

#[dom_struct]
pub(crate) struct SVGGeometryElement {
    svggraphicselement: SVGGraphicsElement,
}

impl SVGGeometryElement {
    pub(crate) fn new_inherited(
        tag_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGGeometryElement {
        SVGGeometryElement::new_inherited_with_state(
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
    ) -> SVGGeometryElement {
        SVGGeometryElement {
            svggraphicselement: SVGGraphicsElement::new_inherited_with_state(
                state, tag_name, prefix, document,
            ),
        }
    }
}

impl SVGGeometryElement {
    /// The element's path in user units, from its geometry attributes:
    /// <https://svgwg.org/svg2-draft/paths.html#PathElement> and
    /// <https://svgwg.org/svg2-draft/shapes.html>. Lengths other than plain
    /// numbers or `px` values (percentages, font-relative units) and the
    /// CSS `d` property are not supported and count as missing.
    fn path(&self) -> BezPath {
        self.path_with(LENGTH_ARC_TOLERANCE)
    }

    /// [`Self::path`], with elliptical arcs approximated to `tolerance`.
    fn path_with(&self, tolerance: ArcTolerance) -> BezPath {
        let element = self.upcast::<Element>();
        let attribute = |name: &LocalName| element.get_string_attribute(name);
        let number = |name: &LocalName| parse_length(&attribute(name).str());

        match *element.local_name() {
            local_name!("path") => parse_path_data(&attribute(&local_name!("d")).str(), tolerance),
            local_name!("rect") => rect_path(
                number(&local_name!("x")).unwrap_or(0.),
                number(&local_name!("y")).unwrap_or(0.),
                number(&local_name!("width")).unwrap_or(0.),
                number(&local_name!("height")).unwrap_or(0.),
                number(&local_name!("rx")),
                number(&local_name!("ry")),
                tolerance,
            ),
            local_name!("circle") => {
                let r = number(&local_name!("r")).unwrap_or(0.);
                ellipse_path(
                    number(&local_name!("cx")).unwrap_or(0.),
                    number(&local_name!("cy")).unwrap_or(0.),
                    r,
                    r,
                    tolerance,
                )
            },
            local_name!("ellipse") => {
                // `auto` (or a missing or invalid value, and a negative one,
                // which is an error) takes the other radius.
                let rx = number(&local_name!("rx")).filter(|rx| *rx >= 0.);
                let ry = number(&local_name!("ry")).filter(|ry| *ry >= 0.);
                ellipse_path(
                    number(&local_name!("cx")).unwrap_or(0.),
                    number(&local_name!("cy")).unwrap_or(0.),
                    rx.or(ry).unwrap_or(0.),
                    ry.or(rx).unwrap_or(0.),
                    tolerance,
                )
            },
            local_name!("line") => {
                let mut path = BezPath::new();
                path.move_to((
                    number(&local_name!("x1")).unwrap_or(0.),
                    number(&local_name!("y1")).unwrap_or(0.),
                ));
                path.line_to((
                    number(&local_name!("x2")).unwrap_or(0.),
                    number(&local_name!("y2")).unwrap_or(0.),
                ));
                path
            },
            local_name!("polyline") => {
                points_path(&attribute(&local_name!("points")).str(), false, tolerance)
            },
            local_name!("polygon") => {
                points_path(&attribute(&local_name!("points")).str(), true, tolerance)
            },
            _ => BezPath::new(),
        }
    }

    /// The bounding box of the element's fill in user units:
    /// <https://svgwg.org/svg2-draft/coords.html#BoundingBoxes>. `None` for
    /// an element without geometry (no path commands, or a shape whose
    /// sizes disable its rendering), which also adds nothing to the
    /// bounding box of a container.
    pub(crate) fn fill_bounds(&self) -> Option<Rect> {
        path_bounds(&self.path_with(PRECISE_ARC_TOLERANCE))
    }

    /// Whether the computed `fill-rule` is `evenodd`; `clip-rule` never
    /// applies here.
    fn fills_even_odd(&self) -> bool {
        computed_value(self.upcast::<Element>(), LonghandId::FillRule) == "evenodd"
    }
}

impl SVGGeometryElementMethods<crate::DomTypeHolder> for SVGGeometryElement {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGGeometryElement__isPointInFill>
    fn IsPointInFill(&self, point: &DOMPointInit) -> bool {
        if !point.x.is_finite() || !point.y.is_finite() {
            return false;
        }
        let point = Point::new(point.x, point.y);
        let path = closed_subpaths(&self.path_with(PRECISE_ARC_TOLERANCE));
        // Points on the outline are inside, as in other engines.
        if is_on_outline(&path, point) {
            return true;
        }
        let winding = path.winding(point);
        if self.fills_even_odd() {
            winding % 2 != 0
        } else {
            winding != 0
        }
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGGeometryElement__getTotalLength>
    fn GetTotalLength(&self) -> Finite<f32> {
        let length: f64 = self
            .path()
            .segments()
            .map(|segment| segment.arclen(ARC_LENGTH_ACCURACY))
            // Not `sum()`, which yields -0 for a path without segments.
            .fold(0., |total, length| total + length);
        Finite::wrap((length as f32).min(f32::MAX))
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGGeometryElement__getPointAtLength>
    fn GetPointAtLength(
        &self,
        cx: &mut JSContext,
        distance: Finite<f32>,
    ) -> Fallible<DomRoot<DOMPoint>> {
        let point =
            point_at_length(&self.path(), *distance as f64).ok_or(Error::InvalidState(None))?;
        Ok(DOMPoint::new(cx, &self.global(), point.x, point.y, 0., 1.))
    }
}

/// Parses a geometry attribute that holds a length in user units.
fn parse_length(value: &str) -> Option<f64> {
    let value = value.trim();
    let number = value.strip_suffix("px").unwrap_or(value);
    parse_number(number)
}

fn parse_number(value: &str) -> Option<f64> {
    // Rust also accepts `inf` and `NaN`, which are not SVG numbers.
    if !value.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

/// Parses SVG path data. Stylo's parser keeps the commands before an
/// error, as <https://svgwg.org/svg2-draft/paths.html#PathDataErrorHandling>
/// requires.
fn parse_path_data(data: &str, tolerance: ArcTolerance) -> BezPath {
    let to_point = |pair: CoordPair| Point::new(pair.x as f64, pair.y as f64);
    let (data, _) = SVGPathData::parse_bytes(data.as_bytes());
    let mut path = BezPath::new();
    let mut current = Point::ZERO;
    let mut subpath_start = Point::ZERO;
    let mut closed = false;

    // Absolute commands, with every curve reduced to a cubic Bézier or an arc.
    for command in data.normalize(true).commands() {
        // A command after a closepath starts a new subpath at the same point.
        if closed && !matches!(command, PathCommand::Move { .. }) {
            path.move_to(subpath_start);
        }
        closed = false;

        match *command {
            PathCommand::Move { point } => {
                current = to_point(point.into());
                subpath_start = current;
                path.move_to(current);
            },
            PathCommand::Line { point } => {
                current = to_point(point.into());
                path.line_to(current);
            },
            PathCommand::CubicCurve {
                point,
                control1,
                control2,
            } => {
                current = to_point(point.into());
                path.curve_to(
                    to_point(control1.into()),
                    to_point(control2.into()),
                    current,
                );
            },
            PathCommand::Arc {
                point,
                radii,
                arc_sweep,
                arc_size,
                rotate,
            } => {
                let rx = radii.rx;
                let ry = match radii.ry {
                    Optional::Some(ry) => ry,
                    Optional::None => rx,
                };
                let end = to_point(point.into());
                let arc = SvgArc {
                    from: current,
                    to: end,
                    radii: Vec2::new(rx.abs() as f64, ry.abs() as f64),
                    x_rotation: (rotate as f64).to_radians(),
                    large_arc: arc_size == ArcSize::Large,
                    sweep: arc_sweep == ArcSweep::Cw,
                };
                // A zero radius makes the arc a straight line.
                match Arc::from_svg_arc(&arc) {
                    Some(arc) => append_arc(&mut path, &arc, tolerance),
                    None => path.line_to(end),
                }
                current = end;
            },
            PathCommand::Close => {
                path.close_path();
                current = subpath_start;
                closed = true;
            },
            // `normalize(true)` leaves only the commands above.
            _ => {},
        }
    }
    path
}

/// Appends an arc to a path whose current point is the arc's start.
fn append_arc(path: &mut BezPath, arc: &Arc, tolerance: ArcTolerance) {
    let tolerance = tolerance
        .absolute
        .max(arc.radii.x.max(arc.radii.y) * tolerance.relative);
    path.extend(arc.append_iter(tolerance));
}

/// <https://svgwg.org/svg2-draft/shapes.html#RectElement>
fn rect_path(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    rx: Option<f64>,
    ry: Option<f64>,
    tolerance: ArcTolerance,
) -> BezPath {
    let mut path = BezPath::new();
    if width <= 0. || height <= 0. {
        return path;
    }

    // A negative radius is an error and counts as `auto`, which takes the
    // other radius.
    let rx = rx.filter(|rx| *rx >= 0.);
    let ry = ry.filter(|ry| *ry >= 0.);
    let (rx, ry) = (rx.or(ry).unwrap_or(0.), ry.or(rx).unwrap_or(0.));
    let (rx, ry) = (rx.min(width / 2.), ry.min(height / 2.));
    let (right, bottom) = (x + width, y + height);
    if rx == 0. || ry == 0. {
        path.move_to((x, y));
        path.line_to((right, y));
        path.line_to((right, bottom));
        path.line_to((x, bottom));
        path.close_path();
        return path;
    }

    let corner = |path: &mut BezPath, center: (f64, f64), start_angle: f64| {
        append_arc(
            path,
            &Arc::new(center, (rx, ry), start_angle, FRAC_PI_2, 0.),
            tolerance,
        );
    };
    path.move_to((x + rx, y));
    path.line_to((right - rx, y));
    corner(&mut path, (right - rx, y + ry), -FRAC_PI_2);
    path.line_to((right, bottom - ry));
    corner(&mut path, (right - rx, bottom - ry), 0.);
    path.line_to((x + rx, bottom));
    corner(&mut path, (x + rx, bottom - ry), FRAC_PI_2);
    path.line_to((x, y + ry));
    corner(&mut path, (x + rx, y + ry), PI);
    path.close_path();
    path
}

/// <https://svgwg.org/svg2-draft/shapes.html#CircleElement> and
/// <https://svgwg.org/svg2-draft/shapes.html#EllipseElement>: the path
/// starts at the rightmost point and runs clockwise.
fn ellipse_path(cx: f64, cy: f64, rx: f64, ry: f64, tolerance: ArcTolerance) -> BezPath {
    let mut path = BezPath::new();
    if rx <= 0. || ry <= 0. {
        return path;
    }
    path.move_to((cx + rx, cy));
    append_arc(
        &mut path,
        &Arc::new((cx, cy), (rx, ry), 0., 2. * PI, 0.),
        tolerance,
    );
    path.close_path();
    path
}

/// <https://svgwg.org/svg2-draft/shapes.html#PolylineElement> and
/// <https://svgwg.org/svg2-draft/shapes.html#PolygonElement>. The points
/// grammar is that of the coordinate pairs after a path's move command, so
/// the path data parser reads them: points up to an error (a leading or
/// doubled comma, a command letter) are kept, and an odd last coordinate
/// is dropped.
fn points_path(points: &str, close: bool, tolerance: ArcTolerance) -> BezPath {
    let end = points
        .find(|c: char| {
            !(c.is_ascii_digit() ||
                c.is_ascii_whitespace() ||
                matches!(c, '+' | '-' | '.' | 'e' | 'E' | ','))
        })
        .unwrap_or(points.len());
    let mut path = parse_path_data(&format!("M{}", &points[..end]), tolerance);
    if close && !path.elements().is_empty() {
        path.close_path();
    }
    path
}

/// The segments of a path element, from the end of the previous element.
/// `None` for a move.
fn segment(previous: Point, subpath_start: Point, element: PathEl) -> Option<PathSeg> {
    match element {
        PathEl::MoveTo(_) => None,
        PathEl::LineTo(end) => Some(PathSeg::Line(Line::new(previous, end))),
        PathEl::QuadTo(control, end) => Some(PathSeg::Quad(QuadBez::new(previous, control, end))),
        PathEl::CurveTo(control1, control2, end) => Some(PathSeg::Cubic(CubicBez::new(
            previous, control1, control2, end,
        ))),
        PathEl::ClosePath => Some(PathSeg::Line(Line::new(previous, subpath_start))),
    }
}

/// The end point of a path element; a closepath returns to its subpath's
/// start.
fn element_end(element: PathEl, subpath_start: Point) -> Point {
    match element {
        PathEl::MoveTo(point) | PathEl::LineTo(point) => point,
        PathEl::QuadTo(_, point) | PathEl::CurveTo(_, _, point) => point,
        PathEl::ClosePath => subpath_start,
    }
}

/// The tight bounding box of a path's segments; segments of zero width
/// and height count. Moves count only for a path without segments, which
/// is then the point of its last move: other engines ignore the moves
/// before another move or after the last segment, and the bounding box of
/// `M 10 20` is the point (10, 20).
fn path_bounds(path: &BezPath) -> Option<Rect> {
    let mut bounds: Option<Rect> = None;
    let mut last_move = None;
    let mut current = Point::ZERO;
    let mut subpath_start = Point::ZERO;
    for &element in path.elements() {
        if let PathEl::MoveTo(point) = element {
            last_move = Some(point);
            subpath_start = point;
        } else if let Some(segment) = segment(current, subpath_start, element) {
            let segment_bounds = ParamCurveExtrema::bounding_box(&segment);
            bounds = Some(bounds.map_or(segment_bounds, |bounds| bounds.union(segment_bounds)));
        }
        current = element_end(element, subpath_start);
    }
    bounds.or_else(|| last_move.map(|point| Rect::from_points(point, point)))
}

/// Whether `point` lies on the outline of a subpath of `closed` that
/// encloses an area. Other engines count points on a lone move, on a
/// zero-length segment or on a straight line, which enclose nothing, as
/// outside.
fn is_on_outline(closed: &BezPath, point: Point) -> bool {
    let tolerance = OUTLINE_TOLERANCE * point.x.abs().max(point.y.abs()).max(1.);
    subpaths(closed).iter().any(|subpath| {
        if subpath.area() == 0. {
            return false;
        }
        let mut current = Point::ZERO;
        let mut subpath_start = Point::ZERO;
        subpath.elements().iter().any(|&element| {
            if let PathEl::MoveTo(start) = element {
                subpath_start = start;
            }
            let on_segment = segment(current, subpath_start, element).is_some_and(|segment| {
                segment.nearest(point, NEAREST_ACCURACY).distance_sq <= tolerance * tolerance
            });
            current = element_end(element, subpath_start);
            on_segment
        })
    })
}

/// The path's subpaths, each starting at a move.
fn subpaths(path: &BezPath) -> Vec<BezPath> {
    let mut subpaths: Vec<BezPath> = Vec::new();
    for &element in path.elements() {
        match (element, subpaths.last_mut()) {
            (PathEl::MoveTo(_), _) | (_, None) => subpaths.push(BezPath::from_vec(vec![element])),
            (_, Some(subpath)) => subpath.push(element),
        }
    }
    subpaths
}

/// The path with every open subpath closed, as filling it closes them:
/// <https://svgwg.org/svg2-draft/painting.html#FillProperties>.
fn closed_subpaths(path: &BezPath) -> BezPath {
    let mut closed = BezPath::new();
    let mut open = false;
    for &element in path.elements() {
        match element {
            PathEl::MoveTo(_) => {
                if open {
                    closed.close_path();
                }
                open = false;
            },
            PathEl::ClosePath => open = false,
            _ => open = true,
        }
        closed.push(element);
    }
    if open {
        closed.close_path();
    }
    closed
}

/// The point at `distance` along the path, clamped to the path's length.
/// Returns `None` for a path without any commands.
fn point_at_length(path: &BezPath, distance: f64) -> Option<Point> {
    let mut remaining = distance.max(0.);
    let mut end = None;
    for segment in path.segments() {
        let length = segment.arclen(ARC_LENGTH_ACCURACY);
        if remaining <= length {
            let t = segment.inv_arclen(remaining, ARC_LENGTH_ACCURACY);
            return Some(segment.eval(t));
        }
        remaining -= length;
        end = Some(segment.end());
    }

    // A path of only move commands has no length and stays at its first point.
    end.or_else(|| {
        path.elements().iter().find_map(|element| match element {
            PathEl::MoveTo(point) => Some(*point),
            _ => None,
        })
    })
}

impl VirtualMethods for SVGGeometryElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }
}
