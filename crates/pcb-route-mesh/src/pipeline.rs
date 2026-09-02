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
//! and stitches only the *fully successful* nets into copper. It remains an
//! implementation primitive; [`route_tuned`] is the single production routing
//! entry point used by the PCB engine.
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

use crate::copper::copper_obstacles;
use crate::crossing::assign_crossings;
use crate::detail::{self, CellRoute, CellRouteResult};
use crate::heuristics::{
    connection_crossing_pressures, connection_obstacle_pressure_um,
    connection_segment_obstacle_pressure_um, connection_span_um,
};
use crate::pathing::{GlobalRouteResult, global_route_with_mesh};
#[cfg(test)]
use crate::sequential::SequentialGridRouter;
use geom::JOIN_EPS;
#[cfg(test)]
use pcb_model::RoutingCapabilities;
use crate::deps::MeshDeps;
use pcb_model::{
    Budget, Connection, FailedNet, LayerRef, Obstacle, Point2, RouteQuality, RouteResult, RouteSolution,
    RoutingView, Trace, Via, ViaSpan,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// This engine's [`RouteResult::engine`] provenance tag.
pub const ENGINE: &str = "detailed";
const ADAPTIVE_GRID_RESCUE_ENGINE: &str = "adaptive-grid-rescue";
const ADAPTIVE_GRID_RIPUP_RESCUE_ENGINE: &str = "adaptive-grid-ripup-rescue";
const ADAPTIVE_RESCUE_PORTFOLIO_MAX_FAILED: usize = 8;
const ADAPTIVE_RIPUP_MAX_BLOCKERS: usize = 3;

// ── pipeline entry points ──────────────────────────────────────────────────────

/// Route `problem` through the full detailed pipeline (global → assign → cell →
/// stitch). Never panics; every stage's failures are folded into
/// [`RouteResult::failed`] with provenance in the reason, and a net that fails at
/// *any* stage contributes no copper to the returned solution. The result is
/// tagged with [`ENGINE`] (`"detailed"`).
pub fn route_detailed(deps: &MeshDeps, problem: &RoutingView) -> RouteResult {
    let mut result = route_detailed_with_global(deps, problem).0;
    postroute_cleanup(deps, problem, &mut result.solution);
    reconcile_connectivity(deps, problem, &mut result.solution, &mut result.failed);
    result
}

/// As [`route_detailed`], but also returns the negotiated global-routing result
/// that the detailed pipeline already computed. This lets callers surface
/// congestion diagnostics without rerunning global routing on the failure path.
pub fn route_detailed_with_global(deps: &MeshDeps, problem: &RoutingView) -> (RouteResult, GlobalRouteResult) {
    let mesh = crate::mesh::CapacityMesh::build(problem);
    let global: GlobalRouteResult = global_route_with_mesh(problem, &mesh);
    let route = route_detailed_from_global(deps, problem, &mesh, &global);
    (route, global)
}

fn route_detailed_from_global(
    deps: &MeshDeps,
    problem: &RoutingView,
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
        return RouteResult {
            solution: RouteSolution {
                traces: Vec::new(),
                vias: Vec::new(),
            },
            failed,
            engine: ENGINE.to_owned(),
        };
    }

    // 2. Crossing assignment.
    let assignment = assign_crossings(problem, mesh, &global.plan);
    for f in &assignment.failures {
        // An assignment failure is per-boundary or per-via; surface it as a
        // board-level fault with assign provenance (its detailed payload is the
        // honest record).
        let connection = assignment_failure_connection(f);
        let reason = format!("assign: {}", assignment_failure_reason(f));
        if connection.is_empty() {
            failed.push(FailedNet { connection, reason });
        } else {
            fail(&mut failed, &mut failed_names, &connection, reason);
        }
    }
    if !assignment.failures.is_empty() {
        return RouteResult {
            solution: RouteSolution {
                traces: Vec::new(),
                vias: Vec::new(),
            },
            failed,
            engine: ENGINE.to_owned(),
        };
    }

    // 3. Per-cell detailed routing.
    let cells: CellRouteResult = detail::route_cells(deps.drc, problem, mesh, &assignment);
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

/// The premium detailed [`Router`]: the pcb-route-mesh pipeline ([`route_detailed`])
/// behind the SDK trait, with its copper reconciled through the DRC oracle so the
/// returned result is geometry-clean.
///
/// It DECLINES (`can_route` = `false`) a board carrying a per-net inner-layer escape
/// assignment ([`RoutingView::escape_layers`]): the detailed engine free-mazes every
/// net over all layers and has no per-net layer restriction, so it would defeat the
/// structured escape (self-blocking, runtime-exploding) the assignment exists to
/// enable. On such a board only the grid router is offered, exactly as the old
/// `skip_detailed` flag intended.
#[derive(Debug, Clone, Copy, Default)]
#[cfg(test)]
struct NegotiatedMeshRouter;

#[cfg(test)]
impl NegotiatedMeshRouter {
    pub fn capabilities(&self) -> RoutingCapabilities {
        RoutingCapabilities {
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

    pub fn can_route(&self, problem: &RoutingView) -> bool {
        self.capabilities().can_route(problem)
    }
}

/// Diagnostic side data from the tuned routing algorithm.
#[derive(Debug, Clone)]
pub struct TunedRouteRun {
    pub result: RouteResult,
    /// Pass summaries in execution order. Each entry is captured after shared
    /// post-route cleanup and quality scoring.
    pub passes: Vec<RoutePassReport>,
    /// Negotiated global-routing result from the detailed primary candidate, when
    /// that candidate ran. Failure callers can use this instead of rerunning
    /// global routing just to recover congestion hotspots.
    pub global: Option<GlobalRouteResult>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutePassReport {
    pub engine: String,
    pub failed: Vec<FailedNet>,
    pub elapsed_ms: u128,
    pub fault_weight: usize,
    pub geometry_violations: usize,
    pub failed_nets: usize,
    pub vias: usize,
    pub wirelength: f64,
}

fn consider_candidate_recording(
    deps: &MeshDeps,
    problem: &RoutingView,
    best: &mut Option<(RouteResult, RouteQuality)>,
    mut result: RouteResult,
    passes: Option<&mut Vec<RoutePassReport>>,
    elapsed_ms: u128,
) -> bool {
    let (q, geometry_violations) = cleaned_route_quality(deps, problem, &mut result);
    if let Some(passes) = passes {
        passes.push(RoutePassReport {
            engine: result.engine.clone(),
            failed: result.failed.clone(),
            elapsed_ms,
            fault_weight: q.fault_weight,
            geometry_violations,
            failed_nets: q.failed_nets,
            vias: q.via_count,
            wirelength: q.wirelength,
        });
    }
    let stop = q.faults() == 0;
    *best = match best.take() {
        None => Some((result, q)),
        Some((bi, bq)) if better(problem, &bq, &q) => Some((bi, bq)),
        Some(_) => Some((result, q)),
    };
    stop
}

fn cleaned_route_quality(deps: &MeshDeps, problem: &RoutingView, result: &mut RouteResult) -> (RouteQuality, usize) {
    postroute_cleanup(deps, problem, &mut result.solution);
    let geometry_violations = deps.drc.geometry_violations(problem, &result.solution);
    (
        RouteQuality::of(problem, result, geometry_violations),
        geometry_violations,
    )
}

fn route_best_is_clean(best: &Option<(RouteResult, RouteQuality)>) -> bool {
    best.as_ref()
        .is_some_and(|(_, quality)| quality.faults() == 0)
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
fn better(_problem: &RoutingView, incumbent: &RouteQuality, challenger: &RouteQuality) -> bool {
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

/// The tuned routing algorithm selected by the production routing policy.
///
/// It performs one deterministic orthogonal grid pass followed by targeted
/// adaptive rip-up/rescue for any failed nets.  The rescue is a phase of the
/// same algorithm, not selection among independent routers.
pub fn route_tuned(deps: &MeshDeps, problem: &RoutingView) -> RouteResult {
    route_tuned_with_diagnostics(deps, problem).result
}

/// The premium routing leaf behind the [`pcb_model::PcbRouter`] contract, with
/// its design-rule oracle and grid sub-routers injected.
pub struct MeshRouter<'a> {
    deps: MeshDeps<'a>,
}

impl<'a> MeshRouter<'a> {
    /// A premium router composed from `deps`.
    pub const fn new(deps: MeshDeps<'a>) -> Self {
        MeshRouter { deps }
    }
}

impl pcb_model::PcbRouter for MeshRouter<'_> {
    fn name(&self) -> &'static str {
        ENGINE
    }

    /// An already-expired budget returns immediately with every connection
    /// reported failed rather than starting a search it cannot finish; the
    /// budget is otherwise handed on to the injected sub-routers.
    fn route(&self, view: &RoutingView, budget: &Budget) -> RouteResult {
        if budget.expired() {
            return RouteResult::abandoned(view, ENGINE, "routing budget expired before the pass");
        }
        let deps = MeshDeps {
            budget: *budget,
            ..self.deps
        };
        route_tuned(&deps, view)
    }
}

pub fn route_tuned_with_diagnostics(deps: &MeshDeps, problem: &RoutingView) -> TunedRouteRun {
    with_plane_fanout(deps, problem, route_tuned_inner)
}

fn route_tuned_inner(deps: &MeshDeps, problem: &RoutingView) -> TunedRouteRun {
    let started = Instant::now();
    let initial = deps.grid_seed.route(problem, &deps.budget);
    let mut best = None;
    let mut passes = Vec::new();
    let _ = consider_candidate_recording(deps, 
        problem,
        &mut best,
        initial,
        Some(&mut passes),
        started.elapsed().as_millis(),
    );
    if !route_best_is_clean(&best) {
        try_adaptive_grid_rescue(deps, problem, &mut best, &mut passes);
    }
    TunedRouteRun {
        result: best.expect("tuned grid pass populated a candidate").0,
        passes,
        global: None,
    }
}

/// Route only the detailed implementation primitive for regression diagnostics.
#[cfg(test)]
pub fn route_mesh_with_diagnostics(deps: &MeshDeps, problem: &RoutingView) -> TunedRouteRun {
    with_plane_fanout(deps, problem, route_mesh_with_diagnostics_inner)
}

#[cfg(test)]
fn route_mesh_with_diagnostics_inner(deps: &MeshDeps, problem: &RoutingView) -> TunedRouteRun {
    if !NegotiatedMeshRouter.can_route(problem) {
        return TunedRouteRun {
            result: RouteResult {
                solution: RouteSolution {
                    traces: vec![],
                    vias: vec![],
                },
                failed: vec![],
                engine: "none".to_owned(),
            },
            passes: vec![],
            global: None,
        };
    }

    let started = Instant::now();
    let (mut result, global) = route_detailed_with_global(deps, problem);
    reconcile_connectivity(deps, problem, &mut result.solution, &mut result.failed);
    let elapsed_ms = started.elapsed().as_millis();
    let mut best = None;
    let mut passes = Vec::new();
    let _ = consider_candidate_recording(deps, problem, &mut best, result, Some(&mut passes), elapsed_ms);
    try_adaptive_grid_rescue(deps, problem, &mut best, &mut passes);
    let result = best
        .map(|(result, _)| result)
        .expect("mesh candidate just populated best");
    TunedRouteRun {
        result,
        passes,
        global: Some(global),
    }
}

/// Route only the contextual sequential-grid engine, preserving one-pass attempt
/// diagnostics for callers that explicitly select this strategy.
#[cfg(test)]
pub fn route_sequential_with_diagnostics(deps: &MeshDeps, problem: &RoutingView) -> TunedRouteRun {
    with_plane_fanout(deps, problem, route_sequential_with_diagnostics_inner)
}

#[cfg(test)]
fn route_sequential_with_diagnostics_inner(deps: &MeshDeps, problem: &RoutingView) -> TunedRouteRun {
    let sequential = SequentialGridRouter;
    let started = Instant::now();
    let result = sequential.route(problem);
    let elapsed_ms = started.elapsed().as_millis();
    let geometry_violations = deps.drc.geometry_violations(problem, &result.solution);
    let quality = RouteQuality::of(problem, &result, geometry_violations);
    let passes = vec![RoutePassReport {
        engine: result.engine.clone(),
        failed: result.failed.clone(),
        elapsed_ms,
        fault_weight: quality.fault_weight,
        geometry_violations,
        failed_nets: quality.failed_nets,
        vias: quality.via_count,
        wirelength: quality.wirelength,
    }];
    TunedRouteRun {
        result,
        passes,
        global: None,
    }
}

/// Split plane-net connections out of `problem`: a net carried by one solid
/// full-board copper layer connects by ONE through-via per off-layer pad (the plane provides the
/// tree), so the trace engines never see it — a 100-pad power net costs
/// O(pads) instead of a board-wide multi-terminal search. Returns the
/// engines' subproblem and the fanout copper to merge into its solution, or
/// None when the problem has no routable plane connections.
fn plane_fanout(deps: &MeshDeps, problem: &RoutingView) -> Option<(RoutingView, RouteSolution)> {
    if problem.plane_nets.is_empty() {
        return None;
    }
    let (plane_conns, rest): (Vec<_>, Vec<_>) = problem
        .connections
        .iter()
        .cloned()
        .partition(|c| problem.plane_nets.contains_key(&c.name));
    if plane_conns.is_empty() {
        return None;
    }
    let mut sub = problem.clone();
    sub.connections = rest;
    // A via only lands where its barrel clears foreign copper. Fine-pitch pads
    // use a short minimum-width neck-down to a nearby site; every candidate is
    // accepted only when the ordinary geometry oracle clears the combined
    // stub, via, outline, holes, keepouts, and earlier fanout copper.
    let via_r = problem.via_diameter / 2.0;
    let via_fits = |at: Point2, net: &str| {
        problem.obstacles.iter().all(|ob| {
            if ob.connected_to.iter().any(|o| o == net) {
                return true;
            }
            let (hw, hh) = (ob.width / 2.0, ob.height / 2.0);
            let dx = (at.x - ob.center.x).abs() - hw;
            let dy = (at.y - ob.center.y).abs() - hh;
            let gap = match (dx > 0.0, dy > 0.0) {
                (true, true) => (dx * dx + dy * dy).sqrt(),
                (true, false) => dx,
                (false, true) => dy,
                (false, false) => f64::NEG_INFINITY,
            };
            // One extra routing channel beyond bare clearance: a barrel that
            // legally clears a fine-pitch pad by the design clearance still
            // blocks the only escape lane past that pad, walling the pad in
            // for every signal engine.
            gap >= via_r + problem.clearance + (problem.min_trace_width + problem.clearance)
        })
    };
    const SITE_STEP_MM: f64 = 0.1;
    const SITE_RADIUS_MM: f64 = 5.0;
    let offsets = octilinear_offsets((SITE_RADIUS_MM / SITE_STEP_MM) as i32);

    let mut fanout = RouteSolution {
        traces: Vec::new(),
        vias: Vec::new(),
    };
    let mut handled_plane_connection = false;
    for c in &plane_conns {
        let plane_layer = problem.plane_nets[&c.name];
        let reaches_plane = |pt: &pcb_model::RoutePoint| {
            problem.obstacles.iter().any(|ob| {
                // A same-net zone spans most of the board and is represented as
                // an obstacle too.  It proves that the plane exists, not that a
                // surface-mount terminal physically reaches that plane.
                ob.kind != "zone"
                    && ob.connected_to.iter().any(|owner| owner == &c.name)
                    && (ob
                        .layers
                        .iter()
                        .any(|layer| layer.index(problem.layer_count) == Some(plane_layer))
                        // Placement models through-hole pads by their two outer
                        // copper faces.  The plated barrel spans the intervening
                        // inner layers even though they are not enumerated.
                        || (ob.layers.iter().any(|layer| {
                            layer.index(problem.layer_count) == Some(0)
                        }) && ob.layers.iter().any(|layer| {
                            layer.index(problem.layer_count)
                                == problem.layer_count.checked_sub(1)
                        })))
                    && (pt.x - ob.center.x).abs() <= ob.width / 2.0 + geom::EPS
                    && (pt.y - ob.center.y).abs() <= ob.height / 2.0 + geom::EPS
            })
        };
        let mut planned = RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        };
        let mut failed_points = Vec::new();
        let mut existing_plane_anchor = None;
        for pt in &c.points_to_connect {
            if reaches_plane(pt) {
                existing_plane_anchor.get_or_insert_with(|| pt.point());
                continue;
            }
            let direct = pt.point();
            let candidates = std::iter::once(direct).chain(offsets.iter().map(|(dx, dy)| Point2 {
                x: direct.x + f64::from(*dx) * SITE_STEP_MM,
                y: direct.y + f64::from(*dy) * SITE_STEP_MM,
            }));
            let site = candidates.into_iter().find(|candidate| {
                if !via_fits(*candidate, &c.name) {
                    return false;
                }
                let mut proposed = fanout.clone();
                proposed.traces.extend(planned.traces.clone());
                proposed.vias.extend(planned.vias.clone());
                if candidate.dist(direct) > geom::EPS {
                    proposed.traces.push(Trace {
                        connection: c.name.clone(),
                        layer: pt.layer.clone(),
                        width: problem.min_trace_width,
                        path: vec![direct, *candidate],
                    });
                }
                proposed.vias.push(Via {
                    connection: c.name.clone(),
                    at: *candidate,
                    diameter: problem.via_diameter,
                    drill: problem.via_drill,
                    span: ViaSpan::Through,
                });
                deps.drc.geometry_violations(problem, &proposed) == 0
            });
            let Some(site) = site else {
                failed_points.push(pt.clone());
                continue;
            };
            if site.dist(direct) > geom::EPS {
                planned.traces.push(Trace {
                    connection: c.name.clone(),
                    layer: pt.layer.clone(),
                    width: problem.min_trace_width,
                    path: vec![direct, site],
                });
            }
            planned.vias.push(Via {
                connection: c.name.clone(),
                at: site,
                diameter: problem.via_diameter,
                drill: problem.via_drill,
                span: ViaSpan::Through,
            });
        }
        let plane_anchor = existing_plane_anchor.or_else(|| planned.vias.first().map(|via| via.at));
        if !failed_points.is_empty() && plane_anchor.is_none() {
            // Nothing on this connection reaches the plane, so the ordinary
            // router remains the only honest fallback.
            sub.connections.push(c.clone());
            continue;
        }
        if let (Some(anchor), Some(layer)) = (
            plane_anchor,
            failed_points.first().map(|point| point.layer.clone()),
        ) {
            // One crowded pad must not demote an otherwise valid plane net into
            // a board-wide routed tree. Route only the pads that could not take
            // a local stitching via to one already-stitched plane anchor.
            failed_points.push(pcb_model::RoutePoint {
                x: anchor.x,
                y: anchor.y,
                layer,
            });
            sub.connections.push(Connection {
                name: c.name.clone(),
                points_to_connect: failed_points,
            });
        }
        handled_plane_connection = true;
        fanout.traces.extend(planned.traces);
        fanout.vias.extend(planned.vias);
    }
    if !handled_plane_connection {
        return None;
    }
    // Signal engines must see both neck-downs and barrels.
    sub.obstacles.extend(copper_obstacles(problem, &fanout));
    Some((sub, fanout))
}

/// Neck wide nets down at fine-pitch pads before detailed routing.
///
/// The transformed terminal sits at the end of a short full-width landing, so
/// every router can use the requested net width without trying to place that
/// width between adjacent pads.  Escape copper is validated cumulatively and
/// exposed as obstacles to later routing stages.  A net is transformed only
/// when every narrow terminal on that net has a legal escape.
pub fn prepare_wide_terminal_escapes(deps: &MeshDeps, problem: &RoutingView) -> (RoutingView, RouteSolution) {
    const SITE_STEP_MM: f64 = 0.1;
    const SITE_RADIUS_MM: f64 = 1.5;
    const LANDING_MM: f64 = 0.1;

    let offsets = octilinear_offsets((SITE_RADIUS_MM / SITE_STEP_MM) as i32);

    let mut transformed = problem.clone();
    let mut escapes = RouteSolution {
        traces: Vec::new(),
        vias: Vec::new(),
    };

    for (conn_index, conn) in problem.connections.iter().enumerate() {
        let width = problem.net_width(&conn.name);
        if width <= problem.min_trace_width + geom::EPS
            || problem.plane_nets.contains_key(&conn.name)
        {
            continue;
        }

        let narrow: Vec<usize> = conn
            .points_to_connect
            .iter()
            .enumerate()
            .filter_map(|(index, terminal)| {
                problem
                    .obstacles
                    .iter()
                    .any(|ob| {
                        ob.kind.starts_with("pad:")
                            && ob.connected_to.iter().any(|net| net == &conn.name)
                            && ob.layers.iter().any(|layer| layer == &terminal.layer)
                            && (terminal.x - ob.center.x).abs() <= ob.width / 2.0 + geom::EPS
                            && (terminal.y - ob.center.y).abs() <= ob.height / 2.0 + geom::EPS
                            && ob.width.min(ob.height) < width - geom::EPS
                    })
                    .then_some(index)
            })
            .collect();
        if narrow.is_empty() {
            continue;
        }

        let mut planned = RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        };
        let mut replacements = Vec::with_capacity(narrow.len());
        let mut complete = true;
        for terminal_index in narrow {
            let terminal = &conn.points_to_connect[terminal_index];
            let origin = terminal.point();
            let found = offsets.iter().find_map(|(dx, dy)| {
                let vx = f64::from(*dx) * SITE_STEP_MM;
                let vy = f64::from(*dy) * SITE_STEP_MM;
                let distance = (vx * vx + vy * vy).sqrt();
                let ux = vx / distance;
                let uy = vy / distance;
                let landing_start = Point2 {
                    x: origin.x + vx,
                    y: origin.y + vy,
                };
                let endpoint = Point2 {
                    x: landing_start.x + ux * LANDING_MM,
                    y: landing_start.y + uy * LANDING_MM,
                };
                let mut proposed = escapes.clone();
                proposed.traces.extend(planned.traces.clone());
                proposed.traces.push(Trace {
                    connection: conn.name.clone(),
                    layer: terminal.layer.clone(),
                    width: problem.min_trace_width,
                    path: vec![origin, landing_start],
                });
                proposed.traces.push(Trace {
                    connection: conn.name.clone(),
                    layer: terminal.layer.clone(),
                    width,
                    path: vec![landing_start, endpoint],
                });
                (deps.drc.geometry_violations(problem, &proposed) == 0)
                    .then_some((landing_start, endpoint))
            });
            let Some((landing_start, endpoint)) = found else {
                complete = false;
                break;
            };
            planned.traces.push(Trace {
                connection: conn.name.clone(),
                layer: terminal.layer.clone(),
                width: problem.min_trace_width,
                path: vec![origin, landing_start],
            });
            planned.traces.push(Trace {
                connection: conn.name.clone(),
                layer: terminal.layer.clone(),
                width,
                path: vec![landing_start, endpoint],
            });
            replacements.push((terminal_index, endpoint));
        }
        if !complete {
            continue;
        }
        for (terminal_index, endpoint) in replacements {
            let terminal =
                &mut transformed.connections[conn_index].points_to_connect[terminal_index];
            terminal.x = endpoint.x;
            terminal.y = endpoint.y;
        }
        escapes.traces.extend(planned.traces);
    }

    transformed
        .obstacles
        .extend(copper_obstacles(problem, &escapes));
    (transformed, escapes)
}

fn with_plane_fanout(
    deps: &MeshDeps,
    problem: &RoutingView,
    route: impl Fn(&MeshDeps, &RoutingView) -> TunedRouteRun,
) -> TunedRouteRun {
    let Some((sub, fanout)) = plane_fanout(deps, problem) else {
        return route(deps, problem);
    };
    let mut run = route(deps, &sub);
    run.result.solution.traces.extend(fanout.traces);
    run.result.solution.vias.extend(fanout.vias);
    run
}

fn try_adaptive_grid_rescue(
    deps: &MeshDeps,
    problem: &RoutingView,
    best: &mut Option<(RouteResult, RouteQuality)>,
    passes: &mut Vec<RoutePassReport>,
) {
    let Some((result, q)) = best.as_ref() else {
        return;
    };
    if q.faults() == 0 || result.failed.is_empty() {
        return;
    }
    let started = Instant::now();
    let Some(candidate) = adaptive_grid_rescue(deps, problem, result) else {
        return;
    };
    let elapsed_ms = started.elapsed().as_millis();
    let _ = consider_candidate_recording(deps, problem, best, candidate, Some(passes), elapsed_ms);
}

/// Deterministic work budget for each adaptive-rescue phase. A fixed candidate
/// count keeps equal inputs byte-reproducible across fast and slow machines.
const ADAPTIVE_RESCUE_MAX_ORDERS: usize = 16;

fn adaptive_grid_rescue(deps: &MeshDeps, problem: &RoutingView, selected: &RouteResult) -> Option<RouteResult> {
    let failed_names: BTreeSet<String> = selected
        .failed
        .iter()
        .map(|f| f.connection.clone())
        .filter(|name| !name.is_empty())
        .collect();
    if failed_names.is_empty() {
        return None;
    }

    let mut best: Option<(RouteResult, RouteQuality)> = None;
    let base = adaptive_rescue_base(problem, selected, &failed_names);
    let orders = adaptive_rescue_orders(problem, &failed_names);
    let hopeless = hopeless_nets(deps, problem, &base.obstacles, &orders);
    for order in prune_orders(orders, &hopeless)
        .into_iter()
        .take(ADAPTIVE_RESCUE_MAX_ORDERS)
    {
        let Some(mut candidate) = adaptive_grid_rescue_order(deps, problem, selected, &base, &order)
        else {
            continue;
        };
        let (quality, _) = cleaned_route_quality(deps, problem, &mut candidate);
        let done = adaptive_rescue_candidate_can_short_circuit(&quality);
        best = match best.take() {
            None => Some((candidate, quality)),
            Some((incumbent, incumbent_quality))
                if better(problem, &incumbent_quality, &quality) =>
            {
                Some((incumbent, incumbent_quality))
            }
            Some(_) => Some((candidate, quality)),
        };
        if done {
            break;
        }
    }

    let selected_quality = RouteQuality::of(
        problem,
        selected,
        deps.drc.geometry_violations(problem, &selected.solution),
    );
    let ripup_orders = adaptive_ripup_rescue_orders(problem, selected, &failed_names);
    let floor = ripup_floor(problem, selected, &ripup_orders);
    let ripup_hopeless = hopeless_nets(deps, problem, &floor, &ripup_orders);
    for order in prune_orders(ripup_orders, &ripup_hopeless)
        .into_iter()
        .take(ADAPTIVE_RESCUE_MAX_ORDERS)
    {
        let Some(mut candidate) =
            adaptive_grid_ripup_rescue_order(deps, problem, selected, &failed_names, &order)
        else {
            continue;
        };
        let (quality, _) = cleaned_route_quality(deps, problem, &mut candidate);
        if !adaptive_ripup_candidate_reduces_failures(&selected_quality, &quality) {
            continue;
        }
        let done = adaptive_rescue_candidate_can_short_circuit(&quality);
        best = match best.take() {
            None => Some((candidate, quality)),
            Some((incumbent, incumbent_quality))
                if better(problem, &incumbent_quality, &quality) =>
            {
                Some((incumbent, incumbent_quality))
            }
            Some(_) => Some((candidate, quality)),
        };
        if done {
            break;
        }
    }

    if let Some((residual, residual_quality)) = best.clone()
        && residual_quality.faults() > 0
        && !residual.failed.is_empty()
    {
        let residual_failed_names: BTreeSet<String> = residual
            .failed
            .iter()
            .map(|f| f.connection.clone())
            .filter(|name| !name.is_empty())
            .collect();
        let residual_orders =
            adaptive_ripup_rescue_orders(problem, &residual, &residual_failed_names);
        let residual_floor = ripup_floor(problem, &residual, &residual_orders);
        let residual_hopeless = hopeless_nets(deps, problem, &residual_floor, &residual_orders);
        for order in prune_orders(residual_orders, &residual_hopeless)
            .into_iter()
            .take(ADAPTIVE_RESCUE_MAX_ORDERS)
        {
            let Some(mut candidate) = adaptive_grid_ripup_rescue_order(deps, 
                problem,
                &residual,
                &residual_failed_names,
                &order,
            ) else {
                continue;
            };
            let (quality, _) = cleaned_route_quality(deps, problem, &mut candidate);
            if !adaptive_ripup_candidate_reduces_failures(&residual_quality, &quality) {
                continue;
            }
            let done = adaptive_rescue_candidate_can_short_circuit(&quality);
            best = match best.take() {
                None => Some((candidate, quality)),
                Some((incumbent, incumbent_quality))
                    if better(problem, &incumbent_quality, &quality) =>
                {
                    Some((incumbent, incumbent_quality))
                }
                Some(_) => Some((candidate, quality)),
            };
            if done {
                break;
            }
        }
    }

    best.map(|(result, _)| result)
}

fn adaptive_ripup_candidate_reduces_failures(
    selected: &RouteQuality,
    candidate: &RouteQuality,
) -> bool {
    (candidate.faults(), candidate.failed_nets) < (selected.faults(), selected.failed_nets)
}

fn adaptive_rescue_candidate_can_short_circuit(quality: &RouteQuality) -> bool {
    quality.faults() == 0 && quality.via_count == 0
}

#[derive(Clone)]
struct AdaptiveRescueBase {
    solution: RouteSolution,
    failed: Vec<FailedNet>,
    obstacles: Vec<Obstacle>,
}

fn adaptive_rescue_base(
    problem: &RoutingView,
    selected: &RouteResult,
    failed_names: &BTreeSet<String>,
) -> AdaptiveRescueBase {
    let mut solution = selected.solution.clone();
    solution
        .traces
        .retain(|trace| !failed_names.contains(&trace.connection));
    solution
        .vias
        .retain(|via| !failed_names.contains(&via.connection));
    let obstacles = copper_obstacles(problem, &solution);
    AdaptiveRescueBase {
        solution,
        failed: selected.failed.clone(),
        obstacles,
    }
}

fn adaptive_grid_rescue_order(
    deps: &MeshDeps,
    problem: &RoutingView,
    selected: &RouteResult,
    base: &AdaptiveRescueBase,
    order: &[usize],
) -> Option<RouteResult> {
    let mut solution = base.solution.clone();
    let mut failed = base.failed.clone();
    let mut rescued = BTreeSet::new();
    let mut residual_obstacles = base.obstacles.clone();

    for &idx in order {
        let Some(conn) = problem.connections.get(idx) else {
            continue;
        };
        if conn.points_to_connect.len() < 2 {
            continue;
        }

        let subproblem =
            problem_with_single_connection_and_obstacles(problem, idx, &residual_obstacles);
        let routed = deps.grid.route(&subproblem, &deps.budget);
        if !routed.failed.is_empty() {
            continue;
        }

        let mut candidate = solution.clone();
        let routed_obstacles = copper_obstacles(problem, &routed.solution);
        candidate.traces.extend(routed.solution.traces);
        candidate.vias.extend(routed.solution.vias);
        let validation = problem_with_solution_connections(problem, &candidate);
        crate::via_cleanup::normalize_redundant_vias(deps.drc, &validation, &mut candidate);
        if !deps.drc.check(&validation, &candidate).is_empty() {
            continue;
        }

        solution = candidate;
        residual_obstacles.extend(routed_obstacles);
        rescued.insert(conn.name.clone());
        failed.retain(|f| f.connection != conn.name);
    }

    if rescued.is_empty() {
        return None;
    }

    Some(RouteResult {
        solution,
        failed,
        engine: format!("{}+{}", selected.engine, ADAPTIVE_GRID_RESCUE_ENGINE),
    })
}

fn adaptive_grid_ripup_rescue_order(
    deps: &MeshDeps,
    problem: &RoutingView,
    selected: &RouteResult,
    failed_names: &BTreeSet<String>,
    order: &[usize],
) -> Option<RouteResult> {
    let ripup_names: BTreeSet<String> = order
        .iter()
        .filter_map(|&idx| problem.connections.get(idx).map(|conn| conn.name.clone()))
        .collect();
    if ripup_names.is_empty() || ripup_names.iter().all(|name| failed_names.contains(name)) {
        return None;
    }

    let mut solution = selected.solution.clone();
    solution
        .traces
        .retain(|trace| !ripup_names.contains(&trace.connection));
    solution
        .vias
        .retain(|via| !ripup_names.contains(&via.connection));
    let mut failed = selected.failed.clone();
    let mut residual_obstacles = copper_obstacles(problem, &solution);
    let mut routed_any = false;

    for &idx in order {
        let Some(conn) = problem.connections.get(idx) else {
            continue;
        };
        if conn.points_to_connect.len() < 2 {
            continue;
        }

        let subproblem =
            problem_with_single_connection_and_obstacles(problem, idx, &residual_obstacles);
        let routed = deps.grid.route(&subproblem, &deps.budget);
        if !routed.failed.is_empty() {
            if !failed.iter().any(|f| f.connection == conn.name) {
                failed.push(FailedNet {
                    connection: conn.name.clone(),
                    reason: "adaptive rip-up rescue could not reroute displaced net".to_owned(),
                });
            }
            continue;
        }

        let mut candidate = solution.clone();
        let routed_obstacles = copper_obstacles(problem, &routed.solution);
        candidate.traces.extend(routed.solution.traces);
        candidate.vias.extend(routed.solution.vias);
        let validation = problem_with_solution_connections(problem, &candidate);
        crate::via_cleanup::normalize_redundant_vias(deps.drc, &validation, &mut candidate);
        if !deps.drc.check(&validation, &candidate).is_empty() {
            if !failed.iter().any(|f| f.connection == conn.name) {
                failed.push(FailedNet {
                    connection: conn.name.clone(),
                    reason: "adaptive rip-up rescue reroute conflicted with fixed copper"
                        .to_owned(),
                });
            }
            continue;
        }

        solution = candidate;
        residual_obstacles.extend(routed_obstacles);
        failed.retain(|f| f.connection != conn.name);
        routed_any = true;
    }

    if !routed_any {
        return None;
    }

    Some(RouteResult {
        solution,
        failed,
        engine: format!("{}+{}", selected.engine, ADAPTIVE_GRID_RIPUP_RESCUE_ENGINE),
    })
}

/// The failed nets no attempt in a rescue phase can save.
///
/// Every attempt routes one net against `floor` plus whatever copper the nets
/// before it in that attempt laid down, so the obstacle set an attempt presents
/// only ever grows from `floor`. A net the grid router cannot route against
/// `floor` is therefore unroutable in *every* order of that phase, and probing
/// it once per order is exactly where a board that cannot be finished spends
/// its time — `mcu-board` burnt 46 of its 62 routing seconds inside A* searches
/// that had already been proven hopeless.
///
/// Order-independent and clock-free, so the emitted board is unchanged.
fn hopeless_nets(
    deps: &MeshDeps,
    problem: &RoutingView,
    floor: &[Obstacle],
    orders: &[Vec<usize>],
) -> BTreeSet<usize> {
    let mut probed = BTreeSet::new();
    let mut hopeless = BTreeSet::new();
    for idx in orders.iter().flatten().copied() {
        if !probed.insert(idx) {
            continue;
        }
        let Some(conn) = problem.connections.get(idx) else {
            continue;
        };
        if conn.points_to_connect.len() < 2 {
            continue;
        }
        let sub = problem_with_single_connection_and_obstacles(problem, idx, floor);
        if !deps.grid.route(&sub, &deps.budget).failed.is_empty() {
            hopeless.insert(idx);
        }
    }
    hopeless
}

/// Drop the hopeless nets from every order, then drop the orders that collapse
/// to nothing or to a duplicate of an earlier one — different orders of the same
/// nets are only worth trying while the nets differ.
fn prune_orders(orders: Vec<Vec<usize>>, hopeless: &BTreeSet<usize>) -> Vec<Vec<usize>> {
    let mut pruned = Vec::new();
    for order in orders {
        let kept: Vec<usize> = order
            .into_iter()
            .filter(|idx| !hopeless.contains(idx))
            .collect();
        push_adaptive_rescue_order(&mut pruned, kept);
    }
    pruned
}

/// The copper `selected` keeps once every net any rip-up order might remove is
/// removed — the emptiest board those orders can present.
fn ripup_floor(problem: &RoutingView, selected: &RouteResult, orders: &[Vec<usize>]) -> Vec<Obstacle> {
    let removable: BTreeSet<&str> = orders
        .iter()
        .flatten()
        .filter_map(|&idx| problem.connections.get(idx).map(|c| c.name.as_str()))
        .collect();
    let mut solution = selected.solution.clone();
    solution
        .traces
        .retain(|t| !removable.contains(t.connection.as_str()));
    solution
        .vias
        .retain(|v| !removable.contains(v.connection.as_str()));
    copper_obstacles(problem, &solution)
}

fn adaptive_rescue_orders(
    problem: &RoutingView,
    failed_names: &BTreeSet<String>,
) -> Vec<Vec<usize>> {
    let metrics = adaptive_rescue_order_metrics(problem);
    let pressure_first = adaptive_rescue_order_with_metrics(problem, failed_names, &metrics);
    if failed_names.len() > ADAPTIVE_RESCUE_PORTFOLIO_MAX_FAILED {
        return vec![pressure_first];
    }

    let mut orders = Vec::new();
    push_adaptive_rescue_order(&mut orders, pressure_first.clone());

    let original: Vec<usize> = problem
        .connections
        .iter()
        .enumerate()
        .filter_map(|(idx, conn)| failed_names.contains(&conn.name).then_some(idx))
        .collect();
    push_adaptive_rescue_order(&mut orders, original);
    if orders.is_empty() {
        return orders;
    }

    let mut reverse_pressure = pressure_first;
    reverse_pressure.reverse();
    push_adaptive_rescue_order(&mut orders, reverse_pressure);

    let mut obstacle_first: Vec<usize> = orders[0].clone();
    obstacle_first.sort_by(|&a, &b| {
        metrics[b]
            .obstacle_pressure_um
            .cmp(&metrics[a].obstacle_pressure_um)
            .then_with(|| metrics[b].pin_count.cmp(&metrics[a].pin_count))
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_adaptive_rescue_order(&mut orders, obstacle_first);

    let mut segment_obstacle_first: Vec<usize> = orders[0].clone();
    segment_obstacle_first.sort_by(|&a, &b| {
        metrics[b]
            .segment_obstacle_pressure_um
            .cmp(&metrics[a].segment_obstacle_pressure_um)
            .then_with(|| {
                metrics[b]
                    .obstacle_pressure_um
                    .cmp(&metrics[a].obstacle_pressure_um)
            })
            .then_with(|| metrics[b].pin_count.cmp(&metrics[a].pin_count))
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_adaptive_rescue_order(&mut orders, segment_obstacle_first);

    let mut short_span_first: Vec<usize> = orders[0].clone();
    short_span_first.sort_by(|&a, &b| {
        metrics[a]
            .span_um
            .cmp(&metrics[b].span_um)
            .then_with(|| metrics[a].pin_count.cmp(&metrics[b].pin_count))
            .then_with(|| {
                metrics[a]
                    .obstacle_pressure_um
                    .cmp(&metrics[b].obstacle_pressure_um)
            })
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_adaptive_rescue_order(&mut orders, short_span_first);

    let mut many_pins_first: Vec<usize> = orders[0].clone();
    many_pins_first.sort_by(|&a, &b| {
        metrics[b]
            .pin_count
            .cmp(&metrics[a].pin_count)
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_adaptive_rescue_order(&mut orders, many_pins_first);

    orders
}

fn adaptive_ripup_rescue_orders(
    problem: &RoutingView,
    selected: &RouteResult,
    failed_names: &BTreeSet<String>,
) -> Vec<Vec<usize>> {
    if failed_names.is_empty() || failed_names.len() > ADAPTIVE_RESCUE_PORTFOLIO_MAX_FAILED {
        return Vec::new();
    }

    let blockers = adaptive_ripup_blocker_scores(problem, selected, failed_names);
    if blockers.is_empty() {
        return Vec::new();
    }

    let metrics = adaptive_rescue_order_metrics(problem);
    let failed_first = adaptive_rescue_order_with_metrics(problem, failed_names, &metrics);
    let blocker_scores: BTreeMap<String, u64> = blockers.into_iter().collect();
    let blocker_names: BTreeSet<String> = blocker_scores.keys().cloned().collect();
    let mut blocker_order: Vec<usize> = problem
        .connections
        .iter()
        .enumerate()
        .filter_map(|(idx, conn)| blocker_names.contains(&conn.name).then_some(idx))
        .collect();
    blocker_order.sort_by(|&a, &b| {
        blocker_scores[&problem.connections[b].name]
            .cmp(&blocker_scores[&problem.connections[a].name])
            .then_with(|| {
                metrics[b]
                    .segment_obstacle_pressure_um
                    .cmp(&metrics[a].segment_obstacle_pressure_um)
            })
            .then_with(|| {
                metrics[b]
                    .obstacle_pressure_um
                    .cmp(&metrics[a].obstacle_pressure_um)
            })
            .then_with(|| metrics[b].pin_count.cmp(&metrics[a].pin_count))
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });

    let mut orders = Vec::new();
    let mut pressure_first = failed_first;
    pressure_first.extend(blocker_order.iter().copied());
    push_adaptive_rescue_order(&mut orders, pressure_first);

    let original: Vec<usize> = problem
        .connections
        .iter()
        .enumerate()
        .filter_map(|(idx, conn)| {
            (failed_names.contains(&conn.name) || blocker_names.contains(&conn.name)).then_some(idx)
        })
        .collect();
    push_adaptive_rescue_order(&mut orders, original);

    orders
}

fn adaptive_ripup_blocker_scores(
    problem: &RoutingView,
    selected: &RouteResult,
    failed_names: &BTreeSet<String>,
) -> Vec<(String, u64)> {
    let failed_segments = failed_corridor_segments(problem, failed_names);
    let mut failed_terminals = failed_terminal_points(problem, failed_names);
    failed_terminals.extend(failed_reason_terminal_points(
        problem,
        failed_names,
        &selected.failed,
    ));
    if failed_segments.is_empty() && failed_terminals.is_empty() {
        return Vec::new();
    }

    let known_connections: BTreeSet<&str> = problem
        .connections
        .iter()
        .map(|conn| conn.name.as_str())
        .collect();
    let mut scores: BTreeMap<String, u64> = BTreeMap::new();
    for trace in &selected.solution.traces {
        if failed_names.contains(&trace.connection)
            || !known_connections.contains(trace.connection.as_str())
        {
            continue;
        }
        for window in trace.path.windows(2) {
            let trace_segment = geom::Segment::new(window[0], window[1]);
            for failed in &failed_segments {
                if failed.layer != trace.layer {
                    continue;
                }
                let clearance = problem.clearance + failed.width / 2.0 + trace.width / 2.0;
                let dist = failed.segment.dist_to_segment(trace_segment);
                if dist <= clearance + geom::EPS {
                    let trace_um = (window[0].dist(window[1]) * 1000.0).round().max(0.0) as u64;
                    let closeness_um = ((clearance - dist).max(0.0) * 1000.0).round() as u64;
                    *scores.entry(trace.connection.clone()).or_default() +=
                        1_000_000 + trace_um + closeness_um;
                }
            }
        }
        for failed in &failed_terminals {
            if failed.layer != trace.layer {
                continue;
            }
            let clearance = problem.clearance + failed.width / 2.0 + trace.width / 2.0;
            let radius = terminal_relief_radius(problem, clearance);
            for window in trace.path.windows(2) {
                let trace_segment = geom::Segment::new(window[0], window[1]);
                let dist = trace_segment.dist_to_point(failed.point);
                if dist <= radius + geom::EPS {
                    let trace_um = (window[0].dist(window[1]) * 1000.0).round().max(0.0) as u64;
                    let closeness_um = ((radius - dist).max(0.0) * 1000.0).round() as u64;
                    *scores.entry(trace.connection.clone()).or_default() +=
                        1_500_000 + trace_um + closeness_um;
                }
            }
        }
    }
    for via in &selected.solution.vias {
        if failed_names.contains(&via.connection)
            || !known_connections.contains(via.connection.as_str())
        {
            continue;
        }
        let via_obstacles = copper_obstacles(
            problem,
            &RouteSolution {
                traces: Vec::new(),
                vias: vec![via.clone()],
            },
        );
        for via_obstacle in via_obstacles {
            for failed in &failed_segments {
                if !via_obstacle
                    .layers
                    .iter()
                    .any(|layer| layer == &failed.layer)
                {
                    continue;
                }
                let clearance = problem.clearance + failed.width / 2.0 + via_obstacle.width / 2.0;
                let dist = failed.segment.dist_to_point(via_obstacle.center);
                if dist <= clearance + geom::EPS {
                    let closeness_um = ((clearance - dist).max(0.0) * 1000.0).round() as u64;
                    *scores.entry(via.connection.clone()).or_default() += 2_000_000 + closeness_um;
                }
            }
            for failed in &failed_terminals {
                if !via_obstacle
                    .layers
                    .iter()
                    .any(|layer| layer == &failed.layer)
                {
                    continue;
                }
                let clearance = problem.clearance + failed.width / 2.0 + via_obstacle.width / 2.0;
                let radius = terminal_relief_radius(problem, clearance);
                let dist = failed.point.dist(via_obstacle.center);
                if dist <= radius + geom::EPS {
                    let closeness_um = ((radius - dist).max(0.0) * 1000.0).round() as u64;
                    *scores.entry(via.connection.clone()).or_default() += 2_500_000 + closeness_um;
                }
            }
        }
    }

    let mut blockers: Vec<(String, u64)> = scores.into_iter().collect();
    blockers.sort_by(|(a_name, a_score), (b_name, b_score)| {
        b_score.cmp(a_score).then_with(|| a_name.cmp(b_name))
    });
    blockers
        .into_iter()
        .take(ADAPTIVE_RIPUP_MAX_BLOCKERS)
        .collect()
}

fn terminal_relief_radius(problem: &RoutingView, clearance: f64) -> f64 {
    clearance + problem.min_trace_width.max(problem.clearance) * 2.0
}

struct FailedCorridorSegment {
    segment: geom::Segment,
    layer: LayerRef,
    width: f64,
}

struct FailedTerminalPoint {
    point: Point2,
    layer: LayerRef,
    width: f64,
}

fn failed_terminal_points(
    problem: &RoutingView,
    failed_names: &BTreeSet<String>,
) -> Vec<FailedTerminalPoint> {
    let mut out = Vec::new();
    for conn in &problem.connections {
        if !failed_names.contains(&conn.name) {
            continue;
        }
        let width = problem.net_width(&conn.name);
        for pt in &conn.points_to_connect {
            out.push(FailedTerminalPoint {
                point: pt.point(),
                layer: pt.layer.clone(),
                width,
            });
        }
    }
    out
}

fn failed_reason_terminal_points(
    problem: &RoutingView,
    failed_names: &BTreeSet<String>,
    failures: &[FailedNet],
) -> Vec<FailedTerminalPoint> {
    let layers = copper_layers(problem.layer_count);
    let mut out = Vec::new();
    for failure in failures {
        if !failed_names.contains(&failure.connection) {
            continue;
        }
        let Some(point) = parse_failed_terminal_point(&failure.reason) else {
            continue;
        };
        let width = problem.net_width(&failure.connection);
        for layer in &layers {
            out.push(FailedTerminalPoint {
                point,
                layer: layer.clone(),
                width,
            });
        }
    }
    out
}

fn parse_failed_terminal_point(reason: &str) -> Option<Point2> {
    let (_, after) = reason.split_once(" at (")?;
    let (coords, _) = after.split_once(')')?;
    let (x, y) = coords.split_once(',')?;
    Some(Point2 {
        x: x.trim().parse().ok()?,
        y: y.trim().parse().ok()?,
    })
}

fn copper_layers(layer_count: u32) -> Vec<LayerRef> {
    let layer_count = layer_count.max(1);
    let mut layers = Vec::with_capacity(layer_count as usize);
    for idx in 0..layer_count {
        layers.push(if idx == 0 {
            LayerRef::top()
        } else if idx + 1 == layer_count {
            LayerRef::bottom()
        } else {
            LayerRef(format!("inner{idx}"))
        });
    }
    layers
}

fn failed_corridor_segments(
    problem: &RoutingView,
    failed_names: &BTreeSet<String>,
) -> Vec<FailedCorridorSegment> {
    let mut out = Vec::new();
    for conn in &problem.connections {
        if !failed_names.contains(&conn.name) || conn.points_to_connect.len() < 2 {
            continue;
        }
        let width = problem.net_width(&conn.name);
        for (a_idx, b_idx) in failed_corridor_tree_pairs(conn) {
            let a = &conn.points_to_connect[a_idx];
            let b = &conn.points_to_connect[b_idx];
            if a.layer == b.layer {
                out.push(FailedCorridorSegment {
                    segment: geom::Segment::new(a.point(), b.point()),
                    layer: a.layer.clone(),
                    width,
                });
            } else {
                let segment = geom::Segment::new(a.point(), b.point());
                out.push(FailedCorridorSegment {
                    segment,
                    layer: a.layer.clone(),
                    width,
                });
                out.push(FailedCorridorSegment {
                    segment,
                    layer: b.layer.clone(),
                    width,
                });
            }
        }
    }
    out
}

fn failed_corridor_tree_pairs(conn: &pcb_model::Connection) -> Vec<(usize, usize)> {
    match conn.points_to_connect.as_slice() {
        [] | [_] => Vec::new(),
        [_, _] => vec![(0, 1)],
        points => {
            let positions: Vec<Point2> = points.iter().map(|pt| pt.point()).collect();
            let mut pairs = Vec::with_capacity(points.len().saturating_sub(1));
            let mut in_tree = vec![false; points.len()];
            in_tree[0] = true;
            for _ in 1..points.len() {
                let mut best: Option<(usize, usize)> = None;
                for (ai, &ai_in_tree) in in_tree.iter().enumerate() {
                    if !ai_in_tree {
                        continue;
                    }
                    for (bi, &bi_in_tree) in in_tree.iter().enumerate() {
                        if bi_in_tree {
                            continue;
                        }
                        let replace = best.is_none_or(|(old_a, old_b)| {
                            failed_corridor_tree_pair_better(conn, &positions, ai, bi, old_a, old_b)
                        });
                        if replace {
                            best = Some((ai, bi));
                        }
                    }
                }
                let Some((ai, bi)) = best else {
                    break;
                };
                in_tree[bi] = true;
                pairs.push((ai, bi));
            }
            pairs
        }
    }
}

fn failed_corridor_tree_pair_better(
    conn: &pcb_model::Connection,
    positions: &[Point2],
    a: usize,
    b: usize,
    old_a: usize,
    old_b: usize,
) -> bool {
    let dist = positions[a].dist(positions[b]);
    let old_dist = positions[old_a].dist(positions[old_b]);
    if dist < old_dist - 1e-9 {
        return true;
    }
    if (dist - old_dist).abs() > 1e-9 {
        return false;
    }

    let layer_change = failed_corridor_pair_requires_layer_change(conn, a, b);
    let old_layer_change = failed_corridor_pair_requires_layer_change(conn, old_a, old_b);
    if layer_change != old_layer_change {
        return !layer_change;
    }

    (a, b) < (old_a, old_b)
}

fn failed_corridor_pair_requires_layer_change(
    conn: &pcb_model::Connection,
    a: usize,
    b: usize,
) -> bool {
    conn.points_to_connect[a].layer != conn.points_to_connect[b].layer
}

fn push_adaptive_rescue_order(orders: &mut Vec<Vec<usize>>, order: Vec<usize>) {
    if !order.is_empty() && !orders.iter().any(|existing| existing == &order) {
        orders.push(order);
    }
}

#[cfg(test)]
fn problem_with_single_connection_and_copper(
    problem: &RoutingView,
    idx: usize,
    solution: &RouteSolution,
) -> RoutingView {
    problem_with_single_connection_and_obstacles(problem, idx, &copper_obstacles(problem, solution))
}

fn problem_with_single_connection_and_obstacles(
    problem: &RoutingView,
    idx: usize,
    obstacles: &[Obstacle],
) -> RoutingView {
    let mut out = problem.clone();
    out.connections = problem
        .connections
        .get(idx)
        .cloned()
        .into_iter()
        .collect::<Vec<_>>();
    out.obstacles.extend(obstacles.iter().cloned());
    out
}

fn problem_with_solution_connections(
    problem: &RoutingView,
    solution: &RouteSolution,
) -> RoutingView {
    let names: BTreeSet<&str> = solution
        .traces
        .iter()
        .map(|trace| trace.connection.as_str())
        .chain(solution.vias.iter().map(|via| via.connection.as_str()))
        .collect();
    let mut out = problem.clone();
    out.connections = problem
        .connections
        .iter()
        .filter(|conn| names.contains(conn.name.as_str()))
        .cloned()
        .collect();
    out
}

#[cfg(test)]
fn adaptive_rescue_order(problem: &RoutingView, failed_names: &BTreeSet<String>) -> Vec<usize> {
    let metrics = adaptive_rescue_order_metrics(problem);
    adaptive_rescue_order_with_metrics(problem, failed_names, &metrics)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AdaptiveOrderMetric {
    crossing_pressure: usize,
    segment_obstacle_pressure_um: u64,
    obstacle_pressure_um: u64,
    pin_count: usize,
    span_um: u64,
}

fn adaptive_rescue_order_metrics(problem: &RoutingView) -> Vec<AdaptiveOrderMetric> {
    let crossing_pressures = connection_crossing_pressures(problem);
    problem
        .connections
        .iter()
        .enumerate()
        .map(|(idx, conn)| AdaptiveOrderMetric {
            crossing_pressure: crossing_pressures.get(idx).copied().unwrap_or(0),
            segment_obstacle_pressure_um: connection_segment_obstacle_pressure_um(problem, conn),
            obstacle_pressure_um: connection_obstacle_pressure_um(problem, conn),
            pin_count: conn.points_to_connect.len(),
            span_um: connection_span_um(conn),
        })
        .collect()
}

fn adaptive_rescue_order_with_metrics(
    problem: &RoutingView,
    failed_names: &BTreeSet<String>,
    metrics: &[AdaptiveOrderMetric],
) -> Vec<usize> {
    let mut order: Vec<usize> = problem
        .connections
        .iter()
        .enumerate()
        .filter_map(|(idx, conn)| failed_names.contains(&conn.name).then_some(idx))
        .collect();
    order.sort_by(|&a, &b| {
        metrics[b]
            .crossing_pressure
            .cmp(&metrics[a].crossing_pressure)
            .then_with(|| {
                metrics[b]
                    .segment_obstacle_pressure_um
                    .cmp(&metrics[a].segment_obstacle_pressure_um)
            })
            .then_with(|| {
                metrics[b]
                    .obstacle_pressure_um
                    .cmp(&metrics[a].obstacle_pressure_um)
            })
            .then_with(|| metrics[b].pin_count.cmp(&metrics[a].pin_count))
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    order
}

/// Freerouter-style postroute cleanup for selected copper: drop redundant vias,
/// merge degree-2 same-net trace fragments, then pull local trace corners tight
/// when the exact DRC/connectivity oracle says the shortcut is equivalent.
pub fn postroute_cleanup(deps: &MeshDeps, problem: &RoutingView, solution: &mut RouteSolution) {
    drop_redundant_thruhole_vias(problem, solution);
    crate::via_cleanup::normalize_redundant_vias(deps.drc, problem, solution);
    drop_dangling_vias(deps, problem, solution);
    drop_duplicate_traces(solution);
    simplify_trace_paths(deps, problem, solution);
    drop_trace_spurs(deps, problem, solution);
    drop_duplicate_traces(solution);
    drop_covered_collinear_traces(deps, problem, solution);
    merge_touching_traces(deps, problem, solution);
    pcb_grid::tidy::straighten_traces(deps.drc, problem, solution, pcb_grid::tidy::Corners::Octilinear);
    drop_trace_spurs(deps, problem, solution);
    simplify_trace_paths(deps, problem, solution);
    drop_duplicate_traces(solution);
    drop_covered_collinear_traces(deps, problem, solution);
}

/// Drop a via that sits inside a SAME-NET through-hole pad: the pad's barrel
/// already spans every copper layer, so a via on it is a redundant layer change —
/// and its drill collides with the pad's (a KiCAD `hole_to_hole` defect). The
/// trace stays connected THROUGH the pad (both trace ends land inside it, and the
/// pad bridges the layers). A pad is through-hole when its obstacle reaches both
/// the top and bottom copper layers.
///
/// This removal is unguarded, so containment is judged on the pad's *proven*
/// capsule, never its bounding box: a via in a round pad's box corner sits on
/// bare laminate, and dropping it silently opens the layer change.
fn drop_redundant_thruhole_vias(problem: &RoutingView, solution: &mut RouteSolution) {
    let (top, bottom) = (LayerRef::top(), LayerRef::bottom());
    solution.vias.retain(|v| {
        !problem.obstacles.iter().any(|ob| {
            ob.connected_to.contains(&v.connection)
                && ob.layers.contains(&top)
                && ob.layers.contains(&bottom)
                && ob.proven_capsule().dist_to_point(v.at) <= 0.0
        })
    });
}

/// Drop same-net vias that do not actually bridge copper on at least two layers.
/// Detailed stitching already suppresses these; this applies the same cleanup to
/// fast-path and fallback router output. Every removal is lint-guarded so a via
/// anchor that preserves connectivity or DRC is kept.
fn drop_dangling_vias(deps: &MeshDeps, problem: &RoutingView, solution: &mut RouteSolution) {
    let mut baseline = deps.drc.check(problem, solution);
    let mut idx = 0usize;
    while idx < solution.vias.len() {
        if via_connected_layers(problem, solution, &solution.vias[idx]).len() >= 2 {
            idx += 1;
            continue;
        }

        let mut candidate = solution.clone();
        candidate.vias.remove(idx);
        let findings = deps.drc.check(problem, &candidate);
        if !pcb_grid::tidy::introduces_new_findings(&baseline, &findings)
            && candidate.metrics().via_count < solution.metrics().via_count
        {
            *solution = candidate;
            baseline = findings;
        } else {
            idx += 1;
        }
    }
}

fn via_connected_layers(
    problem: &RoutingView,
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
fn simplify_trace_paths(deps: &MeshDeps, problem: &RoutingView, solution: &mut RouteSolution) {
    let mut baseline = deps.drc.check(problem, solution);
    for ti in 0..solution.traces.len() {
        let simplified = geom::Polyline::new(solution.traces[ti].path.clone())
            .simplify()
            .into_points();
        if simplified.len() < 2 || simplified.len() >= solution.traces[ti].path.len() {
            continue;
        }
        let mut candidate = solution.clone();
        candidate.traces[ti].path = simplified;
        let findings = deps.drc.check(problem, &candidate);
        if findings == baseline {
            *solution = candidate;
            baseline = findings;
        }
    }
}

/// Remove closed subpaths inside a trace: `... P -> ... -> P ...` carries a spur
/// loop that adds copper but no connectivity. Every candidate goes through the
/// full lint report, so via anchors, terminal reachability, and DRC invariants
/// remain protected; a loop may also be accepted when deleting it removes an
/// existing detour-caused lint finding without adding any new one.
fn drop_trace_spurs(deps: &MeshDeps, problem: &RoutingView, solution: &mut RouteSolution) {
    let mut baseline = deps.drc.check(problem, solution);
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
                    let findings = deps.drc.check(problem, &candidate);
                    lint_budget -= 1;
                    if !pcb_grid::tidy::introduces_new_findings(&baseline, &findings) {
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
fn drop_covered_collinear_traces(deps: &MeshDeps, problem: &RoutingView, solution: &mut RouteSolution) {
    let mut baseline = deps.drc.check(problem, solution);
    let mut idx = 0usize;
    while idx < solution.traces.len() {
        if trace_is_covered_by_another(solution, idx) {
            let mut candidate = solution.clone();
            candidate.traces.remove(idx);
            let findings = deps.drc.check(problem, &candidate);
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
fn merge_touching_traces(deps: &MeshDeps, problem: &RoutingView, solution: &mut RouteSolution) {
    loop {
        let baseline = deps.drc.check(problem, solution);
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
                && deps.drc.check(problem, &candidate) == baseline
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
/// the trace without introducing new lint findings, which protects via anchors,
/// T-junctions, clearance, board-edge, and connectivity invariants while allowing
/// cleanup to remove an existing detour-caused finding.
/// Escape-site offsets: the eight octilinear rays out to `steps`, nearest first.
///
/// A pad's escape stub is one straight or 45-degree leg to its site, so picking
/// sites off these rays makes that stub octilinear by construction instead of
/// leaving it at whatever angle a full square scan happened to land on. It also
/// cuts the DRC-gated search from `(2n+1)²` sites to `8n`.
fn octilinear_offsets(steps: i32) -> Vec<(i32, i32)> {
    const DIRS: [(i32, i32); 8] = [
        (1, 0),
        (1, 1),
        (0, 1),
        (-1, 1),
        (-1, 0),
        (-1, -1),
        (0, -1),
        (1, -1),
    ];
    let mut offsets: Vec<(i32, i32)> = (1..=steps)
        .flat_map(|r| DIRS.map(|(ux, uy)| (ux * r, uy * r)))
        .collect();
    offsets.sort_by_key(|(dx, dy)| (dx * dx + dy * dy, *dx, *dy));
    offsets
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
/// surviving copper is fully DRC-clean, so `route_tuned`'s comparison ranks a
/// silent violation or phantom-route below an engine that cleanly connected
/// fewer nets, and the engine never ships copper that fails DRC.
fn reconcile_connectivity(
    deps: &MeshDeps,
    problem: &RoutingView,
    solution: &mut RouteSolution,
    failed: &mut Vec<FailedNet>,
) {
    let mut broken = deps.drc.drop_violating_copper(problem, solution);
    broken.extend(deps.drc.drop_unconnected_copper(problem, solution));
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
    problem: &RoutingView,
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
            nc.vias.push(v.at);
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
                    layer: pcb_model::LayerRef(layer.clone()),
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
        // naive fallback in `route_tuned` catch any over-drop.
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
    if failed_names.contains(connection) {
        return;
    }
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
    use pcb_model::Drc as _;
    use pcb_drc::StandardDrc;
    use pcb_route_grid::router::{GridRouter, GridSinglePassRouter};

    /// The production leaves, wired together for the pipeline under test.
    const DRC: StandardDrc = StandardDrc;
    const GRID: GridRouter<'static> = GridRouter::new(&DRC);
    const GRID_SEED: GridSinglePassRouter<'static> = GridSinglePassRouter::new(&DRC);
    const DEPS: MeshDeps<'static> = MeshDeps {
        drc: &DRC,
        grid: &GRID,
        grid_seed: &GRID_SEED,
        budget: Budget::unlimited(),
    };

    use super::*;
    
    use std::path::Path;

    fn load(name: &str) -> RoutingView {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name);
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
    }

    #[test]
    fn fail_helper_dedupes_repeated_net_failures() {
        let mut failed = Vec::new();
        let mut failed_names = std::collections::BTreeSet::new();

        fail(&mut failed, &mut failed_names, "S1", "cell 1".to_owned());
        fail(&mut failed, &mut failed_names, "S1", "cell 2".to_owned());

        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].connection, "S1");
        assert_eq!(failed[0].reason, "cell 1");
    }

    fn simple_two_point_problem() -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "N".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
        }
    }

    #[test]
    fn plane_fanout_uses_through_hole_pad_without_redundant_via() {
        let mut p = simple_two_point_problem();
        p.layer_count = 4;
        p.plane_nets.insert("N".to_owned(), 1);
        p.obstacles = vec![
            pcb_model::Obstacle {
                kind: "zone".to_owned(),
                layers: vec![LayerRef("inner1".to_owned())],
                center: Point2 { x: 10.0, y: 5.0 },
                width: 20.0,
                height: 10.0,
                connected_to: vec!["N".to_owned()],
            },
            pcb_model::Obstacle {
                kind: "rect".to_owned(),
                // `pcb-model::place::routing_view` represents a plated
                // through-hole pad by its two outer copper faces; the barrel's
                // inner-layer span is implicit.
                layers: vec![LayerRef::top(), LayerRef::bottom()],
                center: Point2 { x: 2.0, y: 5.0 },
                width: 1.0,
                height: 1.0,
                connected_to: vec!["N".to_owned()],
            },
            pcb_model::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 18.0, y: 5.0 },
                width: 1.0,
                height: 1.0,
                connected_to: vec!["N".to_owned()],
            },
        ];

        let (sub, fanout) = plane_fanout(&DEPS, &p).expect("plane connection handled");
        assert!(sub.connections.is_empty());
        assert_eq!(fanout.vias.len(), 1);
        assert_eq!(fanout.vias[0].at, Point2 { x: 18.0, y: 5.0 });
        assert!(pcb_drc::connectivity::check(&p, &fanout).is_empty());
    }

    #[test]
    fn plane_fanout_stitches_top_pads_to_a_bottom_pour() {
        let mut p = simple_two_point_problem();
        p.layer_count = 2;
        p.plane_nets.insert("N".to_owned(), 1);
        p.obstacles = p.connections[0]
            .points_to_connect
            .iter()
            .map(|point| pcb_model::Obstacle {
                kind: "pad".to_owned(),
                layers: vec![LayerRef::top()],
                center: point.point(),
                width: 1.0,
                height: 1.0,
                connected_to: vec!["N".to_owned()],
            })
            .collect();

        let (sub, fanout) = plane_fanout(&DEPS, &p).expect("outer pour connection handled");

        assert!(sub.connections.is_empty());
        assert_eq!(fanout.vias.len(), 2);
        assert!(fanout.vias.iter().all(|via| via.span == ViaSpan::Through));
    }

    #[test]
    fn plane_fanout_neckdowns_a_blocked_pad_to_a_nearby_via() {
        let mut p = simple_two_point_problem();
        p.layer_count = 2;
        p.plane_nets.insert("N".to_owned(), 1);
        for point in &p.connections[0].points_to_connect {
            p.obstacles.push(pcb_model::Obstacle {
                kind: "pad".to_owned(),
                layers: vec![LayerRef::top()],
                center: point.point(),
                width: 0.3,
                height: 0.6,
                connected_to: vec!["N".to_owned()],
            });
        }
        let blocked = p.connections[0].points_to_connect[1].point();
        p.obstacles.push(pcb_model::Obstacle {
            kind: "pad".to_owned(),
            layers: vec![LayerRef::top()],
            center: Point2 {
                x: blocked.x + 0.45,
                y: blocked.y,
            },
            width: 0.1,
            height: 0.1,
            connected_to: vec!["FOREIGN".to_owned()],
        });

        let (sub, fanout) = plane_fanout(&DEPS, &p).expect("blocked pad should get a legal neck-down");

        assert!(sub.connections.is_empty());
        assert_eq!(fanout.vias.len(), 2);
        assert_eq!(fanout.traces.len(), 1);
        assert_eq!(fanout.traces[0].width, p.min_trace_width);
        assert_eq!(DRC.geometry_violations(&p, &fanout), 0);
        assert!(fanout.traces[0].path[0].dist(fanout.traces[0].path[1]) <= 5.0);
    }

    #[test]
    fn one_unstitchable_plane_pad_does_not_demote_the_whole_net() {
        let mut p = simple_two_point_problem();
        p.layer_count = 4;
        p.plane_nets.insert("N".to_owned(), 1);
        p.obstacles = p.connections[0]
            .points_to_connect
            .iter()
            .map(|point| pcb_model::Obstacle {
                kind: "pad".to_owned(),
                layers: vec![LayerRef::top()],
                center: point.point(),
                width: 1.0,
                height: 1.0,
                connected_to: vec!["N".to_owned()],
            })
            .collect();
        p.obstacles.push(pcb_model::Obstacle {
            kind: "keepout".to_owned(),
            layers: vec![LayerRef::top(), LayerRef("inner1".to_owned())],
            center: p.connections[0].points_to_connect[0].point(),
            width: 11.0,
            height: 11.0,
            connected_to: vec!["FOREIGN".to_owned()],
        });

        let (sub, fanout) = plane_fanout(&DEPS, &p).expect("the stitchable pad is retained");

        assert_eq!(fanout.vias.len(), 1);
        assert_eq!(fanout.vias[0].at, Point2 { x: 18.0, y: 5.0 });
        assert_eq!(sub.connections.len(), 1);
        assert_eq!(sub.connections[0].points_to_connect.len(), 2);
        assert_eq!(
            sub.connections[0].points_to_connect[0].point(),
            Point2 { x: 2.0, y: 5.0 }
        );
        assert_eq!(
            sub.connections[0].points_to_connect[1].point(),
            fanout.vias[0].at
        );
    }

    #[test]
    fn wide_terminal_escape_necks_down_narrow_pad_and_reserves_copper() {
        let mut p = simple_two_point_problem();
        p.net_widths.insert("N".to_owned(), 0.5);
        p.obstacles = vec![
            pcb_model::Obstacle {
                kind: "pad:U1".to_owned(),
                layers: vec![LayerRef::top()],
                center: p.connections[0].points_to_connect[0].point(),
                width: 0.5,
                height: 0.35,
                connected_to: vec!["N".to_owned()],
            },
            pcb_model::Obstacle {
                kind: "pad:J1".to_owned(),
                layers: vec![LayerRef::top()],
                center: p.connections[0].points_to_connect[1].point(),
                width: 1.0,
                height: 1.0,
                connected_to: vec!["N".to_owned()],
            },
        ];
        let original = p.connections[0].points_to_connect[0].point();

        let (transformed, escapes) = prepare_wide_terminal_escapes(&DEPS, &p);

        assert_eq!(escapes.traces.len(), 2);
        assert_eq!(escapes.traces[0].width, p.min_trace_width);
        assert_eq!(escapes.traces[1].width, 0.5);
        assert_ne!(
            transformed.connections[0].points_to_connect[0].point(),
            original
        );
        assert_eq!(DRC.geometry_violations(&p, &escapes), 0);
        assert!(
            transformed
                .obstacles
                .iter()
                .any(|ob| { ob.kind == "route-trace" && ob.connected_to == ["N".to_owned()] })
        );
    }

    #[test]
    fn wide_terminal_escape_is_atomic_when_a_pad_is_enclosed() {
        let mut p = simple_two_point_problem();
        p.net_widths.insert("N".to_owned(), 0.5);
        let origin = p.connections[0].points_to_connect[0].point();
        p.obstacles.push(pcb_model::Obstacle {
            kind: "pad:U1".to_owned(),
            layers: vec![LayerRef::top()],
            center: origin,
            width: 0.5,
            height: 0.35,
            connected_to: vec!["N".to_owned()],
        });
        for (x, y, width, height) in [
            (origin.x - 0.7, origin.y, 0.4, 2.0),
            (origin.x + 0.7, origin.y, 0.4, 2.0),
            (origin.x, origin.y - 0.7, 2.0, 0.4),
            (origin.x, origin.y + 0.7, 2.0, 0.4),
        ] {
            p.obstacles.push(pcb_model::Obstacle {
                kind: "pad:X".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x, y },
                width,
                height,
                connected_to: vec!["OTHER".to_owned()],
            });
        }

        let (transformed, escapes) = prepare_wide_terminal_escapes(&DEPS, &p);

        assert!(escapes.traces.is_empty());
        assert_eq!(transformed.connections, p.connections);
        assert_eq!(transformed.obstacles, p.obstacles);
    }

    fn top_blocked_two_point_problem() -> RoutingView {
        let mut p = simple_two_point_problem();
        p.obstacles = vec![
            pcb_model::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 2.0, y: 5.0 },
                width: 0.6,
                height: 0.6,
                connected_to: vec!["N".to_owned()],
            },
            pcb_model::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 18.0, y: 5.0 },
                width: 0.6,
                height: 0.6,
                connected_to: vec!["N".to_owned()],
            },
            pcb_model::Obstacle {
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

    fn layer_change_problem() -> RoutingView {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0] = pcb_model::RoutePoint {
            x: 1.0,
            y: 1.0,
            layer: LayerRef::top(),
        };
        p.connections[0].points_to_connect[1] = pcb_model::RoutePoint {
            x: 4.0,
            y: 4.0,
            layer: LayerRef::bottom(),
        };
        p
    }

    fn stacked_layer_change_problem() -> RoutingView {
        let mut p = simple_two_point_problem();
        p.obstacles = vec![
            pcb_model::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 5.0, y: 5.0 },
                width: 0.6,
                height: 0.6,
                connected_to: vec!["N".to_owned()],
            },
            pcb_model::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::bottom()],
                center: Point2 { x: 5.0, y: 5.0 },
                width: 0.6,
                height: 0.6,
                connected_to: vec!["N".to_owned()],
            },
        ];
        p.connections[0].points_to_connect = vec![
            pcb_model::RoutePoint {
                x: 5.0,
                y: 5.0,
                layer: LayerRef::top(),
            },
            pcb_model::RoutePoint {
                x: 5.0,
                y: 5.0,
                layer: LayerRef::bottom(),
            },
        ];
        p
    }

    fn pad_obstacle(net: &str, x: f64, y: f64, layer: LayerRef) -> pcb_model::Obstacle {
        pcb_model::Obstacle {
            kind: "rect".to_owned(),
            layers: vec![layer],
            center: Point2 { x, y },
            width: 0.6,
            height: 0.6,
            connected_to: vec![net.to_owned()],
        }
    }

    fn heterogeneous_pattern_problem() -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![
                pad_obstacle("D", 2.0, 2.0, LayerRef::top()),
                pad_obstacle("D", 8.0, 2.0, LayerRef::top()),
                pad_obstacle("L", 2.0, 6.0, LayerRef::top()),
                pad_obstacle("L", 8.0, 6.0, LayerRef::bottom()),
                pad_obstacle("V", 2.0, 14.0, LayerRef::top()),
                pad_obstacle("V", 20.0, 14.0, LayerRef::top()),
                pcb_model::Obstacle {
                    kind: "rect".to_owned(),
                    layers: vec![LayerRef::top()],
                    center: Point2 { x: 11.0, y: 10.0 },
                    width: 1.0,
                    height: 20.0,
                    connected_to: vec![],
                },
            ],
            connections: vec![
                pcb_model::Connection {
                    name: "D".to_owned(),
                    points_to_connect: vec![
                        pcb_model::RoutePoint {
                            x: 2.0,
                            y: 2.0,
                            layer: LayerRef::top(),
                        },
                        pcb_model::RoutePoint {
                            x: 8.0,
                            y: 2.0,
                            layer: LayerRef::top(),
                        },
                    ],
                },
                pcb_model::Connection {
                    name: "L".to_owned(),
                    points_to_connect: vec![
                        pcb_model::RoutePoint {
                            x: 2.0,
                            y: 6.0,
                            layer: LayerRef::top(),
                        },
                        pcb_model::RoutePoint {
                            x: 8.0,
                            y: 6.0,
                            layer: LayerRef::bottom(),
                        },
                    ],
                },
                pcb_model::Connection {
                    name: "V".to_owned(),
                    points_to_connect: vec![
                        pcb_model::RoutePoint {
                            x: 2.0,
                            y: 14.0,
                            layer: LayerRef::top(),
                        },
                        pcb_model::RoutePoint {
                            x: 20.0,
                            y: 14.0,
                            layer: LayerRef::top(),
                        },
                    ],
                },
            ],
            bounds: pcb_model::Rect {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
        }
    }

    fn heterogeneous_multi_pin_channel_problem() -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![
                pad_obstacle("L", 2.0, 6.0, LayerRef::top()),
                pad_obstacle("L", 8.0, 6.0, LayerRef::bottom()),
                pad_obstacle("BUS", 2.0, 14.0, LayerRef::top()),
                pad_obstacle("BUS", 8.0, 14.0, LayerRef::top()),
                pad_obstacle("BUS", 8.0, 18.0, LayerRef::top()),
                pcb_model::Obstacle {
                    kind: "rect".to_owned(),
                    layers: vec![LayerRef::top()],
                    center: Point2 { x: 8.0, y: 16.0 },
                    width: 1.0,
                    height: 3.0,
                    connected_to: vec![],
                },
            ],
            connections: vec![
                pcb_model::Connection {
                    name: "L".to_owned(),
                    points_to_connect: vec![
                        pcb_model::RoutePoint {
                            x: 2.0,
                            y: 6.0,
                            layer: LayerRef::top(),
                        },
                        pcb_model::RoutePoint {
                            x: 8.0,
                            y: 6.0,
                            layer: LayerRef::bottom(),
                        },
                    ],
                },
                pcb_model::Connection {
                    name: "BUS".to_owned(),
                    points_to_connect: vec![
                        pcb_model::RoutePoint {
                            x: 2.0,
                            y: 14.0,
                            layer: LayerRef::top(),
                        },
                        pcb_model::RoutePoint {
                            x: 8.0,
                            y: 14.0,
                            layer: LayerRef::top(),
                        },
                        pcb_model::RoutePoint {
                            x: 8.0,
                            y: 18.0,
                            layer: LayerRef::top(),
                        },
                    ],
                },
            ],
            bounds: pcb_model::Rect {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
        }
    }

    // ── led-r: the strict gate that must pass through route_detailed today ──────

    #[test]
    fn led_r_detailed_is_clean_and_lints_empty() {
        let p = load("led-r.json");
        let r = route_detailed(&DEPS, &p);
        assert_eq!(r.engine, ENGINE);
        assert!(
            r.failed.is_empty(),
            "led-r must route cleanly through route_detailed today: {:?}",
            r.failed
        );
        let vs = DRC.check(&p, &r.solution);
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
        let r = route_detailed(&DEPS, &p);
        assert_eq!(r.engine, ENGINE);
        assert!(
            r.failed.is_empty(),
            "quad must route cleanly through route_detailed after the finisher: {:?}",
            r.failed
        );
        let vs = DRC.check(&p, &r.solution);
        assert!(
            vs.is_empty(),
            "quad detailed solution must lint CLEAN, got {vs:?}"
        );
    }

    #[test]
    fn route_detailed_short_circuits_infeasible_global_route() {
        let p = load("quad.json");
        let mesh = crate::mesh::CapacityMesh::build(&p);
        let global = GlobalRouteResult {
            plan: crate::pathing::GlobalPlan { nets: Vec::new() },
            report: crate::pathing::CongestionReport {
                iterations: 40,
                final_overflow: 3,
                edge_hotspots: Vec::new(),
                unrouted: Vec::new(),
            },
        };

        let r = route_detailed_from_global(&DEPS, &p, &mesh, &global);

        assert_eq!(r.engine, ENGINE);
        assert!(
            r.solution.traces.is_empty() && r.solution.vias.is_empty(),
            "infeasible global routes should not spend cell routing or emit partial copper"
        );
        assert!(
            r.failed
                .iter()
                .any(|f| f.reason.contains("global: 3 unit(s)")),
            "infeasible global route should be reported with global provenance: {:?}",
            r.failed
        );
    }

    #[test]
    fn quad_auto_is_clean() {
        // The premium auto portfolio is direct fast-path, layer-hop/via-escape
        // micro-routers, contextual sequential grid, negotiated detailed primary,
        // and grid fallback. Quad is no longer forced to pay the detailed mesh:
        // the sequential candidate can solve it cleanly and should short-circuit.
        let p = load("quad.json");
        let r = route_tuned(&DEPS, &p);
        assert_eq!(r.engine, pcb_route_grid::router::ENGINE);
        assert!(
            r.failed.is_empty(),
            "route_tuned routes quad cleanly: {:?}",
            r.failed
        );
        let vs = DRC.check(&p, &r.solution);
        assert!(
            vs.is_empty(),
            "quad route_tuned solution must lint CLEAN, got {vs:?}"
        );
    }

    #[test]
    fn tuned_route_reports_one_clean_grid_pass_for_direct_net() {
        let p = simple_two_point_problem();
        let r = route_tuned_with_diagnostics(&DEPS, &p);
        assert_eq!(r.result.engine, pcb_route_grid::router::ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert!(
            r.global.is_none(),
            "the tuned grid algorithm does not run the diagnostic mesh"
        );
        assert_eq!(r.passes.len(), 1);
        assert_eq!(r.passes[0].engine, pcb_route_grid::router::ENGINE);
        assert_eq!(r.passes[0].failed_nets, 0);
        assert_eq!(r.passes[0].geometry_violations, 0);
    }

    #[test]
    fn adaptive_rescue_short_circuit_only_accepts_clean_via_free_candidates() {
        let clean_via_free = RouteQuality {
            fault_weight: 0,
            geom: 0,
            failed_nets: 0,
            via_count: 0,
            wirelength: 20.0,
        };
        let clean_via_heavy = RouteQuality {
            via_count: 1,
            ..clean_via_free
        };
        let failed = RouteQuality {
            fault_weight: 1,
            failed_nets: 1,
            ..clean_via_free
        };

        assert!(adaptive_rescue_candidate_can_short_circuit(&clean_via_free));
        assert!(
            !adaptive_rescue_candidate_can_short_circuit(&clean_via_heavy),
            "clean via-heavy rescue should keep competing with later rescue orders"
        );
        assert!(
            !adaptive_rescue_candidate_can_short_circuit(&failed),
            "failed rescue candidates must not stop the rescue portfolio"
        );
    }

    #[test]
    fn adaptive_ripup_candidate_must_reduce_failure_outcome_before_tidiness() {
        let selected = RouteQuality {
            fault_weight: 2,
            geom: 0,
            failed_nets: 1,
            via_count: 2,
            wirelength: 100.0,
        };
        let equal_failure_tidier = RouteQuality {
            via_count: 0,
            wirelength: 10.0,
            ..selected
        };
        let lower_weight = RouteQuality {
            fault_weight: 1,
            failed_nets: 1,
            via_count: 10,
            wirelength: 200.0,
            ..selected
        };
        let fewer_failed_nets = RouteQuality {
            fault_weight: 2,
            failed_nets: 0,
            via_count: 10,
            wirelength: 200.0,
            ..selected
        };

        assert!(
            !adaptive_ripup_candidate_reduces_failures(&selected, &equal_failure_tidier),
            "rip-up rescue must not swap equivalent failures just for tidier surviving copper"
        );
        assert!(adaptive_ripup_candidate_reduces_failures(
            &selected,
            &lower_weight
        ));
        assert!(adaptive_ripup_candidate_reduces_failures(
            &selected,
            &fewer_failed_nets
        ));
    }

    #[test]
    fn adaptive_grid_rescue_routes_failed_net_against_selected_copper() {
        let mut p = simple_two_point_problem();
        p.connections = vec![
            pcb_model::Connection {
                name: "A".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "B".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 8.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "A".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(2.0, 5.0), pt(18.0, 5.0)],
                }],
                vias: vec![],
            },
            failed: vec![FailedNet {
                connection: "B".to_owned(),
                reason: "selected router failed B".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };

        let rescued = adaptive_grid_rescue(&DEPS, &p, &selected).expect("B should grid-rescue");

        assert!(rescued.failed.is_empty(), "{:?}", rescued.failed);
        assert!(
            rescued.engine.starts_with("synthetic+adaptive-grid"),
            "unexpected rescue phase: {}",
            rescued.engine
        );
        assert!(
            rescued
                .solution
                .traces
                .iter()
                .any(|trace| trace.connection == "A")
        );
        assert!(
            rescued
                .solution
                .traces
                .iter()
                .any(|trace| trace.connection == "B")
        );
        assert!(
            DRC.check(&p, &rescued.solution).is_empty(),
            "rescued hybrid must be clean"
        );
    }

    #[test]
    fn cached_residual_obstacles_match_solution_rebuild_subproblem() {
        let p = simple_two_point_problem();
        let solution = RouteSolution {
            traces: vec![Trace {
                connection: "N".to_owned(),
                layer: LayerRef::top(),
                width: p.min_trace_width,
                path: vec![pt(2.0, 5.0), pt(10.0, 5.0), pt(18.0, 5.0)],
            }],
            vias: vec![Via {
                connection: "N".to_owned(),
                at: pt(10.0, 5.0),
                diameter: p.via_diameter,
                drill: p.via_drill,
                span: ViaSpan::Through,
            }],
        };
        let rebuilt = problem_with_single_connection_and_copper(&p, 0, &solution);
        let cached_obstacles = copper_obstacles(&p, &solution);
        let cached = problem_with_single_connection_and_obstacles(&p, 0, &cached_obstacles);

        assert_eq!(
            serde_json::to_string(&cached).unwrap(),
            serde_json::to_string(&rebuilt).unwrap(),
            "cached residual copper obstacles must preserve the routed subproblem"
        );
    }

    #[test]
    fn adaptive_rescue_base_drops_failed_net_copper_once() {
        let p = simple_two_point_problem();
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![
                    Trace {
                        connection: "KEEP".to_owned(),
                        layer: LayerRef::top(),
                        width: p.min_trace_width,
                        path: vec![pt(2.0, 4.0), pt(18.0, 4.0)],
                    },
                    Trace {
                        connection: "DROP".to_owned(),
                        layer: LayerRef::top(),
                        width: p.min_trace_width,
                        path: vec![pt(2.0, 6.0), pt(18.0, 6.0)],
                    },
                ],
                vias: vec![Via {
                    connection: "DROP".to_owned(),
                    at: pt(10.0, 6.0),
                    diameter: p.via_diameter,
                    drill: p.via_drill,
                    span: ViaSpan::Through,
                }],
            },
            failed: vec![FailedNet {
                connection: "DROP".to_owned(),
                reason: "failed".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };
        let failed_names = ["DROP".to_owned()].into_iter().collect();

        let base = adaptive_rescue_base(&p, &selected, &failed_names);

        assert!(
            base.solution
                .traces
                .iter()
                .all(|trace| trace.connection != "DROP")
        );
        assert!(
            base.solution
                .vias
                .iter()
                .all(|via| via.connection != "DROP")
        );
        assert!(
            base.obstacles
                .iter()
                .all(|obstacle| !obstacle.connected_to.contains(&"DROP".to_owned())),
            "base residual obstacles should only represent accepted copper"
        );
        assert!(
            base.obstacles
                .iter()
                .any(|obstacle| obstacle.connected_to.contains(&"KEEP".to_owned())),
            "accepted copper should remain as residual obstacles"
        );
        assert_eq!(base.failed, selected.failed);
    }

    #[test]
    fn adaptive_ripup_rescue_orders_include_blocking_accepted_copper() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "SIG".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "BLOCK".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 18.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "BLOCK".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(10.0, 2.0), pt(10.0, 18.0)],
                }],
                vias: vec![],
            },
            failed: vec![FailedNet {
                connection: "SIG".to_owned(),
                reason: "selected router failed SIG".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };
        let failed_names = ["SIG".to_owned()].into_iter().collect();

        let orders = adaptive_ripup_rescue_orders(&p, &selected, &failed_names);

        assert!(
            orders.iter().any(|order| order == &[0, 1]),
            "rip-up rescue should route the failed net before accepted copper blocking its corridor: {orders:?}"
        );
    }

    #[test]
    fn adaptive_ripup_rescue_orders_include_terminal_enclosure_blocker() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "SIG".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "BLOCK".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.5,
                        y: 9.4,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 2.5,
                        y: 9.4,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "BLOCK".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.5, 9.4), pt(2.5, 9.4)],
                }],
                vias: vec![],
            },
            failed: vec![FailedNet {
                connection: "SIG".to_owned(),
                reason: "selected router failed SIG".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };
        let failed_names = ["SIG".to_owned()].into_iter().collect();

        let scores = adaptive_ripup_blocker_scores(&p, &selected, &failed_names);
        let orders = adaptive_ripup_rescue_orders(&p, &selected, &failed_names);

        assert!(
            scores.iter().any(|(name, _)| name == "BLOCK"),
            "terminal-neighborhood blocker should be scored even when it is outside strict corridor clearance: {scores:?}"
        );
        assert!(
            orders.iter().any(|order| order == &[0, 1]),
            "rip-up rescue should route the failed terminal before accepted copper enclosing it: {orders:?}"
        );
    }

    #[test]
    fn adaptive_ripup_scores_failed_detail_via_location_on_all_layers() {
        let mut p = simple_two_point_problem();
        p.layer_count = 4;
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "SIG".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "BOTTOM_BLOCK".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 8.0,
                        y: 10.0,
                        layer: LayerRef::bottom(),
                    },
                    pcb_model::RoutePoint {
                        x: 12.0,
                        y: 10.0,
                        layer: LayerRef::bottom(),
                    },
                ],
            },
        ];
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "BOTTOM_BLOCK".to_owned(),
                    layer: LayerRef::bottom(),
                    width: p.min_trace_width,
                    path: vec![pt(8.0, 10.0), pt(12.0, 10.0)],
                }],
                vias: vec![],
            },
            failed: vec![FailedNet {
                connection: "SIG".to_owned(),
                reason: "cell 7: no in-cell path for terminal Via at (10.0000,10.0000) (congestion or enclosure)".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };
        let failed_names = ["SIG".to_owned()].into_iter().collect();

        let scores = adaptive_ripup_blocker_scores(&p, &selected, &failed_names);
        let orders = adaptive_ripup_rescue_orders(&p, &selected, &failed_names);

        assert!(
            scores.iter().any(|(name, _)| name == "BOTTOM_BLOCK"),
            "failed detailed via coordinate should score blockers on non-endpoint layers: {scores:?}"
        );
        assert!(
            orders.iter().any(|order| order == &[0, 1]),
            "rip-up rescue should route the failed net before the detailed-via blocker: {orders:?}"
        );
    }

    #[test]
    fn parse_failed_terminal_point_extracts_detail_coordinates() {
        let point = parse_failed_terminal_point(
            "cell 74: no in-cell path for terminal Via at (16.0875,15.6000) (congestion or enclosure)",
        )
        .expect("detail failure coordinate should parse");

        assert_eq!(point, pt(16.0875, 15.6));
        assert!(parse_failed_terminal_point("global: no path").is_none());
    }

    #[test]
    fn adaptive_ripup_rescue_orders_include_blocking_accepted_via() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "SIG".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "VIA_BLOCK".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 4.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 16.0,
                        layer: LayerRef::bottom(),
                    },
                ],
            },
        ];
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![Via {
                    connection: "VIA_BLOCK".to_owned(),
                    at: pt(10.0, 10.0),
                    diameter: p.via_diameter,
                    drill: p.via_drill,
                    span: ViaSpan::Through,
                }],
            },
            failed: vec![FailedNet {
                connection: "SIG".to_owned(),
                reason: "selected router failed SIG".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };
        let failed_names = ["SIG".to_owned()].into_iter().collect();

        let orders = adaptive_ripup_rescue_orders(&p, &selected, &failed_names);

        assert!(
            orders.iter().any(|order| order == &[0, 1]),
            "rip-up rescue should reroute an accepted via whose barrel blocks the failed corridor: {orders:?}"
        );
    }

    #[test]
    fn adaptive_ripup_rescue_orders_include_blockers_for_layer_changing_failed_net() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "VIA_SIG".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 10.0,
                        layer: LayerRef::bottom(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "TOP_BLOCK".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 18.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "TOP_BLOCK".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(10.0, 2.0), pt(10.0, 18.0)],
                }],
                vias: vec![],
            },
            failed: vec![FailedNet {
                connection: "VIA_SIG".to_owned(),
                reason: "selected router failed layer change".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };
        let failed_names = ["VIA_SIG".to_owned()].into_iter().collect();

        let orders = adaptive_ripup_rescue_orders(&p, &selected, &failed_names);

        assert!(
            orders.iter().any(|order| order == &[0, 1]),
            "rip-up rescue should project layer-changing failed nets onto endpoint layers: {orders:?}"
        );
    }

    #[test]
    fn adaptive_ripup_failed_corridors_ignore_non_tree_multi_pin_diagonals() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "BUS".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 18.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "DIAGONAL_FALSE_BLOCK".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 9.0,
                        y: 9.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 11.0,
                        y: 11.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "DIAGONAL_FALSE_BLOCK".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(9.0, 9.0), pt(11.0, 11.0)],
                }],
                vias: vec![],
            },
            failed: vec![FailedNet {
                connection: "BUS".to_owned(),
                reason: "selected router failed BUS".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };
        let failed_names = ["BUS".to_owned()].into_iter().collect();

        let orders = adaptive_ripup_rescue_orders(&p, &selected, &failed_names);

        assert!(
            orders.is_empty(),
            "multi-pin failed corridors should follow the nearest tree, not every pad-pair diagonal: {orders:?}"
        );
    }

    #[test]
    fn adaptive_ripup_failed_corridor_tree_prefers_same_layer_edge_on_tie() {
        let conn = pcb_model::Connection {
            name: "BUS".to_owned(),
            points_to_connect: vec![
                pcb_model::RoutePoint {
                    x: 2.0,
                    y: 2.0,
                    layer: LayerRef::top(),
                },
                pcb_model::RoutePoint {
                    x: 12.0,
                    y: 2.0,
                    layer: LayerRef::bottom(),
                },
                pcb_model::RoutePoint {
                    x: 2.0,
                    y: 12.0,
                    layer: LayerRef::top(),
                },
            ],
        };

        let pairs = failed_corridor_tree_pairs(&conn);

        assert_eq!(
            pairs.first().copied(),
            Some((0, 2)),
            "equal-length failed corridor tree edges should prefer same-layer endpoints: {pairs:?}"
        );
    }

    #[test]
    fn adaptive_ripup_rescue_orders_sort_blockers_by_corridor_pressure() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "SIG".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "LONG_BLOCK".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 6.0,
                        y: 6.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 14.0,
                        y: 14.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "SHORT_BLOCK".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 9.5,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 10.5,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![
                    Trace {
                        connection: "SHORT_BLOCK".to_owned(),
                        layer: LayerRef::top(),
                        width: p.min_trace_width,
                        path: vec![pt(10.0, 9.5), pt(10.0, 10.5)],
                    },
                    Trace {
                        connection: "LONG_BLOCK".to_owned(),
                        layer: LayerRef::top(),
                        width: p.min_trace_width,
                        path: vec![pt(6.0, 6.0), pt(14.0, 14.0)],
                    },
                ],
                vias: vec![],
            },
            failed: vec![FailedNet {
                connection: "SIG".to_owned(),
                reason: "selected router failed SIG".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };
        let failed_names = ["SIG".to_owned()].into_iter().collect();

        let orders = adaptive_ripup_rescue_orders(&p, &selected, &failed_names);

        assert_eq!(
            orders[0],
            vec![0, 1, 2],
            "larger actual corridor blocker should be rerouted before shorter blocker: {orders:?}"
        );
    }

    #[test]
    fn adaptive_ripup_rescue_orders_sort_trace_blockers_by_corridor_proximity() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "SIG".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "CENTER_TRACE".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 8.0,
                        y: 9.5,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 8.0,
                        y: 10.5,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "EDGE_TRACE".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 12.0,
                        y: 9.95,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 12.0,
                        y: 10.95,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![
                    Trace {
                        connection: "EDGE_TRACE".to_owned(),
                        layer: LayerRef::top(),
                        width: p.min_trace_width,
                        path: vec![pt(12.0, 9.95), pt(12.0, 10.95)],
                    },
                    Trace {
                        connection: "CENTER_TRACE".to_owned(),
                        layer: LayerRef::top(),
                        width: p.min_trace_width,
                        path: vec![pt(8.0, 9.5), pt(8.0, 10.5)],
                    },
                ],
                vias: vec![],
            },
            failed: vec![FailedNet {
                connection: "SIG".to_owned(),
                reason: "selected router failed SIG".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };
        let failed_names = ["SIG".to_owned()].into_iter().collect();

        let orders = adaptive_ripup_rescue_orders(&p, &selected, &failed_names);

        assert_eq!(
            orders[0],
            vec![0, 1, 2],
            "trace centered on the failed corridor should be rerouted before an equal-length edge trace: {orders:?}"
        );
    }

    #[test]
    fn adaptive_ripup_rescue_orders_sort_via_blockers_by_corridor_proximity() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "SIG".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "CENTER_VIA".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 8.0,
                        y: 4.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 8.0,
                        y: 16.0,
                        layer: LayerRef::bottom(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "EDGE_VIA".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 12.0,
                        y: 4.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 12.0,
                        y: 16.0,
                        layer: LayerRef::bottom(),
                    },
                ],
            },
        ];
        let selected = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![
                    Via {
                        connection: "EDGE_VIA".to_owned(),
                        at: pt(12.0, 10.35),
                        diameter: p.via_diameter,
                        drill: p.via_drill,
                        span: ViaSpan::Through,
                    },
                    Via {
                        connection: "CENTER_VIA".to_owned(),
                        at: pt(8.0, 10.0),
                        diameter: p.via_diameter,
                        drill: p.via_drill,
                        span: ViaSpan::Through,
                    },
                ],
            },
            failed: vec![FailedNet {
                connection: "SIG".to_owned(),
                reason: "selected router failed SIG".to_owned(),
            }],
            engine: "synthetic".to_owned(),
        };
        let failed_names = ["SIG".to_owned()].into_iter().collect();

        let orders = adaptive_ripup_rescue_orders(&p, &selected, &failed_names);

        assert_eq!(
            orders[0],
            vec![0, 1, 2],
            "via centered on the failed corridor should be rerouted before a merely nearby via: {orders:?}"
        );
    }

    #[test]
    fn adaptive_rescue_order_prioritizes_crossing_pressure() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "OPEN".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 4.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "HARD_H".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "HARD_V".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 18.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let failed_names = ["OPEN".to_owned(), "HARD_H".to_owned()]
            .into_iter()
            .collect();

        let order = adaptive_rescue_order(&p, &failed_names);

        assert_eq!(
            order,
            vec![1, 0],
            "adaptive rescue should let crossing-pressure nets claim residual space first"
        );
    }

    #[test]
    fn adaptive_rescue_order_cached_metrics_match_public_order() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "OPEN".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 4.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "HARD_H".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "HARD_V".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 18.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let failed_names = ["OPEN".to_owned(), "HARD_H".to_owned(), "HARD_V".to_owned()]
            .into_iter()
            .collect();

        let metrics = adaptive_rescue_order_metrics(&p);

        assert_eq!(
            adaptive_rescue_order_with_metrics(&p, &failed_names, &metrics),
            adaptive_rescue_order(&p, &failed_names),
            "cached adaptive rescue metrics must preserve the public pressure-first order"
        );
    }

    #[test]
    fn adaptive_rescue_order_prefers_segment_obstacle_pressure_before_bbox_pressure() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 12.0;
        p.obstacles = vec![
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: pt(8.0, 2.0),
                width: 0.5,
                height: 0.5,
                connected_to: vec![],
            },
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: pt(5.0, 1.0),
                width: 0.5,
                height: 0.5,
                connected_to: vec![],
            },
        ];
        p.connections = vec![
            pcb_model::Connection {
                name: "BBOX_ONLY".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 9.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 9.0,
                        y: 9.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "SEGMENT_BLOCKED".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 9.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let failed_names = ["BBOX_ONLY".to_owned(), "SEGMENT_BLOCKED".to_owned()]
            .into_iter()
            .collect();
        let metrics = adaptive_rescue_order_metrics(&p);

        assert_eq!(metrics[0].segment_obstacle_pressure_um, 0);
        assert!(metrics[0].obstacle_pressure_um > 0);
        assert!(metrics[1].segment_obstacle_pressure_um > 0);
        assert_eq!(
            adaptive_rescue_order(&p, &failed_names),
            vec![1, 0],
            "adaptive rescue should prioritize nets whose actual tree corridor is obstructed before broad bbox-only pressure"
        );
    }

    #[test]
    fn adaptive_rescue_orders_include_bounded_order_portfolio() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "OPEN".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 4.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "HARD_H".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 2.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 18.0,
                        y: 10.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "HARD_V".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 18.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let failed_names = ["OPEN".to_owned(), "HARD_H".to_owned(), "HARD_V".to_owned()]
            .into_iter()
            .collect();

        let orders = adaptive_rescue_orders(&p, &failed_names);

        assert_eq!(orders[0], vec![1, 2, 0]);
        assert!(
            orders.iter().any(|order| order == &[0, 1, 2]),
            "adaptive rescue should still try baseline/original order: {orders:?}"
        );
        assert!(
            orders.len() > 1,
            "small failed sets should get a bounded rescue order portfolio: {orders:?}"
        );
    }

    #[test]
    fn adaptive_rescue_orders_include_short_span_first_order() {
        let mut p = simple_two_point_problem();
        p.bounds.max_y = 25.0;
        p.connections = vec![
            pcb_model::Connection {
                name: "LONG".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 22.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 19.0,
                        y: 22.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "SHORT".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 3.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "CROSS_H".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 4.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 16.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
            pcb_model::Connection {
                name: "CROSS_V".to_owned(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 10.0,
                        y: 18.0,
                        layer: LayerRef::top(),
                    },
                ],
            },
        ];
        let failed_names = p.connections.iter().map(|conn| conn.name.clone()).collect();

        let orders = adaptive_rescue_orders(&p, &failed_names);

        assert!(
            orders.iter().any(|order| order == &[1, 2, 3, 0]),
            "small failed rescue sets should include a shortest-local-net-first pass: {orders:?}"
        );
    }

    #[test]
    fn adaptive_rescue_orders_skip_portfolio_for_large_failed_sets() {
        let mut p = simple_two_point_problem();
        p.connections.clear();
        for i in 0..=ADAPTIVE_RESCUE_PORTFOLIO_MAX_FAILED {
            p.connections.push(pcb_model::Connection {
                name: format!("N{i}"),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0 + i as f64,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 1.0 + i as f64,
                        layer: LayerRef::top(),
                    },
                ],
            });
        }
        let failed_names = p.connections.iter().map(|conn| conn.name.clone()).collect();

        let orders = adaptive_rescue_orders(&p, &failed_names);

        assert_eq!(
            orders.len(),
            1,
            "large failed sets should avoid multiplying grid rescue retries"
        );
    }

    #[test]
    fn tuned_route_handles_layer_change() {
        let p = layer_change_problem();

        let r = route_tuned_with_diagnostics(&DEPS, &p);

        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert_eq!(r.result.solution.vias.len(), 1);
        assert!(DRC.check(&p, &r.result.solution).is_empty());
        assert!(r.global.is_none());
    }

    #[test]
    fn tuned_route_handles_stacked_layer_change() {
        let p = stacked_layer_change_problem();

        let r = route_tuned_with_diagnostics(&DEPS, &p);

        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert_eq!(r.result.solution.vias.len(), 1);
        assert!(DRC.check(&p, &r.result.solution).is_empty());
        assert!(
            r.global.is_none(),
            "the tuned grid algorithm does not run the diagnostic mesh"
        );
    }

    #[test]
    fn tuned_route_handles_mixed_layer_star() {
        let mut p = layer_change_problem();
        p.connections[0]
            .points_to_connect
            .push(pcb_model::RoutePoint {
                x: 6.0,
                y: 4.0,
                layer: LayerRef::bottom(),
            });
        let r = route_tuned_with_diagnostics(&DEPS, &p);

        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert!(!r.result.solution.traces.is_empty());
        assert_eq!(r.result.solution.vias.len(), 1);
        assert!(DRC.check(&p, &r.result.solution).is_empty());
        assert!(
            r.global.is_none(),
            "the tuned grid algorithm does not run the diagnostic mesh"
        );
    }

    #[test]
    fn tuned_route_handles_blocked_top_layer() {
        let p = top_blocked_two_point_problem();

        let r = route_tuned_with_diagnostics(&DEPS, &p);

        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert_eq!(r.result.solution.vias.len(), 2);
        assert!(DRC.check(&p, &r.result.solution).is_empty());
        assert!(
            r.global.is_none(),
            "the tuned grid algorithm does not run the diagnostic mesh"
        );
    }

    #[test]
    fn tuned_route_handles_heterogeneous_nets() {
        let p = heterogeneous_pattern_problem();

        let r = route_tuned_with_diagnostics(&DEPS, &p);

        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert!(r.result.solution.traces.len() >= 3);
        assert_eq!(r.result.solution.vias.len(), 3);
        assert!(DRC.check(&p, &r.result.solution).is_empty());
        assert!(
            r.global.is_none(),
            "the tuned grid algorithm does not run the diagnostic mesh"
        );
    }

    #[test]
    fn tuned_route_handles_multi_pin_channel() {
        let p = heterogeneous_multi_pin_channel_problem();

        let r = route_tuned_with_diagnostics(&DEPS, &p);

        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert!(
            r.result
                .solution
                .traces
                .iter()
                .filter(|trace| trace.connection == "BUS")
                .count()
                >= 2,
            "BUS should be routed as a connected multi-leg tree: {:?}",
            r.result.solution
        );
        assert!(DRC.check(&p, &r.result.solution).is_empty());
        assert!(
            r.global.is_none(),
            "the tuned grid algorithm does not run the diagnostic mesh"
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

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 4.0)]);
        assert!(solution.metrics().wirelength < before);
        assert!(DRC.check(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_octilinear_shortcut_can_remove_existing_clearance_finding() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 4.0;
        p.connections[0].points_to_connect[1].y = 4.0;
        p.obstacles.push(pcb_model::Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef::top()],
            center: pt(2.0, 4.15),
            width: 0.4,
            height: 0.4,
            connected_to: vec![],
        });
        let mut solution = RouteSolution {
            traces: vec![Trace {
                connection: "N".to_owned(),
                layer: LayerRef::top(),
                width: p.min_trace_width,
                path: vec![pt(1.0, 1.0), pt(1.0, 4.0), pt(4.0, 4.0)],
            }],
            vias: vec![],
        };
        let before = DRC.check(&p, &solution);
        assert!(
            before.iter().any(|finding| matches!(
                finding,
                pcb_model::Finding::ClearanceTraceObstacle { .. }
            )),
            "fixture should start with a trace-obstacle clearance finding: {before:?}"
        );

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 4.0)]);
        assert!(
            DRC.check(&p, &solution).is_empty(),
            "octilinear shortcut should be allowed to remove existing lint findings without adding new ones"
        );
    }

    #[test]
    fn postroute_cleanup_pulls_a_detour_into_an_axial_run_and_one_45_degree_leg() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 5.0;
        p.connections[0].points_to_connect[1].y = 4.0;
        let mut solution = RouteSolution {
            traces: vec![Trace {
                connection: "N".to_owned(),
                layer: LayerRef::top(),
                width: p.min_trace_width,
                path: vec![
                    pt(1.0, 1.0),
                    pt(1.0, 5.0),
                    pt(3.0, 5.0),
                    pt(3.0, 4.0),
                    pt(5.0, 4.0),
                ],
            }],
            vias: vec![],
        };
        let before = solution.metrics().wirelength;

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(
            solution.traces[0].path,
            vec![pt(1.0, 1.0), pt(2.0, 1.0), pt(5.0, 4.0)]
        );
        assert!(solution.metrics().wirelength < before);
        assert!(DRC.check(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_line_pull_can_remove_existing_clearance_finding() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 5.0;
        p.connections[0].points_to_connect[1].y = 4.0;
        p.obstacles.push(pcb_model::Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef::top()],
            center: pt(2.0, 5.15),
            width: 0.4,
            height: 0.4,
            connected_to: vec![],
        });
        let mut solution = RouteSolution {
            traces: vec![Trace {
                connection: "N".to_owned(),
                layer: LayerRef::top(),
                width: p.min_trace_width,
                path: vec![
                    pt(1.0, 1.0),
                    pt(1.0, 5.0),
                    pt(3.0, 5.0),
                    pt(3.0, 4.0),
                    pt(5.0, 4.0),
                ],
            }],
            vias: vec![],
        };
        let before = DRC.check(&p, &solution);
        assert!(
            before.iter().any(|finding| matches!(
                finding,
                pcb_model::Finding::ClearanceTraceObstacle { .. }
            )),
            "fixture should start with a trace-obstacle clearance finding: {before:?}"
        );

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(
            solution.traces[0].path,
            vec![pt(1.0, 1.0), pt(2.0, 1.0), pt(5.0, 4.0)]
        );
        assert!(
            DRC.check(&p, &solution).is_empty(),
            "straightening should be allowed to remove existing lint findings without adding new ones"
        );
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
        assert!(DRC.check(&p, &solution).is_empty());

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 1.0)]);
        assert!(DRC.check(&p, &solution).is_empty());
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

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 1.0)]);
        assert!(solution.metrics().wirelength < before);
        assert!(DRC.check(&p, &solution).is_empty());
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
        assert!(DRC.check(&p, &solution).is_empty());

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 1.0)]);
        assert!(
            (solution.metrics().wirelength - before).abs() < 1e-9,
            "collinear simplification should preserve wirelength"
        );
        assert!(DRC.check(&p, &solution).is_empty());
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
        assert!(DRC.check(&p, &solution).is_empty());

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(5.0, 1.0)]);
        assert!(
            solution.metrics().wirelength < before,
            "closed spur loop should be removed"
        );
        assert!(DRC.check(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_spur_removal_can_remove_existing_clearance_finding() {
        let mut p = simple_two_point_problem();
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[1].x = 5.0;
        p.connections[0].points_to_connect[1].y = 1.0;
        p.obstacles.push(pcb_model::Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef::top()],
            center: pt(2.5, 3.15),
            width: 0.4,
            height: 0.4,
            connected_to: vec![],
        });
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
        let before = DRC.check(&p, &solution);
        assert!(
            before.iter().any(|finding| matches!(
                finding,
                pcb_model::Finding::ClearanceTraceObstacle { .. }
            )),
            "fixture should start with a trace-obstacle clearance finding: {before:?}"
        );

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(5.0, 1.0)]);
        assert!(
            DRC.check(&p, &solution).is_empty(),
            "spur removal should be allowed to remove existing lint findings without adding new ones"
        );
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

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(4.0, 1.0)]);
        assert!(
            solution.metrics().wirelength < before,
            "simplification-created duplicate should be dropped"
        );
        assert!(DRC.check(&p, &solution).is_empty());
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
        assert!(DRC.check(&p, &solution).is_empty());

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(5.0, 1.0)]);
        assert!(
            solution.metrics().wirelength < before,
            "covered same-net segment should be dropped"
        );
        assert!(DRC.check(&p, &solution).is_empty());
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
        assert!(DRC.check(&p, &solution).is_empty());

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(
            solution.traces[0].path,
            vec![pt(1.0, 1.0), pt(5.0, 1.0), pt(5.0, 4.0)]
        );
        assert!(
            solution.metrics().wirelength < before,
            "segment covered by one leg of a longer trace should be dropped"
        );
        assert!(DRC.check(&p, &solution).is_empty());
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
        assert!(DRC.check(&p, &solution).is_empty());

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(5.0, 1.0)]);
        assert!(
            solution.metrics().wirelength < before,
            "shortcut-created duplicate should be dropped"
        );
        assert!(DRC.check(&p, &solution).is_empty());
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
        let baseline = DRC.check(&p, &solution);
        assert!(
            baseline
                .iter()
                .any(|finding| matches!(finding, pcb_model::Finding::DanglingEnd { .. }))
        );

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(
            solution.traces[0].path,
            vec![pt(1.0, 1.0), pt(1.0, 4.0), pt(4.0, 4.0)],
            "shortcut must be rejected because it disconnects the via anchor"
        );
        assert_eq!(DRC.check(&p, &solution), baseline);
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

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.vias.len(), 1);
        assert_eq!(solution.vias[0].at, pt(1.0, 4.0));
        assert!(DRC.check(&p, &solution).is_empty());
    }

    #[test]
    fn postroute_cleanup_drops_partial_via_covered_by_through_via() {
        let mut p = simple_two_point_problem();
        p.layer_count = 4;
        p.connections[0].points_to_connect[0].x = 1.0;
        p.connections[0].points_to_connect[0].y = 1.0;
        p.connections[0].points_to_connect[0].layer = LayerRef::top();
        p.connections[0].points_to_connect[1].x = 5.0;
        p.connections[0].points_to_connect[1].y = 1.0;
        p.connections[0].points_to_connect[1].layer = LayerRef::bottom();
        let mut solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![pt(1.0, 1.0), pt(3.0, 1.0)],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::bottom(),
                    width: p.min_trace_width,
                    path: vec![pt(3.0, 1.0), pt(5.0, 1.0)],
                },
            ],
            vias: vec![
                Via {
                    connection: "N".to_owned(),
                    at: pt(3.0, 1.0),
                    diameter: p.via_diameter,
                    drill: p.via_drill,
                    span: ViaSpan::Through,
                },
                Via {
                    connection: "N".to_owned(),
                    at: pt(3.0, 1.0),
                    diameter: p.via_diameter,
                    drill: p.via_drill,
                    span: ViaSpan::Partial {
                        from: 0,
                        to: 1,
                        micro: false,
                    },
                },
            ],
        };

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert_eq!(solution.vias.len(), 1);
        assert!(matches!(solution.vias[0].span, ViaSpan::Through));
        assert!(DRC.check(&p, &solution).is_empty());
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
        assert!(matches!(
            DRC.check(&p, &solution).as_slice(),
            [pcb_model::Finding::DanglingEnd { net, .. }] if net == "N"
        ));

        postroute_cleanup(&DEPS, &p, &mut solution);

        assert!(
            solution.vias.is_empty(),
            "single-layer dangling via should be dropped"
        );
        assert_eq!(solution.traces[0].path, vec![pt(1.0, 1.0), pt(5.0, 1.0)]);
        assert!(DRC.check(&p, &solution).is_empty());
    }

    #[test]
    fn tuned_route_diagnostics_report_grid_pass() {
        let p = load("quad.json");
        let r = route_tuned_with_diagnostics(&DEPS, &p);
        assert_eq!(r.result.engine, pcb_route_grid::router::ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        let engines: Vec<&str> = r
            .passes
            .iter()
            .map(|attempt| attempt.engine.as_str())
            .collect();
        assert_eq!(engines, vec![pcb_route_grid::router::ENGINE]);
        assert!(
            r.global.is_none(),
            "the tuned grid algorithm does not run the diagnostic mesh"
        );
    }

    #[test]
    fn route_mesh_diagnostics_capture_global_report() {
        let p = load("quad.json");
        let r = route_mesh_with_diagnostics(&DEPS, &p);
        assert_eq!(r.result.engine, ENGINE);
        assert!(r.result.failed.is_empty(), "{:?}", r.result.failed);
        assert_eq!(r.passes.len(), 1);
        assert_eq!(r.passes[0].engine, ENGINE);
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
    //    with finisher provenance; route_tuned then falls back to the grid baseline
    //    if it has fewer failed nets. (See `tests/detailed_gate.rs` for the
    //    geometry note.)

    #[test]
    fn congested_auto_reports_honest_failures() {
        let p = load("congested.json");
        // route_detailed(&DEPS, congested) is the expensive path — call it ONCE and derive
        // route_tuned's outcome from it + naive (route_tuned runs exactly this
        // route_detailed internally, then naive, and returns the fewer-failed result),
        // rather than paying for a second full detailed route.
        let detailed = route_detailed(&DEPS, &p);
        let naive = pcb_route_grid::router::route(&DRC, &p);
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
                .all(|f| f.reason.contains("finisher") || f.reason.contains("DRC oracle")),
            "every congested residual must carry finisher or DRC provenance: {:?}",
            detailed.failed
        );
        let geom_violations: Vec<_> = DRC.check(&p, &detailed.solution)
            .into_iter()
            .filter(|v| !format!("{v:?}").contains("Connectivity"))
            .collect();
        assert!(
            geom_violations.is_empty(),
            "congested detailed copper must be geometry-clean (only connectivity gaps \
             from dropped nets are allowed), got {geom_violations:?}"
        );
        // route_tuned keeps the negotiated primary on equal faults and falls back
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
        let a = route_detailed(&DEPS, &p);
        let b = route_detailed(&DEPS, &p);
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
        let r = route_detailed(&DEPS, &p);
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
