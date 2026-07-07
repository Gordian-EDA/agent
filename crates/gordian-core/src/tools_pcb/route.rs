//! Routing orchestration for the active KiCAD board: route generation, IPC copper
//! write-back, and route-result lint/triage.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde_json::{Value, json};

use drc_lint::connectivity::Violation as ConnViolation;
use drc_lint::lint::{DrcViolation, drop_unconnected_copper, lint};
use kicad_ipc::snapshot::ImportedPart;
use negotiated_mesh::pathing::{GlobalRouteResult, global_route};
use negotiated_mesh::pipeline::{
    RouteEngineAttempt, route_auto_with_diagnostics, route_mesh_with_diagnostics,
    route_sequential_with_diagnostics, select_best,
};
use pcb_model::{
    FailedNet, LayerRef, Point2, RouteProblem, RouteQuality, RouteResult, RouteSolution, Trace,
    Via, ViaSpan,
};

use crate::{AgentRuntime, PcbRouterEngine};

// ── route_board ──────────────────────────────────────────────────────────────

/// A `lint_summary` for a routed solution, split into two buckets.
///
/// The connectivity oracle flags an `Unconnected` violation for EVERY net the
/// router honestly dropped — but a `route_auto` failure is not an engine bug, it
/// is the board being too tight, exactly what the model triages. So we classify:
///
/// - **expected gaps** — `Connectivity::Unconnected` whose net is in the
///   already-reported `failed` set. These are the honest finisher/global drops;
///   they are NOT engine bugs (the model already sees them in `failed`).
/// - **real violations** — geometry defects (clearance, width, via, bounds,
///   invalid layer), any `CrossNetMerge` (a short — always a bug), and any
///   `Unconnected` on a net the router claimed to ROUTE. A non-zero real count
///   means a violation escaped the router's own oracles: an engine bug.
struct LintSplit {
    /// Counts of REAL violations by serde `kind` tag (empty ⇒ clean copper).
    by_kind: Value,
    /// Number of real violations (the engine-bug signal; should be 0).
    real: usize,
    /// Number of expected connectivity gaps from already-failed nets.
    expected_gaps: usize,
}

fn lint_summary(
    rp: &RouteProblem,
    solution: &RouteSolution,
    failed: &[FailedNet],
    plane_nets: &std::collections::BTreeSet<String>,
) -> LintSplit {
    let failed_nets: std::collections::BTreeSet<&str> =
        failed.iter().map(|f| f.connection.as_str()).collect();

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut real = 0usize;
    let mut expected_gaps = 0usize;

    for v in lint(rp, solution) {
        // An Unconnected is an EXPECTED gap (not an engine bug) when:
        //  - the net was already reported failed (an honest finisher/global drop), OR
        //  - the net is a copper PLANE net. A plane net is NOT trace-routed — it was
        //    removed from the routed connections and its pins connect through the inner
        //    plane (emitted at export) plus a per-pad stitching via; any pad whose via
        //    couldn't be placed is already reported as a failed stitch. So the trace-
        //    connectivity oracle, which sees the stitch vias but not the plane copper,
        //    reads every stitched plane pad as "unconnected" — a false signal. KiCAD DRC
        //    (which has the plane) is the authority on real plane connectivity.
        if let DrcViolation::Connectivity {
            violation: ConnViolation::Unconnected { connection, .. },
        } = &v
            && (failed_nets.contains(connection.as_str()) || plane_nets.contains(connection))
        {
            expected_gaps += 1;
            continue;
        }
        // Everything else is a real violation the router should have prevented.
        let kind = serde_json::to_value(&v)
            .ok()
            .and_then(|j| j.get("kind").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_owned());
        *counts.entry(kind).or_default() += 1;
        real += 1;
    }

    let by_kind: Value = counts
        .into_iter()
        .map(|(k, c)| (k, json!(c)))
        .collect::<serde_json::Map<_, _>>()
        .into();
    LintSplit {
        by_kind,
        real,
        expected_gaps,
    }
}

/// Build the congestion-hotspot enrichment for a FAILED route from a global
/// routing report already produced by the negotiated router.
fn congestion_json_from_global(g: &GlobalRouteResult) -> Value {
    let hotspots: Vec<Value> = g
        .report
        .edge_hotspots
        .iter()
        .map(|h| {
            json!({
                "edge": h.edge,
                "leaves": [h.a, h.b],
                "layer": h.layer,
                "usage": h.usage,
                "capacity": h.capacity,
                "load": h.load,
            })
        })
        .collect();
    json!({
        "iterations": g.report.iterations,
        "final_overflow": g.report.final_overflow,
        "hotspots": hotspots,
    })
}

fn congestion_json(rp: &RouteProblem) -> Value {
    // Fallback for explicit non-auto engines, which do not surface negotiated
    // global diagnostics through their simple `RouteResult`.
    let g = global_route(rp);
    congestion_json_from_global(&g)
}

/// When routing leaves nets unrouted, decide whether they CONCENTRATE on one part (a fine-pitch
/// pin-escape bottleneck — not fixable by moving OTHER parts) or are scattered (movable congestion).
/// Returns `(reference, footprint, pins_on_this_part, total_failed_pins)` for the dominant part when
/// it owns a clear majority (≥60%) of the failed-net pad incidences across ≥3 pins; else `None`.
/// Plane-stitch pseudo-failures ("<N plane stitching vias>") are not net names, so they don't match
/// any pad and are naturally excluded.
fn escape_bottleneck(
    parts: &[ImportedPart],
    failed: &[FailedNet],
) -> Option<(String, String, usize, usize)> {
    let failed_nets: std::collections::BTreeSet<&str> =
        failed.iter().map(|f| f.connection.as_str()).collect();
    let mut per_part: Vec<(String, String, usize)> = Vec::new();
    let mut total = 0usize;
    for p in parts {
        let c = p
            .pads
            .iter()
            .filter_map(|(_, net)| net.as_deref())
            .filter(|net| failed_nets.contains(net))
            .count();
        if c > 0 {
            per_part.push((p.reference.clone(), p.lib_id.clone(), c));
            total += c;
        }
    }
    per_part.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let (r, fp, c) = per_part.into_iter().next()?;
    (total >= 3 && c * 100 >= total * 60).then_some((r, fp, c, total))
}

pub fn route_board(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    match route_live_board(ctx) {
        Ok(out) => Ok(out),
        Err(err) => Ok(json!({ "error": err })),
    }
}

