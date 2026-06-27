//! Detailed-routing pipeline entry point: stitch + fallback (slice 3, Task 3).
//!
//! This is the top of the detailed router. It runs the three detailed stages in
//! sequence and stitches their cell-local copper into a board [`RouteSolution`]:
//!
//! 1. [`global_route`] — coarse, congestion-negotiated cell paths over the mesh.
//! 2. [`assign_crossings`] — concrete per-boundary crossing points + cell jobs.
//! 3. `route_cells` — fine octilinear A* inside each cell, emitting cell-local
//!    polylines that meet **byte-exactly** at the shared crossing points.
//!
//! [`route_detailed`] folds every stage's failures into one [`RouteResult`] with
//! provenance in the reason string (`"global: …"`, `"assign: …"`, `"cell N: …"`),
//! and stitches only the *fully successful* nets into copper. [`NegotiatedMeshRouter`]
//! is the [`Router`] impl wrapping it. The generic selector [`select_best`] runs
//! the offered [`Router`]s and keeps the best by a [`RouteQuality`] key (faults,
//! then via count, then wirelength): faults are primary (never trade routability),
//! then the lower-risk/tidier copper wins — so the detailed router's
//! capacity-aware routing is kept where it reduces faults, and a lighter router is
//! kept where it is cleaner on a board both can route. The earlier-injected router
//! wins exact ties as the
//! battle-tested path. [`RouteResult::engine`] records which engine produced the
//! returned result.
//!
//! ## Stitching (the connectivity contract)
//!
//! `route_cells` snaps every terminal endpoint to its exact mm position, so a
//! leaf's `Exit` and the neighbour's `Entry` are byte-identical [`Point2`]s.
//! Stitching therefore joins cell-local polylines **by exact coordinate
//! identity**: per net, per layer, two polylines that share a byte-identical
//! endpoint where exactly two polyline-ends meet are concatenated into one
//! continuous run; the joined run is simplified with [`geom::Polyline`] so a
//! straight crossing collapses to a single segment. Where
//! **three or more** polyline-ends meet at one point — a T-junction in a
//! multi-point net — the polylines are left meeting at the shared vertex (KiCAD
//! and the connectivity oracle treat a shared vertex as connected), never forced
//! into one impossible polyline. A net with a failure anywhere contributes **no**
//! copper at all: a half-routed net would (rightly) trip the connectivity lint,
//! so its cell routes are dropped and it is reported failed.

use crate::channel::ChannelRouter;
use crate::crossing::assign_crossings;
use crate::detail::{self, CellRoute, CellRouteResult};
use crate::direct::DirectLineRouter;
use crate::layer_hop::LayerHopRouter;
use crate::pathing::{GlobalRouteResult, global_route_with_mesh};
use crate::pattern::PatternRouter;
use crate::problem::{
    Capabilities, FailedNet, LayerRef, Point2, RouteProblem, RouteQuality, RouteResult,
    RouteSolution, Router, Trace, Via, ViaSpan,
};
use crate::router::{self, GridAStarRouter};
use crate::sequential::SequentialGridRouter;
use crate::via_escape::ViaEscapeRouter;
use geom::JOIN_EPS;
use std::collections::{BTreeMap, BTreeSet};

/// This engine's [`RouteResult::engine`] provenance tag.
pub const ENGINE: &str = "detailed";

// ── pipeline entry points ──────────────────────────────────────────────────────

/// Route `problem` through the full detailed pipeline (global → assign → cell →
/// stitch). Never panics; every stage's failures are folded into
/// [`RouteResult::failed`] with provenance in the reason, and a net that fails at
/// *any* stage contributes no copper to the returned solution. The result is
/// tagged with [`ENGINE`] (`"detailed"`).
pub fn route_detailed(problem: &RouteProblem) -> RouteResult {
    route_detailed_with_global(problem).0
}

/// As [`route_detailed`], but also returns the negotiated global-routing result
/// that the detailed pipeline already computed. This lets callers surface
/// congestion diagnostics without rerunning global routing on the failure path.
pub fn route_detailed_with_global(problem: &RouteProblem) -> (RouteResult, GlobalRouteResult) {
    let mesh = crate::mesh::CapacityMesh::build(problem);
    let global: GlobalRouteResult = global_route_with_mesh(problem, &mesh);
    let route = route_detailed_from_global(problem, &mesh, &global);
    (route, global)
}

fn route_detailed_from_global(
    problem: &RouteProblem,
    mesh: &crate::mesh::CapacityMesh,
    global: &GlobalRouteResult,
) -> RouteResult {
    let mut failed: Vec<FailedNet> = Vec::new();
    // A net failing anywhere drops its copper everywhere. Collected by name.
    let mut failed_names: std::collections::BTreeSet<String> = Default::default();

    // 1. Global routing.
    if !global.is_feasible() {
        // Fold the per-net "unrouted" failures, then a single overflow
        // pseudo-failure if any edge is over capacity (the overflow is not
        // attributable to one net, so it is reported as a board-level fault).
        for u in &global.report.unrouted {
            fail(
                &mut failed,
                &mut failed_names,
                &u.connection,
                format!("global: {}", u.reason),
            );
        }
        if global.report.final_overflow > 0 {
            failed.push(FailedNet {
                connection: String::new(),
                reason: format!(
                    "global: {} unit(s) of residual edge overflow after {} rip-up iteration(s)",
                    global.report.final_overflow, global.report.iterations
                ),
            });
        }
    }

    // 2. Crossing assignment.
    let assignment = assign_crossings(problem, &mesh, &global.plan);
    for f in &assignment.failures {
        // An assignment failure is per-boundary or per-via; surface it as a
        // board-level fault with assign provenance (its detailed payload is the
        // honest record).
        failed.push(FailedNet {
            connection: assignment_failure_connection(f),
            reason: format!("assign: {}", assignment_failure_reason(f)),
        });
    }

    // 3. Per-cell detailed routing.
    let cells: CellRouteResult = detail::route_cells(problem, &mesh, &assignment);
    for f in &cells.failed {
        // route_cells already prefixes the reason with "cell N: …"; keep that
        // provenance and mark the net as failed so its copper is dropped.
        fail(
            &mut failed,
            &mut failed_names,
            &f.connection,
            f.reason.clone(),
        );
    }

    // 4. Stitch the SUCCESSFUL nets' cell copper into a board solution. A net
    //    listed in `failed_names` contributes nothing (no half-routed copper).
    let solution = stitch(problem, &cells.cell_routes, &failed_names);

    RouteResult {
        solution,
        failed,
        engine: ENGINE.to_owned(),
    }
}

// ── NegotiatedMeshRouter (the SDK Router impl) ───────────────────────────────────

/// The premium detailed [`Router`]: the negotiated-mesh pipeline ([`route_detailed`])
/// behind the SDK trait, with its copper reconciled through the DRC oracle so the
/// returned result is geometry-clean.
///
/// It DECLINES (`can_route` = `false`) a board carrying a per-net inner-layer escape
/// assignment ([`RouteProblem::escape_layers`]): the detailed engine free-mazes every
/// net over all layers and has no per-net layer restriction, so it would defeat the
/// structured escape (self-blocking, runtime-exploding) the assignment exists to
/// enable. On such a board only the grid router is offered, exactly as the old
/// `skip_detailed` flag intended.
#[derive(Debug, Clone, Copy, Default)]
pub struct NegotiatedMeshRouter;

impl Router for NegotiatedMeshRouter {
    fn name(&self) -> &'static str {
        ENGINE
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_layers: u32::MAX,
            // Cannot honour a per-net inner-layer escape restriction (free-mazes).
            honors_escape_layers: false,
            honors_net_widths: true,
            // The detailed engine routes within the rectangular `bounds`; like the
            // grid router it does not carve a concave custom outline, so a clearance
            // to a true outline edge is the DRC oracle's / the agent's concern, not
            // a reason to decline the board.
            honors_outline: true,
        }
    }

    fn route(&self, problem: &RouteProblem) -> RouteResult {
        let mut detailed = route_detailed(problem);
        reconcile_connectivity(problem, &mut detailed.solution, &mut detailed.failed);
        detailed
    }
}

/// Route `problem` with the best of the offered `routers`.
///
/// The PCB instantiation of the kernel selector: it supplies the DRC-aware
/// [`RouteQuality`] scorer (the geometry-violation count lives outside the kernel),
/// the routability-then-tidiness [`better`] rule, and the clean-route
/// short-circuit. The selector filters by [`Router::can_route`], runs each
/// surviving router in injection order, and keeps the best — the FIRST router that
/// routes with zero faults after the shared postroute cleanup short-circuits the
/// rest, so heavier engines are never paid for on a board an earlier candidate
/// already routes clean. Candidates are cleaned before scoring, so injected
/// router portfolios use the same quality key as [`route_auto`].
///
/// `routers` is injected by the caller, mirroring `Box<dyn PlacementEngine>`:
/// free tier = `[&GridAStarRouter]`; premium =
/// `[&DirectLineRouter, &LayerHopRouter, &ViaEscapeRouter, &PatternRouter,
/// &NegotiatedMeshRouter, &GridAStarRouter]`.
/// Returns an empty-solution result tagged `"none"` if no router can route the
/// problem (never for a non-empty list containing the always-routable grid baseline).
pub fn select_best(problem: &RouteProblem, routers: &[&dyn Router]) -> RouteResult {
    let mut best: Option<(RouteResult, RouteQuality)> = None;
    for router in routers {
        if !router.can_route(problem) {
            continue;
        }
        let result = router.route(problem);
        if consider_candidate(problem, &mut best, result) {
            break;
        }
    }
    best.map(|(result, _)| result)
        .unwrap_or_else(|| RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: vec![],
            engine: "none".to_owned(),
        })
}

