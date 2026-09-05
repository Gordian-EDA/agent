//! Wire routing: the obstacle scene, the drawn segments, the legality predicates that
//! define a valid path, and the [`SchRouter`] contract a router leaf implements.

use geom::{Dir, EPS, Point2, Rect, Segment};

/// A drawn segment assigned to a concrete net.
#[derive(Debug, Clone, PartialEq)]
pub struct NetSegment {
    pub segment: Segment,
    pub net: String,
}

/// A drawn segment whose net attribution may be unknown.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawnSegment {
    pub segment: Segment,
    pub net: Option<String>,
}

impl NetSegment {
    pub fn new(a: Point2, b: Point2, net: impl Into<String>) -> Self {
        Self {
            segment: Segment::new(a, b),
            net: net.into(),
        }
    }
}

impl DrawnSegment {
    pub fn new(a: Point2, b: Point2, net: Option<String>) -> Self {
        Self {
            segment: Segment::new(a, b),
            net,
        }
    }
}

/// One placed symbol's drawn INK — its body graphics and its pin name/number text —
/// with the connection points of its own pins.
///
/// A wire that runs over another symbol's ink reads as a connection the netlist does
/// not have: through a filled body, or across a pin number so the number appears to
/// name the wire. Neither the length gate nor the shape budget can see that, so it is
/// stated here as an obstacle.
///
/// The one exception is the symbol's OWN stub. A wire leaving a pin starts at the pin's
/// connection point, which sits on the symbol's ink by construction, so a segment with
/// an endpoint on one of `pins` is that symbol's own and passes.
#[derive(Debug, Default, Clone)]
pub struct SymbolInk {
    pub boxes: Vec<Rect>,
    pub pins: Vec<Point2>,
}

impl SymbolInk {
    /// Whether `seg` starts or ends on one of this symbol's pins — its own wire, which
    /// stands next to its own ink by construction and is not judged for hugging it.
    pub(crate) fn touches_pin(&self, seg: Segment) -> bool {
        self.pins
            .iter()
            .any(|p| p.dist(seg.a) < EPS || p.dist(seg.b) < EPS)
    }

    /// Whether `seg` crosses this symbol's drawn ink.
    ///
    /// Landing on one of the symbol's own pins is not on its own a licence to cross it.
    /// A pin sits on the EDGE of the ink it belongs to, so the stub leaving it outward
    /// never enters the interior in the first place, while a wire that arrives at that
    /// same pin from across the body is the defect itself — the run down a switch's own
    /// axis, or along a pin row. The one pin that does need forgiving is one the ink
    /// ENCLOSES — a power symbol's origin inside its glyph — which no wire could reach
    /// otherwise; and only for the box that encloses it.
    pub(crate) fn crossed_by(&self, seg: Segment) -> bool {
        self.boxes
            .iter()
            .any(|r| seg.axis_aligned_hits_rect_interior(r) && !self.reachable_only_through(r, seg))
    }

    /// Whether `seg` is the wire of a pin this box ENCLOSES, which cannot be drawn
    /// without entering it.
    fn reachable_only_through(&self, r: &Rect, seg: Segment) -> bool {
        self.pins.iter().any(|p| {
            (p.dist(seg.a) < EPS || p.dist(seg.b) < EPS)
                && p.x > r.min_x + EPS
                && p.x < r.max_x - EPS
                && p.y > r.min_y + EPS
                && p.y < r.max_y - EPS
        })
    }
}

