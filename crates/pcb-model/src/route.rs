//! Routing phase results, capabilities, and quality measures.

use crate::{FailedNet, RouteSolution, RoutingView, Trace};

// ── RouteResult ────────────────────────────────────────────────────────────────

/// The outcome of routing a [`RoutingView`]: the emitted copper, the nets that
/// could not be fully routed, and which engine produced it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteResult {
    /// Newly emitted traces and vias for the nets that routed. Fixed input
    /// copper remains owned by the caller and is not repeated here.
    pub solution: RouteSolution,
    /// Nets that could not be fully routed (deterministic order), each with a
    /// human-readable cause.
    pub failed: Vec<FailedNet>,
    /// Internal routing-phase provenance.
    pub engine: String,
}

impl RouteResult {
    /// The honest empty result for a view a router declined to solve — an expired
    /// [`Budget`](crate::Budget), or a problem outside its
    /// [`RoutingCapabilities`]. No copper, every requested connection reported
    /// failed with `reason`.
    pub fn abandoned(view: &RoutingView, engine: &str, reason: &str) -> Self {
        RouteResult {
            solution: RouteSolution::default(),
            failed: view
                .connections
                .iter()
                .map(|c| FailedNet {
                    connection: c.name.clone(),
                    reason: reason.to_owned(),
                })
                .collect(),
            engine: engine.to_owned(),
        }
    }
}

// ── metrics ────────────────────────────────────────────────────────────────────

/// Comparable size/quality metrics for a [`RouteSolution`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteMetrics {
    /// Total copper wirelength (mm): the sum of every trace polyline's length.
    pub wirelength: f64,
    /// Number of vias in the solution.
    pub via_count: usize,
    /// Number of trace polylines in the solution.
    pub trace_count: usize,
    /// Corners in the emitted copper: vertices where the direction changes.
    /// The tidiness number a human reads first — a straight run has none.
    pub bend_count: usize,
    /// Segments whose direction is neither axis-aligned nor 45°. Zero is the
    /// house style; anything else is a route no board editor would draw.
    pub off_angle_segments: usize,
}

/// Tolerance (radians) within which a segment counts as being on one of the
/// eight octilinear directions.
const ANGLE_EPS: f64 = 1e-6;

impl RouteSolution {
    /// Compute [`RouteMetrics`] (wirelength = Σ polyline segment lengths).
    pub fn metrics(&self) -> RouteMetrics {
        let mut wirelength = 0.0;
        let mut bend_count = 0;
        let mut off_angle_segments = 0;
        for t in &self.traces {
            for w in t.path.windows(2) {
                wirelength += w[1].dist(w[0]);
                if !is_octilinear(w[0], w[1]) {
                    off_angle_segments += 1;
                }
            }
            for w in t.path.windows(3) {
                if is_bend(w[0], w[1], w[2]) {
                    bend_count += 1;
                }
            }
        }
        RouteMetrics {
            wirelength,
            via_count: self.vias.len(),
            trace_count: self.traces.len(),
            bend_count,
            off_angle_segments,
        }
    }
}

/// Is the segment `a`→`b` horizontal, vertical, or exactly diagonal?
pub fn is_octilinear(a: crate::Point2, b: crate::Point2) -> bool {
    let (dx, dy) = ((b.x - a.x).abs(), (b.y - a.y).abs());
    dx < ANGLE_EPS || dy < ANGLE_EPS || (dx - dy).abs() < ANGLE_EPS
}

/// The octilinear path from `from` to `to`: a straight leg along the dominant
/// axis, then one 45° diagonal.
///
/// This is how a board editor draws a connection, and it is the shape a pad
/// exit must have — copper leaves the pad square-on and turns once. A pure
/// axial or pure diagonal move needs no knee and comes back as two points.
pub fn octilinear_path(from: crate::Point2, to: crate::Point2) -> Vec<crate::Point2> {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let diagonal = dx.abs().min(dy.abs());
    let knee = if dx.abs() >= dy.abs() {
        crate::Point2::new(from.x + dx.signum() * (dx.abs() - diagonal), from.y)
    } else {
        crate::Point2::new(from.x, from.y + dy.signum() * (dy.abs() - diagonal))
    };
    if knee.dist(from) < ANGLE_EPS || knee.dist(to) < ANGLE_EPS {
        return vec![from, to];
    }
    vec![from, knee, to]
}

