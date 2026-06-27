//! The PCB routing engine SDK: the [`Router`] trait every routing engine
//! implements, the one unified [`RouteResult`] they return, the [`Capabilities`]
//! a selector queries, and the pure [`RouteQuality`] key that ranks results.
//!
//! A third party builds a router against this module and `pcb-model` ALONE — no
//! dependency on the in-house engines (`grid-astar`, `negotiated-mesh`) — impls
//! [`Router`], reads a [`RouteProblem`], builds a [`RouteSolution`] of
//! [`Trace`](crate::Trace)/[`Via`](crate::Via) in mm, declares its limits via
//! [`capabilities`](Router::capabilities), and returns a [`RouteResult`]. The
//! agent drops their `&dyn Router` into the generic selector ([`select`])
//! alongside the built-ins, exactly as it injects a `Box<dyn PlacementEngine>`
//! today.

use crate::{FailedNet, RouteProblem, RouteSolution, Trace};

// ── RouteResult ────────────────────────────────────────────────────────────────

/// The outcome of routing a [`RouteProblem`]: the emitted copper, the nets that
/// could not be fully routed, and which engine produced it.
///
/// This is the ONE result type every [`Router`] returns — the free grid router,
/// the premium detailed router, and any third-party engine all build this. The
/// [`engine`](RouteResult::engine) string is OPEN provenance (a router's
/// [`name`](Router::name)), replacing the old closed enum.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteResult {
    /// Emitted traces and vias for the nets that routed.
    pub solution: RouteSolution,
    /// Nets that could not be fully routed (deterministic order), each with a
    /// human-readable cause.
    pub failed: Vec<FailedNet>,
    /// The [`name`](Router::name) of the engine that produced this result.
    pub engine: String,
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

// ── Capabilities ─────────────────────────────────────────────────────────────

/// What a [`Router`] can honour, queried by the selector to decide whether to
/// offer a router a given [`RouteProblem`] (see [`Router::can_route`]).
///
/// A router declares its limits here once instead of being special-cased in the
/// selector: a router that cannot honour a board's inner-layer escape assignment
/// sets [`honors_escape_layers`](Capabilities::honors_escape_layers) `false` and
/// is automatically skipped for boards that carry one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The maximum copper layer count the router supports.
    pub max_layers: u32,
    /// Honours a non-empty [`RouteProblem::escape_layers`] (per-net inner-layer
    /// escape assignment). A router that free-mazes every net over all layers
    /// cannot respect this restriction and sets it `false`.
    pub honors_escape_layers: bool,
    /// Honours per-net trace-width overrides ([`RouteProblem::net_widths`]).
    pub honors_net_widths: bool,
    /// Honours a custom board [`outline`](RouteProblem::outline) (copper kept
    /// inside a concave polygon, not just its bounding box).
    pub honors_outline: bool,
}

impl Capabilities {
    /// Can a router with these capabilities route `problem`? The default
    /// [`Router::can_route`] derives from this: a feature the problem uses must
    /// be one the router honours, and the board's layer count must fit.
    pub fn can_route(&self, problem: &RouteProblem) -> bool {
        self.max_layers >= problem.layer_count
            && (self.honors_escape_layers || problem.escape_layers.is_empty())
            && (self.honors_net_widths || problem.net_widths.is_empty())
            && (self.honors_outline || problem.outline.is_none())
    }
}

// ── Router trait ───────────────────────────────────────────────────────────────

