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
    /// Whether `seg` may cross this symbol's ink: only as the stub leaving one of its
    /// own pins.
    fn own_stub(&self, seg: Segment) -> bool {
        self.pins
            .iter()
            .any(|p| p.dist(seg.a) < EPS || p.dist(seg.b) < EPS)
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
        if scene.ink.iter().any(|ink| {
            !ink.own_stub(seg)
                && ink
                    .boxes
                    .iter()
                    .any(|r| seg.axis_aligned_hits_rect_interior(r))
        }) {
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
