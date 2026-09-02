//! Routing orchestration for the active KiCAD board: route generation, IPC copper
//! write-back, and route-result lint/triage.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use kicad_board::ImportedPart;
use pcb_model::Violation as ConnViolation;
use pcb_engine::check as lint;
use pcb_model::Finding as DrcViolation;
use pcb_model::{
    FailedNet, LayerRef, Point2, RouteResult, RouteSolution, RoutingView, Trace, Via, ViaSpan,
};
use pcb_route_mesh::copper::copper_obstacles;
use pcb_route_mesh::pathing::GlobalRouteResult;
use pcb_engine::{postroute_cleanup, prepare_wide_terminal_escapes};
use pcb_route_mesh::pipeline::RoutePassReport;

use gordian_runtime::AgentRuntime;

// ── route_board ──────────────────────────────────────────────────────────────

/// A `lint_summary` for a routed solution, split into two buckets.
///
/// The connectivity oracle flags an `Unconnected` violation for EVERY net the
/// router honestly dropped — but a `route_tuned` failure is not an engine bug, it
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
    rp: &RoutingView,
    solution: &RouteSolution,
    failed: &[FailedNet],
    plane_nets: &std::collections::BTreeSet<String>,
) -> LintSplit {
    let violations = lint(rp, solution);
    lint_summary_from_violations(&violations, failed, plane_nets)
}

/// An `Unconnected` is an EXPECTED gap (not an engine bug) when:
///  - the net was already reported failed (an honest finisher/global drop), OR
///  - the net is a copper PLANE net. A plane net is NOT trace-routed — it was
///    removed from the routed connections and its pins connect through the full-board
///    plane (emitted at export) plus a per-pad stitching via; any pad whose via
///    couldn't be placed is already reported as a failed stitch. So the trace-
///    connectivity oracle, which sees the stitch vias but not the plane copper,
///    reads every stitched plane pad as "unconnected" — a false signal. KiCAD DRC
///    (which has the plane) is the authority on real plane connectivity.
fn is_expected_gap(
    v: &DrcViolation,
    failed_nets: &BTreeSet<&str>,
    plane_nets: &BTreeSet<String>,
) -> bool {
    matches!(
        v,
        DrcViolation::Connectivity {
            violation: ConnViolation::Unconnected { connection, .. },
        } if failed_nets.contains(connection.as_str()) || plane_nets.contains(connection)
    )
}

/// The violations a refusal must explain: everything the split counts as real.
fn real_violations(
    violations: &[DrcViolation],
    failed: &[FailedNet],
    plane_nets: &BTreeSet<String>,
) -> Vec<DrcViolation> {
    let failed_nets: BTreeSet<&str> = failed.iter().map(|f| f.connection.as_str()).collect();
    violations
        .iter()
        .filter(|v| !is_expected_gap(v, &failed_nets, plane_nets))
        .cloned()
        .collect()
}