fn route_live_board(ctx: &AgentRuntime) -> std::result::Result<Value, String> {
    let mut board = super::active::board_problem(ctx)?;
    if is_seed_placement(&board.imported.bounds, &board.imported.parts) {
        return Err("board has only the initial seed-row footprint positions — run place_board before route_board".to_owned());
    }

    let existing = (board.copper.traces.len(), board.copper.vias.len());
    if existing.0 > 0 || existing.1 > 0 {
        clear_existing_copper(ctx)
            .map_err(|e| format!("could not clear existing copper before reroute: {e}"))?;
        board = super::active::board_problem(ctx)?;
    }

    let rp = board.problem.clone();
    let routed = route_with_engine(&rp, ctx.config().engines.pcb_router);
    let global_diagnostics = routed.global;
    let router_attempts = routed.attempts;
    let mut result = routed.result;
    let _used_direct_fallback = apply_direct_rescue_fallback(&rp, &mut result);
    let dropped_failed = drop_failed_net_copper(&mut result);
    let failed = failed_connections(&result);
    add_terminal_stubs(&rp, &mut result.solution, &failed);
    let original_solution = result.solution.clone();
    let mut pruned_spurs = prune_dangling_spurs_if_safe(&rp, &mut result);
    let dropped = make_route_honest(&rp, &mut result);
    let mut split = lint_summary(&rp, &result.solution, &result.failed, &Default::default());
    if split.real > 0 && pruned_spurs > 0 {
        result.solution = original_solution;
        pruned_spurs = 0;
        split = lint_summary(&rp, &result.solution, &result.failed, &Default::default());
    }
    if split.real > 0 {
        return Err(format!(
            "router produced {} real DRC violation(s); refusing to write copper to live KiCAD board",
            split.real
        ));
    }

    write_route(ctx, &rp, &result.solution, &board.layer_names)
        .map_err(|e| format!("could not write route to live KiCAD board: {e}"))?;

    let failed: Vec<Value> = result
        .failed
        .iter()
        .map(|f| json!({ "connection": f.connection, "reason": f.reason }))
        .collect();
    let congestion = if result.failed.is_empty() {
        Value::Null
    } else if let Some(global) = &global_diagnostics {
        congestion_json_from_global(global)
    } else {
        congestion_json(&rp)
    };
    let escape = escape_bottleneck(&board.imported.parts, &result.failed).map(
        |(reference, footprint, pins_on_part, total_failed_pins)| {
            json!({
                "reference": reference,
                "footprint": footprint,
                "pins_on_part": pins_on_part,
                "total_failed_pins": total_failed_pins,
            })
        },
    );
    let m = result.solution.metrics();
    Ok(json!({
        "router": result.engine,
        "router_attempts": route_attempts_json(&router_attempts),
        "failed": failed,
        "metrics": {
            "wirelength": m.wirelength,
            "vias": m.via_count,
            "traces": m.trace_count,
        },
        "lint_summary": split.by_kind,
        "expected_connectivity_gaps": split.expected_gaps,
        "pruned_dangling_spurs": pruned_spurs,
        "dropped_failed_net_copper": dropped_failed,
        "dropped_violating_nets": dropped,
        "cleared_existing_copper": {
            "traces": existing.0,
            "vias": existing.1,
        },
        "congestion": congestion,
        "escape_bottleneck": escape,
        "note": if result.failed.is_empty() {
            "routed and saved the KiCAD board cleanly"
        } else {
            "routed and saved the KiCAD board with honest failed nets"
        },
    }))
}

fn clear_existing_copper(
    ctx: &AgentRuntime,
) -> std::result::Result<(usize, usize), kicad_ipc::Error> {
    let path = ctx.pcb_path();
    ctx.kicad()
        .with_session(&path, |session| session.kicad().delete_tracks_and_vias())
}

fn make_route_honest(rp: &RouteProblem, result: &mut RouteResult) -> Vec<String> {
    let mut dropped = Vec::new();
    for net in drop_unconnected_copper(rp, &mut result.solution) {
        append_failed(
            result,
            &net,
            "dropped copper: connectivity oracle reported it unconnected",
        );
        dropped.push(net);
    }
    let violating: BTreeSet<String> = lint(rp, &result.solution)
        .iter()
        .flat_map(geometry_violation_nets)
        .collect();
    if !violating.is_empty() {
        result
            .solution
            .traces
            .retain(|trace| !violating.contains(&trace.connection));
        result
            .solution
            .vias
            .retain(|via| !violating.contains(&via.connection));
        for net in violating {
            append_failed(
                result,
                &net,
                "dropped copper: route had geometry DRC violations",
            );
            dropped.push(net);
        }
    }
    for net in drop_unconnected_copper(rp, &mut result.solution) {
        append_failed(
            result,
            &net,
            "dropped copper: connectivity oracle reported it unconnected after geometry cleanup",
        );
        dropped.push(net);
    }
    dropped.sort();
    dropped.dedup();
    dropped
}

fn failed_connections(result: &RouteResult) -> BTreeSet<String> {
    result
        .failed
        .iter()
        .map(|f| f.connection.clone())
        .filter(|name| !name.is_empty())
        .collect()
}

fn drop_failed_net_copper(result: &mut RouteResult) -> Vec<String> {
    let failed = failed_connections(result);
    if failed.is_empty() {
        return Vec::new();
    }
    let before_traces = result.solution.traces.len();
    let before_vias = result.solution.vias.len();
    result
        .solution
        .traces
        .retain(|trace| !failed.contains(&trace.connection));
    result
        .solution
        .vias
        .retain(|via| !failed.contains(&via.connection));
    if result.solution.traces.len() == before_traces && result.solution.vias.len() == before_vias {
        return Vec::new();
    }
    failed.into_iter().collect()
}

fn add_terminal_stubs(
    rp: &RouteProblem,
    solution: &mut RouteSolution,
    skip_connections: &BTreeSet<String>,
) {
    let pitch = grid_astar::grid::grid_pitch(rp);
    for conn in &rp.connections {
        if skip_connections.contains(&conn.name) {
            continue;
        }
        let width = rp.net_width(&conn.name);
        for point in &conn.points_to_connect {
            let cell_x = cell_center(rp.bounds.min_x, point.x, pitch);
            let cell_y = cell_center(rp.bounds.min_y, point.y, pitch);
            let exact = Point2 {
                x: point.x,
                y: point.y,
            };
            let center = Point2 {
                x: cell_x,
                y: cell_y,
            };
            if (exact.x - center.x).abs() < geom::EPS && (exact.y - center.y).abs() < geom::EPS {
                continue;
            }
            solution.traces.push(Trace {
                connection: conn.name.clone(),
                layer: point.layer.clone(),
                width,
                path: vec![exact, center],
            });
        }
    }
}