/// A PCB routing engine: turns a [`RouteProblem`] into a [`RouteResult`].
///
/// The single SDK seam for routing — the free grid router, the premium detailed
/// router, and any third-party engine all implement it, and the generic selector
/// ([`select`]) ranks them uniformly. Mirrors the `Placer` / `PlacementEngine` /
/// `Synthesizer` / DRC `Rule` shape: open [`name`](Router::name) provenance, a
/// defaulted [`capabilities`](Router::capabilities) descriptor a selector
/// queries, a [`can_route`](Router::can_route) predicate derived from it, and a
/// value-returning [`route`](Router::route).
///
/// # Determinism contract
///
/// An implementation MUST be:
/// - **Deterministic** — the same [`RouteProblem`] always yields the same
///   [`RouteResult`] (byte-identical when serialized).
/// - **Honest** — every net it cannot fully route is reported in
///   [`RouteResult::failed`] with a human-readable reason, never silently
///   dropped.
/// - **Clean per unit** — a net that fails routing contributes NO copper to the
///   solution (no half-routed traces or dangling vias).
/// - **Panic-free** — it never panics; a problem it cannot solve is reported as
///   failures, not a crash.
pub trait Router {
    /// The engine's name — open provenance recorded in [`RouteResult::engine`].
    fn name(&self) -> &'static str;

    /// What this engine can honour. The selector queries it to decide whether to
    /// offer the router a problem (see [`can_route`](Router::can_route)).
    fn capabilities(&self) -> Capabilities;

    /// Can this router route `problem`? Defaults to deriving the answer from
    /// [`capabilities`](Router::capabilities) — a router needing finer control
    /// (e.g. a per-problem feasibility probe) overrides it.
    fn can_route(&self, problem: &RouteProblem) -> bool {
        self.capabilities().can_route(problem)
    }

    /// Route `problem`. Honours the determinism contract above.
    fn route(&self, problem: &RouteProblem) -> RouteResult;
}

// ── RouteQuality ───────────────────────────────────────────────────────────────