/// `route_auto` plus diagnostic side data captured from engines that expose it.
#[derive(Debug, Clone)]
pub struct RouteAutoRun {
    pub result: RouteResult,
    /// Negotiated global-routing result from the detailed primary candidate, when
    /// that candidate ran. Failure callers can use this instead of rerunning
    /// global routing just to recover congestion hotspots.
    pub global: Option<GlobalRouteResult>,
}

fn consider_candidate(
    problem: &RouteProblem,
    best: &mut Option<(RouteResult, RouteQuality)>,
    mut result: RouteResult,
) -> bool {
    postroute_cleanup(problem, &mut result.solution);
    let q = RouteQuality::of(
        problem,
        &result,
        router::geometry_violations(problem, &result.solution),
    );
    let stop = q.faults() == 0;
    *best = match best.take() {
        None => Some((result, q)),
        Some((bi, bq)) if better(problem, &bq, &q) => Some((bi, bq)),
        Some(_) => Some((result, q)),
    };
    stop
}

/// Keep the incumbent? Routability is primary (fewer total faults wins outright);
/// at EQUAL faults the FAILED-NET COUNT breaks the tie before tidiness (the naive
/// route can strand more small nets that sum to the same pad weight, and the corpus
/// reports net count — without this the diagonal's shorter wirelength would flip an
/// equal-pad-weight tie toward the route that connects fewer nets); only at equal
/// faults AND equal net count does via count decide manufacturability; at equal
/// vias the incumbent (the earlier-injected, more battle-tested router) keeps its
/// result unless it detours more than [`NAIVE_DETOUR_TOLERANCE`] longer than the
/// challenger — the asymmetric tidiness tiebreak. Asymmetric in the incumbent's
/// favour, so the selector is a left-fold in injection order, not a global argmin.
/// The `_problem` arg matches the kernel selector's `better` signature (the rule
/// is problem-independent here).
fn better(_problem: &RouteProblem, incumbent: &RouteQuality, challenger: &RouteQuality) -> bool {
    if incumbent.faults() != challenger.faults() {
        incumbent.faults() < challenger.faults()
    } else if incumbent.failed_nets != challenger.failed_nets {
        incumbent.failed_nets < challenger.failed_nets
    } else if incumbent.via_count != challenger.via_count {
        incumbent.via_count < challenger.via_count
    } else {
        incumbent.wirelength <= challenger.wirelength * NAIVE_DETOUR_TOLERANCE
    }
}

/// Route `problem` with the premium portfolio: direct line-of-sight for trivial
/// clean nets, layer-hop for trivial different-layer nets, via-escape for simple
/// same-layer escapes, a composite pattern router for heterogeneous simple
/// boards, a directional channel router for two-pin crossing/channel cases, a
/// contextual sequential-grid router for ordering-sensitive boards, the
/// negotiated-mesh detailed router as the primary engine, and the free grid router
/// as the fallback baseline. The convenience entry the agent's PCB tool uses; a
/// free-tier caller injects only `[&GridAStarRouter]`.
pub fn route_auto(problem: &RouteProblem) -> RouteResult {
    route_auto_with_diagnostics(problem).result
}

/// Route only the negotiated-mesh detailed engine, returning the global report it
/// already computed. This is the diagnostics-preserving equivalent of injecting
/// only [`NegotiatedMeshRouter`] into [`select_best`].
pub fn route_mesh_with_diagnostics(problem: &RouteProblem) -> RouteAutoRun {
    if !NegotiatedMeshRouter.can_route(problem) {
        return RouteAutoRun {
            result: RouteResult {
                solution: RouteSolution {
                    traces: vec![],
                    vias: vec![],
                },
                failed: vec![],
                engine: "none".to_owned(),
            },
            global: None,
        };
    }

    let (mut result, global) = route_detailed_with_global(problem);
    reconcile_connectivity(problem, &mut result.solution, &mut result.failed);
    postroute_cleanup(problem, &mut result.solution);
    RouteAutoRun {
        result,
        global: Some(global),
    }
}

/// As [`route_auto`], but keeps the negotiated global-routing report from the
/// primary detailed candidate when that candidate ran.
pub fn route_auto_with_diagnostics(problem: &RouteProblem) -> RouteAutoRun {
    let direct = DirectLineRouter;
    let layer_hop = LayerHopRouter;
    let via_escape = ViaEscapeRouter;
    let pattern = PatternRouter;
    let channel = ChannelRouter;
    let sequential = SequentialGridRouter;
    let mesh = NegotiatedMeshRouter;
    let grid = GridAStarRouter;

    let mut best: Option<(RouteResult, RouteQuality)> = None;
    let mut global = None;

    if direct.can_route(problem) {
        let result = direct.route(problem);
        if consider_candidate(problem, &mut best, result) {
            let result = best.expect("direct candidate just populated best").0;
            return RouteAutoRun { result, global };
        }
    }

    if layer_hop.can_route(problem) {
        let result = layer_hop.route(problem);
        if consider_candidate(problem, &mut best, result) {
            let result = best.expect("layer-hop candidate just populated best").0;
            return RouteAutoRun { result, global };
        }
    }

    if via_escape.can_route(problem) {
        let result = via_escape.route(problem);
        if consider_candidate(problem, &mut best, result) {
            let result = best.expect("via-escape candidate just populated best").0;
            return RouteAutoRun { result, global };
        }
    }

    if pattern.can_route(problem) {
        let result = pattern.route(problem);
        if consider_candidate(problem, &mut best, result) {
            let result = best.expect("pattern candidate just populated best").0;
            return RouteAutoRun { result, global };
        }
    }

    if channel.can_route(problem) {
        let result = channel.route(problem);
        if consider_candidate(problem, &mut best, result) {
            let result = best.expect("channel candidate just populated best").0;
            return RouteAutoRun { result, global };
        }
    }

    if sequential.can_route(problem) {
        let result = sequential.route(problem);
        if consider_candidate(problem, &mut best, result) {
            let result = best.expect("sequential candidate just populated best").0;
            return RouteAutoRun { result, global };
        }
    }

    if mesh.can_route(problem) {
        let (mut result, g) = route_detailed_with_global(problem);
        reconcile_connectivity(problem, &mut result.solution, &mut result.failed);
        global = Some(g);
        if consider_candidate(problem, &mut best, result) {
            let result = best.expect("mesh candidate just populated best").0;
            return RouteAutoRun { result, global };
        }
    }

    if grid.can_route(problem) {
        let result = grid.route(problem);
        let _ = consider_candidate(problem, &mut best, result);
    }

    let result = best.map(|(r, _)| r).unwrap_or_else(|| RouteResult {
        solution: RouteSolution {
            traces: vec![],
            vias: vec![],
        },
        failed: vec![],
        engine: "none".to_owned(),
    });
    RouteAutoRun { result, global }
}

/// Freerouter-style postroute cleanup for selected copper: drop redundant vias,
/// merge degree-2 same-net trace fragments, then pull local trace corners tight
/// when the exact DRC/connectivity oracle says the shortcut is equivalent.
fn postroute_cleanup(problem: &RouteProblem, solution: &mut RouteSolution) {
    drop_redundant_thruhole_vias(problem, solution);
    drop_duplicate_vias(solution);
    drop_dangling_vias(problem, solution);
    drop_duplicate_traces(solution);
    simplify_trace_paths(problem, solution);
    drop_trace_spurs(problem, solution);
    drop_duplicate_traces(solution);
    drop_covered_collinear_traces(problem, solution);
    merge_touching_traces(problem, solution);
    shortcut_octilinear_traces(problem, solution);
    drop_trace_spurs(problem, solution);
    drop_duplicate_traces(solution);
    drop_covered_collinear_traces(problem, solution);
}

/// Drop a via that sits inside a SAME-NET through-hole pad: the pad's barrel
/// already spans every copper layer, so a via on it is a redundant layer change —
/// and its drill collides with the pad's (a KiCAD `hole_to_hole` defect). The
/// trace stays connected THROUGH the pad (both trace ends land inside it, and the
/// pad bridges the layers). A pad is through-hole when its obstacle reaches both
/// the top and bottom copper layers.
fn drop_redundant_thruhole_vias(problem: &RouteProblem, solution: &mut RouteSolution) {
    let (top, bottom) = (LayerRef::top(), LayerRef::bottom());
    solution.vias.retain(|v| {
        !problem.obstacles.iter().any(|ob| {
            ob.connected_to.contains(&v.connection)
                && ob.layers.contains(&top)
                && ob.layers.contains(&bottom)
                && (v.at.x - ob.center.x).abs() <= ob.width / 2.0
                && (v.at.y - ob.center.y).abs() <= ob.height / 2.0
        })
    });
}