fn prune_dangling_spurs_if_safe(rp: &RouteProblem, result: &mut RouteResult) -> usize {
    let original = result.solution.clone();
    let removed = prune_dangling_spurs(rp, &mut result.solution);
    if removed > 0
        && lint_summary(rp, &result.solution, &result.failed, &Default::default()).real > 0
    {
        result.solution = original;
        0
    } else {
        removed
    }
}

fn cell_center(min: f64, value: f64, pitch: f64) -> f64 {
    let idx = ((value - min) / pitch).floor().max(0.0);
    min + (idx + 0.5) * pitch
}

fn geometry_violation_nets(v: &DrcViolation) -> Vec<String> {
    match v {
        DrcViolation::ClearanceTraceTrace { a, b, .. } => vec![a.clone(), b.clone()],
        DrcViolation::ClearanceTraceObstacle { connection, .. }
        | DrcViolation::ClearanceViaAny { connection, .. }
        | DrcViolation::TraceWidthBelowMin { connection, .. }
        | DrcViolation::OutOfBounds { connection, .. }
        | DrcViolation::ViaDiameterBelowMin { connection, .. }
        | DrcViolation::InvalidLayer { connection, .. } => vec![connection.clone()],
        DrcViolation::Connectivity { .. } => Vec::new(),
    }
}

fn append_failed(result: &mut RouteResult, connection: &str, reason: &str) {
    if connection.is_empty()
        || result
            .failed
            .iter()
            .any(|f| f.connection == connection && f.reason == reason)
    {
        return;
    }
    result.failed.push(FailedNet {
        connection: connection.to_string(),
        reason: reason.to_string(),
    });
}

struct RouteRun {
    result: RouteResult,
    attempts: Vec<RouteEngineAttempt>,
    global: Option<GlobalRouteResult>,
}

fn route_with_engine(rp: &RouteProblem, engine: PcbRouterEngine) -> RouteRun {
    match engine {
        PcbRouterEngine::Auto => {
            let run = route_auto_with_diagnostics(rp);
            RouteRun {
                result: run.result,
                attempts: run.attempts,
                global: run.global,
            }
        }
        PcbRouterEngine::Astar => {
            let grid = grid_astar::router::GridAStarRouter;
            let result = select_best(rp, &[&grid]);
            RouteRun {
                attempts: vec![route_engine_attempt(rp, &result)],
                result,
                global: None,
            }
        }
        PcbRouterEngine::Mesh => {
            let run = route_mesh_with_diagnostics(rp);
            RouteRun {
                result: run.result,
                attempts: run.attempts,
                global: run.global,
            }
        }
        PcbRouterEngine::Sequential => {
            let run = route_sequential_with_diagnostics(rp);
            RouteRun {
                result: run.result,
                attempts: run.attempts,
                global: run.global,
            }
        }
    }
}

fn route_engine_attempt(rp: &RouteProblem, result: &RouteResult) -> RouteEngineAttempt {
    let geometry_violations = grid_astar::router::geometry_violations(rp, &result.solution);
    let quality = RouteQuality::of(rp, result, geometry_violations);
    RouteEngineAttempt {
        engine: result.engine.clone(),
        failed: result.failed.clone(),
        elapsed_ms: 0,
        fault_weight: quality.fault_weight,
        geometry_violations,
        failed_nets: quality.failed_nets,
        vias: quality.via_count,
        wirelength: quality.wirelength,
    }
}

fn route_attempts_json(attempts: &[RouteEngineAttempt]) -> Value {
    Value::Array(
        attempts
            .iter()
            .map(|attempt| {
                json!({
                    "engine": attempt.engine,
                    "elapsed_ms": attempt.elapsed_ms,
                    "failed": attempt.failed.iter().map(|f| {
                        json!({
                            "connection": f.connection,
                            "reason": f.reason,
                        })
                    }).collect::<Vec<_>>(),
                    "quality": {
                        "fault_weight": attempt.fault_weight,
                        "geometry_violations": attempt.geometry_violations,
                        "failed_nets": attempt.failed_nets,
                        "vias": attempt.vias,
                        "wirelength": attempt.wirelength,
                    }
                })
            })
            .collect(),
    )
}

pub fn apply_direct_rescue_fallback(rp: &RouteProblem, result: &mut RouteResult) -> bool {
    if result.failed.is_empty() {
        return false;
    }
    let failed: std::collections::BTreeSet<String> = result
        .failed
        .iter()
        .map(|f| f.connection.clone())
        .filter(|name| !name.is_empty())
        .collect();
    if failed.is_empty() {
        return false;
    }
    let mut applied = BTreeSet::new();
    let mut rescue_solution = result.solution.clone();
    rescue_solution
        .traces
        .retain(|trace| !failed.contains(&trace.connection));
    rescue_solution
        .vias
        .retain(|via| !failed.contains(&via.connection));
    for conn in &rp.connections {
        if !failed.contains(&conn.name) {
            continue;
        }
        let candidate = if conn.points_to_connect.len() == 2 {
            direct_two_pin_candidate(rp, &rescue_solution, conn)
        } else if (3..=6).contains(&conn.points_to_connect.len()) {
            direct_multi_pin_candidate(rp, &rescue_solution, conn)
        } else {
            None
        };
        if let Some(solution) = candidate {
            rescue_solution = solution;
            applied.insert(conn.name.clone());
        }
    }
    if applied.is_empty() {
        return false;
    }
    result.solution = rescue_solution;
    result.failed.retain(|f| !applied.contains(&f.connection));
    if result.engine != "direct-rescue" {
        result.engine = format!("{}+direct-rescue", result.engine);
    }
    true
}

fn direct_two_pin_candidate(
    rp: &RouteProblem,
    solution: &RouteSolution,
    conn: &pcb_model::Connection,
) -> Option<RouteSolution> {
    let a = conn.points_to_connect[0].point();
    let b = conn.points_to_connect[1].point();
    for layer in direct_candidate_layers_for_conn(rp, conn) {
        for path in direct_candidate_paths(rp, solution, &conn.name, &layer, a, b) {
            let mut candidate = direct_candidate_solution(rp, solution, conn, layer.clone(), path);
            simplify_candidate_paths(&mut candidate);
            if direct_candidate_is_clean(rp, &candidate, &conn.name) {
                return Some(candidate);
            }
        }
    }
    None
}

