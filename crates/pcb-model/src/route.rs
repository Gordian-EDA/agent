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
}

impl RouteSolution {
    /// Compute [`RouteMetrics`] (wirelength = Σ polyline segment lengths).
    pub fn metrics(&self) -> RouteMetrics {
        let mut wirelength = 0.0;
        for t in &self.traces {
            for w in t.path.windows(2) {
                wirelength += w[1].dist(w[0]);
            }
        }
        RouteMetrics {
            wirelength,
            via_count: self.vias.len(),
            trace_count: self.traces.len(),
        }
    }
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