/// Drop exact duplicate same-net vias. A second via with the same barrel geometry,
/// same span, and same coordinate adds no connectivity beyond the first one, but
/// does inflate via count and may trip hole-spacing checks downstream.
fn drop_duplicate_vias(solution: &mut RouteSolution) {
    let mut seen = BTreeSet::new();
    solution.vias.retain(|v| {
        seen.insert((
            v.connection.clone(),
            v.at.quantized_key(POINT_KEY_SCALE),
            (v.diameter * 1000.0).round() as i64,
            (v.drill * 1000.0).round() as i64,
            via_span_key(&v.span),
        ))
    });
}

fn via_span_key(span: &ViaSpan) -> (u32, u32, bool, bool) {
    match span {
        ViaSpan::Through => (0, 0, false, false),
        ViaSpan::Partial { from, to, micro } => (*from, *to, *micro, true),
    }
}

/// Drop same-net vias that do not actually bridge copper on at least two layers.
/// Detailed stitching already suppresses these; this applies the same cleanup to
/// fast-path and fallback router output. Every removal is lint-guarded so a via
/// anchor that preserves connectivity or DRC is kept.
fn drop_dangling_vias(problem: &RouteProblem, solution: &mut RouteSolution) {
    let mut baseline = crate::lint::lint(problem, solution);
    let mut idx = 0usize;
    while idx < solution.vias.len() {
        if via_connected_layers(problem, solution, &solution.vias[idx]).len() >= 2 {
            idx += 1;
            continue;
        }

        let mut candidate = solution.clone();
        candidate.vias.remove(idx);
        let findings = crate::lint::lint(problem, &candidate);
        if !introduces_new_findings(&baseline, &findings)
            && candidate.metrics().via_count < solution.metrics().via_count
        {
            *solution = candidate;
            baseline = findings;
        } else {
            idx += 1;
        }
    }
}

fn introduces_new_findings(
    baseline: &[crate::lint::DrcViolation],
    candidate: &[crate::lint::DrcViolation],
) -> bool {
    candidate
        .iter()
        .any(|finding| !baseline.iter().any(|known| known == finding))
}

fn via_connected_layers(
    problem: &RouteProblem,
    solution: &RouteSolution,
    via: &Via,
) -> BTreeSet<String> {
    const TOUCH: f64 = 0.02;
    let mut layers = BTreeSet::new();
    for trace in &solution.traces {
        if trace.connection == via.connection
            && trace
                .path
                .windows(2)
                .any(|w| geom::Segment::new(w[0], w[1]).dist_to_point(via.at) < TOUCH)
        {
            layers.insert(trace.layer.0.clone());
        }
    }
    for ob in &problem.obstacles {
        if ob.connected_to.contains(&via.connection)
            && (via.at.x - ob.center.x).abs() <= ob.width / 2.0 + 0.01
            && (via.at.y - ob.center.y).abs() <= ob.height / 2.0 + 0.01
        {
            for layer in &ob.layers {
                layers.insert(layer.0.clone());
            }
        }
    }
    layers
}

/// Drop exact duplicate same-net trace runs, including reversed duplicates.
/// Parallel copper is only removed when every vertex, layer, width, and net
/// matches exactly after canonical orientation.
fn drop_duplicate_traces(solution: &mut RouteSolution) {
    let mut seen = BTreeSet::new();
    solution
        .traces
        .retain(|t| seen.insert(trace_duplicate_key(t)))
}

fn trace_duplicate_key(trace: &Trace) -> (String, String, i64, Vec<(i64, i64)>) {
    let forward: Vec<(i64, i64)> = trace
        .path
        .iter()
        .map(|p| p.quantized_key(POINT_KEY_SCALE))
        .collect();
    let mut reverse = forward.clone();
    reverse.reverse();
    (
        trace.connection.clone(),
        trace.layer.0.clone(),
        (trace.width * 1000.0).round() as i64,
        forward.min(reverse),
    )
}

/// Remove redundant vertices inside individual traces when the full DRC/connectivity
/// lint report is unchanged. This catches equal-length collinear simplifications
/// that the shortcut pass deliberately skips because they do not reduce wirelength.
fn simplify_trace_paths(problem: &RouteProblem, solution: &mut RouteSolution) {
    let mut baseline = crate::lint::lint(problem, solution);
    for ti in 0..solution.traces.len() {
        let simplified = geom::Polyline::new(solution.traces[ti].path.clone())
            .simplify()
            .into_points();
        if simplified.len() < 2 || simplified.len() >= solution.traces[ti].path.len() {
            continue;
        }
        let mut candidate = solution.clone();
        candidate.traces[ti].path = simplified;
        let findings = crate::lint::lint(problem, &candidate);
        if findings == baseline {
            *solution = candidate;
            baseline = findings;
        }
    }
}