fn direct_multi_pin_candidate(
    rp: &RouteProblem,
    solution: &RouteSolution,
    conn: &pcb_model::Connection,
) -> Option<RouteSolution> {
    let pairs = direct_multi_pin_tree_pairs(conn);
    for layer in direct_candidate_layers_for_conn(rp, conn) {
        let mut candidate = solution.clone();
        for point in &conn.points_to_connect {
            push_terminal_via_if_needed(rp, &mut candidate, conn, point, &layer);
        }
        let mut ok = true;
        for (a_idx, b_idx) in &pairs {
            let a = conn.points_to_connect[*a_idx].point();
            let b = conn.points_to_connect[*b_idx].point();
            let mut routed_leg = false;
            for path in direct_candidate_paths(rp, &candidate, &conn.name, &layer, a, b) {
                let mut leg = candidate.clone();
                leg.traces.push(Trace {
                    connection: conn.name.clone(),
                    layer: layer.clone(),
                    width: rp.net_width(&conn.name),
                    path,
                });
                simplify_candidate_paths(&mut leg);
                if direct_candidate_is_geometry_clean(rp, &leg, &conn.name) {
                    candidate = leg;
                    routed_leg = true;
                    break;
                }
            }
            if !routed_leg {
                ok = false;
                break;
            }
        }
        if ok && direct_candidate_is_clean(rp, &candidate, &conn.name) {
            return Some(candidate);
        }
    }
    None
}

fn direct_multi_pin_tree_pairs(conn: &pcb_model::Connection) -> Vec<(usize, usize)> {
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
                            direct_multi_pin_tree_pair_better(
                                conn, &positions, ai, bi, old_a, old_b,
                            )
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

fn direct_multi_pin_tree_pair_better(
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

    let layer_change = direct_multi_pin_pair_requires_layer_change(conn, a, b);
    let old_layer_change = direct_multi_pin_pair_requires_layer_change(conn, old_a, old_b);
    if layer_change != old_layer_change {
        return !layer_change;
    }

    (a, b) < (old_a, old_b)
}

fn direct_multi_pin_pair_requires_layer_change(
    conn: &pcb_model::Connection,
    a: usize,
    b: usize,
) -> bool {
    conn.points_to_connect[a].layer != conn.points_to_connect[b].layer
}

fn direct_candidate_paths(
    rp: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
    layer: &LayerRef,
    a: Point2,
    b: Point2,
) -> Vec<Vec<Point2>> {
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    push_candidate_path(&mut paths, &mut seen, vec![a, b]);
    let mut xs = vec![a.x, b.x, (a.x + b.x) / 2.0];
    let mut ys = vec![a.y, b.y, (a.y + b.y) / 2.0];
    let route_width = rp.net_width(connection);
    let axis_clearance = route_width.max(rp.min_trace_width);
    let route_radius = axis_clearance / 2.0;
    let inset = (rp.clearance + axis_clearance + 0.5).max(1.0);
    xs.extend([rp.bounds.min_x + inset, rp.bounds.max_x - inset]);
    ys.extend([rp.bounds.min_y + inset, rp.bounds.max_y - inset]);
    for obstacle in &rp.obstacles {
        if !obstacle
            .layers
            .iter()
            .any(|ob_layer| same_layer_ref(ob_layer, layer, rp.layer_count))
        {
            continue;
        }
        let dx = obstacle.width / 2.0 + rp.clearance + axis_clearance;
        let dy = obstacle.height / 2.0 + rp.clearance + axis_clearance;
        xs.extend([obstacle.center.x - dx, obstacle.center.x + dx]);
        ys.extend([obstacle.center.y - dy, obstacle.center.y + dy]);
    }
    for trace in &solution.traces {
        if trace.connection == connection || !same_layer_ref(&trace.layer, layer, rp.layer_count) {
            continue;
        }
        let Some(first) = trace.path.first() else {
            continue;
        };
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.x, first.x, first.y, first.y);
        for point in &trace.path {
            min_x = min_x.min(point.x);
            max_x = max_x.max(point.x);
            min_y = min_y.min(point.y);
            max_y = max_y.max(point.y);
        }
        let d = trace.width / 2.0 + rp.clearance + route_radius;
        xs.extend([min_x - d, max_x + d]);
        ys.extend([min_y - d, max_y + d]);
    }
    for via in &solution.vias {
        if via.connection == connection {
            continue;
        }
        let d = rp.via_diameter / 2.0 + rp.clearance + route_radius;
        xs.extend([via.at.x - d, via.at.x + d]);
        ys.extend([via.at.y - d, via.at.y + d]);
    }
    xs.retain(|x| *x >= rp.bounds.min_x + inset && *x <= rp.bounds.max_x - inset);
    ys.retain(|y| *y >= rp.bounds.min_y + inset && *y <= rp.bounds.max_y - inset);
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    xs.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    ys.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    let near_mid = |v: f64, lo: f64, hi: f64| {
        let mid = (lo + hi) / 2.0;
        ((v - mid).abs() * 1000.0).round() as i64
    };
    xs.sort_by_key(|x| near_mid(*x, a.x.min(b.x), a.x.max(b.x)));
    ys.sort_by_key(|y| near_mid(*y, a.y.min(b.y), a.y.max(b.y)));
    for x in xs.into_iter().take(16) {
        push_candidate_path(
            &mut paths,
            &mut seen,
            vec![a, Point2 { x, y: a.y }, Point2 { x, y: b.y }, b],
        );
    }
    for y in ys.into_iter().take(16) {
        push_candidate_path(
            &mut paths,
            &mut seen,
            vec![a, Point2 { x: a.x, y }, Point2 { x: b.x, y }, b],
        );
    }
    paths
}

fn push_candidate_path(
    paths: &mut Vec<Vec<Point2>>,
    seen: &mut BTreeSet<Vec<(i64, i64)>>,
    path: Vec<Point2>,
) {
    let path = geom::Polyline::new(path).simplify().into_points();
    if path.len() < 2 {
        return;
    }
    let key: Vec<(i64, i64)> = path
        .iter()
        .map(|p| (quantize_mm(p.x), quantize_mm(p.y)))
        .collect();
    if seen.insert(key) {
        paths.push(path);
    }
}

fn direct_candidate_solution(
    rp: &RouteProblem,
    solution: &RouteSolution,
    conn: &pcb_model::Connection,
    layer: LayerRef,
    path: Vec<Point2>,
) -> RouteSolution {
    let mut candidate = solution.clone();
    push_terminal_via_if_needed(rp, &mut candidate, conn, &conn.points_to_connect[0], &layer);
    push_terminal_via_if_needed(rp, &mut candidate, conn, &conn.points_to_connect[1], &layer);
    candidate.traces.push(Trace {
        connection: conn.name.clone(),
        layer,
        width: rp.net_width(&conn.name),
        path,
    });
    candidate
}