/// Does the polyline turn at `b`? Zero-length steps are not corners.
pub fn is_bend(a: crate::Point2, b: crate::Point2, c: crate::Point2) -> bool {
    let (ux, uy) = (b.x - a.x, b.y - a.y);
    let (vx, vy) = (c.x - b.x, c.y - b.y);
    let (un, vn) = ((ux * ux + uy * uy).sqrt(), (vx * vx + vy * vy).sqrt());
    if un < ANGLE_EPS || vn < ANGLE_EPS {
        return false;
    }
    (ux * vy - uy * vx).abs() / (un * vn) > ANGLE_EPS
}

// ── RoutingCapabilities ─────────────────────────────────────────────────────────────

/// Capability metadata retained by private routing diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutingCapabilities {
    /// The maximum copper layer count the router supports.
    pub max_layers: u32,
    /// Honours a non-empty [`RoutingView::escape_layers`] (per-net inner-layer
    /// escape assignment). A router that free-mazes every net over all layers
    /// cannot respect this restriction and sets it `false`.
    pub honors_escape_layers: bool,
    /// Honours per-net trace-width overrides ([`RoutingView::net_widths`]).
    pub honors_net_widths: bool,
    /// Honours a custom board [`outline`](RoutingView::outline) (copper kept
    /// inside a concave polygon, not just its bounding box).
    pub honors_outline: bool,
}

impl RoutingCapabilities {
    /// Whether this capability set covers a routing view.
    pub fn can_route(&self, problem: &RoutingView) -> bool {
        self.max_layers >= problem.layer_count
            && (self.honors_escape_layers || problem.escape_layers.is_empty())
            && (self.honors_net_widths || problem.net_widths.is_empty())
            && (self.honors_outline || problem.outline.is_none())
    }
}

// ── RouteQuality ───────────────────────────────────────────────────────────────

/// The connectivity cost of a result: the number of PADS left unconnected (sum
/// over failed nets of their pin count), not the net count — failing one 8-pin
/// power net is worse than failing two 2-pin signals. A failed net not found in
/// the problem (a board-level pseudo-failure) counts as one pad.
pub fn failed_pad_weight(problem: &RoutingView, failed: &[FailedNet]) -> usize {
    failed
        .iter()
        .map(|f| {
            problem
                .connections
                .iter()
                .find(|c| c.name == f.connection)
                .map(|c| c.points_to_connect.len().max(1))
                .unwrap_or(1)
        })
        .sum()
}

/// A pure, comparable quality summary of a [`RouteResult`] for one problem.
///
/// Shared by the concrete routing phase and its internal rip-up retry. The
/// [`fault_weight`](RouteQuality::fault_weight) + [`geom`](RouteQuality::geom)
/// pair is the routability cost (lower is better), then the
/// [`failed_nets`](RouteQuality::failed_nets) count, then
/// [`via_count`](RouteQuality::via_count) and [`wirelength`](RouteQuality::wirelength)
/// are the manufacturability/tidiness tiebreakers.
///
/// `geom` (geometry DRC violations) is supplied by the caller because the DRC
/// oracle lives in `pcb-drc`, which depends on `pcb-model` — so this kernel
/// crate cannot compute it without a dependency cycle. An engine that has
/// already reconciled its copper to be DRC-clean passes `0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteQuality {
    /// Unconnected-pad weight of the failed nets (see [`failed_pad_weight`]).
    pub fault_weight: usize,
    /// Geometry DRC violations of the emitted copper (clearance / width / via /
    /// bounds — NOT connectivity). Caller-supplied; see the type docs.
    pub geom: usize,
    /// Number of nets left unrouted (NOT pad-weighted). The tiebreak BEFORE
    /// wirelength: an octilinear route can strand more, smaller nets that sum to the
    /// same pad weight as another route's fewer, fatter ones — and the corpus reports
    /// net count, so without this tiebreak the diagonal route's shorter wirelength
    /// would flip an equal-pad-weight tie toward the route that connects FEWER nets.
    pub failed_nets: usize,
    /// Number of vias in the solution. At equal routability, fewer vias generally
    /// means easier fabrication and less risk than a slightly shorter via-heavy route.
    pub via_count: usize,
    /// Total copper wirelength (mm) — the final tidiness tiebreaker.
    pub wirelength: f64,
}