fn lint_summary_from_violations(
    violations: &[DrcViolation],
    failed: &[FailedNet],
    plane_nets: &std::collections::BTreeSet<String>,
) -> LintSplit {
    let failed_nets: std::collections::BTreeSet<&str> =
        failed.iter().map(|f| f.connection.as_str()).collect();

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut real = 0usize;
    let mut expected_gaps = 0usize;

    for v in violations {
        if is_expected_gap(v, &failed_nets, plane_nets) {
            expected_gaps += 1;
            continue;
        }
        // Everything else is a real violation the router should have prevented.
        let kind = serde_json::to_value(v)
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

fn route_congestion_json(global: Option<&GlobalRouteResult>, route_failed: bool) -> Value {
    if !route_failed {
        return Value::Null;
    }
    global.map_or(Value::Null, congestion_json_from_global)
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
            .filter_map(|pad| pad.net.as_deref())
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

#[tracing::instrument(skip_all, fields(project = %ctx.project_dir().display()))]
pub fn route_board(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let nets = match requested_nets(&input) {
        Ok(nets) => nets,
        Err(message) => return Ok(refusal(message)),
    };
    match route_live_board(ctx, nets) {
        Ok(out) | Err(out) => Ok(out),
    }
}

/// A recoverable routing failure, as the JSON payload the caller receives.
fn refusal(message: impl Into<String>) -> Value {
    json!({ "error": message.into() })
}

/// The nets a `route_board` call is allowed to touch.
///
/// `nets` names a subset — the repair loop after a `move_parts`: everything else
/// keeps the copper it already has, and that copper stays in the problem as
/// obstacle so the new routes go around it.
fn requested_nets(input: &Value) -> std::result::Result<Option<BTreeSet<String>>, String> {
    let value = match input.get("nets") {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let Some(items) = value.as_array() else {
        return Err("nets must be an array of net names".to_owned());
    };
    let nets: BTreeSet<String> = items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "each entry of nets must be a net name string".to_owned())
        })
        .collect::<std::result::Result<_, String>>()?;
    if nets.is_empty() {
        return Err("nets was empty — omit it to route the whole board".to_owned());
    }
    Ok(Some(nets))
}

fn route_live_board(
    ctx: &AgentRuntime,
    nets: Option<BTreeSet<String>>,
) -> std::result::Result<Value, Value> {
    let board = crate::active_board(ctx).map_err(refusal)?;
    if is_seed_placement(&board.imported.bounds, &board.imported.parts) {
        return Err(refusal(
            "board has only the initial seed-row footprint positions — run place_board before route_board",
        ));
    }

    let existing = (board.copper.traces.len(), board.copper.vias.len());
    let mut rp = board.problem.clone();
    // Solve against an in-memory copper-free view. The live/file board keeps
    // its prior route until the replacement has passed cleanup and lint, so a
    // timeout or failed solve cannot destroy useful manual/existing copper.
    if existing.0 > 0 || existing.1 > 0 {
        remove_existing_copper_obstacles(&mut rp);
    }
    rp.bounds = super::place::routing_bounds(&rp.bounds, rp.outline.as_ref());

    // The subset view the router actually solves, and the copper it must not
    // disturb. For a whole-board route these are the full view and nothing.
    let (solve_view, kept) = match &nets {
        None => (rp.clone(), RouteSolution::default()),
        Some(nets) => {
            let known: BTreeSet<&str> = board
                .problem
                .connections
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            if let Some(unknown) = nets.iter().find(|n| !known.contains(n.as_str())) {
                return Err(refusal(format!(
                    "no net named {unknown} on this board; get_board lists the board's nets"
                )));
            }
            let mut view = board.problem.clone();
            // Foreign copper stays an obstacle so the re-route goes around it;
            // the re-routed nets' own copper is about to be replaced.
            view.obstacles.retain(|obstacle| {
                !(matches!(obstacle.kind.as_str(), "track" | "via")
                    && obstacle.connected_to.iter().any(|n| nets.contains(n)))
            });
            view.connections.retain(|c| nets.contains(&c.name));
            view.bounds = rp.bounds;
            let kept = RouteSolution {
                traces: board
                    .copper
                    .traces
                    .iter()
                    .filter(|t| !nets.contains(&t.connection))
                    .cloned()
                    .collect(),
                vias: board
                    .copper
                    .vias
                    .iter()
                    .filter(|v| !nets.contains(&v.connection))
                    .cloned()
                    .collect(),
            };
            (view, kept)
        }
    };
    let kept_counts = (kept.traces.len(), kept.vias.len());
    let (router_problem, terminal_escapes) = prepare_wide_terminal_escapes(&solve_view);
    let (routing_subproblem, reserved_wide_routes) = reserve_wide_multi_pin_routes(&router_problem);
    let routed = route_with_engine(&routing_subproblem);
    let router_postroute_cleaned = routed.postroute_cleaned;
    let global_diagnostics = routed.global;
    let router_attempts = routed.attempts;
    let mut result = routed.result;
    let has_auxiliary_copper = !reserved_wide_routes.traces.is_empty()
        || !reserved_wide_routes.vias.is_empty()
        || !terminal_escapes.traces.is_empty()
        || !terminal_escapes.vias.is_empty();
    result.solution.traces.extend(reserved_wide_routes.traces);
    result.solution.vias.extend(reserved_wide_routes.vias);
    result.solution.traces.extend(terminal_escapes.traces);
    result.solution.vias.extend(terminal_escapes.vias);
    let dropped_failed = drop_failed_net_copper(&mut result);
    let failed = failed_connections(&result);
    add_terminal_stubs(&routing_subproblem, &mut result.solution, &failed);
    // Auto/Astar/Mesh candidates already passed this exact cleanup before
    // selection. Terminal stubs are simple pad-to-grid joins and the strict
    // final oracle validates them; rerunning the full lint-guarded cleanup here
    // is redundant unless raw auxiliary copper was merged or the explicitly
    // selected sequential engine has not run shared cleanup yet.
    if has_auxiliary_copper || !router_postroute_cleaned {
        postroute_cleanup(&solve_view, &mut result.solution);
    }
    // Everything from here on judges the WHOLE board: the copper this call left
    // alone is as much part of the result as the copper it just made.
    result.solution.traces.extend(kept.traces);
    result.solution.vias.extend(kept.vias);
    // A net this call was not asked to route, which had no copper to keep, is
    // still unrouted afterwards — but that is a fact about the board it started
    // from, not a defect in the copper this call made. Recording it as failed
    // now is what keeps the whole-board lint from reading it as an engine bug
    // and refusing the write, and is what puts it in `unrouted` where the caller
    // can see it is out of scope.
    if let Some(scope) = &nets {
        let out_of_scope: Vec<String> = board
            .problem
            .connections
            .iter()
            .map(|connection| connection.name.clone())
            .filter(|name| !scope.contains(name) && !has_copper(&result.solution, name))
            .collect();
        for net in out_of_scope {
            append_failed(
                &mut result,
                &net,
                "not in this call's `nets`, and it had no copper to keep",
            );
        }
    }
    // `prune_dangling_spurs_if_safe` reverts its own pruning whenever the lint
    // reads worse afterwards, so a surviving prune is already known clean.
    let pruned_spurs = prune_dangling_spurs_if_safe(&rp, &mut result);
    let plane_nets = rp.plane_nets.keys().cloned().collect();
    let (dropped, final_violations) = make_route_honest_with_report(&rp, &mut result, &plane_nets);
    let split = lint_summary_from_violations(&final_violations, &result.failed, &plane_nets);
    if split.real > 0 {
        let failure_summary = failed_route_summary(&result.failed);
        return Err(crate::diagnose::route_refusal(
            &real_violations(&final_violations, &result.failed, &plane_nets),
            &rp,
            &board.imported.parts,
            &failure_summary.connections,
        ));
    }

    let revision = ctx
        .revisions()
        .capture("route_board", "Route the project board", &[ctx.pcb_path()])
        .map_err(|error| {
            refusal(format!(
                "could not capture the board before routing: {error}"
            ))
        })?;
    replace_route_atomically(ctx, &rp, &result.solution, &board.layer_names, existing).map_err(
        |e| {
            json!({
                "error": format!("could not write route to the board: {e}"),
                "revision": revision,
            })
        },
    )?;

    // A connection can acquire more than one failure reason as the route is
    // cleaned up (for example, an initial router miss followed by an honest
    // connectivity drop). Preserve those diagnostic records in `failed`, but
    // also publish an unambiguous net-level summary so callers do not mistake
    // record count for the number of unrouted connections.
    let failure_summary = failed_route_summary(&result.failed);
    // Congestion is engine-produced diagnostic evidence, not a reason to run a
    // second router after routing has completed. Engines without a negotiated
    // global report return an honest null here.
    let congestion = route_congestion_json(global_diagnostics.as_ref(), !result.failed.is_empty());
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
    let total_connections = board.problem.connections.len();
    let routed_connections = total_connections.saturating_sub(failure_summary.connection_count);
    // Every net's terminals (so an out-of-scope net can still be named by its
    // pads) over the obstacles the router actually faced (so the copper this
    // call kept can be named as the thing in the way).
    let report_view = RoutingView {
        obstacles: solve_view.obstacles.clone(),
        ..rp.clone()
    };
    let unrouted = crate::diagnose::unrouted_report(
        &report_view,
        &board.imported.parts,
        &result.failed,
        &board.layer_names,
        nets.as_ref(),
    );
    Ok(json!({
        "router": result.engine,
        "routed": format!("{routed_connections}/{total_connections}"),
        "routed_connection_count": routed_connections,
        "total_connection_count": total_connections,
        "unrouted": unrouted,
        "scope": match &nets {
            Some(nets) => json!(nets.iter().collect::<Vec<_>>()),
            None => json!("whole board"),
        },
        "router_attempts": route_attempts_json(&router_attempts),
        "failed": failure_summary.records,
        "failed_record_count": failure_summary.record_count,
        "failed_connection_count": failure_summary.connection_count,
        "failed_connections": failure_summary.connections,
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
        // What this call actually replaced. A scoped route rewrites only the
        // named nets' copper, so reporting the whole board's would say it threw
        // away work it in fact kept.
        "cleared_existing_copper": {
            "traces": existing.0 - kept_counts.0,
            "vias": existing.1 - kept_counts.1,
        },
        "kept_existing_copper": {
            "traces": kept_counts.0,
            "vias": kept_counts.1,
        },
        "congestion": congestion,
        "escape_bottleneck": escape,
        "revision": revision,
        "note": if result.failed.is_empty() {
            "routed and saved the KiCAD board cleanly".to_owned()
        } else {
            format!(
                "saved the KiCAD board with {routed_connections} of {total_connections} net(s) \
                 routed; the rest carry no copper — see `unrouted` for the pads, the obstacle and \
                 the repair for each"
            )
        },
    }))
}

fn remove_existing_copper_obstacles(problem: &mut RoutingView) {
    problem
        .obstacles
        .retain(|obstacle| !matches!(obstacle.kind.as_str(), "track" | "via"));
}

fn replace_route_atomically(
    ctx: &AgentRuntime,
    rp: &RoutingView,
    solution: &RouteSolution,
    layer_names: &[String],
    existing: (usize, usize),
) -> std::result::Result<(), String> {
    if existing == (0, 0) {
        return write_route(ctx, rp, solution, layer_names);
    }
    let path = ctx.pcb_path();
    let original = std::fs::read(&path).map_err(|error| {
        format!("could not snapshot existing board before replacement: {error}")
    })?;
    let ipc_route = kicad_board::bridge_route(rp, solution);
    let live = ctx.kicad().with_session(&path, |session| {
        session
            .kicad()
            .replace_route_solution(&ipc_route, layer_names)
    });
    let replace = match live {
        Ok(_) => Ok(()),
        Err(live_error) => replace_route_offline(ctx, rp, solution, layer_names)
            .map_err(|offline| format!("{live_error}; offline fallback failed: {offline}")),
    };
    if let Err(error) = replace {
        ctx.close_kicad_session();
        write_board_atomically(&path, &original).map_err(|restore| {
            format!("{error}; restoring the prior board also failed: {restore}")
        })?;
        return Err(format!(
            "{error}; restored the prior board without changing its copper"
        ));
    }
    Ok(())
}

fn replace_route_offline(
    ctx: &AgentRuntime,
    rp: &RoutingView,
    solution: &RouteSolution,
    layer_names: &[String],
) -> std::result::Result<(), String> {
    ctx.close_kicad_session();
    let path = ctx.pcb_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read the board: {error}"))?;
    let (stripped, _, _) = kicad_board::strip_copper(&text)?;
    let replacement = kicad_board::append_copper(&stripped, solution, rp.layer_count, layer_names)?;
    write_board_atomically(&path, replacement.as_bytes())
        .map_err(|error| format!("could not write the board: {error}"))
}

pub(super) fn write_board_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let permissions = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
    replacement.write_all(contents)?;
    replacement.as_file_mut().sync_all()?;
    if let Some(permissions) = permissions {
        std::fs::set_permissions(replacement.path(), permissions)?;
    }
    replacement
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

/// Whether `solution` still carries copper for `net` — the only nets worth
/// dropping, and what makes the honesty loop below terminate.
fn has_copper(solution: &RouteSolution, net: &str) -> bool {
    solution.traces.iter().any(|t| t.connection == net)
        || solution.vias.iter().any(|v| v.connection == net)
}

/// Drop copper until the lint is clean, and report what went.
///
/// The engine's policy is that a net it cannot route cleanly is reported
/// unrouted, never shipped as copper that lies. Enforcing it takes a FIXPOINT,
/// not a fixed number of passes: dropping one net's copper can expose a
/// violation on another (a trace that only reached its pad through the copper
/// just removed), and a pass count that runs out leaves violations standing —
/// which made `route_board` refuse, with the same count every time and nothing
/// the caller could do about it. Each round drops at least one net that still
/// has copper, so the loop is bounded by the net count.
fn make_route_honest_with_report(
    rp: &RoutingView,
    result: &mut RouteResult,
    plane_nets: &BTreeSet<String>,
) -> (Vec<String>, Vec<DrcViolation>) {
    let mut dropped = Vec::new();
    let mut violations = lint(rp, &result.solution);
    // Each round drops at least one net that still has copper, and copper never
    // comes back, so the net count bounds the loop.
    for _ in 0..=rp.connections.len() {
        let disconnected = defective_nets(&violations, plane_nets);
        let violating: BTreeSet<String> = violations
            .iter()
            .flat_map(geometry_violation_nets)
            .collect();
        let reason = |net: &String| {
            if violating.contains(net) {
                "dropped copper: route had geometry DRC violations"
            } else {
                "dropped copper: connectivity oracle reported it unconnected"
            }
        };
        let round: Vec<String> = disconnected
            .union(&violating)
            .filter(|net| has_copper(&result.solution, net))
            .cloned()
            .collect();
        if round.is_empty() {
            break;
        }
        drop_solution_nets(&mut result.solution, &round.iter().cloned().collect());
        for net in round {
            append_failed(result, &net, reason(&net));
            dropped.push(net);
        }
        violations = lint(rp, &result.solution);
    }
    dropped.sort();
    dropped.dedup();
    (dropped, violations)
}

#[cfg(test)]
fn make_route_honest(
    rp: &RoutingView,
    result: &mut RouteResult,
    plane_nets: &BTreeSet<String>,
) -> Vec<String> {
    make_route_honest_with_report(rp, result, plane_nets).0
}

/// The nets whose copper the connectivity oracle says must go.
///
/// An `Unconnected` plane net is expected — a plane joins its pads through
/// copper this solution does not carry. A SHORT is a defect on any net, but the
/// copper that must go is the non-plane side wherever there is one: dropping a
/// plane's stitch vias to clear another net's stray trace would take the whole
/// plane's connectivity with it. Only when every side of a short is a plane net
/// are the planes themselves dropped, so the loop can still make progress.
fn defective_nets(violations: &[DrcViolation], plane_nets: &BTreeSet<String>) -> BTreeSet<String> {
    let mut defective = BTreeSet::new();
    for violation in violations {
        match violation {
            DrcViolation::Connectivity {
                violation: ConnViolation::Unconnected { connection, .. },
            } if !plane_nets.contains(connection) => {
                defective.insert(connection.clone());
            }
            DrcViolation::Connectivity {
                violation: ConnViolation::CrossNetMerge { a, b },
            } => {
                let non_plane: Vec<&String> = [a, b]
                    .into_iter()
                    .filter(|net| !plane_nets.contains(*net))
                    .collect();
                if non_plane.is_empty() {
                    // Plane against plane: there is no other side to give, so
                    // the stitching itself has to go.
                    defective.extend([a.clone(), b.clone()]);
                } else {
                    defective.extend(non_plane.into_iter().cloned());
                }
            }
            _ => {}
        }
    }
    defective
}

fn drop_solution_nets(solution: &mut RouteSolution, nets: &BTreeSet<String>) {
    solution
        .traces
        .retain(|trace| !nets.contains(&trace.connection));
    solution.vias.retain(|via| !nets.contains(&via.connection));
}

fn failed_connections(result: &RouteResult) -> BTreeSet<String> {
    result
        .failed
        .iter()
        .map(|f| f.connection.clone())
        .filter(|name| !name.is_empty())
        .collect()
}

struct FailedRouteSummary {
    records: Vec<Value>,
    record_count: usize,
    connections: Vec<String>,
    connection_count: usize,
}

fn failed_route_summary(failed: &[FailedNet]) -> FailedRouteSummary {
    let records = failed
        .iter()
        .map(|failure| {
            json!({
                "connection": failure.connection,
                "reason": failure.reason,
            })
        })
        .collect::<Vec<_>>();
    let connections = failed
        .iter()
        .map(|failure| failure.connection.clone())
        .filter(|connection| !connection.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    FailedRouteSummary {
        record_count: records.len(),
        connection_count: connections.len(),
        records,
        connections,
    }
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
    rp: &RoutingView,
    solution: &mut RouteSolution,
    skip_connections: &BTreeSet<String>,
) {
    let pitch = rp.grid_pitch();
    for conn in &rp.connections {
        if skip_connections.contains(&conn.name) {
            continue;
        }
        let width = rp.net_width(&conn.name);
        for point in &conn.points_to_connect {
            if rp.plane_nets.contains_key(&conn.name)
                && (solution.vias.iter().any(|via| {
                    via.connection == conn.name && via.at.dist(point.point()) < geom::EPS
                }) || solution.traces.iter().any(|trace| {
                    trace.connection == conn.name
                        && trace
                            .path
                            .first()
                            .is_some_and(|at| at.dist(point.point()) < geom::EPS)
                }))
            {
                continue;
            }
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

fn prune_dangling_spurs_if_safe(rp: &RoutingView, result: &mut RouteResult) -> usize {
    let plane_nets: BTreeSet<_> = rp.plane_nets.keys().cloned().collect();
    let plane_traces: Vec<_> = result
        .solution
        .traces
        .iter()
        .filter(|trace| plane_nets.contains(&trace.connection))
        .cloned()
        .collect();
    result
        .solution
        .traces
        .retain(|trace| !plane_nets.contains(&trace.connection));
    let original = result.solution.clone();
    let removed = prune_dangling_spurs(rp, &mut result.solution);
    if removed > 0 && lint_summary(rp, &result.solution, &result.failed, &plane_nets).real > 0 {
        result.solution = original;
        result.solution.traces.extend(plane_traces);
        0
    } else {
        result.solution.traces.extend(plane_traces);
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
    attempts: Vec<RoutePassReport>,
    global: Option<GlobalRouteResult>,
    postroute_cleaned: bool,
}

fn route_with_engine(rp: &RoutingView) -> RouteRun {
    let run = pcb_engine::route_tuned(rp);
    RouteRun {
        result: run.result,
        attempts: run.passes,
        global: None,
        postroute_cleaned: true,
    }
}

fn route_attempts_json(attempts: &[RoutePassReport]) -> Value {
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

const MAX_MULTI_PIN_TERMINALS: usize = 16;
const MAX_DIRECT_RESCUE_FAILED_NETS: usize = 8;

/// Route wide multi-terminal nets before ordinary signals can occupy their
/// scarce escape corridors. Successfully reserved trees are removed from the
/// remaining problem and represented as copper obstacles for every engine.
fn reserve_wide_multi_pin_routes(rp: &RoutingView) -> (RoutingView, RouteSolution) {
    let targets: BTreeSet<_> = rp
        .connections
        .iter()
        .filter(|conn| {
            rp.net_width(&conn.name) > rp.min_trace_width + geom::EPS
                && !rp.plane_nets.contains_key(&conn.name)
                && (3..=MAX_MULTI_PIN_TERMINALS).contains(&conn.points_to_connect.len())
        })
        .map(|conn| conn.name.clone())
        .collect();
    if targets.is_empty() {
        return (
            rp.clone(),
            RouteSolution {
                traces: Vec::new(),
                vias: Vec::new(),
            },
        );
    }
    let mut reservation = RouteResult {
        solution: RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        },
        failed: targets
            .iter()
            .map(|connection| FailedNet {
                connection: connection.clone(),
                reason: "wide-net reservation".to_owned(),
            })
            .collect(),
        engine: "wide-net-reservation".to_owned(),
    };
    apply_direct_rescue_fallback(rp, &mut reservation);
    // L/Z visibility candidates are intentionally cheap but cannot invent a
    // shared Steiner trunk. Give each still-failed wide net an isolated grid
    // tree before ordinary signals are present, reserving earlier wide copper
    // between attempts.
    let remaining = reservation
        .failed
        .iter()
        .map(|failed| failed.connection.clone())
        .collect::<Vec<_>>();
    for name in remaining {
        let Some(connection) = rp
            .connections
            .iter()
            .find(|connection| connection.name == name)
            .cloned()
        else {
            continue;
        };
        if let Some(candidate) = wide_shared_trunk_candidate(rp, &reservation.solution, &connection)
        {
            reservation.solution = candidate;
            reservation
                .failed
                .retain(|failed| failed.connection != name);
            continue;
        }
        let mut isolated = rp.clone();
        isolated.connections = vec![connection];
        isolated
            .obstacles
            .extend(copper_obstacles(rp, &reservation.solution));
        let candidate = pcb_engine::route_grid(&isolated);
        if candidate.failed.is_empty()
            && pcb_engine::geometry_violations(&isolated, &candidate.solution) == 0
            && direct_candidate_is_clean(&isolated, &candidate.solution, &name)
        {
            reservation
                .solution
                .traces
                .extend(candidate.solution.traces);
            reservation.solution.vias.extend(candidate.solution.vias);
            reservation
                .failed
                .retain(|failed| failed.connection != name);
        }
    }
    let still_failed: BTreeSet<_> = reservation
        .failed
        .iter()
        .map(|failed| failed.connection.as_str())
        .collect();
    let reserved: BTreeSet<_> = targets
        .iter()
        .filter(|name| !still_failed.contains(name.as_str()))
        .cloned()
        .collect();
    let mut subproblem = rp.clone();
    subproblem
        .connections
        .retain(|conn| !reserved.contains(&conn.name));
    subproblem
        .obstacles
        .extend(copper_obstacles(rp, &reservation.solution));
    (subproblem, reservation.solution)
}

fn wide_shared_trunk_candidate(
    rp: &RoutingView,
    solution: &RouteSolution,
    conn: &pcb_model::Connection,
) -> Option<RouteSolution> {
    let first_layer = conn.points_to_connect.first()?.layer.clone();
    if conn
        .points_to_connect
        .iter()
        .any(|point| !same_layer_ref(&point.layer, &first_layer, rp.layer_count))
    {
        return None;
    }
    let width = rp.net_width(&conn.name);
    let inset = (rp.clearance + width + 0.5).max(1.0);
    let mut xs = vec![rp.bounds.min_x + inset, rp.bounds.max_x - inset];
    let mut ys = vec![rp.bounds.min_y + inset, rp.bounds.max_y - inset];
    for obstacle in &rp.obstacles {
        if !obstacle
            .layers
            .iter()
            .any(|layer| same_layer_ref(layer, &first_layer, rp.layer_count))
        {
            continue;
        }
        let dx = obstacle.width / 2.0 + rp.clearance + width;
        let dy = obstacle.height / 2.0 + rp.clearance + width;
        xs.extend([obstacle.center.x - dx, obstacle.center.x + dx]);
        ys.extend([obstacle.center.y - dy, obstacle.center.y + dy]);
    }
    xs.retain(|x| *x >= rp.bounds.min_x + inset && *x <= rp.bounds.max_x - inset);
    ys.retain(|y| *y >= rp.bounds.min_y + inset && *y <= rp.bounds.max_y - inset);
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    xs.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    ys.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    limit_trunk_axes(&mut xs, (rp.bounds.min_x + rp.bounds.max_x) / 2.0);
    limit_trunk_axes(&mut ys, (rp.bounds.min_y + rp.bounds.max_y) / 2.0);

    let points: Vec<_> = conn
        .points_to_connect
        .iter()
        .map(|point| point.point())
        .collect();
    let min_x = points
        .iter()
        .map(|point| point.x)
        .fold(f64::INFINITY, f64::min);
    let max_x = points
        .iter()
        .map(|point| point.x)
        .fold(f64::NEG_INFINITY, f64::max);
    let min_y = points
        .iter()
        .map(|point| point.y)
        .fold(f64::INFINITY, f64::min);
    let max_y = points
        .iter()
        .map(|point| point.y)
        .fold(f64::NEG_INFINITY, f64::max);

    for y in ys {
        let mut candidate = solution.clone();
        candidate.traces.push(Trace {
            connection: conn.name.clone(),
            layer: first_layer.clone(),
            width,
            path: vec![Point2 { x: min_x, y }, Point2 { x: max_x, y }],
        });
        let mut complete = true;
        for point in &points {
            if (point.y - y).abs() <= geom::EPS {
                continue;
            }
            let mut access_xs = points
                .iter()
                .map(|candidate| candidate.x)
                .collect::<Vec<_>>();
            access_xs.sort_by(|a, b| {
                (a - point.x)
                    .abs()
                    .partial_cmp(&(b - point.x).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            access_xs.dedup_by(|a, b| (*a - *b).abs() < geom::EPS);
            let branch = access_xs.into_iter().find_map(|x| {
                let mut proposed = candidate.clone();
                proposed.traces.push(Trace {
                    connection: conn.name.clone(),
                    layer: first_layer.clone(),
                    width,
                    path: vec![*point, Point2 { x, y: point.y }, Point2 { x, y }],
                });
                simplify_candidate_paths(&mut proposed);
                let clean = direct_candidate_is_geometry_clean(rp, &proposed, &conn.name);
                clean.then_some(proposed)
            });
            let Some(proposed) = branch else {
                complete = false;
                break;
            };
            candidate = proposed;
        }
        if complete && direct_candidate_is_clean(rp, &candidate, &conn.name) {
            return Some(candidate);
        }
    }
    for x in xs {
        let mut candidate = solution.clone();
        candidate.traces.push(Trace {
            connection: conn.name.clone(),
            layer: first_layer.clone(),
            width,
            path: vec![Point2 { x, y: min_y }, Point2 { x, y: max_y }],
        });
        let mut complete = true;
        for point in &points {
            if (point.x - x).abs() <= geom::EPS {
                continue;
            }
            let mut access_ys = points
                .iter()
                .map(|candidate| candidate.y)
                .collect::<Vec<_>>();
            access_ys.sort_by(|a, b| {
                (a - point.y)
                    .abs()
                    .partial_cmp(&(b - point.y).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            access_ys.dedup_by(|a, b| (*a - *b).abs() < geom::EPS);
            let branch = access_ys.into_iter().find_map(|y| {
                let mut proposed = candidate.clone();
                proposed.traces.push(Trace {
                    connection: conn.name.clone(),
                    layer: first_layer.clone(),
                    width,
                    path: vec![*point, Point2 { x: point.x, y }, Point2 { x, y }],
                });
                simplify_candidate_paths(&mut proposed);
                direct_candidate_is_geometry_clean(rp, &proposed, &conn.name).then_some(proposed)
            });
            let Some(proposed) = branch else {
                complete = false;
                break;
            };
            candidate = proposed;
        }
        if complete && direct_candidate_is_clean(rp, &candidate, &conn.name) {
            return Some(candidate);
        }
    }
    None
}

fn limit_trunk_axes(axes: &mut Vec<f64>, center: f64) {
    const MAX_TRUNK_AXES: usize = 64;
    if axes.len() <= MAX_TRUNK_AXES {
        return;
    }
    let first = axes[0];
    let last = axes[axes.len() - 1];
    axes.sort_by(|a, b| {
        (a - center)
            .abs()
            .partial_cmp(&(b - center).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    });
    axes.truncate(MAX_TRUNK_AXES - 2);
    axes.extend([first, last]);
    axes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    axes.dedup_by(|a, b| (*a - *b).abs() < 0.05);
}

pub(crate) fn apply_direct_rescue_fallback(rp: &RoutingView, result: &mut RouteResult) -> bool {
    if result.failed.is_empty() {
        return false;
    }
    let failed: std::collections::BTreeSet<String> = result
        .failed
        .iter()
        .map(|f| f.connection.clone())
        .filter(|name| !name.is_empty())
        .collect();
    // Direct rescue enumerates dozens of candidate paths per failed net and
    // validates each against the complete growing solution. Bound that work
    // before cloning or modifying anything: a broad failure is placement or
    // congestion feedback, not a useful direct-rescue portfolio. The original
    // result remains byte-for-byte unchanged when the cap is exceeded.
    if failed.is_empty() || failed.len() > MAX_DIRECT_RESCUE_FAILED_NETS {
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
        } else if (3..=MAX_MULTI_PIN_TERMINALS).contains(&conn.points_to_connect.len()) {
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
    // Geometry was checked incrementally against every foreign board item.
    // Prove the intended topology with one small connectivity problem per
    // rescued net instead of rebuilding the whole-board O(n²) connectivity
    // graph. The route pipeline still runs its unchanged full-board oracle
    // before writeback; this gate keeps rescue itself atomic and connected.
    if !direct_rescued_connections_are_clean(rp, &rescue_solution, &applied) {
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
    rp: &RoutingView,
    solution: &RouteSolution,
    conn: &pcb_model::Connection,
) -> Option<RouteSolution> {
    let a = conn.points_to_connect[0].point();
    let b = conn.points_to_connect[1].point();
    for layer in direct_candidate_layers_for_conn(rp, conn) {
        for path in direct_candidate_paths(rp, solution, &conn.name, &layer, a, b) {
            let mut candidate = direct_candidate_solution(rp, solution, conn, layer.clone(), path);
            simplify_candidate_paths(&mut candidate);
            if direct_candidate_is_geometry_clean(rp, &candidate, &conn.name) {
                return Some(candidate);
            }
        }
    }
    None
}

fn direct_multi_pin_candidate(
    rp: &RoutingView,
    solution: &RouteSolution,
    conn: &pcb_model::Connection,
) -> Option<RouteSolution> {
    let positions: Vec<Point2> = conn
        .points_to_connect
        .iter()
        .map(|point| point.point())
        .collect();
    for layer in direct_candidate_layers_for_conn(rp, conn) {
        let mut candidate = solution.clone();
        for point in &conn.points_to_connect {
            push_terminal_via_if_needed(rp, &mut candidate, conn, point, &layer);
        }
        let mut in_tree = vec![false; positions.len()];
        in_tree[0] = true;
        let mut ok = true;
        while in_tree.iter().any(|present| !present) {
            let mut frontier = Vec::new();
            for (a, &a_in_tree) in in_tree.iter().enumerate() {
                if !a_in_tree {
                    continue;
                }
                for (b, &b_in_tree) in in_tree.iter().enumerate() {
                    if !b_in_tree {
                        frontier.push((a, b));
                    }
                }
            }
            frontier.sort_by(|&(a, b), &(old_a, old_b)| {
                use std::cmp::Ordering;
                if direct_multi_pin_tree_pair_better(conn, &positions, a, b, old_a, old_b) {
                    Ordering::Less
                } else if direct_multi_pin_tree_pair_better(conn, &positions, old_a, old_b, a, b) {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            });

            let mut accepted = None;
            for (a_idx, b_idx) in frontier {
                let a = positions[a_idx];
                let b = positions[b_idx];
                // Stacked connector pads are already joined by their shared
                // physical copper, so they extend the tree without a trace.
                if a.dist(b) < geom::EPS {
                    accepted = Some((b_idx, candidate.clone()));
                    break;
                }
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
                        accepted = Some((b_idx, leg));
                        break;
                    }
                }
                if accepted.is_some() {
                    break;
                }
            }
            let Some((joined, next_candidate)) = accepted else {
                ok = false;
                break;
            };
            in_tree[joined] = true;
            candidate = next_candidate;
        }
        if ok && direct_candidate_is_geometry_clean(rp, &candidate, &conn.name) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
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
    rp: &RoutingView,
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
    rp: &RoutingView,
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
    rp: &RoutingView,
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
    let plane_layers: BTreeSet<u32> = pcb_model::plane_layers(layer_count)
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
    rp: &RoutingView,
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

fn direct_candidate_is_clean(rp: &RoutingView, solution: &RouteSolution, connection: &str) -> bool {
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

fn direct_rescued_connections_are_clean(
    rp: &RoutingView,
    solution: &RouteSolution,
    rescued: &BTreeSet<String>,
) -> bool {
    for connection in rescued {
        let mut net_problem = rp.clone();
        net_problem
            .connections
            .retain(|candidate| candidate.name == *connection);
        net_problem.obstacles.retain(|obstacle| {
            obstacle
                .connected_to
                .iter()
                .any(|owner| owner == connection)
        });
        let net_solution = RouteSolution {
            traces: solution
                .traces
                .iter()
                .filter(|trace| trace.connection == *connection)
                .cloned()
                .collect(),
            vias: solution
                .vias
                .iter()
                .filter(|via| via.connection == *connection)
                .cloned()
                .collect(),
        };
        if !pcb_engine::connectivity(&net_problem, &net_solution).is_empty() {
            return false;
        }
    }
    true
}

fn direct_candidate_is_geometry_clean(
    rp: &RoutingView,
    solution: &RouteSolution,
    connection: &str,
) -> bool {
    // Rescue starts after failed-net copper has been removed, so every item
    // owned by `connection` is candidate copper and every differently-owned
    // item is established board copper. Use the exact segment/rectangle/disc
    // distance primitives here instead of the full DRC suite: connectivity is
    // quadratic in all board elements and adds no information while exploring
    // an incomplete candidate. `direct_candidate_is_clean` remains the final
    // authority before a complete rescue is accepted.
    let target_traces = solution
        .traces
        .iter()
        .filter(|trace| trace.connection == connection)
        .collect::<Vec<_>>();
    let target_vias = solution
        .vias
        .iter()
        .filter(|via| via.connection == connection)
        .collect::<Vec<_>>();

    for trace in &target_traces {
        if trace.width + geom::EPS < rp.min_trace_width
            || trace.layer.index(rp.layer_count).is_none()
        {
            return false;
        }
        let half_width = trace.width / 2.0;
        for points in trace.path.windows(2) {
            let segment = geom::Segment::new(points[0], points[1]);
            if rp.bounds.disc_overshoot(points[0], half_width) > geom::EPS
                || rp.bounds.disc_overshoot(points[1], half_width) > geom::EPS
                || rp.outline.as_ref().is_some_and(|outline| {
                    outline.segment_dist_to_edge(segment) + geom::EPS < 0.5 + half_width
                })
            {
                return false;
            }
            for obstacle in &rp.obstacles {
                if obstacle
                    .connected_to
                    .iter()
                    .any(|owner| owner == connection)
                    || !obstacle
                        .layers
                        .iter()
                        .any(|layer| same_layer_ref(layer, &trace.layer, rp.layer_count))
                {
                    continue;
                }
                let rect = geom::Rect::from_center_half(
                    obstacle.center,
                    (obstacle.width / 2.0, obstacle.height / 2.0),
                );
                if segment.dist_to_rect(&rect) - half_width + geom::EPS < rp.clearance {
                    return false;
                }
            }
            // Connectivity also treats route terminals as zero-size copper
            // anchors. Real boards normally have a matching pad obstacle, but
            // include the anchor explicitly so reduced final connectivity can
            // never hide a cross-net touch on synthetic/partial problems.
            for other in rp
                .connections
                .iter()
                .filter(|other| other.name != connection)
            {
                for point in &other.points_to_connect {
                    if same_layer_ref(&point.layer, &trace.layer, rp.layer_count)
                        && segment.dist_to_point(point.point()) - half_width + geom::EPS
                            < rp.clearance
                    {
                        return false;
                    }
                }
            }
            for other in solution
                .traces
                .iter()
                .filter(|other| other.connection != connection)
            {
                if !same_layer_ref(&other.layer, &trace.layer, rp.layer_count) {
                    continue;
                }
                for other_points in other.path.windows(2) {
                    let other_segment = geom::Segment::new(other_points[0], other_points[1]);
                    if segment.dist_to_segment(other_segment) - half_width - other.width / 2.0
                        + geom::EPS
                        < rp.clearance
                    {
                        return false;
                    }
                }
            }
            for via in solution
                .vias
                .iter()
                .filter(|via| via.connection != connection)
            {
                if segment.dist_to_point(via.at) - half_width - via.diameter / 2.0 + geom::EPS
                    < rp.clearance
                {
                    return false;
                }
            }
        }
    }

    for via in target_vias {
        let radius = via.diameter / 2.0;
        let via_segment = geom::Segment::new(via.at, via.at);
        if via.diameter + geom::EPS < rp.via_diameter
            || rp.bounds.disc_overshoot(via.at, radius) > geom::EPS
            || rp.outline.as_ref().is_some_and(|outline| {
                outline.segment_dist_to_edge(via_segment) + geom::EPS < 0.5 + radius
            })
        {
            return false;
        }
        for obstacle in &rp.obstacles {
            if obstacle
                .connected_to
                .iter()
                .any(|owner| owner == connection)
            {
                continue;
            }
            let rect = geom::Rect::from_center_half(
                obstacle.center,
                (obstacle.width / 2.0, obstacle.height / 2.0),
            );
            if rect.dist_to_point(via.at) - radius + geom::EPS < rp.clearance {
                return false;
            }
        }
        for other in rp
            .connections
            .iter()
            .filter(|other| other.name != connection)
        {
            if other
                .points_to_connect
                .iter()
                .any(|point| via.at.dist(point.point()) - radius + geom::EPS < rp.clearance)
            {
                return false;
            }
        }
        for trace in solution
            .traces
            .iter()
            .filter(|trace| trace.connection != connection)
        {
            for points in trace.path.windows(2) {
                let segment = geom::Segment::new(points[0], points[1]);
                if segment.dist_to_point(via.at) - trace.width / 2.0 - radius + geom::EPS
                    < rp.clearance
                {
                    return false;
                }
            }
        }
        for other in solution
            .vias
            .iter()
            .filter(|other| other.connection != connection)
        {
            if via.at.dist(other.at) - radius - other.diameter / 2.0 + geom::EPS < rp.clearance {
                return false;
            }
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

fn prune_dangling_spurs(rp: &RoutingView, solution: &mut RouteSolution) -> usize {
    let mut segments = flatten_segments(solution);
    if segments.is_empty() {
        return 0;
    }
    let protected = protected_route_nodes(rp, solution);
    let own_pads = same_net_pads(rp);
    let lands_on_own_pad = |connection: &str, at: Point2| {
        own_pads
            .iter()
            .any(|(net, pad)| net == connection && pad.contains(at))
    };
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
            let a_dangles = degree.get(&a).copied().unwrap_or(0) <= 1
                && !protected.contains(&a)
                && !lands_on_own_pad(&segment.connection, segment.start);
            let b_dangles = degree.get(&b).copied().unwrap_or(0) <= 1
                && !protected.contains(&b)
                && !lands_on_own_pad(&segment.connection, segment.end);
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

/// Every pad, with the net it belongs to.
///
/// A connection's `points_to_connect` carries one point per pad, but a pad is an
/// AREA: a track that stops anywhere inside it is joined, and KiCAD agrees. The
/// spur prune tested the exact point only, so it read a track landing a hair off
/// pad centre as dangling, tried to remove it, broke the net, and reverted the
/// whole prune — leaving copper KiCAD then reports as `track_dangling`.
fn same_net_pads(rp: &RoutingView) -> Vec<(String, geom::Rect)> {
    rp.obstacles
        .iter()
        .filter(|obstacle| obstacle.kind.starts_with("pad:"))
        .flat_map(|obstacle| {
            let rect = geom::Rect::new(
                obstacle.center.x - obstacle.width / 2.0,
                obstacle.center.y - obstacle.height / 2.0,
                obstacle.center.x + obstacle.width / 2.0,
                obstacle.center.y + obstacle.height / 2.0,
            );
            obstacle
                .connected_to
                .iter()
                .map(move |net| (net.clone(), rect))
        })
        .collect()
}

fn protected_route_nodes(
    rp: &RoutingView,
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
    kicad_ipc::units::mm_to_nm(v)
}

fn write_route(
    ctx: &AgentRuntime,
    rp: &RoutingView,
    solution: &RouteSolution,
    layer_names: &[String],
) -> std::result::Result<(), String> {
    let ipc_route = kicad_board::bridge_route(rp, solution);
    let path = ctx.pcb_path();
    let live = ctx.kicad().with_session(&path, |session| {
        session
            .kicad()
            .create_route_solution(&ipc_route, layer_names)
    });
    let Err(live_err) = live else { return Ok(()) };
    // Headless fallback: append the copper to the board file directly.
    write_route_offline(ctx, rp, solution, layer_names)
        .map_err(|err| format!("{live_err}; offline fallback failed: {err}"))
}

/// Append one solution directly to the board file while preserving its copper.
/// Drop the live session first so stale KiCad state cannot later overwrite it.
pub(super) fn write_route_offline(
    ctx: &AgentRuntime,
    rp: &RoutingView,
    solution: &RouteSolution,
    layer_names: &[String],
) -> std::result::Result<(), String> {
    ctx.close_kicad_session();
    kicad_board::append_copper_file(&ctx.pcb_path(), solution, rp.layer_count, layer_names)
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
    use kicad_board::ImportedPad;

    fn part(reference: &str, footprint: &str, nets: &[(&str, &str)]) -> ImportedPart {
        ImportedPart {
            reference: reference.to_owned(),
            lib_id: footprint.to_owned(),
            at: Point2 { x: 0.0, y: 0.0 },
            rotation: 0,
            locked: false,
            pads: nets
                .iter()
                .map(|(p, n)| ImportedPad {
                    number: p.to_string(),
                    net: Some(n.to_string()),
                    at: Point2 { x: 0.0, y: 0.0 },
                    layers: vec![LayerRef::top()],
                })
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

    fn sensor_v3v3_problem() -> RoutingView {
        let terminals = [
            (20.725, 25.5, 0.9, 0.95),
            (17.975, 23.325, 0.5, 0.35),
            (20.025, 22.675, 0.5, 0.35),
            (20.025, 23.975, 0.5, 0.35),
            (15.725, 27.5, 0.9, 0.95),
            (36.23, 18.333333, 1.7, 1.7),
            (15.325, 20.5, 0.8, 0.95),
            (20.3, 15.5, 1.0, 1.5),
            (36.175, 29.0, 0.8, 0.95),
            (16.325, 24.0, 0.8, 0.95),
            (36.1375, 11.05, 1.325, 0.6),
            (19.05, 20.0, 1.0, 1.45),
        ];
        let mut obstacles: Vec<_> = terminals
            .iter()
            .enumerate()
            .map(|(index, &(x, y, width, height))| pcb_model::Obstacle {
                kind: format!("pad:V{index}"),
                layers: vec![LayerRef::top()],
                center: Point2 { x, y },
                width,
                height,
                connected_to: vec!["/V3V3".to_owned()],
            })
            .collect();
        for (index, (net, x, y)) in [
            ("SCL", 17.975, 22.025),
            ("SDA", 17.975, 22.675),
            ("GND", 17.975, 23.975),
            ("ADDR", 20.025, 22.025),
            ("GND", 20.025, 23.325),
        ]
        .into_iter()
        .enumerate()
        {
            obstacles.push(pcb_model::Obstacle {
                kind: format!("pad:U1-{index}"),
                layers: vec![LayerRef::top()],
                center: Point2 { x, y },
                width: 0.5,
                height: 0.35,
                connected_to: vec![net.to_owned()],
            });
        }
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections: vec![pcb_model::Connection {
                name: "/V3V3".to_owned(),
                points_to_connect: terminals
                    .iter()
                    .map(|&(x, y, _, _)| pcb_model::RoutePoint {
                        x,
                        y,
                        layer: LayerRef::top(),
                    })
                    .collect(),
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 40.0,
                max_y: 30.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: [("/V3V3".to_owned(), 0.5)].into_iter().collect(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: [("GND".to_owned(), 1)].into_iter().collect(),
            fixed_copper: Default::default(),
            nets: None,
        }
    }

    fn power_v5_problem() -> RoutingView {
        let terminals = [
            (13.025, 23.5, 1.15, 2.7, false),
            (16.0, 8.0, 3.0, 3.0, true),
            (18.325, 11.5, 0.8, 0.95, false),
            (35.5, 24.29, 2.6, 2.6, true),
            (17.24, 17.0, 6.4, 5.8, false),
        ];
        let mut obstacles = terminals
            .iter()
            .enumerate()
            .map(
                |(index, &(x, y, width, height, through))| pcb_model::Obstacle {
                    kind: format!("pad:V5-{index}"),
                    layers: if through {
                        vec![LayerRef::top(), LayerRef::bottom()]
                    } else {
                        vec![LayerRef::top()]
                    },
                    center: Point2 { x, y },
                    width,
                    height,
                    connected_to: vec!["V5".to_owned()],
                },
            )
            .collect::<Vec<_>>();
        for (index, (net, x, y, width, height)) in [
            ("LED", 16.675, 11.5, 0.8, 0.95),
            ("GND", 23.54, 19.28, 2.2, 1.2),
            ("VIN_PROT", 23.54, 14.72, 2.2, 1.2),
            ("GND", 15.975, 23.5, 1.15, 2.7),
            ("GND", 30.5, 24.29, 2.6, 2.6),
        ]
        .into_iter()
        .enumerate()
        {
            obstacles.push(pcb_model::Obstacle {
                kind: format!("pad:X-{index}"),
                layers: vec![LayerRef::top()],
                center: Point2 { x, y },
                width,
                height,
                connected_to: vec![net.to_owned()],
            });
        }
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections: vec![pcb_model::Connection {
                name: "V5".to_owned(),
                points_to_connect: terminals
                    .iter()
                    .map(|&(x, y, _, _, _)| pcb_model::RoutePoint {
                        x,
                        y,
                        layer: LayerRef::top(),
                    })
                    .collect(),
            }],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 45.0,
                max_y: 30.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: [("V5".to_owned(), 0.5)].into_iter().collect(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: [("GND".to_owned(), 1)].into_iter().collect(),
            fixed_copper: Default::default(),
            nets: None,
        }
    }

    #[test]
    fn live_power_wide_net_reserves_an_isolated_grid_tree() {
        let problem = power_v5_problem();
        let (routing_problem, escapes) = prepare_wide_terminal_escapes(&problem);
        assert!(escapes.traces.is_empty(), "all V5 pads accept full width");
        let (subproblem, reserved) = reserve_wide_multi_pin_routes(&routing_problem);
        assert!(
            subproblem.connections.is_empty(),
            "V5 must be reserved first"
        );
        assert!(!reserved.traces.is_empty());
        assert!(reserved.traces.iter().all(|trace| trace.width == 0.5));
        assert!(lint(&problem, &reserved).is_empty());
    }

    #[test]
    fn reroute_problem_drops_only_prior_track_and_via_obstacles() {
        let mut problem = power_v5_problem();
        for kind in ["track", "via", "keepout"] {
            problem.obstacles.push(pcb_model::Obstacle {
                kind: kind.to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 1.0, y: 1.0 },
                width: 0.5,
                height: 0.5,
                connected_to: Vec::new(),
            });
        }

        remove_existing_copper_obstacles(&mut problem);

        assert!(
            !problem
                .obstacles
                .iter()
                .any(|obstacle| matches!(obstacle.kind.as_str(), "track" | "via"))
        );
        assert!(
            problem
                .obstacles
                .iter()
                .any(|obstacle| obstacle.kind == "keepout")
        );
        assert!(
            problem
                .obstacles
                .iter()
                .any(|obstacle| obstacle.kind.starts_with("pad:"))
        );
    }

    #[test]
    fn live_sensor_wide_net_escapes_and_routes_as_a_clean_tree() {
        let original = sensor_v3v3_problem();
        let (routing_problem, escapes) = prepare_wide_terminal_escapes(&original);
        assert_eq!(
            escapes.traces.len(),
            6,
            "three narrow pads need two-stage escapes"
        );
        let (subproblem, reserved) = reserve_wide_multi_pin_routes(&routing_problem);
        assert!(subproblem.connections.is_empty());
        assert!(!reserved.traces.is_empty());

        // Exercise the production merge order: reserved full-width copper,
        // then terminal neckdowns, then the shared cleanup pass.
        let mut result = RouteResult {
            solution: reserved,
            failed: Vec::new(),
            engine: "fixture".to_owned(),
        };
        result.solution.traces.extend(escapes.traces);
        result.solution.vias.extend(escapes.vias);
        postroute_cleanup(&original, &mut result.solution);
        assert!(lint(&original, &result.solution).is_empty());
        let neckdowns = result
            .solution
            .traces
            .iter()
            .filter(|trace| (trace.width - 0.2).abs() < geom::EPS)
            .collect::<Vec<_>>();
        assert_eq!(neckdowns.len(), 3);
        assert!(neckdowns.iter().all(|trace| {
            trace.path.len() == 2 && (trace.path[0].dist(trace.path[1]) - 0.1).abs() < geom::EPS
        }));
        assert!(result.failed.is_empty());
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
    fn congestion_json_uses_only_engine_produced_global_diagnostics() {
        assert_eq!(route_congestion_json(None, true), Value::Null);

        let global = GlobalRouteResult {
            plan: pcb_route_mesh::pathing::GlobalPlan { nets: Vec::new() },
            report: pcb_route_mesh::pathing::CongestionReport {
                iterations: 3,
                final_overflow: 2,
                edge_hotspots: Vec::new(),
                unrouted: Vec::new(),
            },
        };
        assert_eq!(route_congestion_json(Some(&global), false), Value::Null);
        assert_eq!(
            route_congestion_json(Some(&global), true),
            json!({
                "iterations": 3,
                "final_overflow": 2,
                "hotspots": [],
            })
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
    fn tuned_engine_reports_route_attempt_diagnostics() {
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
        };

        let run = route_with_engine(&problem);

        assert_eq!(run.attempts.len(), 1);
        assert_eq!(run.attempts[0].engine, run.result.engine);
        assert_eq!(run.attempts[0].failed_nets, 0);
        assert_eq!(run.attempts[0].geometry_violations, 0);
    }

    #[test]
    fn direct_rescue_fallback_rescues_clean_failed_net() {
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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

    fn direct_rescue_limit_fixture(net_count: usize) -> (RoutingView, RouteResult) {
        let connections = (0..net_count)
            .map(|index| {
                let y = 1.0 + index as f64;
                pcb_model::Connection {
                    name: format!("SIG{index}"),
                    points_to_connect: vec![
                        pcb_model::RoutePoint {
                            x: 1.0,
                            y,
                            layer: LayerRef::top(),
                        },
                        pcb_model::RoutePoint {
                            x: 5.0,
                            y,
                            layer: LayerRef::top(),
                        },
                    ],
                }
            })
            .collect::<Vec<_>>();
        let failed = connections
            .iter()
            .map(|connection| FailedNet {
                connection: connection.name.clone(),
                reason: "fixture failure".to_owned(),
            })
            .collect();
        (
            RoutingView {
                layer_count: 2,
                min_trace_width: 0.2,
                obstacles: Vec::new(),
                connections,
                bounds: pcb_model::Rect {
                    min_x: 0.0,
                    min_y: 0.0,
                    max_x: 6.0,
                    max_y: net_count as f64 + 1.0,
                },
                clearance: 0.15,
                via_diameter: 0.6,
                via_drill: 0.3,
                net_widths: Default::default(),
                outline: None,
                escape_layers: Default::default(),
                plane_nets: Default::default(),
                fixed_copper: Default::default(),
                nets: None,
            },
            RouteResult {
                solution: RouteSolution {
                    traces: Vec::new(),
                    vias: Vec::new(),
                },
                failed,
                engine: "naive".to_owned(),
            },
        )
    }

    #[test]
    fn direct_rescue_runs_at_failed_net_limit() {
        let (problem, mut result) = direct_rescue_limit_fixture(MAX_DIRECT_RESCUE_FAILED_NETS);

        assert!(apply_direct_rescue_fallback(&problem, &mut result));
        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), MAX_DIRECT_RESCUE_FAILED_NETS);
        assert!(
            lint(&problem, &result.solution).is_empty(),
            "every committed rescue must still pass the complete geometry and connectivity oracle"
        );
    }

    #[test]
    fn direct_rescue_skips_atomically_above_failed_net_limit() {
        let (problem, mut result) = direct_rescue_limit_fixture(MAX_DIRECT_RESCUE_FAILED_NETS + 1);
        let before = serde_json::to_vec(&result).expect("serialize fixture result");

        assert!(!apply_direct_rescue_fallback(&problem, &mut result));
        assert_eq!(
            serde_json::to_vec(&result).expect("serialize result after skipped rescue"),
            before
        );
    }

    #[test]
    fn direct_rescue_geometry_screen_rejects_out_of_bounds_atomically() {
        let (mut problem, mut result) = direct_rescue_limit_fixture(1);
        problem.bounds.min_x = 1.0;
        problem.bounds.max_x = 5.0;
        let before = serde_json::to_vec(&result).expect("serialize fixture result");

        // The centreline is unobstructed, but its half-width leaves the
        // deliberately tight bounds. Geometry rejection must remain atomic.
        assert!(!apply_direct_rescue_fallback(&problem, &mut result));
        assert_eq!(
            serde_json::to_vec(&result).expect("serialize rejected rescue result"),
            before,
            "a failed final oracle must not partially commit candidate copper"
        );
    }

    #[test]
    fn direct_rescue_geometry_screen_rejects_cross_net_terminal_contact() {
        let (mut problem, _) = direct_rescue_limit_fixture(1);
        problem.connections.push(pcb_model::Connection {
            name: "OTHER".to_owned(),
            points_to_connect: vec![pcb_model::RoutePoint {
                x: 3.0,
                y: 1.0,
                layer: LayerRef::top(),
            }],
        });
        let candidate = RouteSolution {
            traces: vec![Trace {
                connection: "SIG0".to_owned(),
                layer: LayerRef::top(),
                width: problem.min_trace_width,
                path: vec![Point2 { x: 1.0, y: 1.0 }, Point2 { x: 5.0, y: 1.0 }],
            }],
            vias: Vec::new(),
        };

        assert!(
            !direct_candidate_is_geometry_clean(&problem, &candidate, "SIG0"),
            "candidate selection must reject contact with foreign terminal copper before net-local connectivity validation"
        );
    }

    #[test]
    fn direct_rescue_treats_stacked_same_net_pads_as_connected() {
        let problem = RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "VBUS".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: failed(&["VBUS"]),
            engine: "naive".to_string(),
        };

        assert!(apply_direct_rescue_fallback(&problem, &mut result));
        assert!(result.failed.is_empty());
        assert!(lint(&problem, &result.solution).is_empty());
    }

    #[test]
    fn direct_fallback_ignores_stale_failed_net_copper() {
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
        };
        let mut result = RouteResult {
            solution: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            failed: failed(&["SIG"]),
            engine: "naive".to_string(),
        };

        let blocked_candidate = RouteSolution {
            traces: vec![Trace {
                connection: "SIG".to_owned(),
                layer: LayerRef::top(),
                width: problem.min_trace_width,
                path: vec![Point2 { x: 0.0, y: 0.0 }, Point2 { x: 4.0, y: 0.0 }],
            }],
            vias: Vec::new(),
        };
        assert!(
            !direct_candidate_is_geometry_clean(&problem, &blocked_candidate, "SIG"),
            "the exploration oracle must reject trace-obstacle clearance faults before full connectivity lint"
        );

        assert!(!apply_direct_rescue_fallback(&problem, &mut result));
        assert_eq!(result.failed.len(), 1);
        assert!(result.solution.traces.is_empty());
    }

    #[test]
    fn direct_rescue_fallback_can_escape_to_bottom_layer_with_vias() {
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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
        assert_eq!(result.solution.vias.len(), 2);
        assert_eq!(result.engine, "detailed+direct-rescue");
    }

    #[test]
    fn direct_rescue_fallback_keeps_bottom_layer_net_via_free() {
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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
    fn direct_fallback_rescues_clean_medium_multi_pin_net() {
        let problem = RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "BUS".to_string(),
                points_to_connect: (0..12)
                    .map(|index| pcb_model::RoutePoint {
                        x: 1.0 + f64::from(index) * 0.5,
                        y: 1.0,
                        layer: LayerRef::top(),
                    })
                    .collect(),
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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
        assert_eq!(
            lint_summary(&problem, &result.solution, &[], &Default::default()).real,
            0
        );
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

    fn bottom_plane_problem() -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![pcb_model::Connection {
                name: "GND".to_string(),
                points_to_connect: vec![
                    pcb_model::RoutePoint {
                        x: 1.13,
                        y: 1.13,
                        layer: LayerRef::top(),
                    },
                    pcb_model::RoutePoint {
                        x: 4.13,
                        y: 1.13,
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
            plane_nets: BTreeMap::from([("GND".to_string(), 1)]),
            fixed_copper: Default::default(),
            nets: None,
        }
    }

    #[test]
    fn plane_fanout_survives_route_honesty_and_spur_pruning() {
        let problem = bottom_plane_problem();
        let mut result = RouteResult {
            engine: "plane-fanout".to_string(),
            failed: vec![],
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "GND".to_string(),
                    layer: LayerRef::top(),
                    width: 0.6,
                    path: vec![Point2 { x: 1.13, y: 1.13 }, Point2 { x: 1.5, y: 1.5 }],
                }],
                vias: vec![Via {
                    connection: "GND".to_string(),
                    at: Point2 { x: 1.5, y: 1.5 },
                    diameter: 0.6,
                    drill: 0.3,
                    span: pcb_model::ViaSpan::Through,
                }],
            },
        };
        let planes = BTreeSet::from(["GND".to_string()]);

        assert_eq!(prune_dangling_spurs_if_safe(&problem, &mut result), 0);
        assert!(make_route_honest(&problem, &mut result, &planes).is_empty());
        assert_eq!(result.solution.traces.len(), 1);
        assert_eq!(result.solution.vias.len(), 1);
        assert!(result.failed.is_empty());
    }

    /// A net this call was not asked to route, and which has no copper, is a
    /// fact about the board the call started from — not a defect in the copper
    /// it made. Counting it as a real violation made every scoped repair refuse
    /// the whole board, which is precisely the state the partial-commit path
    /// creates: the two features would have been mutually exclusive.
    #[test]
    fn an_out_of_scope_net_with_no_copper_does_not_condemn_a_scoped_route() {
        let point = |x: f64| pcb_model::RoutePoint {
            x,
            y: 1.0,
            layer: LayerRef::top(),
        };
        let problem = RoutingView {
            fixed_copper: pcb_model::RouteSolution::default(),
            nets: None,
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![
                pcb_model::Connection {
                    name: "ROUTED".to_string(),
                    points_to_connect: vec![point(1.0), point(5.0)],
                },
                pcb_model::Connection {
                    name: "OUT_OF_SCOPE".to_string(),
                    points_to_connect: vec![point(10.0), point(14.0)],
                },
            ],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 20.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
        };
        let mut result = RouteResult {
            engine: "test".to_string(),
            failed: vec![],
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "ROUTED".to_string(),
                    layer: LayerRef::top(),
                    width: 0.2,
                    path: vec![Point2 { x: 1.0, y: 1.0 }, Point2 { x: 5.0, y: 1.0 }],
                }],
                vias: vec![],
            },
        };
        let planes = BTreeSet::new();

        // Without the record, the copperless net reads as a real violation.
        let violations = lint(&problem, &result.solution);
        assert_eq!(
            lint_summary_from_violations(&violations, &result.failed, &planes).real,
            1
        );

        append_failed(
            &mut result,
            "OUT_OF_SCOPE",
            "not in this call's `nets`, and it had no copper to keep",
        );
        let (_, remaining) = make_route_honest_with_report(&problem, &mut result, &planes);
        let split = lint_summary_from_violations(&remaining, &result.failed, &planes);

        assert_eq!(split.real, 0, "{remaining:?}");
        assert_eq!(split.expected_gaps, 1);
        assert_eq!(result.solution.traces.len(), 1, "the routed net is kept");

        let report = crate::diagnose::unrouted_report(
            &problem,
            &[],
            &result.failed,
            &["F.Cu".to_string()],
            Some(&BTreeSet::from(["ROUTED".to_string()])),
        );
        assert_eq!(report.len(), 1);
        assert_eq!(report[0]["net"], "OUT_OF_SCOPE");
        assert_eq!(report[0]["in_scope"], false);
        assert!(
            report[0]["suggestion"]
                .as_str()
                .unwrap()
                .contains("did not route OUT_OF_SCOPE"),
            "{}",
            report[0]["suggestion"]
        );
    }

    /// `route_board{nets}` is the repair loop after a `move_parts`, so the
    /// scope has to be unambiguous: absent means the whole board, and an empty
    /// list is a mistake rather than a request to route nothing.
    #[test]
    fn the_route_scope_is_the_whole_board_unless_nets_names_a_subset() {
        assert_eq!(requested_nets(&json!({})).unwrap(), None);
        assert_eq!(requested_nets(&json!({ "nets": null })).unwrap(), None);
        assert_eq!(
            requested_nets(&json!({ "nets": ["GND", "VBUS"] })).unwrap(),
            Some(BTreeSet::from(["GND".to_owned(), "VBUS".to_owned()]))
        );
        assert!(
            requested_nets(&json!({ "nets": [] }))
                .unwrap_err()
                .contains("omit it to route the whole board")
        );
        assert!(
            requested_nets(&json!({ "nets": "GND" }))
                .unwrap_err()
                .contains("must be an array")
        );
    }

    /// The honesty pass must leave NO real violation standing, whatever mix of
    /// defects the router handed it. A pass that runs out with violations
    /// remaining makes `route_board` refuse with the same count forever, and no
    /// `move_parts` or a re-sync can clear it.
    #[test]
    fn cleanup_leaves_no_real_violation_for_a_mixed_bag_of_defects() {
        let point = |x: f64, y: f64| pcb_model::RoutePoint {
            x,
            y,
            layer: LayerRef::top(),
        };
        let problem = RoutingView {
            fixed_copper: pcb_model::RouteSolution::default(),
            nets: None,
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![
                pcb_model::Connection {
                    name: "A".to_string(),
                    points_to_connect: vec![point(1.0, 1.0), point(5.0, 1.0)],
                },
                pcb_model::Connection {
                    name: "B".to_string(),
                    points_to_connect: vec![point(1.0, 1.25), point(5.0, 1.25)],
                },
                pcb_model::Connection {
                    name: "C".to_string(),
                    points_to_connect: vec![point(1.0, 8.0), point(5.0, 8.0)],
                },
            ],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 20.0,
                max_y: 10.0,
            },
            clearance: 0.5,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
        };
        let trace = |connection: &str, y: f64, to: f64| Trace {
            connection: connection.to_string(),
            layer: LayerRef::top(),
            width: 0.2,
            path: vec![Point2 { x: 1.0, y }, Point2 { x: to, y }],
        };
        let mut result = RouteResult {
            engine: "test".to_string(),
            failed: vec![],
            solution: RouteSolution {
                // A and B run 0.25 mm apart under a 0.5 mm rule; C stops short
                // of its own second terminal.
                traces: vec![
                    trace("A", 1.0, 5.0),
                    trace("B", 1.25, 5.0),
                    trace("C", 8.0, 3.0),
                ],
                vias: vec![],
            },
        };
        let planes = BTreeSet::new();

        let (dropped, remaining) = make_route_honest_with_report(&problem, &mut result, &planes);

        assert_eq!(dropped, vec!["A", "B", "C"]);
        assert!(result.solution.traces.is_empty());
        assert_eq!(
            lint_summary_from_violations(&remaining, &result.failed, &planes).real,
            0,
            "cleanup left violations standing: {remaining:?}"
        );
    }

    /// A short involving a plane net used to be exempt from the honesty drop, so
    /// the shorted copper stayed and the lint kept counting it — every later
    /// route_board refused with the same count, and no `move_parts` could clear
    /// it. Copper that shorts two nets is always dropped now.
    #[test]
    fn a_short_onto_a_plane_net_is_dropped_rather_than_refused_forever() {
        let mut problem = bottom_plane_problem();
        problem.connections.push(pcb_model::Connection {
            name: "V3V3".to_string(),
            points_to_connect: vec![
                pcb_model::RoutePoint {
                    x: 6.0,
                    y: 1.0,
                    layer: LayerRef::top(),
                },
                pcb_model::RoutePoint {
                    x: 8.0,
                    y: 1.0,
                    layer: LayerRef::top(),
                },
            ],
        });
        // V3V3's copper runs straight through GND's terminal, so the two nets
        // are electrically one.
        let mut result = RouteResult {
            engine: "test".to_string(),
            failed: vec![],
            solution: RouteSolution {
                traces: vec![Trace {
                    connection: "V3V3".to_string(),
                    layer: LayerRef::top(),
                    width: 0.2,
                    path: vec![Point2 { x: 6.0, y: 1.0 }, Point2 { x: 1.13, y: 1.13 }],
                }],
                vias: vec![],
            },
        };
        let planes = BTreeSet::from(["GND".to_string()]);

        let violations = lint(&problem, &result.solution);
        assert!(violations.iter().any(|v| matches!(
            v,
            DrcViolation::Connectivity {
                violation: ConnViolation::CrossNetMerge { .. }
            }
        )));

        let (dropped, remaining) = make_route_honest_with_report(&problem, &mut result, &planes);
        assert_eq!(
            dropped,
            vec!["V3V3".to_string()],
            "the plane keeps its copper"
        );
        assert!(result.solution.traces.is_empty());
        assert_eq!(
            lint_summary_from_violations(&remaining, &result.failed, &planes).real,
            0,
            "{remaining:?}"
        );
    }

    #[test]
    fn terminal_stubs_do_not_duplicate_a_plane_via_anchor() {
        let problem = bottom_plane_problem();
        let mut solution = RouteSolution {
            traces: vec![],
            vias: vec![Via {
                connection: "GND".to_string(),
                at: Point2 { x: 1.13, y: 1.13 },
                diameter: 0.6,
                drill: 0.3,
                span: pcb_model::ViaSpan::Through,
            }],
        };

        add_terminal_stubs(&problem, &mut solution, &BTreeSet::new());

        assert_eq!(solution.traces.len(), 1);
        assert_eq!(solution.traces[0].path[0], Point2 { x: 4.13, y: 1.13 });
    }

    #[test]
    fn dangling_route_spurs_are_pruned_before_write() {
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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
        let problem = RoutingView {
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
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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

    #[test]
    fn failed_route_summary_distinguishes_records_from_connections() {
        let summary = failed_route_summary(&[
            FailedNet {
                connection: "/IN2".to_string(),
                reason: "router could not find a path".to_string(),
            },
            FailedNet {
                connection: "/IN2".to_string(),
                reason: "connectivity cleanup dropped partial copper".to_string(),
            },
            FailedNet {
                connection: "/IN7".to_string(),
                reason: "router could not find a path".to_string(),
            },
        ]);

        assert_eq!(summary.record_count, 3);
        assert_eq!(summary.connection_count, 2);
        assert_eq!(summary.connections, vec!["/IN2", "/IN7"]);
        assert_eq!(summary.records[0]["connection"], "/IN2");
        assert_eq!(
            summary.records[1]["reason"],
            "connectivity cleanup dropped partial copper"
        );
    }
}