/// Remove closed subpaths inside a trace: `... P -> ... -> P ...` carries a spur
/// loop that adds copper but no connectivity. Every candidate goes through the
/// full lint report, so via anchors, terminal reachability, and DRC invariants
/// remain protected.
fn drop_trace_spurs(problem: &RouteProblem, solution: &mut RouteSolution) {
    let mut baseline = crate::lint::lint(problem, solution);
    let mut lint_budget = 256usize;

    loop {
        let mut improved = false;
        'trace: for ti in 0..solution.traces.len() {
            let path = &solution.traces[ti].path;
            if path.len() < 4 {
                continue;
            }
            for i in 0..path.len() - 2 {
                for j in (i + 2..path.len()).rev() {
                    if lint_budget == 0 {
                        return;
                    }
                    if !path[i].near_eq(path[j], JOIN_EPS) {
                        continue;
                    }
                    let mut new_path = Vec::with_capacity(path.len() - (j - i));
                    new_path.extend_from_slice(&path[..=i]);
                    new_path.extend_from_slice(&path[j + 1..]);
                    new_path.dedup_by(|a, b| a.near_eq(*b, JOIN_EPS));
                    if new_path.len() < 2 {
                        continue;
                    }
                    let mut candidate = solution.clone();
                    candidate.traces[ti].path =
                        geom::Polyline::new(new_path).simplify().into_points();
                    if candidate.traces[ti].path.len() < 2
                        || candidate.metrics().wirelength >= solution.metrics().wirelength
                    {
                        continue;
                    }
                    let findings = crate::lint::lint(problem, &candidate);
                    lint_budget -= 1;
                    if findings == baseline {
                        *solution = candidate;
                        baseline = findings;
                        improved = true;
                        break 'trace;
                    }
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// Drop same-net straight trace segments that are wholly covered by another
/// same-net, same-layer, same-width straight segment. This removes redundant
/// overlapped copper left by pattern/grid retries while preserving every
/// connectivity and DRC invariant through the lint oracle.
fn drop_covered_collinear_traces(problem: &RouteProblem, solution: &mut RouteSolution) {
    let mut baseline = crate::lint::lint(problem, solution);
    let mut idx = 0usize;
    while idx < solution.traces.len() {
        if trace_is_covered_by_another(solution, idx) {
            let mut candidate = solution.clone();
            candidate.traces.remove(idx);
            let findings = crate::lint::lint(problem, &candidate);
            if findings == baseline
                && candidate.metrics().wirelength < solution.metrics().wirelength
            {
                *solution = candidate;
                baseline = findings;
                continue;
            }
        }
        idx += 1;
    }
}

fn trace_is_covered_by_another(solution: &RouteSolution, idx: usize) -> bool {
    let trace = &solution.traces[idx];
    let Some((a, b)) = straight_trace_segment(trace) else {
        return false;
    };
    solution
        .traces
        .iter()
        .enumerate()
        .any(|(other_idx, other)| {
            if other_idx == idx
                || other.connection != trace.connection
                || other.layer != trace.layer
                || (other.width - trace.width).abs() > 1e-9
            {
                return false;
            }
            trace_straight_segments(other)
                .into_iter()
                .any(|(c, d)| segment_covers_segment(c, d, a, b))
        })
}

fn straight_trace_segment(trace: &Trace) -> Option<(Point2, Point2)> {
    let path = geom::Polyline::new(trace.path.clone())
        .simplify()
        .into_points();
    match path.as_slice() {
        [a, b] if a.dist(*b) >= geom::EPS => Some((*a, *b)),
        _ => None,
    }
}

fn trace_straight_segments(trace: &Trace) -> Vec<(Point2, Point2)> {
    let path = geom::Polyline::new(trace.path.clone())
        .simplify()
        .into_points();
    path.windows(2)
        .filter_map(|w| (w[0].dist(w[1]) >= geom::EPS).then_some((w[0], w[1])))
        .collect()
}

fn segment_covers_segment(
    outer_a: Point2,
    outer_b: Point2,
    inner_a: Point2,
    inner_b: Point2,
) -> bool {
    if !points_collinear(outer_a, outer_b, inner_a) || !points_collinear(outer_a, outer_b, inner_b)
    {
        return false;
    }
    if outer_a.dist(outer_b) + 1e-9 < inner_a.dist(inner_b) {
        return false;
    }
    point_on_segment(inner_a, outer_a, outer_b) && point_on_segment(inner_b, outer_a, outer_b)
}

fn points_collinear(a: Point2, b: Point2, p: Point2) -> bool {
    let abx = b.x - a.x;
    let aby = b.y - a.y;
    let apx = p.x - a.x;
    let apy = p.y - a.y;
    (abx * apy - aby * apx).abs() <= 1e-6
}

fn point_on_segment(p: Point2, a: Point2, b: Point2) -> bool {
    p.x >= a.x.min(b.x) - 1e-6
        && p.x <= a.x.max(b.x) + 1e-6
        && p.y >= a.y.min(b.y) - 1e-6
        && p.y <= a.y.max(b.y) + 1e-6
}

/// Merge same-net, same-layer, same-width trace fragments that meet at degree-2
/// endpoints. This is pure cleanup: every proposed rewrite is accepted only if
/// the full lint report is unchanged, so T-junctions, via anchors, and clearance
/// constraints remain under the same oracle as the selected route.
fn merge_touching_traces(problem: &RouteProblem, solution: &mut RouteSolution) {
    loop {
        let baseline = crate::lint::lint(problem, solution);
        let mut groups: BTreeMap<(String, String, i64), Vec<usize>> = BTreeMap::new();
        for (idx, trace) in solution.traces.iter().enumerate() {
            groups
                .entry((
                    trace.connection.clone(),
                    trace.layer.0.clone(),
                    (trace.width * 1000.0).round() as i64,
                ))
                .or_default()
                .push(idx);
        }

        let mut improved = false;
        for (_, idxs) in groups {
            if idxs.len() < 2 {
                continue;
            }
            let polys: Vec<Vec<Point2>> = idxs
                .iter()
                .map(|&idx| solution.traces[idx].path.clone())
                .collect();
            let joined = join_polylines(polys);
            if joined.len() >= idxs.len() {
                continue;
            }

            let first = idxs[0];
            let idx_set: BTreeSet<usize> = idxs.iter().copied().collect();
            let template = solution.traces[first].clone();
            let mut candidate = solution.clone();
            candidate.traces.clear();
            for (idx, trace) in solution.traces.iter().enumerate() {
                if idx_set.contains(&idx) {
                    if idx == first {
                        candidate.traces.extend(joined.iter().map(|path| Trace {
                            connection: template.connection.clone(),
                            layer: template.layer.clone(),
                            width: template.width,
                            path: geom::Polyline::new(path.clone()).simplify().into_points(),
                        }));
                    }
                } else {
                    candidate.traces.push(trace.clone());
                }
            }

            if candidate.traces.len() < solution.traces.len()
                && crate::lint::lint(problem, &candidate) == baseline
            {
                *solution = candidate;
                improved = true;
                break;
            }
        }

        if !improved {
            break;
        }
    }
}

/// Pull trace corners tight with conservative octilinear shortcuts. A candidate
/// replaces `p[i]..p[j]` with the direct segment `p[i]→p[j]` only when it shortens
/// the trace and the full lint report is unchanged, which protects via anchors,
/// T-junctions, clearance, board-edge, and connectivity invariants.
fn shortcut_octilinear_traces(problem: &RouteProblem, solution: &mut RouteSolution) {
    let mut baseline = crate::lint::lint(problem, solution);
    let mut lint_budget = 256usize;

    loop {
        let mut improved = false;
        'candidate: for ti in 0..solution.traces.len() {
            let n = solution.traces[ti].path.len();
            if n < 3 {
                continue;
            }
            for span in (2..n).rev() {
                for i in 0..(n - span) {
                    if lint_budget == 0 {
                        return;
                    }
                    let j = i + span;
                    let a = solution.traces[ti].path[i];
                    let b = solution.traces[ti].path[j];
                    if !is_octilinear_segment(a, b) {
                        continue;
                    }

                    let old_len = path_len(&solution.traces[ti].path[i..=j]);
                    let new_len = a.dist(b);
                    if new_len + 0.01 >= old_len {
                        continue;
                    }

                    let mut candidate = solution.clone();
                    candidate.traces[ti].path.drain(i + 1..j);
                    let findings = crate::lint::lint(problem, &candidate);
                    lint_budget -= 1;
                    if findings == baseline {
                        *solution = candidate;
                        baseline = findings;
                        improved = true;
                        break 'candidate;
                    }
                }
            }
        }

        if !improved {
            break;
        }
    }
}

fn is_octilinear_segment(a: Point2, b: Point2) -> bool {
    let dx = (a.x - b.x).abs();
    let dy = (a.y - b.y).abs();
    dx < geom::EPS || dy < geom::EPS || (dx - dy).abs() < 0.01
}

fn path_len(path: &[Point2]) -> f64 {
    path.windows(2).map(|w| w[0].dist(w[1])).sum()
}

/// At equal faults, keep the tidy orthogonal naive route unless its copper is more
/// than this factor longer than the detailed route (then the detailed router's
/// via-enabled direct routing is the cleaner result).
const NAIVE_DETOUR_TOLERANCE: f64 = 1.15;

/// Make a routed result DRC-HONEST: the lint is the authority, not the router's
/// own bookkeeping. First drop any net whose copper violates GEOMETRY (clearance
/// / width / via / bounds) — the engine must never emit copper that fails DRC —
/// then drop any net left unconnected or shorted (a cross-net merge). Every
/// dropped net is reported failed. After this `failed` is faithful and the
/// surviving copper is fully DRC-clean, so `route_auto`'s comparison ranks a
/// silent violation or phantom-route below an engine that cleanly connected
/// fewer nets, and the engine never ships copper that fails DRC.
fn reconcile_connectivity(
    problem: &RouteProblem,
    solution: &mut RouteSolution,
    failed: &mut Vec<FailedNet>,
) {
    let mut broken = crate::lint::drop_violating_copper(problem, solution);
    broken.extend(crate::lint::drop_unconnected_copper(problem, solution));
    let known: std::collections::BTreeSet<&str> =
        failed.iter().map(|f| f.connection.as_str()).collect();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let new: Vec<FailedNet> = broken
        .into_iter()
        .filter(|name| !known.contains(name.as_str()) && seen.insert(name.clone()))
        .map(|name| FailedNet {
            connection: name,
            reason:
                "DRC oracle: net dropped — could not be routed cleanly (clearance/connectivity)"
                    .to_string(),
        })
        .collect();
    failed.extend(new);
}

// ── stitching ──────────────────────────────────────────────────────────────────

/// Stitch per-cell copper into board traces + vias, skipping nets in `skip`.
///
/// Per net, per layer, cell-local polylines are concatenated by byte-exact
/// endpoint identity (degree-2 joins), re-simplified, and emitted as [`Trace`]s;
/// every [`crate::detail::CellVia`] becomes a [`Via`], deduplicated by exact
/// position. Net/layer/trace order is deterministic (BTreeMap key order).
fn stitch(
    problem: &RouteProblem,
    cell_routes: &[CellRoute],
    skip: &std::collections::BTreeSet<String>,
) -> RouteSolution {
    // Group cell copper by net, preserving deterministic (net, then layer) order.
    // Per net: per-layer list of polylines, plus the net's via sites.
    let mut by_net: BTreeMap<String, NetCopper> = BTreeMap::new();
    for cr in cell_routes {
        if skip.contains(&cr.connection) {
            continue;
        }
        let nc = by_net.entry(cr.connection.clone()).or_default();
        for t in &cr.traces {
            nc.polylines_on(&t.layer.0).push(t.points.clone());
        }
        for v in &cr.vias {
            nc.vias.push(v.at.clone());
        }
    }

    let mut traces: Vec<Trace> = Vec::new();
    let mut vias: Vec<Via> = Vec::new();

    for (connection, nc) in by_net {
        // Traces: join + simplify per layer (layers in BTreeMap key order).
        for (layer, polylines) in nc.by_layer {
            for poly in join_polylines(polylines) {
                let simplified = geom::Polyline::new(poly).simplify().into_points();
                if simplified.len() < 2 {
                    continue;
                }
                traces.push(Trace {
                    connection: connection.clone(),
                    layer: crate::problem::LayerRef(layer.clone()),
                    width: problem.net_width(&connection), // per-net: fat power, thin signals
                    path: simplified,
                });
            }
        }
        // Vias: dedup by exact position, and DROP spurious ones. A via is real
        // only if this net actually changes layer there — i.e. it has copper (a
        // trace endpoint or a pad) on ≥2 distinct layers at the via's position.
        // The cell stitch can emit a via where the net only has copper on one
        // layer (a layer transition that simplified away), which KiCAD flags as
        // `via_dangling`. Dropping it cannot break connectivity: by definition the
        // net is already connected without it, and the connectivity oracle +
        // naive fallback in `route_auto` catch any over-drop.
        for at in nc.vias {
            if vias
                .iter()
                .any(|v| v.connection == connection && v.at.near_eq(at, JOIN_EPS))
            {
                continue;
            }
            // A via connects a layer if a same-net trace TOUCHES it there — and
            // that touch can be at a trace endpoint OR a point the trace passes
            // straight through (a collinear interior point `simplify` removed). So
            // test distance to each trace SEGMENT, not just to its vertices, or a
            // genuinely-connecting via is mistaken for dangling and dropped.
            const TOUCH: f64 = 0.02;
            let mut layers: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            for t in &traces {
                if t.connection == connection
                    && t.path
                        .windows(2)
                        .any(|w| geom::Segment::new(w[0], w[1]).dist_to_point(at) < TOUCH)
                {
                    layers.insert(t.layer.0.as_str());
                }
            }
            for ob in &problem.obstacles {
                if ob.connected_to.contains(&connection)
                    && (at.x - ob.center.x).abs() <= ob.width / 2.0 + 0.01
                    && (at.y - ob.center.y).abs() <= ob.height / 2.0 + 0.01
                {
                    for lr in &ob.layers {
                        layers.insert(lr.0.as_str());
                    }
                }
            }
            if layers.len() < 2 {
                continue; // spurious / dangling — drop
            }
            vias.push(Via {
                connection: connection.clone(),
                at,
                diameter: problem.via_diameter,
                drill: problem.via_drill,
                span: ViaSpan::Through,
            });
        }
    }

    RouteSolution { traces, vias }
}

/// Per-net copper accumulated during stitching: polylines keyed by layer name
/// (BTreeMap for deterministic layer order) plus via sites.
#[derive(Default)]
struct NetCopper {
    by_layer: BTreeMap<String, Vec<Vec<Point2>>>,
    vias: Vec<Point2>,
}

impl NetCopper {
    fn polylines_on(&mut self, layer: &str) -> &mut Vec<Vec<Point2>> {
        self.by_layer.entry(layer.to_owned()).or_default()
    }
}

/// Concatenate polylines that meet byte-exactly at a degree-2 endpoint into
/// continuous runs. A point where three or more polyline-ends meet (a T-junction)
/// is left as a shared vertex — the polylines touching it stay separate (KiCAD /
/// the connectivity oracle treat a shared vertex as connected). Deterministic:
/// merges are tried in input order; the output is sorted by first point.
fn join_polylines(mut polys: Vec<Vec<Point2>>) -> Vec<Vec<Point2>> {
    // Drop degenerate polylines (< 2 points) up front.
    polys.retain(|p| p.len() >= 2);
    if polys.len() <= 1 {
        return polys;
    }

    // Endpoint multiplicity: how many polyline-ends land on each exact point.
    // Only a point shared by exactly two ends is a safe degree-2 join; a T-
    // junction (≥ 3) must keep its polylines separate.
    loop {
        let degree = endpoint_degree(&polys);
        let mut merged = false;

        'outer: for i in 0..polys.len() {
            for j in (i + 1)..polys.len() {
                // Try to join polys[i] and polys[j] at a shared, degree-2 end.
                if let Some(joined) = try_join(&polys[i], &polys[j], &degree) {
                    // Replace i with the joined run, remove j.
                    polys[i] = joined;
                    polys.remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }

        if !merged {
            break;
        }
    }

    // Deterministic output order: by first point, then last point.
    polys.sort_by(|a, b| {
        a[0].cmp_xy(b[0])
            .then_with(|| (*a.last().unwrap()).cmp_xy(*b.last().unwrap()))
    });
    polys
}

/// Count, per exact endpoint, how many polyline-ends (first/last point) coincide
/// with it. Keyed by quantised coordinates so byte-exact-equal points collide.
fn endpoint_degree(polys: &[Vec<Point2>]) -> BTreeMap<(i64, i64), usize> {
    let mut degree: BTreeMap<(i64, i64), usize> = BTreeMap::new();
    for p in polys {
        *degree
            .entry(p[0].quantized_key(POINT_KEY_SCALE))
            .or_insert(0) += 1;
        *degree
            .entry((*p.last().unwrap()).quantized_key(POINT_KEY_SCALE))
            .or_insert(0) += 1;
    }
    degree
}

/// Join `a` and `b` if they share a byte-exact endpoint that is a degree-2 join
/// point (exactly two ends meet there). Returns the concatenated run oriented so
/// the shared point is interior, or `None` if no safe join exists.
fn try_join(
    a: &[Point2],
    b: &[Point2],
    degree: &BTreeMap<(i64, i64), usize>,
) -> Option<Vec<Point2>> {
    let a0 = &a[0];
    let a1 = a.last().unwrap();
    let b0 = &b[0];
    let b1 = b.last().unwrap();

    // A shared point is only joinable when exactly two ends meet there (degree
    // 2). At a T-junction the degree is ≥ 3 and we must not merge.
    let deg2 = |p: &Point2| {
        degree
            .get(&(*p).quantized_key(POINT_KEY_SCALE))
            .copied()
            .unwrap_or(0)
            == 2
    };

    // Four ways the two runs can abut. Pick the one whose shared point is degree
    // 2; concatenate dropping the duplicated shared point.
    if (*a1).near_eq(*b0, JOIN_EPS) && deg2(a1) {
        // a … a1 == b0 … b1
        let mut out = a.to_vec();
        out.extend_from_slice(&b[1..]);
        return non_closed_join(out);
    }
    if (*a1).near_eq(*b1, JOIN_EPS) && deg2(a1) {
        // a … a1 == b1 … b0  (reverse b)
        let mut out = a.to_vec();
        out.extend(b.iter().rev().skip(1).cloned());
        return non_closed_join(out);
    }
    if (*a0).near_eq(*b1, JOIN_EPS) && deg2(a0) {
        // b0 … b1 == a0 … a1
        let mut out = b.to_vec();
        out.extend_from_slice(&a[1..]);
        return non_closed_join(out);
    }
    if (*a0).near_eq(*b0, JOIN_EPS) && deg2(a0) {
        // a1 … a0 == b0 … b1  (reverse a)
        let mut out: Vec<Point2> = a.iter().rev().cloned().collect();
        out.extend_from_slice(&b[1..]);
        return non_closed_join(out);
    }
    None
}

fn non_closed_join(out: Vec<Point2>) -> Option<Vec<Point2>> {
    let (Some(first), Some(last)) = (out.first(), out.last()) else {
        return None;
    };
    if first.near_eq(*last, JOIN_EPS) {
        None
    } else {
        Some(out)
    }
}

/// Fine endpoint quantum: 1e9 is roughly 1 nm in board millimetres.
const POINT_KEY_SCALE: f64 = 1e9;

// ── failure folding helpers ────────────────────────────────────────────────────

/// Record a per-net failure (with its stage-prefixed reason) and mark the net so
/// its copper is dropped from the stitched solution.
fn fail(
    failed: &mut Vec<FailedNet>,
    failed_names: &mut std::collections::BTreeSet<String>,
    connection: &str,
    reason: String,
) {
    failed.push(FailedNet {
        connection: connection.to_owned(),
        reason,
    });
    failed_names.insert(connection.to_owned());
}

/// The connection a crossing-assignment failure is attributable to (empty for a
/// boundary overflow, which is not one net's fault).
fn assignment_failure_connection(f: &crate::crossing::AssignmentFailure) -> String {
    use crate::crossing::AssignmentFailure::*;
    match f {
        Overflow { .. } => String::new(),
        ViaSite { connection, .. } => connection.clone(),
    }
}

/// A human-readable reason for a crossing-assignment failure.
fn assignment_failure_reason(f: &crate::crossing::AssignmentFailure) -> String {
    use crate::crossing::AssignmentFailure::*;
    match f {
        Overflow {
            edge,
            layer,
            needed,
            available,
        } => format!(
            "boundary overflow on edge {edge} layer {layer}: {needed} crossings need slots, {available} fit"
        ),
        ViaSite { connection, leaf } => {
            format!("no clear via site for {connection} in leaf {leaf}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lint::lint;
    use std::path::Path;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn load(name: &str) -> RouteProblem {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name);
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
    }

    fn simple_two_point_problem() -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![crate::problem::Connection {
                name: "N".to_owned(),
                points_to_connect: vec![
                    crate::problem::RoutePoint {
                        x: 2.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                    crate::problem::RoutePoint {
                        x: 18.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: crate::problem::Rect {
                min_x: 0.0,
                max_x: 20.0,
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

    fn top_blocked_two_point_problem() -> RouteProblem {
        let mut p = simple_two_point_problem();
        p.obstacles = vec![
            crate::problem::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 2.0, y: 5.0 },
                width: 0.6,
                height: 0.6,
                connected_to: vec!["N".to_owned()],
            },
            crate::problem::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 18.0, y: 5.0 },
                width: 0.6,
                height: 0.6,
                connected_to: vec!["N".to_owned()],
            },
            crate::problem::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 10.0, y: 5.0 },
                width: 1.0,
                height: 10.0,
                connected_to: vec![],
            },
        ];
        p
    }

    fn layer_change_problem() -> RouteProblem {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0] = crate::problem::RoutePoint {
            x: 1.0,
            y: 1.0,
            layer: LayerRef::top(),
        };
        p.connections[0].points_to_connect[1] = crate::problem::RoutePoint {
            x: 4.0,
            y: 4.0,
            layer: LayerRef::bottom(),
        };
        p
    }

    fn stacked_layer_change_problem() -> RouteProblem {
        let mut p = simple_two_point_problem();
        p.obstacles = vec![
            crate::problem::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 5.0, y: 5.0 },
                width: 0.6,
                height: 0.6,
                connected_to: vec!["N".to_owned()],
            },
            crate::problem::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::bottom()],
                center: Point2 { x: 5.0, y: 5.0 },
                width: 0.6,
                height: 0.6,
                connected_to: vec!["N".to_owned()],
            },
        ];
        p.connections[0].points_to_connect = vec![
            crate::problem::RoutePoint {
                x: 5.0,
                y: 5.0,
                layer: LayerRef::top(),
            },
            crate::problem::RoutePoint {
                x: 5.0,
                y: 5.0,
                layer: LayerRef::bottom(),
            },
        ];
        p
    }

    fn pad_obstacle(net: &str, x: f64, y: f64, layer: LayerRef) -> crate::problem::Obstacle {
        crate::problem::Obstacle {
            kind: "rect".to_owned(),
            layers: vec![layer],
            center: Point2 { x, y },
            width: 0.6,
            height: 0.6,
            connected_to: vec![net.to_owned()],
        }
    }

    fn heterogeneous_pattern_problem() -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![
                pad_obstacle("D", 2.0, 2.0, LayerRef::top()),
                pad_obstacle("D", 8.0, 2.0, LayerRef::top()),
                pad_obstacle("L", 2.0, 6.0, LayerRef::top()),
                pad_obstacle("L", 8.0, 6.0, LayerRef::bottom()),
                pad_obstacle("V", 2.0, 14.0, LayerRef::top()),
                pad_obstacle("V", 20.0, 14.0, LayerRef::top()),
                crate::problem::Obstacle {
                    kind: "rect".to_owned(),
                    layers: vec![LayerRef::top()],
                    center: Point2 { x: 11.0, y: 10.0 },
                    width: 1.0,
                    height: 20.0,
                    connected_to: vec![],
                },
            ],
            connections: vec![
                crate::problem::Connection {
                    name: "D".to_owned(),
                    points_to_connect: vec![
                        crate::problem::RoutePoint {
                            x: 2.0,
                            y: 2.0,
                            layer: LayerRef::top(),
                        },
                        crate::problem::RoutePoint {
                            x: 8.0,
                            y: 2.0,
                            layer: LayerRef::top(),
                        },
                    ],
                },
                crate::problem::Connection {
                    name: "L".to_owned(),
                    points_to_connect: vec![
                        crate::problem::RoutePoint {
                            x: 2.0,
                            y: 6.0,
                            layer: LayerRef::top(),
                        },
                        crate::problem::RoutePoint {
                            x: 8.0,
                            y: 6.0,
                            layer: LayerRef::bottom(),
                        },
                    ],
                },
                crate::problem::Connection {
                    name: "V".to_owned(),
                    points_to_connect: vec![
                        crate::problem::RoutePoint {
                            x: 2.0,
                            y: 14.0,
                            layer: LayerRef::top(),
                        },
                        crate::problem::RoutePoint {
                            x: 20.0,
                            y: 14.0,
                            layer: LayerRef::top(),
                        },
                    ],
                },
            ],
            bounds: crate::problem::Rect {
                min_x: 0.0,
                max_x: 30.0,
                min_y: 0.0,
                max_y: 20.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        }
    }

    // ── led-r: the strict gate that must pass through route_detailed today ──────

    #[test]
    fn led_r_detailed_is_clean_and_lints_empty() {
        let p = load("led-r.json");
        let r = route_detailed(&p);
        assert_eq!(r.engine, ENGINE);
        assert!(
            r.failed.is_empty(),
            "led-r must route cleanly through route_detailed today: {:?}",
            r.failed
        );
        let vs = lint(&p, &r.solution);
        assert!(
            vs.is_empty(),
            "led-r detailed solution must lint CLEAN, got {vs:?}"
        );
        let m = r.solution.metrics();
        assert!(m.wirelength > 0.0, "led-r produced copper");
        assert!(m.trace_count > 0, "led-r has traces");
    }

    // ── quad: now routes cleanly through route_detailed (Task 3.5 finisher) ─────

    #[test]
    fn quad_detailed_is_clean_and_lints_empty() {
        // Task 3.5 (hotspot repair — the per-net finisher) closed quad's residual
        // cell failures: its six over-converged central crossings are completed by
        // the full-board finisher after the per-cell pass. route_detailed is now
        // clean end-to-end and lints empty.
        let p = load("quad.json");
        let r = route_detailed(&p);
        assert_eq!(r.engine, ENGINE);
        assert!(
            r.failed.is_empty(),
            "quad must route cleanly through route_detailed after the finisher: {:?}",
            r.failed
        );
        let vs = lint(&p, &r.solution);
        assert!(
            vs.is_empty(),
            "quad detailed solution must lint CLEAN, got {vs:?}"
        );
    }

    #[test]
    fn quad_auto_is_clean() {
        // The premium auto portfolio is direct fast-path, layer-hop/via-escape
        // micro-routers, contextual sequential grid, negotiated detailed primary,
        // and grid fallback. Quad is no longer forced to pay the detailed mesh:
        // the sequential candidate can solve it cleanly and should short-circuit.
        let p = load("quad.json");
        let r = route_auto(&p);
        assert_eq!(r.engine, crate::sequential::ENGINE);
        assert!(
            r.failed.is_empty(),
            "route_auto routes quad cleanly: {:?}",
            r.failed
        );
        let vs = lint(&p, &r.solution);
        assert!(
            vs.is_empty(),
            "quad route_auto solution must lint CLEAN, got {vs:?}"
        );
    }

    #[test]
    fn route_auto_diagnostics_skip_global_for_clean_direct_route() {
        let p = simple_two_point_problem();
        let r = route_auto_with_diagnostics(&p);
        assert_eq!(r.result.engine, crate::direct::ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert!(
            r.global.is_none(),
            "direct short-circuit should not pay negotiated global routing"
        );
    }

    #[test]
    fn route_auto_direct_fast_path_returns_same_cleaned_result_as_selector() {
        let p = simple_two_point_problem();
        let direct = DirectLineRouter;

        let auto = route_auto(&p);
        let selected = select_best(&p, &[&direct]);

        assert_eq!(auto.engine, crate::direct::ENGINE);
        assert_eq!(auto.failed, selected.failed);
        assert_eq!(
            serde_json::to_string(&auto.solution).unwrap(),
            serde_json::to_string(&selected.solution).unwrap(),
            "route_auto early return should keep the already-cleaned selected candidate"
        );
        assert!(lint(&p, &auto.solution).is_empty());
    }

    struct RedundantViaRouter;

    impl Router for RedundantViaRouter {
        fn name(&self) -> &'static str {
            "redundant-via"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_layers: u32::MAX,
                honors_escape_layers: true,
                honors_net_widths: true,
                honors_outline: true,
            }
        }

        fn route(&self, problem: &RouteProblem) -> RouteResult {
            RouteResult {
                solution: RouteSolution {
                    traces: vec![Trace {
                        connection: "N".to_owned(),
                        layer: LayerRef::top(),
                        width: problem.min_trace_width,
                        path: vec![pt(2.0, 5.0), pt(18.0, 5.0)],
                    }],
                    vias: vec![Via {
                        connection: "N".to_owned(),
                        at: pt(2.0, 5.0),
                        diameter: problem.via_diameter,
                        drill: problem.via_drill,
                        span: ViaSpan::Through,
                    }],
                },
                failed: vec![],
                engine: self.name().to_owned(),
            }
        }
    }

    struct CountingRouter {
        calls: Arc<AtomicUsize>,
    }

    impl Router for CountingRouter {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_layers: u32::MAX,
                honors_escape_layers: true,
                honors_net_widths: true,
                honors_outline: true,
            }
        }

        fn route(&self, _problem: &RouteProblem) -> RouteResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            RouteResult {
                solution: RouteSolution {
                    traces: vec![],
                    vias: vec![],
                },
                failed: vec![],
                engine: self.name().to_owned(),
            }
        }
    }

    #[test]
    fn select_best_cleans_candidate_before_short_circuit_return() {
        let mut p = simple_two_point_problem();
        p.obstacles = vec![
            crate::problem::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top(), LayerRef::bottom()],
                center: pt(2.0, 5.0),
                width: 0.8,
                height: 0.8,
                connected_to: vec!["N".to_owned()],
            },
            crate::problem::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top(), LayerRef::bottom()],
                center: pt(18.0, 5.0),
                width: 0.8,
                height: 0.8,
                connected_to: vec!["N".to_owned()],
            },
        ];
        let redundant = RedundantViaRouter;
        let calls = Arc::new(AtomicUsize::new(0));
        let counting = CountingRouter {
            calls: calls.clone(),
        };

        let selected = select_best(&p, &[&redundant, &counting]);

        assert_eq!(selected.engine, "redundant-via");
        assert!(selected.failed.is_empty(), "{:?}", selected.failed);
        assert!(selected.solution.vias.is_empty());
        assert!(lint(&p, &selected.solution).is_empty());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "clean-after-cleanup candidate should short-circuit before later routers"
        );
    }

    #[test]
    fn route_auto_diagnostics_skip_global_for_clean_layer_hop_route() {
        let p = layer_change_problem();
        assert!(
            !crate::direct::route_direct(&p).failed.is_empty(),
            "direct fast-path must not solve a layer-changing net"
        );
        assert!(
            !crate::via_escape::route_via_escape(&p).failed.is_empty(),
            "via-escape must stay scoped to same-layer nets"
        );

        let r = route_auto_with_diagnostics(&p);

        assert_eq!(r.result.engine, crate::layer_hop::ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert_eq!(r.result.solution.vias.len(), 1);
        assert!(lint(&p, &r.result.solution).is_empty());
        assert!(
            r.global.is_none(),
            "layer-hop short-circuit should not pay negotiated global routing"
        );
    }

    #[test]
    fn route_auto_diagnostics_skip_global_for_via_only_layer_hop_route() {
        let p = stacked_layer_change_problem();
        assert!(
            !crate::direct::route_direct(&p).failed.is_empty(),
            "direct fast-path must not solve a stacked layer change"
        );

        let r = route_auto_with_diagnostics(&p);

        assert_eq!(r.result.engine, crate::layer_hop::ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert!(r.result.solution.traces.is_empty());
        assert_eq!(r.result.solution.vias.len(), 1);
        assert!(lint(&p, &r.result.solution).is_empty());
        assert!(
            r.global.is_none(),
            "via-only layer-hop should not pay negotiated global routing"
        );
    }

    #[test]
    fn route_auto_diagnostics_skip_global_for_clean_layer_hop_star() {
        let mut p = layer_change_problem();
        p.connections[0]
            .points_to_connect
            .push(crate::problem::RoutePoint {
                x: 6.0,
                y: 4.0,
                layer: LayerRef::bottom(),
            });
        assert!(
            !crate::direct::route_direct(&p).failed.is_empty(),
            "direct fast-path must not solve a mixed-layer star"
        );
        assert!(
            !crate::via_escape::route_via_escape(&p).failed.is_empty(),
            "via-escape must stay scoped to same-layer nets"
        );

        let r = route_auto_with_diagnostics(&p);

        assert_eq!(r.result.engine, crate::layer_hop::ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert_eq!(r.result.solution.traces.len(), 1);
        assert_eq!(r.result.solution.vias.len(), 1);
        assert!(lint(&p, &r.result.solution).is_empty());
        assert!(
            r.global.is_none(),
            "layer-hop star short-circuit should not pay negotiated global routing"
        );
    }

    #[test]
    fn route_auto_diagnostics_skip_global_for_clean_via_escape_route() {
        let p = top_blocked_two_point_problem();
        assert!(
            !crate::direct::route_direct(&p).failed.is_empty(),
            "direct fast-path must not solve a via-escape case"
        );

        let r = route_auto_with_diagnostics(&p);

        assert_eq!(r.result.engine, crate::via_escape::ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert_eq!(r.result.solution.vias.len(), 2);
        assert!(lint(&p, &r.result.solution).is_empty());
        assert!(
            r.global.is_none(),
            "via-escape short-circuit should not pay negotiated global routing"
        );
    }

    #[test]
    fn route_auto_diagnostics_skip_global_for_composite_pattern_route() {
        let p = heterogeneous_pattern_problem();
        assert!(!crate::direct::route_direct(&p).failed.is_empty());
        assert!(!crate::layer_hop::route_layer_hop(&p).failed.is_empty());
        assert!(!crate::via_escape::route_via_escape(&p).failed.is_empty());

        let r = route_auto_with_diagnostics(&p);

        assert_eq!(r.result.engine, crate::pattern::ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert_eq!(r.result.solution.traces.len(), 3);
        assert_eq!(r.result.solution.vias.len(), 3);
        assert!(lint(&p, &r.result.solution).is_empty());
        assert!(
            r.global.is_none(),
            "pattern router should compose simple nets without negotiated global routing"
        );
    }

    #[test]
    fn route_quality_tiebreak_prefers_fewer_vias_before_wirelength() {
        let p = simple_two_point_problem();
        let via_heavy_short = RouteQuality {
            fault_weight: 0,
            geom: 0,
            failed_nets: 0,
            via_count: 2,
            wirelength: 10.0,
        };
        let via_free_long = RouteQuality {
            fault_weight: 0,
            geom: 0,
            failed_nets: 0,
            via_count: 0,
            wirelength: 11.0,
        };

        assert!(
            !better(&p, &via_heavy_short, &via_free_long),
            "challenger with fewer vias should replace a shorter via-heavy incumbent"
        );
        assert!(
            better(&p, &via_free_long, &via_heavy_short),
            "incumbent with fewer vias should be kept before wirelength is considered"
        );
    }

    #[test]
    fn postroute_cleanup_shortcuts_clean_octilinear_detour() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 4.0;
        p.connections[0].points_to_connect[1].y = 4.0;
        let mut solution = RouteSolution {
            traces: vec![Trace {
                connection: "N".to_owned(),
                layer: LayerRef::top(),
                width: p.min_trace_width,
                path: vec![pt(1.0, 1.0), pt(1.0, 4.0), pt(4.0, 4.0)],
            }],
            vias: vec![],
        };
        let before = solution.metrics().wirelength;

        postroute_cleanup(&p, &mut solution);

        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 4.0)]);
        assert!(solution.metrics().wirelength < before);
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_merges_degree_two_trace_fragments() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 4.0;
        p.connections[0].points_to_connect[1].y = 1.0;
        let mut solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(2.0, 1.0)],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(2.0, 1.0), pt(4.0, 1.0)],
                },
            ],
            vias: vec![],
        };
        assert!(lint(&p, &solution).is_empty());

        postroute_cleanup(&p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 1.0)]);
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_drops_duplicate_same_net_traces() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 4.0;
        p.connections[0].points_to_connect[1].y = 1.0;
        let mut solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(4.0, 1.0)],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(4.0, 1.0), pt(1.0, 1.0)],
                },
            ],
            vias: vec![],
        };
        let before = solution.metrics().wirelength;

        postroute_cleanup(&p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 1.0)]);
        assert!(solution.metrics().wirelength < before);
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_simplifies_collinear_trace_vertices() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 4.0;
        p.connections[0].points_to_connect[1].y = 1.0;
        let mut solution = RouteSolution {
            traces: vec![Trace {
                connection: "N".to_owned(),
                layer: LayerRef::top(),
                width: p.min_trace_width,
                path: vec![pt(1.0, 1.0), pt(2.0, 1.0), pt(4.0, 1.0)],
            }],
            vias: vec![],
        };
        let before = solution.metrics().wirelength;
        assert!(lint(&p, &solution).is_empty());

        postroute_cleanup(&p, &mut solution);

        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 1.0)]);
        assert!(
            (solution.metrics().wirelength - before).abs() < 1e-9,
            "collinear simplification should preserve wirelength"
        );
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_drops_noncollinear_trace_spur_loop() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 5.0;
        p.connections[0].points_to_connect[1].y = 1.0;
        let mut solution = RouteSolution {
            traces: vec![Trace {
                connection: "N".to_owned(),
                layer: LayerRef::top(),
                width: p.min_trace_width,
                path: vec![
                    pt(1.0, 1.0),
                    pt(2.0, 1.0),
                    pt(2.0, 3.0),
                    pt(3.0, 3.0),
                    pt(2.0, 1.0),
                    pt(5.0, 1.0),
                ],
            }],
            vias: vec![],
        };
        let before = solution.metrics().wirelength;
        assert!(lint(&p, &solution).is_empty());

        postroute_cleanup(&p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(5.0, 1.0)]);
        assert!(
            solution.metrics().wirelength < before,
            "closed spur loop should be removed"
        );
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_drops_duplicates_created_by_trace_simplification() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 4.0;
        p.connections[0].points_to_connect[1].y = 1.0;
        let mut solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(4.0, 1.0)],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(2.0, 1.0), pt(4.0, 1.0)],
                },
            ],
            vias: vec![],
        };
        let before = solution.metrics().wirelength;

        postroute_cleanup(&p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 1.0)]);
        assert!(
            solution.metrics().wirelength < before,
            "simplification-created duplicate should be dropped"
        );
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_drops_same_net_trace_covered_by_longer_trace() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 5.0;
        p.connections[0].points_to_connect[1].y = 1.0;
        let mut solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(5.0, 1.0)],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(2.0, 1.0), pt(4.0, 1.0)],
                },
            ],
            vias: vec![],
        };
        let before = solution.metrics().wirelength;
        assert!(lint(&p, &solution).is_empty());

        postroute_cleanup(&p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(5.0, 1.0)]);
        assert!(
            solution.metrics().wirelength < before,
            "covered same-net segment should be dropped"
        );
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_drops_trace_covered_by_polyline_segment() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 5.0;
        p.connections[0].points_to_connect[1].y = 4.0;
        let mut solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(5.0, 1.0), pt(5.0, 4.0)],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(2.0, 1.0), pt(4.0, 1.0)],
                },
            ],
            vias: vec![],
        };
        let before = solution.metrics().wirelength;
        assert!(lint(&p, &solution).is_empty());

        postroute_cleanup(&p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(
            solution.traces[0].path,
            vec![pt(1.0, 1.0), pt(5.0, 1.0), pt(5.0, 4.0)]
        );
        assert!(
            solution.metrics().wirelength < before,
            "segment covered by one leg of a longer trace should be dropped"
        );
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_drops_duplicates_created_by_shortcutting() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 5.0;
        p.connections[0].points_to_connect[1].y = 1.0;
        let mut solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(5.0, 1.0)],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(1.0, 4.0), pt(5.0, 4.0), pt(5.0, 1.0)],
                },
            ],
            vias: vec![],
        };
        let before = solution.metrics().wirelength;
        assert!(lint(&p, &solution).is_empty());

        postroute_cleanup(&p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(5.0, 1.0)]);
        assert!(
            solution.metrics().wirelength < before,
            "shortcut-created duplicate should be dropped"
        );
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_does_not_bypass_needed_via_anchor() {
        let p = layer_change_problem();
        let mut solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(1.0, 4.0), pt(4.0, 4.0)],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::bottom(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 4.0), pt(4.0, 4.0)],
                },
            ],
            vias: vec![Via {
                connection: "N".to_owned(),
                at: pt(1.0, 4.0),
                diameter: p.via_diameter,
                drill: p.via_drill,
                span: ViaSpan::Through,
            }],
        };
        assert!(lint(&p, &solution).is_empty());

        postroute_cleanup(&p, &mut solution);

        assert_eq!(
            solution.traces[0].path,
            vec![pt(1.0, 1.0), pt(1.0, 4.0), pt(4.0, 4.0)],
            "shortcut must be rejected because it disconnects the via anchor"
        );
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_drops_duplicate_same_net_vias() {
        let p = layer_change_problem();
        let via = Via {
            connection: "N".to_owned(),
            at: pt(1.0, 4.0),
            diameter: p.via_diameter,
            drill: p.via_drill,
            span: ViaSpan::Through,
        };
        let mut solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(1.0, 4.0)],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::bottom(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 4.0), pt(4.0, 4.0)],
                },
            ],
            vias: vec![via.clone(), via],
        };

        postroute_cleanup(&p, &mut solution);

        assert_eq!(solution.vias.len(), 1);
        assert_eq!(solution.vias[0].at, pt(1.0, 4.0));
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_drops_dangling_same_net_via() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 5.0;
        p.connections[0].points_to_connect[1].y = 1.0;
        let mut solution = RouteSolution {
            traces: vec![Trace {
                connection: "N".to_owned(),
                layer: LayerRef::top(),
                width: p.min_trace_width,
                path: vec![pt(1.0, 1.0), pt(5.0, 1.0)],
            }],
            vias: vec![Via {
                connection: "N".to_owned(),
                at: pt(3.0, 1.0),
                diameter: p.via_diameter,
                drill: p.via_drill,
                span: ViaSpan::Through,
            }],
        };
        assert!(lint(&p, &solution).is_empty());

        postroute_cleanup(&p, &mut solution);

        assert!(
            solution.vias.is_empty(),
            "single-layer dangling via should be dropped"
        );
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(5.0, 1.0)]);
        assert!(lint(&p, &solution).is_empty());
    }

    #[test]
    fn route_auto_diagnostics_skip_global_for_clean_sequential_route() {
        let p = load("quad.json");
        let r = route_auto_with_diagnostics(&p);
        assert_eq!(r.result.engine, crate::sequential::ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert!(
            r.global.is_none(),
            "clean sequential-grid route should not pay negotiated global routing"
        );
    }

    #[test]
    fn route_mesh_diagnostics_capture_global_report() {
        let p = load("quad.json");
        let r = route_mesh_with_diagnostics(&p);
        assert_eq!(r.result.engine, ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert!(
            r.global
                .as_ref()
                .is_some_and(GlobalRouteResult::is_feasible),
            "mesh diagnostics should include feasible global report"
        );
    }

    // ── congested: the per-net finisher repairs most of the wall, but the wall is
    //    EXACTLY saturated (8 top-layer crossings for 8 nets, the relief gap's fifth
    //    slot at its blocked margin) and a few nets stay honest failures — the
    //    rip-up case the slice plan put off the table. route_detailed reports them
    //    with finisher provenance; route_auto then falls back to the grid baseline
    //    if it has fewer failed nets. (See `tests/detailed_gate.rs` for the
    //    geometry note.)

    #[test]
    fn congested_auto_reports_honest_failures() {
        let p = load("congested.json");
        // route_detailed(congested) is the expensive path — call it ONCE and derive
        // route_auto's outcome from it + naive (route_auto runs exactly this
        // route_detailed internally, then naive, and returns the fewer-failed result),
        // rather than paying for a second full detailed route.
        let detailed = route_detailed(&p);
        let naive = router::route(&p);
        assert!(
            !naive.failed.is_empty() && !detailed.failed.is_empty(),
            "congested still defeats both engines (naive {} / detailed {} failed); \
             the saturated wall is rip-up territory, out of this slice's scope",
            naive.failed.len(),
            detailed.failed.len()
        );
        // The detailed finisher leaves only honest connectivity failures — every
        // residual carries finisher provenance, and the emitted copper is geometry-
        // clean (the lint shows only Connectivity from the dropped failed nets, no
        // clearance/via violation).
        assert!(
            detailed
                .failed
                .iter()
                .all(|f| f.reason.contains("finisher")),
            "every congested residual must carry finisher provenance: {:?}",
            detailed.failed
        );
        let geom_violations: Vec<_> = lint(&p, &detailed.solution)
            .into_iter()
            .filter(|v| !format!("{v:?}").contains("Connectivity"))
            .collect();
        assert!(
            geom_violations.is_empty(),
            "congested detailed copper must be geometry-clean (only connectivity gaps \
             from dropped nets are allowed), got {geom_violations:?}"
        );
        // route_auto keeps the negotiated primary on equal faults and falls back
        // only when grid has fewer failed nets; with both non-empty here the
        // report is honest either way.
        if naive.failed.len() < detailed.failed.len() {
            // grid wins only on a strict routability improvement.
            assert!(!naive.failed.is_empty());
        } else {
            assert!(!detailed.failed.is_empty());
        }
    }

    // ── determinism ─────────────────────────────────────────────────────────────

    #[test]
    fn route_detailed_is_deterministic() {
        let p = load("quad.json");
        let a = route_detailed(&p);
        let b = route_detailed(&p);
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        assert_eq!(ja, jb, "two route_detailed runs must serialize byte-equal");
    }

    // ── partial-net rule: a failed net contributes NO copper ────────────────────

    #[test]
    fn no_trace_belongs_to_a_failed_net() {
        // The partial-net rule: a net listed in `failed` must contribute zero
        // copper to the solution (a half-routed net would trip connectivity).
        let p = load("quad.json");
        let r = route_detailed(&p);
        let failed_names: std::collections::BTreeSet<&str> =
            r.failed.iter().map(|f| f.connection.as_str()).collect();
        for t in &r.solution.traces {
            assert!(
                !failed_names.contains(t.connection.as_str()),
                "trace for failed net {} must not appear in route_detailed solution",
                t.connection
            );
        }
        for v in &r.solution.vias {
            assert!(
                !failed_names.contains(v.connection.as_str()),
                "via for failed net {} must not appear in route_detailed solution",
                v.connection
            );
        }
    }

    // ── stitch unit tests: joins, T-junctions, simplification ───────────────────

    fn pt(x: f64, y: f64) -> Point2 {
        Point2 { x, y }
    }

    #[test]
    fn join_concatenates_two_polylines_at_a_shared_endpoint() {
        // Two runs meeting byte-exactly at (5,0) join into one, and the collinear
        // join collapses via simplify.
        let polys = vec![
            vec![pt(0.0, 0.0), pt(5.0, 0.0)],
            vec![pt(5.0, 0.0), pt(10.0, 0.0)],
        ];
        let joined = join_polylines(polys);
        assert_eq!(joined.len(), 1, "two abutting runs join into one");
        let simplified = geom::Polyline::new(joined.into_iter().next().unwrap())
            .simplify()
            .into_points();
        assert_eq!(
            simplified,
            vec![pt(0.0, 0.0), pt(10.0, 0.0)],
            "the collinear join collapses to two points"
        );
    }

    #[test]
    fn join_reverses_when_needed() {
        // Runs that meet end-to-end with mismatched orientation still join.
        let polys = vec![
            vec![pt(0.0, 0.0), pt(5.0, 5.0)],
            vec![pt(10.0, 10.0), pt(5.0, 5.0)], // shares its LAST point with run 0's last
        ];
        let joined = join_polylines(polys);
        assert_eq!(joined.len(), 1, "a reversed abutment still joins");
        let run = &joined[0];
        assert_eq!(run.first().unwrap(), &pt(0.0, 0.0));
        assert_eq!(run.last().unwrap(), &pt(10.0, 10.0));
    }

    #[test]
    fn t_junction_keeps_polylines_separate() {
        // Three runs meeting at (0,0) — a T-junction — must NOT be merged into a
        // single polyline (impossible); they stay separate, sharing the vertex.
        let polys = vec![
            vec![pt(0.0, 0.0), pt(-5.0, 0.0)],
            vec![pt(0.0, 0.0), pt(5.0, 0.0)],
            vec![pt(0.0, 0.0), pt(0.0, 5.0)],
        ];
        let joined = join_polylines(polys);
        assert_eq!(
            joined.len(),
            3,
            "a degree-3 junction keeps all three runs separate (shared vertex)"
        );
        // Every run still touches the junction.
        assert!(
            joined
                .iter()
                .all(|r| r.iter().any(|q| (*q).near_eq(pt(0.0, 0.0), JOIN_EPS)))
        );
    }
}