/// Routing obstacles, all coordinates sheet mm.
#[derive(Debug, Default, Clone)]
pub struct RouteScene {
    /// Solid rects (symbol bodies): a path segment may not pass through one
    /// (edge-touching is tolerated).
    pub solids: Vec<Rect>,
    /// Net-anchor points with their net: a segment may not pass through a
    /// point of a DIFFERENT net (touching it would merge the nets).
    pub points: Vec<(Point2, String)>,
    /// Existing segments with their net (cluster wires, stubs, prior routes).
    /// Any touch with a different net's segment — shared point, collinear
    /// overlap, endpoint-on-segment — is forbidden; a strictly-interior
    /// perpendicular crossing is fine (KiCAD draws no connection there).
    pub segments: Vec<NetSegment>,
    /// Port-label (global-tag pennant) boxes with their OWN net. A FOREIGN net's
    /// wire may not pass through one — that draws a wire straight across someone
    /// else's edge tag (the inverting-input net bisecting a VIN pennant). The
    /// label's own net wire DOES reach it (the pennant connects there), so the
    /// box is net-tagged rather than a solid.
    pub label_solids: Vec<(Rect, String)>,
    /// Drawn symbol ink, per symbol: bodies and pin text a foreign wire may not cross.
    pub ink: Vec<SymbolInk>,
}

/// Whether `path` can be drawn for `net` without entering a body, touching a
/// foreign net's anchor point, or merging with a foreign net's segment.
pub fn path_ok(path: &[Point2], net: &str, scene: &RouteScene) -> bool {
    for w in path.windows(2) {
        let (a, b) = (w[0], w[1]);
        let seg = Segment::new(a, b);
        if scene
            .solids
            .iter()
            .any(|r| seg.axis_aligned_hits_rect_interior(r))
        {
            return false;
        }
        if scene
            .points
            .iter()
            .any(|(p, n)| n != net && seg.contains_point(*p))
        {
            return false;
        }
        if scene
            .segments
            .iter()
            .any(|existing| existing.net != net && seg.axis_aligned_connects(existing.segment))
        {
            return false;
        }
        if scene
            .label_solids
            .iter()
            .any(|(r, n)| n != net && seg.axis_aligned_hits_rect_interior(r))
        {
            return false;
        }
        if scene.ink.iter().any(|ink| ink.crossed_by(seg)) {
            return false;
        }
    }
    true
}

/// Number of VISUAL crossings `path` (drawn for `net`) would add against the
/// scene's already-committed foreign-net segments: a perpendicular pair (one
/// horizontal, one vertical) meeting at a point INTERIOR to both — the same
/// over-pass clutter `score::count_crossings` and the corpus oracle count. Used
/// by the router's wire-vs-label decision: a crossing-heavy hop is better named
/// (the human idiom) than drawn as a literal wire that reads as spaghetti.
pub fn path_crossings(path: &[Point2], net: &str, scene: &RouteScene) -> usize {
    let mut n = 0;
    for w in path.windows(2) {
        let seg = Segment::new(w[0], w[1]);
        for existing in &scene.segments {
            if existing.net == net {
                continue; // same net: a deliberate join, not a crossing
            }
            if seg.axis_aligned_crosses_interior(existing.segment) {
                n += 1;
            }
        }
    }
    n
}


/// How many of `path`'s segments run flush against something they do not belong to —
/// within [`HUG_MM`] of a symbol's ink, a foreign net's keepout, or a foreign net's
/// wire, without entering or touching it.
///
/// [`path_ok`] answers whether a wire may be drawn; this answers how well it reads once
/// it is. A run half a millimetre off a capacitor's plates is legal and looks like it
/// touches them; two unrelated verticals half a grid apart are legal and read as one
/// thick line. The shape budget sees neither — so the router ranks a standing-off lane
/// ahead of a flush one. A preference, never a veto: it orders candidates that are
/// already legal and already equal on shape.
pub fn path_hugs(path: &[Point2], net: &str, scene: &RouteScene) -> usize {
    let mut n = 0;
    for w in path.windows(2) {
        let seg = Segment::new(w[0], w[1]);
        let boxes = scene
            .ink
            .iter()
            .filter(|ink| !ink.touches_pin(seg))
            .flat_map(|ink| ink.boxes.iter())
            .chain(
                scene
                    .label_solids
                    .iter()
                    .filter(|(_, n)| n != net)
                    .map(|(r, _)| r),
            )
            .any(|r| {
                seg.dist_to_rect(r) < HUG_MM && !seg.axis_aligned_hits_rect_interior(r)
            });
        let wires = scene
            .segments
            .iter()
            .filter(|existing| existing.net != net)
            .any(|existing| {
                seg.dist_to_segment(existing.segment) < HUG_MM
                    && !seg.axis_aligned_crosses_interior(existing.segment)
            });
        n += usize::from(boxes || wires);
    }
    n
}