/// The connectivity cost of a result: the number of PADS left unconnected (sum
/// over failed nets of their pin count), not the net count — failing one 8-pin
/// power net is worse than failing two 2-pin signals. A failed net not found in
/// the problem (a board-level pseudo-failure) counts as one pad.
pub fn failed_pad_weight(problem: &RouteProblem, failed: &[FailedNet]) -> usize {
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
/// Shared by BOTH the generic selector ([`select`]) and an engine's internal
/// rip-up retry, so the two never disagree on what "better" means. The
/// [`fault_weight`](RouteQuality::fault_weight) + [`geom`](RouteQuality::geom)
/// pair is the routability cost (lower is better), then the
/// [`failed_nets`](RouteQuality::failed_nets) count, then
/// [`via_count`](RouteQuality::via_count) and [`wirelength`](RouteQuality::wirelength)
/// are the manufacturability/tidiness tiebreakers.
///
/// `geom` (geometry DRC violations) is supplied by the caller because the DRC
/// oracle lives in `drc-lint`, which depends on `pcb-model` — so this kernel
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
    pub fn of(problem: &RouteProblem, result: &RouteResult, geom: usize) -> Self {
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

// ── generic selector ─────────────────────────────────────────────────────────

/// Route `problem` with the best of the offered `routers`, ranking each by a
/// caller-supplied [`RouteQuality`]. The generic routing selector every tier
/// reuses (the agent injects the routers it offers — baseline first).
///
/// A LEFT-FOLD in injection order, not a global argmin: the first router that
/// `can_route` the problem is the incumbent, and each later candidate replaces it
/// only when `better(incumbent, challenger)` is `false`. The fold (rather than a
/// total order) is deliberate — the routability-then-tidiness rule is asymmetric
/// (the incumbent wins ties, and tolerates a bounded tidiness detour), which is
/// not a transitive total order. The agent injects routers in PREFERENCE order
/// (the always-correct baseline first).
///
/// `done` short-circuits the fold: once a router's quality satisfies it (the
/// baseline routed cleanly, say), the remaining routers can only differ in
/// tidiness, never routability, so they are not run — keeping the incumbent.
///
/// `quality` scores a result (the caller computes its geometry-violation count,
/// since DRC lives outside this kernel). `better(problem, incumbent, challenger)`
/// returns `true` to KEEP the incumbent. Routers that `!can_route(problem)` are
/// skipped. `None` only if `routers` is empty or none can route the problem.
pub fn select<'a>(
    problem: &RouteProblem,
    routers: &[&'a dyn Router],
    quality: &dyn Fn(&RouteResult) -> RouteQuality,
    better: &dyn Fn(&RouteProblem, &RouteQuality, &RouteQuality) -> bool,
    done: &dyn Fn(&RouteQuality) -> bool,
) -> Option<RouteResult> {
    let mut best: Option<(RouteResult, RouteQuality)> = None;
    for r in routers {
        if !r.can_route(problem) {
            continue;
        }
        let result = r.route(problem);
        let q = quality(&result);
        let stop = done(&q);
        best = match best {
            None => Some((result, q)),
            Some((bi, bq)) if better(problem, &bq, &q) => Some((bi, bq)),
            Some(_) => Some((result, q)),
        };
        if stop {
            break;
        }
    }
    best.map(|(r, _)| r)
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
    use crate::{
        Connection, LayerRef, Point2, Polygon, Rect, RoutePoint, RouteSolution, Trace, Via, ViaSpan,
    };

    fn empty_problem(layer_count: u32) -> RouteProblem {
        RouteProblem {
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
        let full = Capabilities {
            max_layers: 8,
            honors_escape_layers: true,
            honors_net_widths: true,
            honors_outline: true,
        };
        let limited = Capabilities {
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

    /// A toy router: declares limited capabilities and emits a straight trace per
    /// 2-point net. Proves the trait is implementable against `pcb-model` alone.
    struct ToyRouter;
    impl Router for ToyRouter {
        fn name(&self) -> &'static str {
            "toy"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_layers: 2,
                honors_escape_layers: false,
                honors_net_widths: false,
                honors_outline: false,
            }
        }
        fn route(&self, problem: &RouteProblem) -> RouteResult {
            let mut traces = vec![];
            let mut failed = vec![];
            for c in &problem.connections {
                if c.points_to_connect.len() == 2 {
                    let (a, b) = (&c.points_to_connect[0], &c.points_to_connect[1]);
                    traces.push(trace(
                        &c.name,
                        LayerRef::top(),
                        problem.min_trace_width,
                        vec![Point2 { x: a.x, y: a.y }, Point2 { x: b.x, y: b.y }],
                    ));
                } else {
                    failed.push(FailedNet {
                        connection: c.name.clone(),
                        reason: "toy routes only 2-point nets".into(),
                    });
                }
            }
            RouteResult {
                solution: RouteSolution {
                    traces,
                    vias: vec![],
                },
                failed,
                engine: self.name().into(),
            }
        }
    }

    #[test]
    fn select_keeps_the_incumbent_on_a_tie_and_skips_uncapable() {
        let mut p = empty_problem(2);
        p.connections = vec![Connection {
            name: "N".into(),
            points_to_connect: vec![
                RoutePoint {
                    x: 0.0,
                    y: 0.0,
                    layer: LayerRef::top(),
                },
                RoutePoint {
                    x: 5.0,
                    y: 0.0,
                    layer: LayerRef::top(),
                },
            ],
        }];
        let toy = ToyRouter;
        let routers: Vec<&dyn Router> = vec![&toy];
        let q = |r: &RouteResult| RouteQuality::of(&p, r, 0);
        let better =
            |_: &RouteProblem, bi: &RouteQuality, ch: &RouteQuality| bi.faults() <= ch.faults();
        let done = |q: &RouteQuality| q.faults() == 0;
        let r = select(&p, &routers, &q, &better, &done).unwrap();
        assert_eq!(r.engine, "toy");
        assert!(r.failed.is_empty());
        // A 4-layer board is out of the toy's capability range → no router routes it.
        let p4 = empty_problem(4);
        let q4 = |r: &RouteResult| RouteQuality::of(&p4, r, 0);
        assert!(select(&p4, &routers, &q4, &better, &done).is_none());
    }
}