impl RouteQuality {
    /// Summarise `result` for `problem`, given its geometry-violation count
    /// `geom` (the caller computes it; reconciled clean copper passes `0`).
    pub fn of(problem: &RoutingView, result: &RouteResult, geom: usize) -> Self {
        let metrics = result.solution.metrics();
        RouteQuality {
            fault_weight: failed_pad_weight(problem, &result.failed),
            geom,
            failed_nets: result.failed.len(),
            via_count: metrics.via_count,
            wirelength: metrics.wirelength,
        }
    }

    /// Total routability cost: unconnected pads + geometry violations. Primary
    /// ranking key — never traded for tidiness.
    pub fn faults(&self) -> usize {
        self.fault_weight + self.geom
    }
}

/// Build a [`Trace`] from a connection name, [`LayerRef`](crate::LayerRef),
/// width, and a polyline. The one constructor both engines use so a trace is
/// emitted identically regardless of which produced it.
pub fn trace(
    connection: &str,
    layer: crate::LayerRef,
    width: f64,
    path: Vec<crate::Point2>,
) -> Trace {
    Trace {
        connection: connection.to_owned(),
        layer,
        width,
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LayerRef, Point2, Polygon, Rect, RouteSolution, Trace, Via, ViaSpan};

    fn empty_problem(layer_count: u32) -> RoutingView {
        RoutingView {
            layer_count,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![],
            bounds: Rect {
                min_x: 0.0,
                max_x: 10.0,
                min_y: 0.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
            fixed_copper: RouteSolution::default(),
            nets: None,
        }
    }

    #[test]
    fn metrics_sum_polyline_lengths() {
        let s = RouteSolution {
            traces: vec![Trace {
                connection: "A".into(),
                layer: LayerRef::top(),
                width: 0.2,
                path: vec![
                    Point2 { x: 0.0, y: 0.0 },
                    Point2 { x: 3.0, y: 0.0 },
                    Point2 { x: 3.0, y: 4.0 },
                ],
            }],
            vias: vec![Via {
                connection: "A".into(),
                at: Point2 { x: 3.0, y: 0.0 },
                diameter: 0.6,
                drill: 0.3,
                span: ViaSpan::Through,
            }],
        };
        let m = s.metrics();
        assert!((m.wirelength - 7.0).abs() < 1e-9, "3 + 4 = 7mm");
        assert_eq!(m.via_count, 1);
        assert_eq!(m.trace_count, 1);
        assert_eq!(m.bend_count, 1, "one right-angle corner");
        assert_eq!(m.off_angle_segments, 0, "both segments are axis-aligned");
    }

    #[test]
    fn tidiness_counts_corners_and_off_angle_segments() {
        let p = |x: f64, y: f64| Point2 { x, y };
        let s = RouteSolution {
            traces: vec![Trace {
                connection: "A".into(),
                layer: LayerRef::top(),
                width: 0.2,
                // straight, then 45°, then an arbitrary angle.
                path: vec![p(0.0, 0.0), p(2.0, 0.0), p(4.0, 2.0), p(5.0, 5.0)],
            }],
            vias: vec![],
        };
        let m = s.metrics();
        assert_eq!(m.bend_count, 2);
        assert_eq!(
            m.off_angle_segments, 1,
            "only the last segment is off-angle"
        );
    }

    #[test]
    fn an_octilinear_path_leaves_along_the_dominant_axis_then_turns_45() {
        let p = |x: f64, y: f64| Point2 { x, y };
        let path = octilinear_path(p(0.0, 0.0), p(10.0, 3.0));
        assert_eq!(path, vec![p(0.0, 0.0), p(7.0, 0.0), p(10.0, 3.0)]);
        for w in path.windows(2) {
            assert!(is_octilinear(w[0], w[1]), "{:?}", w);
        }
    }

    #[test]
    fn a_pure_axial_or_diagonal_move_needs_no_knee() {
        let p = |x: f64, y: f64| Point2 { x, y };
        assert_eq!(
            octilinear_path(p(0.0, 0.0), p(5.0, 0.0)),
            vec![p(0.0, 0.0), p(5.0, 0.0)]
        );
        assert_eq!(
            octilinear_path(p(0.0, 0.0), p(-4.0, 4.0)),
            vec![p(0.0, 0.0), p(-4.0, 4.0)]
        );
    }

    #[test]
    fn an_octilinear_path_is_octilinear_in_every_quadrant() {
        let p = |x: f64, y: f64| Point2 { x, y };
        for to in [
            p(3.0, 11.0),
            p(-3.0, 11.0),
            p(3.0, -11.0),
            p(-11.0, -3.0),
            p(0.13, 0.41),
        ] {
            let path = octilinear_path(p(0.0, 0.0), to);
            assert_eq!(*path.last().unwrap(), to);
            for w in path.windows(2) {
                assert!(is_octilinear(w[0], w[1]), "{to:?} -> {path:?}");
            }
        }
    }

    #[test]
    fn a_collinear_vertex_is_not_a_bend() {
        let p = |x: f64, y: f64| Point2 { x, y };
        let s = RouteSolution {
            traces: vec![Trace {
                connection: "A".into(),
                layer: LayerRef::top(),
                width: 0.2,
                path: vec![p(0.0, 0.0), p(1.0, 0.0), p(3.0, 0.0)],
            }],
            vias: vec![],
        };
        assert_eq!(s.metrics().bend_count, 0);
    }

    #[test]
    fn route_quality_records_via_count() {
        let p = empty_problem(2);
        let r = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![Via {
                    connection: "A".into(),
                    at: Point2 { x: 1.0, y: 1.0 },
                    diameter: 0.6,
                    drill: 0.3,
                    span: ViaSpan::Through,
                }],
            },
            failed: vec![],
            engine: "test".into(),
        };

        let q = RouteQuality::of(&p, &r, 0);

        assert_eq!(q.via_count, 1);
        assert_eq!(q.wirelength, 0.0);
    }

    #[test]
    fn capabilities_gate_each_problem_feature() {
        let full = RoutingCapabilities {
            max_layers: 8,
            honors_escape_layers: true,
            honors_net_widths: true,
            honors_outline: true,
        };
        let limited = RoutingCapabilities {
            max_layers: 2,
            honors_escape_layers: false,
            honors_net_widths: false,
            honors_outline: false,
        };
        let mut p = empty_problem(4);
        assert!(full.can_route(&p));
        assert!(!limited.can_route(&p), "4 > max_layers 2");
        p.layer_count = 2;
        assert!(limited.can_route(&p));
        p.escape_layers.insert("S".into(), 1);
        assert!(
            !limited.can_route(&p),
            "escape_layers needs honors_escape_layers"
        );
        assert!(full.can_route(&p));
        let mut p = empty_problem(2);
        p.net_widths.insert("P".into(), 0.5);
        assert!(!limited.can_route(&p));
        let mut p = empty_problem(2);
        p.outline = Some(
            Polygon::new(vec![
                Point2 { x: 0.0, y: 0.0 },
                Point2 { x: 1.0, y: 0.0 },
                Point2 { x: 0.0, y: 1.0 },
            ])
            .unwrap(),
        );
        assert!(!limited.can_route(&p));
    }
}