/// How close a wire may pass to ink before it reads as touching it, mm.
const HUG_MM: f64 = 2.54;

/// A schematic wire ROUTER: the leaf that turns "connect these terminals" into drawn
/// orthogonal paths.
///
/// ## Contract
/// - **Deterministic.** The same `(scene, terminals)` always yields the same paths.
/// - **Legal or nothing.** Every returned path satisfies [`path_ok`] for its net; a
///   router that cannot find a legal path returns `None` and the caller degrades to a
///   net label rather than drawing a wrong wire.
/// - **Pure.** No I/O, no KiCAD environment — the scene is the whole world.
pub trait SchRouter {
    /// Open provenance: the router's stable name (e.g. `"elbow"`).
    fn name(&self) -> &'static str;

    /// Which terminal pairs to wire so the net is connected as one tree.
    fn tree_edges(&self, terminals: &[Point2]) -> Vec<(usize, usize)>;

    /// A legal path for `net` from `a` (leaving its pin in direction `dir_a`) to `b`,
    /// as corner points inclusive of both ends. `None` when nothing legal fits.
    fn route_edge(
        &self,
        a: Point2,
        dir_a: Dir,
        b: Point2,
        net: &str,
        scene: &RouteScene,
    ) -> Option<Vec<Point2>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ink_scene() -> RouteScene {
        RouteScene {
            ink: vec![SymbolInk {
                boxes: vec![Rect::new(10.0, 0.0, 20.0, 10.0)],
                pins: vec![Point2::new(10.0, 5.0)],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_foreign_wire_may_not_cross_drawn_ink() {
        let across = [Point2::new(0.0, 5.0), Point2::new(30.0, 5.0)];
        assert!(!path_ok(&across, "SIG", &ink_scene()));
        let around = [
            Point2::new(0.0, 5.0),
            Point2::new(0.0, -5.0),
            Point2::new(30.0, -5.0),
        ];
        assert!(path_ok(&around, "SIG", &ink_scene()));
    }

    #[test]
    fn the_symbols_own_stub_leaves_through_its_own_ink() {
        let stub = [Point2::new(10.0, 5.0), Point2::new(0.0, 5.0)];
        assert!(path_ok(&stub, "SIG", &ink_scene()));
    }

    #[test]
    fn a_lane_that_stands_off_the_ink_hugs_less_than_a_flush_one() {
        let scene = ink_scene();
        let flush = [Point2::new(0.0, -0.5), Point2::new(30.0, -0.5)];
        let clear = [Point2::new(0.0, -8.0), Point2::new(30.0, -8.0)];
        assert!(path_ok(&flush, "SIG", &scene) && path_ok(&clear, "SIG", &scene));
        assert!(path_hugs(&flush, "SIG", &scene) > path_hugs(&clear, "SIG", &scene));
    }

    #[test]
    fn a_run_beside_a_foreign_wire_hugs_it() {
        let mut scene = RouteScene::default();
        scene.segments.push(NetSegment::new(
            Point2::new(0.0, 0.0),
            Point2::new(30.0, 0.0),
            "OTHER",
        ));
        let beside = [Point2::new(0.0, 1.27), Point2::new(30.0, 1.27)];
        let apart = [Point2::new(0.0, 10.0), Point2::new(30.0, 10.0)];
        assert_eq!(path_hugs(&beside, "SIG", &scene), 1);
        assert_eq!(path_hugs(&apart, "SIG", &scene), 0);
    }

    #[test]
    fn owning_one_end_does_not_licence_the_rest_of_the_path() {
        // Leaves its own pin, then doubles back across the body it just left.
        let back = [
            Point2::new(10.0, 5.0),
            Point2::new(5.0, 5.0),
            Point2::new(5.0, 2.0),
            Point2::new(30.0, 2.0),
        ];
        assert!(!path_ok(&back, "SIG", &ink_scene()));
    }
}