fn push_terminal_via_if_needed(
    rp: &RouteProblem,
    solution: &mut RouteSolution,
    conn: &pcb_model::Connection,
    point: &pcb_model::RoutePoint,
    route_layer: &LayerRef,
) {
    if same_layer_ref(&point.layer, route_layer, rp.layer_count) {
        return;
    }
    let at = point.point();
    if solution.vias.iter().any(|via| {
        via.connection == conn.name
            && quantize_mm(via.at.x) == quantize_mm(at.x)
            && quantize_mm(via.at.y) == quantize_mm(at.y)
    }) {
        return;
    }
    solution.vias.push(Via {
        connection: conn.name.clone(),
        at,
        diameter: rp.via_diameter,
        drill: rp.via_drill,
        span: ViaSpan::Through,
    });
}

fn same_layer_ref(a: &LayerRef, b: &LayerRef, layer_count: u32) -> bool {
    a == b || (a.index(layer_count).is_some() && a.index(layer_count) == b.index(layer_count))
}

fn simplify_candidate_paths(solution: &mut RouteSolution) {
    for trace in &mut solution.traces {
        trace.path = geom::Polyline::new(std::mem::take(&mut trace.path))
            .simplify()
            .into_points();
    }
}

fn direct_candidate_layers(layer_count: u32) -> Vec<LayerRef> {
    let plane_layers: BTreeSet<u32> = grid_astar::router::plane_layers(layer_count as usize)
        .into_iter()
        .collect();
    let mut layers = Vec::new();
    for idx in 0..layer_count.max(1) {
        if plane_layers.contains(&idx) {
            continue;
        }
        layers.push(match idx {
            0 => LayerRef::top(),
            i if i + 1 == layer_count => LayerRef::bottom(),
            i => LayerRef(format!("inner{i}")),
        });
    }
    layers
}

fn direct_candidate_layers_for_conn(
    rp: &RouteProblem,
    conn: &pcb_model::Connection,
) -> Vec<LayerRef> {
    let all = direct_candidate_layers(rp.layer_count);
    let mut ordered = Vec::new();
    for point in &conn.points_to_connect {
        if let Some(layer) = all
            .iter()
            .find(|layer| same_layer_ref(&point.layer, layer, rp.layer_count))
        {
            push_layer_once(&mut ordered, layer.clone(), rp.layer_count);
        }
    }
    for layer in all {
        push_layer_once(&mut ordered, layer, rp.layer_count);
    }
    ordered
}

fn push_layer_once(layers: &mut Vec<LayerRef>, layer: LayerRef, layer_count: u32) {
    if !layers
        .iter()
        .any(|existing| same_layer_ref(existing, &layer, layer_count))
    {
        layers.push(layer);
    }
}

fn direct_candidate_is_clean(
    rp: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
) -> bool {
    for violation in lint(rp, solution) {
        match violation {
            DrcViolation::Connectivity {
                violation:
                    ConnViolation::Unconnected {
                        connection: ref net,
                        ..
                    },
            } if net == connection => return false,
            DrcViolation::Connectivity {
                violation: ConnViolation::CrossNetMerge { ref a, ref b },
            } if a == connection || b == connection => return false,
            DrcViolation::Connectivity { .. } => {}
            _ => return false,
        }
    }
    true
}

fn direct_candidate_is_geometry_clean(
    rp: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
) -> bool {
    for violation in lint(rp, solution) {
        match violation {
            DrcViolation::Connectivity {
                violation: ConnViolation::CrossNetMerge { ref a, ref b },
            } if a == connection || b == connection => return false,
            DrcViolation::Connectivity { .. } => {}
            _ => return false,
        }
    }
    true
}

#[derive(Clone, Debug)]
struct RouteSegment {
    connection: String,
    layer: LayerRef,
    width: f64,
    start: Point2,
    end: Point2,
}

fn prune_dangling_spurs(rp: &RouteProblem, solution: &mut RouteSolution) -> usize {
    let mut segments = flatten_segments(solution);
    if segments.is_empty() {
        return 0;
    }
    let protected = protected_route_nodes(rp, solution);
    let mut alive = vec![true; segments.len()];
    let mut removed = 0usize;

    loop {
        let mut degree: BTreeMap<(String, u32, i64, i64), usize> = BTreeMap::new();
        for (idx, segment) in segments.iter().enumerate() {
            if !alive[idx] {
                continue;
            }
            for key in [
                node_key(
                    &segment.connection,
                    &segment.layer,
                    segment.start,
                    rp.layer_count,
                ),
                node_key(
                    &segment.connection,
                    &segment.layer,
                    segment.end,
                    rp.layer_count,
                ),
            ] {
                *degree.entry(key).or_default() += 1;
            }
        }

        let mut changed = false;
        for (idx, segment) in segments.iter().enumerate() {
            if !alive[idx] {
                continue;
            }
            let a = node_key(
                &segment.connection,
                &segment.layer,
                segment.start,
                rp.layer_count,
            );
            let b = node_key(
                &segment.connection,
                &segment.layer,
                segment.end,
                rp.layer_count,
            );
            let a_dangles = degree.get(&a).copied().unwrap_or(0) <= 1 && !protected.contains(&a);
            let b_dangles = degree.get(&b).copied().unwrap_or(0) <= 1 && !protected.contains(&b);
            if a_dangles || b_dangles {
                alive[idx] = false;
                removed += 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    if removed > 0 {
        solution.traces = segments
            .drain(..)
            .zip(alive)
            .filter_map(|(segment, alive)| {
                alive.then_some(Trace {
                    connection: segment.connection,
                    layer: segment.layer,
                    width: segment.width,
                    path: vec![segment.start, segment.end],
                })
            })
            .collect();
    }
    removed
}

fn flatten_segments(solution: &RouteSolution) -> Vec<RouteSegment> {
    let mut segments = Vec::new();
    for trace in &solution.traces {
        for pair in trace.path.windows(2) {
            segments.push(RouteSegment {
                connection: trace.connection.clone(),
                layer: trace.layer.clone(),
                width: trace.width,
                start: pair[0],
                end: pair[1],
            });
        }
    }
    segments
}

fn protected_route_nodes(
    rp: &RouteProblem,
    solution: &RouteSolution,
) -> BTreeSet<(String, u32, i64, i64)> {
    let mut protected = BTreeSet::new();
    for conn in &rp.connections {
        for point in &conn.points_to_connect {
            protected.insert(node_key(
                &conn.name,
                &point.layer,
                point.point(),
                rp.layer_count,
            ));
        }
    }
    for via in &solution.vias {
        for layer in via_span_indices(&via.span, rp.layer_count) {
            protected.insert((
                via.connection.clone(),
                layer,
                quantize_mm(via.at.x),
                quantize_mm(via.at.y),
            ));
        }
    }
    protected
}

fn via_span_indices(span: &ViaSpan, layer_count: u32) -> Vec<u32> {
    match *span {
        ViaSpan::Through => (0..layer_count.max(1)).collect(),
        ViaSpan::Partial { from, to, .. } => {
            let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
            (lo..=hi).filter(|idx| *idx < layer_count).collect()
        }
    }
}

fn node_key(
    connection: &str,
    layer: &LayerRef,
    point: Point2,
    layer_count: u32,
) -> (String, u32, i64, i64) {
    (
        connection.to_string(),
        layer.index(layer_count).unwrap_or(0),
        quantize_mm(point.x),
        quantize_mm(point.y),
    )
}

fn quantize_mm(v: f64) -> i64 {
    (v * 1_000_000.0).round() as i64
}

fn write_route(
    ctx: &AgentRuntime,
    rp: &RouteProblem,
    solution: &RouteSolution,
    layer_names: &[String],
) -> std::result::Result<(), kicad_ipc::Error> {
    let path = ctx.pcb_path();
    ctx.kicad().with_session(&path, |session| {
        session
            .kicad()
            .create_route_solution(rp, solution, layer_names)
    })
}

fn is_seed_placement(bounds: &pcb_model::Rect, parts: &[ImportedPart]) -> bool {
    if parts.is_empty() {
        return false;
    }
    let mut coords: Vec<_> = parts
        .iter()
        .map(|p| (p.at.x, p.at.y, p.rotation as f64))
        .collect();
    coords.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    coords.iter().enumerate().all(|(idx, (x, y, rotation))| {
        let expected_x = bounds.min_x + 2.0 + 2.54 * idx as f64;
        let expected_y = bounds.min_y + 2.0;
        (x - expected_x).abs() < geom::EPS
            && (y - expected_y).abs() < geom::EPS
            && rotation.abs() < geom::EPS
    })
}

#[cfg(test)]
mod escape_bottleneck_tests {
    use super::*;

    fn part(reference: &str, footprint: &str, nets: &[(&str, &str)]) -> ImportedPart {
        ImportedPart {
            reference: reference.to_owned(),
            lib_id: footprint.to_owned(),
            at: Point2 { x: 0.0, y: 0.0 },
            rotation: 0,
            locked: false,
            pads: nets
                .iter()
                .map(|(p, n)| (p.to_string(), Some(n.to_string())))
                .collect(),
        }
    }
    fn failed(nets: &[&str]) -> Vec<FailedNet> {
        nets.iter()
            .map(|n| FailedNet {
                connection: n.to_string(),
                reason: String::new(),
            })
            .collect()
    }

    #[test]
    fn direct_rescue_candidate_path_simplifies_before_dedupe() {
        let mut paths = Vec::new();
        let mut seen = BTreeSet::new();

        push_candidate_path(
            &mut paths,
            &mut seen,
            vec![
                Point2 { x: 1.0, y: 1.0 },
                Point2 { x: 1.0, y: 1.0 },
                Point2 { x: 3.0, y: 1.0 },
                Point2 { x: 5.0, y: 1.0 },
            ],
        );
        push_candidate_path(
            &mut paths,
            &mut seen,
            vec![Point2 { x: 1.0, y: 1.0 }, Point2 { x: 5.0, y: 1.0 }],
        );

        assert_eq!(
            paths,
            vec![vec![Point2 { x: 1.0, y: 1.0 }, Point2 { x: 5.0, y: 1.0 }]]
        );
    }

    #[test]
    fn concentrated_failures_on_one_part_are_an_escape_bottleneck() {
        // A BGA whose 4 inner pins fail + a cap with 1 unrelated fail → 4/5 on U1 (≥60%) → flagged.
        let parts = vec![
            part(
                "U1",
                "Package_BGA:BGA-100",
                &[("A1", "S1"), ("A2", "S2"), ("A3", "S3"), ("A4", "S4")],
            ),
            part("C1", "Capacitor_SMD:C_0402", &[("1", "VCC"), ("2", "GNDX")]),
        ];
        let f = failed(&["S1", "S2", "S3", "S4", "GNDX"]);
        let got = escape_bottleneck(&parts, &f).expect("should flag a bottleneck");
        assert_eq!((got.0.as_str(), got.2, got.3), ("U1", 4, 5));
    }

    #[test]
    fn scattered_failures_are_not_a_bottleneck() {
        // 1 fail each on three different parts → no single part ≥60% → None (movable congestion).
        let parts = vec![
            part("U1", "fp", &[("1", "A")]),
            part("U2", "fp", &[("1", "B")]),
            part("U3", "fp", &[("1", "C")]),
        ];
        assert!(escape_bottleneck(&parts, &failed(&["A", "B", "C"])).is_none());
    }

    #[test]
    fn plane_stitch_pseudo_failures_are_ignored() {
        // "<N plane stitching vias>" is not a net name → matches no pad → no bottleneck.
        let parts = vec![part("U1", "fp", &[("1", "GND")])];
        assert!(escape_bottleneck(&parts, &failed(&["<7 plane stitching vias>"])).is_none());
    }

    #[test]
    fn explicit_astar_engine_reports_route_attempt_diagnostics() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 6.0,
                max_y: 3.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };

        let run = route_with_engine(&problem, PcbRouterEngine::Astar);

        assert_eq!(run.attempts.len(), 1);
        assert_eq!(run.attempts[0].engine, run.result.engine);
        assert_eq!(run.attempts[0].failed_nets, 0);
        assert_eq!(run.attempts[0].geometry_violations, 0);
    }

    #[test]
    fn explicit_sequential_engine_reports_route_attempt_diagnostics() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 6.0,
                max_y: 3.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };

        let run = route_with_engine(&problem, PcbRouterEngine::Sequential);

        assert_eq!(run.attempts.len(), 1);
        assert_eq!(run.attempts[0].engine, run.result.engine);
        assert_eq!(run.attempts[0].failed_nets, 0);
        assert_eq!(run.attempts[0].geometry_violations, 0);
    }

    #[test]
    fn direct_rescue_fallback_rescues_clean_failed_net() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: -1.0,
                min_y: -1.0,
                max_x: 6.0,
                max_y: 2.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: [("FAT_POWER".to_string(), 2.0)].into_iter().collect(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: failed(&["SIG"]),
            engine: "naive".to_string(),
        };

        assert!(apply_direct_rescue_fallback(&problem, &mut result));
        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), 1);
        assert_eq!(result.engine, "naive+direct-rescue");
    }

    #[test]
    fn direct_fallback_ignores_stale_failed_net_copper() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 6.0,
                max_y: 2.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "SIG".to_string(),
                    layer: LayerRef::top(),
                    width: 0.2,
                    path: vec![Point2 { x: -2.0, y: -2.0 }, Point2 { x: -1.0, y: -1.0 }],
                }],
                vias: vec![],
            },
            failed: failed(&["SIG"]),
            engine: "naive".to_string(),
        };

        assert!(apply_direct_rescue_fallback(&problem, &mut result));
        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), 1);
        assert!(
            result.solution.traces[0]
                .path
                .iter()
                .all(|p| p.x >= 0.0 && p.y >= 0.0),
            "stale out-of-bounds failed copper must not survive"
        );
    }

    #[test]
    fn direct_rescue_fallback_rescues_clean_dogleg() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![pcb_model::Obstacle {
                kind: "rect".to_string(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 3.0, y: 3.0 },
                width: 0.8,
                height: 0.8,
                connected_to: vec!["OTHER".to_string()],
            }],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 7.0,
                max_y: 7.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: failed(&["SIG"]),
            engine: "naive".to_string(),
        };

        assert!(apply_direct_rescue_fallback(&problem, &mut result));
        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), 1);
        assert!(result.solution.traces[0].path.len() >= 3);
    }

    #[test]
    fn direct_rescue_fallback_uses_existing_copper_axes_for_local_detour() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![
                pcb_model::Connection {
                    name: "SIG".to_string(),
                    points_to_connect: vec![
                        pcb_model::RoutePoint {
                            x: 1.0,
                            y: 5.0,
                            layer: LayerRef::top(),
                        },
                        pcb_model::RoutePoint {
                            x: 9.0,
                            y: 5.0,
                            layer: LayerRef::top(),
                        },
                    ],
                },
                pcb_model::Connection {
                    name: "OTHER".to_string(),
                    points_to_connect: vec![
                        pcb_model::RoutePoint {
                            x: 4.0,
                            y: 2.0,
                            layer: LayerRef::top(),
                        },
                        pcb_model::RoutePoint {
                            x: 4.0,
                            y: 8.0,
                            layer: LayerRef::top(),
                        },
                    ],
                },
            ],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "OTHER".to_string(),
                    layer: LayerRef::top(),
                    width: 0.2,
                    path: vec![Point2 { x: 4.0, y: 2.0 }, Point2 { x: 4.0, y: 8.0 }],
                }],
                vias: vec![],
            },
            failed: failed(&["SIG"]),
            engine: "detailed".to_string(),
        };

        assert!(apply_direct_rescue_fallback(&problem, &mut result));

        assert!(result.failed.is_empty());
        let sig = result
            .solution
            .traces
            .iter()
            .find(|trace| trace.connection == "SIG")
            .expect("rescue should add SIG trace");
        assert!(
            sig.path.iter().any(|point| (point.y - 1.65).abs() < 0.05),
            "rescue should use the local clearance axis generated from OTHER copper, got {:?}",
            sig.path
        );
        assert!(lint(&problem, &result.solution).is_empty());
    }

    #[test]
    fn direct_rescue_fallback_sizes_obstacle_axes_for_fat_net_width() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![pcb_model::Obstacle {
                kind: "rect".to_string(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 4.0, y: 4.0 },
                width: 0.8,
                height: 0.8,
                connected_to: vec!["OTHER".to_string()],
            }],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 4.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 7.0,
                        y: 4.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 8.0,
                max_y: 8.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: [("SIG".to_string(), 1.0)].into_iter().collect(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: failed(&["SIG"]),
            engine: "detailed".to_string(),
        };

        assert!(apply_direct_rescue_fallback(&problem, &mut result));

        assert!(result.failed.is_empty());
        let sig = result
            .solution
            .traces
            .iter()
            .find(|trace| trace.connection == "SIG")
            .expect("rescue should add SIG trace");
        assert_eq!(sig.width, 1.0);
        assert!(
            sig.path
                .iter()
                .any(|point| (point.y - 2.45).abs() < 0.05 || (point.y - 5.55).abs() < 0.05),
            "fat-net rescue should use width-aware local obstacle axes instead of board-edge axes: {:?}",
            sig.path
        );
        assert!(lint(&problem, &result.solution).is_empty());
    }

    #[test]
    fn direct_rescue_fallback_rejects_foreign_copper() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![pcb_model::Obstacle {
                kind: "rect".to_string(),
                layers: vec![LayerRef::top(), LayerRef::bottom()],
                center: Point2 { x: 2.0, y: 0.0 },
                width: 0.5,
                height: 0.5,
                connected_to: vec!["OTHER".to_string()],
            }],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 0.0,
                        y: 0.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 4.0,
                        y: 0.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: -1.0,
                min_y: -1.0,
                max_x: 5.0,
                max_y: 1.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: failed(&["SIG"]),
            engine: "naive".to_string(),
        };

        assert!(!apply_direct_rescue_fallback(&problem, &mut result));
        assert_eq!(result.failed.len(), 1);
        assert!(result.solution.traces.is_empty());
    }

    #[test]
    fn direct_rescue_fallback_can_escape_to_bottom_layer_with_vias() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![pcb_model::Obstacle {
                kind: "rect".to_string(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 3.0, y: 3.0 },
                width: 0.8,
                height: 6.5,
                connected_to: vec!["OTHER".to_string()],
            }],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 3.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 3.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 6.0,
                max_y: 6.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let direct = negotiated_mesh::direct::route_direct(&problem);
        assert!(!direct.failed.is_empty(), "direct router must not add vias");
        assert!(direct.solution.vias.is_empty());

        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: failed(&["SIG"]),
            engine: "detailed".to_string(),
        };

        assert!(apply_direct_rescue_fallback(&problem, &mut result));
        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), 1);
        assert_eq!(result.solution.traces[0].layer, LayerRef::bottom());
        assert_eq!(result.solution.vias.len(), 2);
        assert_eq!(result.engine, "detailed+direct-rescue");
    }

    #[test]
    fn direct_rescue_fallback_keeps_bottom_layer_net_via_free() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::bottom(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 1.0,
                        layer: LayerRef::bottom(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 6.0,
                max_y: 2.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: failed(&["SIG"]),
            engine: "detailed".to_string(),
        };

        assert!(apply_direct_rescue_fallback(&problem, &mut result));

        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), 1);
        assert_eq!(result.solution.traces[0].layer, LayerRef::bottom());
        assert!(
            result.solution.vias.is_empty(),
            "bottom-layer terminals already on the rescued route layer should not get redundant vias"
        );
    }

    #[test]
    fn direct_rescue_fallback_uses_one_via_for_mixed_layer_pair() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 1.0,
                        layer: LayerRef::bottom(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 6.0,
                max_y: 2.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: failed(&["SIG"]),
            engine: "detailed".to_string(),
        };

        assert!(apply_direct_rescue_fallback(&problem, &mut result));

        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), 1);
        assert_eq!(
            result.solution.vias.len(),
            1,
            "only the off-layer terminal should receive a rescue via"
        );
    }

    #[test]
    fn direct_fallback_rescues_clean_small_multi_pin_net() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "BUS".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 8.0,
                max_y: 8.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: failed(&["BUS"]),
            engine: "naive".to_string(),
        };

        assert!(apply_direct_rescue_fallback(&problem, &mut result));
        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), 2);
    }

    #[test]
    fn direct_multi_pin_rescue_uses_nearest_tree_topology() {
        let conn = pcb_model::Connection {
            name: "BUS".to_string(),
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
                pcb_model::RoutePoint {
                    x: 9.0,
                    y: 2.0,
                    layer: LayerRef::top(),
                },
            ],
        };

        let pairs = direct_multi_pin_tree_pairs(&conn);

        assert_eq!(
            pairs,
            vec![(0, 1), (1, 2)],
            "direct rescue should extend multi-pin nets through the nearest routed point, not a fixed first-pin star"
        );
    }

    #[test]
    fn direct_multi_pin_rescue_prefers_same_layer_edge_on_tie() {
        let conn = pcb_model::Connection {
            name: "BUS".to_string(),
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

        let pairs = direct_multi_pin_tree_pairs(&conn);

        assert_eq!(
            pairs.first().copied(),
            Some((0, 2)),
            "equal-length direct rescue tree edges should prefer same-layer endpoints: {pairs:?}"
        );
    }

    #[test]
    fn dangling_route_spurs_are_pruned_before_write() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "GND".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 0.0,
                        y: 0.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 4.0,
                        y: 0.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "GND".to_string(),
                    layer: LayerRef::top(),
                    width: 0.4,
                    path: vec![
                        Point2 { x: 0.0, y: 0.0 },
                        Point2 { x: 2.0, y: 0.0 },
                        Point2 { x: 4.0, y: 0.0 },
                    ],
                },
                Trace {
                    connection: "GND".to_string(),
                    layer: LayerRef::top(),
                    width: 0.25,
                    path: vec![Point2 { x: 2.0, y: 0.0 }, Point2 { x: 2.0, y: 1.0 }],
                },
            ],
            vias: vec![],
        };

        let removed = prune_dangling_spurs(&problem, &mut solution);

        assert_eq!(removed, 1);
        assert_eq!(solution.traces.len(), 2);
        assert!(
            !solution
                .traces
                .iter()
                .any(|trace| trace.path.contains(&Point2 { x: 2.0, y: 1.0 }))
        );
    }

    #[test]
    fn unsafe_dangling_spur_prune_is_rolled_back() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![pcb_model::Obstacle {
                kind: "rect".to_string(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 3.0, y: 2.0 },
                width: 0.6,
                height: 0.6,
                connected_to: vec!["SIG".to_string()],
            }],
            connections: vec![pcb_model::Connection {
                name: "SIG".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 5.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 3.0,
                        y: 2.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let mut result = RouteResult {
            engine: "test".to_string(),
            failed: vec![],
            solution: RouteSolution {
                traces: vec![
                    Trace {
                        connection: "SIG".to_string(),
                        layer: LayerRef::top(),
                        width: 0.25,
                        path: vec![
                            Point2 { x: 1.0, y: 1.0 },
                            Point2 { x: 3.0, y: 1.0 },
                            Point2 { x: 5.0, y: 1.0 },
                        ],
                    },
                    Trace {
                        connection: "SIG".to_string(),
                        layer: LayerRef::top(),
                        width: 0.25,
                        path: vec![Point2 { x: 3.0, y: 2.2 }, Point2 { x: 3.0, y: 1.0 }],
                    },
                ],
                vias: vec![],
            },
        };
        let initial_findings = lint(&problem, &result.solution);
        assert!(
            initial_findings.is_empty(),
            "initial lint: {initial_findings:?}"
        );

        let mut pruned = result.solution.clone();
        assert_eq!(prune_dangling_spurs(&problem, &mut pruned), 1);
        assert!(lint_summary(&problem, &pruned, &[], &Default::default()).real > 0);

        assert_eq!(prune_dangling_spurs_if_safe(&problem, &mut result), 0);
        assert_eq!(result.solution.traces.len(), 2);
        assert_eq!(
            lint_summary(&problem, &result.solution, &[], &Default::default()).real,
            0
        );
    }

    #[test]
    fn failed_net_copper_is_removed_before_terminal_stubs() {
        let mut result = RouteResult {
            engine: "test".to_string(),
            failed: vec![FailedNet {
                connection: "RUN".to_string(),
                reason: "unrouted".to_string(),
            }],
            solution: RouteSolution {
                traces: vec![
                    Trace {
                        connection: "RUN".to_string(),
                        layer: LayerRef::top(),
                        width: 0.15,
                        path: vec![Point2 { x: 0.0, y: 0.0 }, Point2 { x: 1.0, y: 0.0 }],
                    },
                    Trace {
                        connection: "OK".to_string(),
                        layer: LayerRef::top(),
                        width: 0.15,
                        path: vec![Point2 { x: 2.0, y: 0.0 }, Point2 { x: 3.0, y: 0.0 }],
                    },
                ],
                vias: vec![pcb_model::Via {
                    connection: "RUN".to_string(),
                    at: Point2 { x: 0.5, y: 0.0 },
                    diameter: 0.6,
                    drill: 0.3,
                    span: ViaSpan::Through,
                }],
            },
        };

        let dropped = drop_failed_net_copper(&mut result);

        assert_eq!(dropped, vec!["RUN"]);
        assert_eq!(result.solution.traces.len(), 1);
        assert_eq!(result.solution.traces[0].connection, "OK");
        assert!(result.solution.vias.is_empty());
    }
}
