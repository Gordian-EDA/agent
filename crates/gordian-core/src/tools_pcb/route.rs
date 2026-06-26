//! Routing orchestration for the active KiCAD board: route generation, IPC copper
//! write-back, and route-result lint/triage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use serde_json::{Value, json};

use drc_lint::connectivity::Violation as ConnViolation;
use drc_lint::lint::{DrcViolation, drop_unconnected_copper, lint};
use kicad_ipc::snapshot::ImportedPart;
use negotiated_mesh::pathing::global_route;
use negotiated_mesh::pipeline::{NegotiatedMeshRouter, route_auto, select_best};
use pcb_model::{
    FailedNet, LayerRef, Point2, RouteProblem, RouteResult, RouteSolution, Trace, Via, ViaSpan,
};
use pcb_place::placement::Placement;

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

/// Build the congestion-hotspot enrichment for a FAILED route. `route_auto` does
/// not expose the global stage's congestion report, so we re-run the global
/// router here ONLY on failure to recover the hotspots/iterations for the model's
/// triage. NOTE (duplication cost): this repeats the global-routing pass that
/// `route_auto` already ran internally; it is paid only on the failure path, on
/// boards small enough that one extra global route is cheap. If `route_auto`
/// later surfaces the congestion report directly, drop this.
fn congestion_json(rp: &RouteProblem) -> Value {
    let g = global_route(rp);
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
    if super::place::board_placement_path(ctx).exists() {
        return Ok(route_file_board(ctx));
    }
    match route_live_board(ctx) {
        Ok(out) => Ok(out),
        Err(err) => Ok(json!({ "error": err })),
    }
}

fn route_file_board(ctx: &AgentRuntime) -> Value {
    let seed = match std::fs::read_to_string(super::create::board_seed_path(ctx))
        .ok()
        .and_then(|text| serde_json::from_str::<super::seed::BoardSeed>(&text).ok())
    {
        Some(seed) => seed,
        None => {
            return json!({ "error": "no board seed metadata — run regenerate_board, then place_board before route_board" });
        }
    };
    let placements = match std::fs::read_to_string(super::place::board_placement_path(ctx))
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<Placement>>(&text).ok())
    {
        Some(placements) => placements,
        None => {
            return json!({ "error": "no board placement metadata — run place_board before route_board" });
        }
    };
    let problem = match super::place::debug_place_problem_from_seed(&seed, ctx) {
        Ok(problem) => problem,
        Err(msg) => return json!({ "error": msg }),
    };
    let mut rp = pcb_model::place::to_route_problem(&problem, &placements);
    rp.via_diameter = seed.rules.via_diameter;
    rp.via_drill = seed.rules.via_drill;
    rp.net_widths = seed.rules.net_widths.clone();
    let poured_nets: BTreeSet<String> = seed
        .rules
        .pours
        .iter()
        .map(|pour| pour.net.clone())
        .collect();
    let poured_connections: Vec<_> = rp
        .connections
        .iter()
        .filter(|conn| poured_nets.contains(&conn.name))
        .cloned()
        .collect();
    let route_rp = if poured_nets.is_empty() {
        rp.clone()
    } else {
        let mut filtered = rp.clone();
        filtered
            .connections
            .retain(|conn| !poured_nets.contains(&conn.name));
        filtered
    };

    let external_route_error = match route_file_board_with_freerouting(ctx) {
        Ok(Some(out)) => return out,
        Ok(None) => None,
        Err(err) => Some(err),
    };

    let requested_engine = ctx.config().engines.pcb_router;
    let engine = file_route_engine(requested_engine, &route_rp);
    let mut result = route_with_engine(&route_rp, engine);
    let used_direct_fallback = apply_direct_two_pin_fallback(&route_rp, &mut result);
    let dropped_failed = drop_failed_net_copper(&mut result);
    let failed = failed_connections(&result);
    add_terminal_stubs(&route_rp, &mut result.solution, &failed);
    let original_solution = result.solution.clone();
    let mut pruned_spurs = prune_dangling_spurs_if_safe(&route_rp, &mut result);
    let dropped = make_route_honest(&route_rp, &mut result);
    let mut split = lint_summary(
        &route_rp,
        &result.solution,
        &result.failed,
        &Default::default(),
    );
    if split.real > 0 && pruned_spurs > 0 {
        result.solution = original_solution;
        pruned_spurs = 0;
        split = lint_summary(
            &route_rp,
            &result.solution,
            &result.failed,
            &Default::default(),
        );
    }
    if split.real > 0 {
        return json!({
            "error": format!("router produced {} real DRC violation(s); refusing to write copper to board file", split.real),
            "lint_summary": split.by_kind,
        });
    }
    if let Err(msg) =
        append_route_to_board_file(ctx, &route_rp, &result.solution, &poured_connections)
    {
        return json!({ "error": msg });
    }
    let failed: Vec<Value> = result
        .failed
        .iter()
        .map(|f| json!({ "connection": f.connection, "reason": f.reason }))
        .collect();
    let m = result.solution.metrics();
    json!({
        "router": result.engine,
        "requested_router": format!("{requested_engine:?}"),
        "router_note": if requested_engine != engine {
            "Auto router skipped for a larger file-based route problem to avoid timeout; used Astar"
        } else {
            "used requested router"
        },
        "failed": failed,
        "metrics": {
            "wirelength": m.wirelength,
            "vias": m.via_count,
            "traces": m.trace_count,
        },
        "lint_summary": split.by_kind,
        "expected_connectivity_gaps": split.expected_gaps,
        "pruned_dangling_spurs": pruned_spurs,
        "direct_two_pin_fallback": used_direct_fallback,
        "dropped_failed_net_copper": dropped_failed,
        "dropped_violating_nets": dropped,
        "poured_nets": poured_nets.into_iter().collect::<Vec<_>>(),
        "stitch_vias": stitch_via_count(&poured_connections, &route_rp),
        "external_router_error": external_route_error,
        "note": if result.failed.is_empty() {
            "routed and wrote copper directly to the board file"
        } else {
            "wrote routeable copper to the board file with honest failed nets"
        },
    })
}

fn route_file_board_with_freerouting(
    ctx: &AgentRuntime,
) -> std::result::Result<Option<Value>, String> {
    let Some(jar) = freerouting_jar_path() else {
        return Ok(None);
    };
    let pcb = ctx.pcb_path();
    let dsn = pcb.with_extension("dsn");
    let ses = pcb.with_extension("ses");
    let export = run_python(
        FREEROUTING_EXPORT_DSN_PY,
        &[pcb.as_path(), dsn.as_path()],
        "exporting board to Specctra DSN",
    )?;
    if !export.lines().any(|line| line.trim() == "exported") {
        return Err(format!(
            "unexpected DSN export output: {}",
            short_output(&export)
        ));
    }

    let java = freerouting_java_path();
    let mut cmd = Command::new("timeout");
    cmd.arg("240")
        .arg("xvfb-run")
        .arg("-a")
        .arg(java)
        .arg("-jar")
        .arg(&jar)
        .arg("-de")
        .arg(&dsn)
        .arg("-do")
        .arg(&ses)
        .arg("-mp")
        .arg(freerouting_pass_limit().to_string());
    run_command(cmd, "running Freerouting")?;

    let summary = run_python(
        FREEROUTING_IMPORT_REPAIR_PY,
        &[pcb.as_path(), ses.as_path()],
        "importing Freerouting SES",
    )?;
    let summary = summary
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| "Freerouting import produced no summary".to_string())?;
    let summary: Value = serde_json::from_str(summary)
        .map_err(|e| format!("could not parse Freerouting import summary: {e}: {summary}"))?;
    Ok(Some(json!({
        "router": "freerouting",
        "requested_router": format!("{:?}", ctx.config().engines.pcb_router),
        "failed": [],
        "metrics": {
            "tracks_and_vias": summary.get("tracks_and_vias").cloned().unwrap_or(Value::Null),
            "zones": summary.get("zones").cloned().unwrap_or(Value::Null),
        },
        "postprocess": summary.get("postprocess").cloned().unwrap_or(Value::Null),
        "dsn": dsn,
        "ses": ses,
        "note": "routed with Freerouting, imported SES, refilled zones, and saved the KiCad board file",
    })))
}

fn freerouting_jar_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("GORDIAN_FREEROUTING_JAR") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    let path = PathBuf::from("tools/vendor/freerouting.jar");
    path.exists().then_some(path)
}

fn freerouting_java_path() -> PathBuf {
    if let Ok(path) = std::env::var("GORDIAN_FREEROUTING_JAVA") {
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }
    let java21 = PathBuf::from("/usr/lib/jvm/java-21-openjdk-amd64/bin/java");
    if java21.exists() {
        return java21;
    }
    PathBuf::from("java")
}

fn freerouting_pass_limit() -> u32 {
    std::env::var("GORDIAN_FREEROUTING_PASSES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(300)
}

fn run_python(script: &str, args: &[&Path], what: &str) -> std::result::Result<String, String> {
    let mut cmd = Command::new("python3");
    cmd.arg("-c").arg(script);
    for arg in args {
        cmd.arg(arg);
    }
    run_command(cmd, what)
}

fn run_command(mut cmd: Command, what: &str) -> std::result::Result<String, String> {
    let output = cmd
        .output()
        .map_err(|e| format!("{what} failed to start: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{what} failed with status {}: stdout: {}; stderr: {}",
            output.status,
            short_output(&String::from_utf8_lossy(&output.stdout)),
            short_output(&String::from_utf8_lossy(&output.stderr)),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn short_output(text: &str) -> String {
    const MAX: usize = 1200;
    let text = text.trim();
    if text.len() <= MAX {
        text.to_string()
    } else {
        format!("...{}", &text[text.len() - MAX..])
    }
}

const FREEROUTING_EXPORT_DSN_PY: &str = r#"
import sys
import pcbnew

board_path, dsn_path = sys.argv[1], sys.argv[2]
board = pcbnew.LoadBoard(board_path)
if len(list(board.GetTracks())):
    raise SystemExit("board already contains routed copper; regenerate_board before route_board")
removed_zones = 0
for zone in list(board.Zones()):
    board.Remove(zone)
    removed_zones += 1
ok = pcbnew.ExportSpecctraDSN(board, dsn_path)
if not ok:
    raise SystemExit("pcbnew.ExportSpecctraDSN returned false")
if removed_zones:
    pcbnew.SaveBoard(board_path, board)
print("exported")
"#;

const FREEROUTING_IMPORT_REPAIR_PY: &str = r#"
import json
import math
import sys
import pcbnew

board_path, ses_path = sys.argv[1], sys.argv[2]
board = pcbnew.LoadBoard(board_path)
ok = pcbnew.ImportSpecctraSES(board, ses_path)
if not ok:
    raise SystemExit("pcbnew.ImportSpecctraSES returned false")

IU = lambda mm: int(round(mm * 1_000_000))

def point(obj):
    return (obj.x / 1_000_000.0, obj.y / 1_000_000.0)

def is_via(item):
    return type(item).__name__ == "PCB_VIA"

def add_track(net_name, a, b, width=0.15, layer=pcbnew.F_Cu):
    net = board.FindNet(net_name)
    if net is None:
        return False
    track = pcbnew.PCB_TRACK(board)
    track.SetNet(net)
    track.SetLayer(layer)
    track.SetStart(pcbnew.VECTOR2I(IU(a[0]), IU(a[1])))
    track.SetEnd(pcbnew.VECTOR2I(IU(b[0]), IU(b[1])))
    track.SetWidth(IU(width))
    board.Add(track)
    return True

def add_via(net_name, at, diameter=0.5, drill=0.3):
    net = board.FindNet(net_name)
    if net is None:
        return False
    via = pcbnew.PCB_VIA(board)
    via.SetNet(net)
    via.SetPosition(pcbnew.VECTOR2I(IU(at[0]), IU(at[1])))
    via.SetLayerPair(pcbnew.F_Cu, pcbnew.B_Cu)
    try:
        via.SetWidth(pcbnew.F_Cu, IU(diameter))
    except TypeError:
        via.SetWidth(IU(diameter))
    via.SetDrill(IU(drill))
    board.Add(via)
    return True

def pad_at(ref, number, net_name, x, y):
    for fp in board.GetFootprints():
        if fp.GetReference() != ref:
            continue
        for pad in fp.Pads():
            pos = point(pad.GetPosition())
            if (
                pad.GetNumber() == str(number)
                and pad.GetNetname() == net_name
                and abs(pos[0] - x) < 0.01
                and abs(pos[1] - y) < 0.01
            ):
                return True
    return False

def has_via(net_name, x, y):
    for item in board.GetTracks():
        if not is_via(item) or item.GetNetname() != net_name:
            continue
        pos = point(item.GetPosition())
        if abs(pos[0] - x) < 0.01 and abs(pos[1] - y) < 0.01:
            return True
    return False

post = {
    "zones_filled": 0,
}

zones = list(board.Zones())
if zones:
    pcbnew.ZONE_FILLER(board).Fill(zones)
    post["zones_filled"] = len(zones)

pcbnew.SaveBoard(board_path, board)

tracks = list(board.GetTracks())
summary = {
    "tracks_and_vias": len(tracks),
    "zones": len(zones),
    "postprocess": post,
}
print(json.dumps(summary, sort_keys=True))
"#;

fn route_live_board(ctx: &AgentRuntime) -> std::result::Result<Value, String> {
    let board = super::active::board_problem(ctx)?;
    if is_seed_placement(&board.imported.bounds, &board.imported.parts) {
        return Err("board has only the initial seed-row footprint positions — run place_board before route_board".to_owned());
    }

    let existing = (board.copper.traces.len(), board.copper.vias.len());
    if existing.0 > 0 || existing.1 > 0 {
        return Err(format!(
            "live board already has {} tracks and {} vias; refusing to replace copper until generated-item tagging is implemented",
            existing.0, existing.1
        ));
    }

    let rp = board.problem.clone();
    let mut result = route_with_engine(&rp, ctx.config().engines.pcb_router);
    let _used_direct_fallback = apply_direct_two_pin_fallback(&rp, &mut result);
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
        "congestion": congestion,
        "escape_bottleneck": escape,
        "note": if result.failed.is_empty() {
            "routed and saved the KiCAD board cleanly"
        } else {
            "routed and saved the KiCAD board with honest failed nets"
        },
    }))
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

fn route_with_engine(rp: &RouteProblem, engine: PcbRouterEngine) -> RouteResult {
    match engine {
        PcbRouterEngine::Auto => route_auto(rp),
        PcbRouterEngine::Astar => {
            let grid = grid_astar::router::GridAStarRouter;
            select_best(rp, &[&grid])
        }
        PcbRouterEngine::Mesh => {
            let mesh = NegotiatedMeshRouter;
            select_best(rp, &[&mesh])
        }
    }
}

fn file_route_engine(requested: PcbRouterEngine, rp: &RouteProblem) -> PcbRouterEngine {
    const AUTO_FILE_ROUTE_CONNECTION_LIMIT: usize = 20;
    if requested == PcbRouterEngine::Auto && rp.connections.len() > AUTO_FILE_ROUTE_CONNECTION_LIMIT
    {
        PcbRouterEngine::Astar
    } else {
        requested
    }
}

fn apply_direct_two_pin_fallback(rp: &RouteProblem, result: &mut RouteResult) -> bool {
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
    for conn in &rp.connections {
        if !failed.contains(&conn.name) {
            continue;
        }
        let candidate = if conn.points_to_connect.len() == 2 {
            direct_two_pin_candidate(rp, &result.solution, conn)
        } else if (3..=6).contains(&conn.points_to_connect.len()) {
            direct_multi_pin_candidate(rp, &result.solution, conn)
        } else {
            None
        };
        if let Some(solution) = candidate {
            result.solution = solution;
            applied.insert(conn.name.clone());
        }
    }
    if applied.is_empty() {
        return false;
    }
    result.failed.retain(|f| !applied.contains(&f.connection));
    if result.engine != "direct-two-pin" {
        result.engine = format!("{}+direct-two-pin", result.engine);
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
    for layer in direct_candidate_layers(rp.layer_count) {
        for path in direct_candidate_paths(rp, a, b) {
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
    let anchor = conn.points_to_connect[0].point();
    for layer in direct_candidate_layers(rp.layer_count) {
        let mut candidate = solution.clone();
        if layer.index(rp.layer_count) != Some(0) {
            for point in &conn.points_to_connect {
                candidate.vias.push(Via {
                    connection: conn.name.clone(),
                    at: point.point(),
                    diameter: rp.via_diameter,
                    drill: rp.via_drill,
                    span: ViaSpan::Through,
                });
            }
        }
        let mut ok = true;
        for point in conn.points_to_connect.iter().skip(1) {
            let mut routed_leg = false;
            for path in direct_candidate_paths(rp, anchor, point.point()) {
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

fn direct_candidate_paths(rp: &RouteProblem, a: Point2, b: Point2) -> Vec<Vec<Point2>> {
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    push_candidate_path(&mut paths, &mut seen, vec![a, b]);
    let mut xs = vec![a.x, b.x, (a.x + b.x) / 2.0];
    let mut ys = vec![a.y, b.y, (a.y + b.y) / 2.0];
    let inset = (rp.clearance + rp.min_trace_width + 0.5).max(1.0);
    xs.extend([rp.bounds.min_x + inset, rp.bounds.max_x - inset]);
    ys.extend([rp.bounds.min_y + inset, rp.bounds.max_y - inset]);
    for obstacle in &rp.obstacles {
        let dx = obstacle.width / 2.0 + rp.clearance + rp.min_trace_width;
        let dy = obstacle.height / 2.0 + rp.clearance + rp.min_trace_width;
        xs.extend([obstacle.center.x - dx, obstacle.center.x + dx]);
        ys.extend([obstacle.center.y - dy, obstacle.center.y + dy]);
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
    mut path: Vec<Point2>,
) {
    path.dedup_by(|a, b| a.dist(*b) < geom::EPS);
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
    let a = conn.points_to_connect[0].point();
    let b = conn.points_to_connect[1].point();
    if layer.index(rp.layer_count) != Some(0) {
        candidate.vias.push(Via {
            connection: conn.name.clone(),
            at: a,
            diameter: rp.via_diameter,
            drill: rp.via_drill,
            span: ViaSpan::Through,
        });
        candidate.vias.push(Via {
            connection: conn.name.clone(),
            at: b,
            diameter: rp.via_diameter,
            drill: rp.via_drill,
            span: ViaSpan::Through,
        });
    }
    candidate.traces.push(Trace {
        connection: conn.name.clone(),
        layer,
        width: rp.net_width(&conn.name),
        path,
    });
    candidate
}

fn simplify_candidate_paths(solution: &mut RouteSolution) {
    for trace in &mut solution.traces {
        trace.path.dedup_by(|a, b| a.dist(*b) < geom::EPS);
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

fn append_route_to_board_file(
    ctx: &AgentRuntime,
    rp: &RouteProblem,
    solution: &RouteSolution,
    poured_connections: &[pcb_model::Connection],
) -> std::result::Result<(), String> {
    let path = ctx.pcb_path();
    let board = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    if board.contains("\n\t(segment ") || board.contains("\n\t(via ") {
        return Err("board already contains routed copper; regenerate_board before route_board to replace it".to_owned());
    }
    let net_codes = parse_board_net_codes(&board);
    let mut copper = String::new();
    let mut item = 0usize;
    for trace in &solution.traces {
        let net = resolve_board_net_code(&net_codes, &trace.connection)
            .ok_or_else(|| format!("board has no KiCad net code for {}", trace.connection))?;
        let layer = LayerRef::resolve(&trace.layer.0, rp.layer_count)
            .map(|(_, name)| name)
            .unwrap_or_else(|| "F.Cu".to_string());
        for pair in trace.path.windows(2) {
            let a = pair[0];
            let b = pair[1];
            copper.push_str(&format!(
                "\t(segment (start {} {}) (end {} {}) (width {}) (layer \"{}\") (net {}) (uuid {}))\n",
                super::fmt_num(a.x),
                super::fmt_num(a.y),
                super::fmt_num(b.x),
                super::fmt_num(b.y),
                super::fmt_num(trace.width),
                layer,
                net,
                route_uuid(item),
            ));
            item += 1;
        }
    }
    for via in &solution.vias {
        let net = resolve_board_net_code(&net_codes, &via.connection)
            .ok_or_else(|| format!("board has no KiCad net code for {}", via.connection))?;
        let layers = match via.span {
            ViaSpan::Through => ("F.Cu".to_string(), "B.Cu".to_string(), None),
            ViaSpan::Partial { from, to, micro } => {
                let from = layer_name(from, rp.layer_count).unwrap_or_else(|| "F.Cu".to_string());
                let to = layer_name(to, rp.layer_count).unwrap_or_else(|| "B.Cu".to_string());
                (from, to, Some(if micro { "micro" } else { "blind" }))
            }
        };
        let kind = layers.2.map(|kind| format!(" {kind}")).unwrap_or_default();
        copper.push_str(&format!(
            "\t(via{} (at {} {}) (size {}) (drill {}) (layers \"{}\" \"{}\") (net {}) (uuid {}))\n",
            kind,
            super::fmt_num(via.at.x),
            super::fmt_num(via.at.y),
            super::fmt_num(via.diameter),
            super::fmt_num(via.drill),
            layers.0,
            layers.1,
            net,
            route_uuid(item),
        ));
        item += 1;
    }
    append_poured_net_stitch_vias(&mut copper, &net_codes, poured_connections, rp, &mut item)?;
    let insert = board
        .rfind("\n)")
        .ok_or_else(|| "could not find end of KiCad board file".to_string())?;
    let mut out = String::with_capacity(board.len() + copper.len());
    out.push_str(&board[..insert]);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&copper);
    out.push_str(&board[insert..]);
    std::fs::write(&path, out).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn append_poured_net_stitch_vias(
    copper: &mut String,
    net_codes: &BTreeMap<String, i32>,
    poured_connections: &[pcb_model::Connection],
    rp: &RouteProblem,
    item: &mut usize,
) -> std::result::Result<(), String> {
    let mut seen = BTreeSet::new();
    for conn in poured_connections {
        let net = resolve_board_net_code(net_codes, &conn.name)
            .ok_or_else(|| format!("board has no KiCad net code for {}", conn.name))?;
        for point in &conn.points_to_connect {
            let key = (
                conn.name.clone(),
                quantize_mm(point.x),
                quantize_mm(point.y),
            );
            if !seen.insert(key) {
                continue;
            }
            if !stitch_via_clears_foreign_obstacles(&conn.name, point.point(), rp) {
                continue;
            }
            copper.push_str(&format!(
                "\t(via (at {} {}) (size {}) (drill {}) (layers \"F.Cu\" \"B.Cu\") (net {}) (uuid {}))\n",
                super::fmt_num(point.x),
                super::fmt_num(point.y),
                super::fmt_num(rp.via_diameter),
                super::fmt_num(rp.via_drill),
                net,
                route_uuid(*item),
            ));
            *item += 1;
        }
    }
    Ok(())
}

fn stitch_via_count(poured_connections: &[pcb_model::Connection], rp: &RouteProblem) -> usize {
    poured_connections
        .iter()
        .flat_map(|conn| {
            conn.points_to_connect.iter().filter_map(move |point| {
                stitch_via_clears_foreign_obstacles(&conn.name, point.point(), rp).then_some((
                    conn.name.clone(),
                    quantize_mm(point.x),
                    quantize_mm(point.y),
                ))
            })
        })
        .collect::<BTreeSet<_>>()
        .len()
}

fn stitch_via_clears_foreign_obstacles(net: &str, at: Point2, rp: &RouteProblem) -> bool {
    let required = rp.via_diameter / 2.0 + rp.clearance;
    rp.obstacles.iter().all(|obstacle| {
        if obstacle.connected_to.iter().any(|owned| owned == net) {
            return true;
        }
        let dx = (at.x - obstacle.center.x).abs() - obstacle.width / 2.0;
        let dy = (at.y - obstacle.center.y).abs() - obstacle.height / 2.0;
        let outside_x = dx.max(0.0);
        let outside_y = dy.max(0.0);
        let distance = if dx <= 0.0 && dy <= 0.0 {
            0.0
        } else {
            outside_x.hypot(outside_y)
        };
        distance + geom::EPS >= required
    })
}

fn parse_board_net_codes(board: &str) -> BTreeMap<String, i32> {
    let mut out = BTreeMap::new();
    for line in board.lines().map(str::trim) {
        if !line.starts_with("(net ") {
            continue;
        }
        let rest = &line["(net ".len()..];
        let Some((code, rest)) = rest.split_once(' ') else {
            continue;
        };
        let Ok(code) = code.parse::<i32>() else {
            continue;
        };
        let Some(name_start) = rest.find('"') else {
            continue;
        };
        let rest = &rest[name_start + 1..];
        let Some(name_end) = rest.find('"') else {
            continue;
        };
        out.insert(rest[..name_end].to_string(), code);
    }
    out
}

fn resolve_board_net_code(net_codes: &BTreeMap<String, i32>, connection: &str) -> Option<i32> {
    net_codes
        .get(connection)
        .copied()
        .or_else(|| {
            connection
                .strip_prefix('/')
                .and_then(|name| net_codes.get(name).copied())
        })
        .or_else(|| net_codes.get(&format!("/{connection}")).copied())
}

fn layer_name(index: u32, layer_count: u32) -> Option<String> {
    match index {
        0 => Some("F.Cu".to_string()),
        i if i + 1 == layer_count => Some("B.Cu".to_string()),
        i if i > 0 && i + 1 < layer_count => Some(format!("In{i}.Cu")),
        _ => None,
    }
}

fn route_uuid(item: usize) -> String {
    format!("00000000-0000-4000-8000-{item:012x}")
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
    fn poured_net_stitch_count_deduplicates_pad_points() {
        let connections = vec![pcb_model::Connection {
            name: "GND".to_string(),
            points_to_connect: vec![
                pcb_model::RoutePoint {
                    x: 1.0,
                    y: 2.0,
                    layer: LayerRef::top(),
                },
                pcb_model::RoutePoint {
                    x: 1.0,
                    y: 2.0,
                    layer: LayerRef::top(),
                },
                pcb_model::RoutePoint {
                    x: 3.0,
                    y: 4.0,
                    layer: LayerRef::top(),
                },
            ],
        }];

        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![
                pcb_model::Obstacle {
                    kind: "rect".to_string(),
                    layers: vec![LayerRef::top()],
                    center: Point2 { x: 1.0, y: 1.0 },
                    width: 0.6,
                    height: 0.6,
                    connected_to: vec!["SIG".to_string()],
                },
                pcb_model::Obstacle {
                    kind: "rect".to_string(),
                    layers: vec![LayerRef::top()],
                    center: Point2 { x: 5.0, y: 1.0 },
                    width: 0.6,
                    height: 0.6,
                    connected_to: vec!["SIG".to_string()],
                },
            ],
            connections: vec![],
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

        assert_eq!(stitch_via_count(&connections, &problem), 2);
    }

    #[test]
    fn poured_net_stitch_count_skips_foreign_pad_clearance() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![pcb_model::Obstacle {
                kind: "rect".to_string(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 1.4, y: 2.0 },
                width: 0.25,
                height: 0.25,
                connected_to: vec!["GPIO0".to_string()],
            }],
            connections: vec![],
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
        let connections = vec![pcb_model::Connection {
            name: "GND".to_string(),
            points_to_connect: vec![pcb_model::RoutePoint {
                x: 1.0,
                y: 2.0,
                layer: LayerRef::top(),
            }],
        }];

        assert_eq!(stitch_via_count(&connections, &problem), 0);
    }

    #[test]
    fn direct_two_pin_fallback_rescues_clean_failed_net() {
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

        assert!(apply_direct_two_pin_fallback(&problem, &mut result));
        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), 1);
    }

    #[test]
    fn direct_two_pin_fallback_rescues_clean_dogleg() {
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

        assert!(apply_direct_two_pin_fallback(&problem, &mut result));
        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), 1);
        assert!(result.solution.traces[0].path.len() >= 3);
    }

    #[test]
    fn direct_two_pin_fallback_rejects_foreign_copper() {
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

        assert!(!apply_direct_two_pin_fallback(&problem, &mut result));
        assert_eq!(result.failed.len(), 1);
        assert!(result.solution.traces.is_empty());
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

        assert!(apply_direct_two_pin_fallback(&problem, &mut result));
        assert!(result.failed.is_empty());
        assert_eq!(result.solution.traces.len(), 2);
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

    #[test]
    fn board_net_lookup_accepts_leading_slash_variants() {
        let mut codes = BTreeMap::new();
        codes.insert("/HOLD_N".to_string(), 3);
        codes.insert("RUN".to_string(), 22);

        assert_eq!(resolve_board_net_code(&codes, "/HOLD_N"), Some(3));
        assert_eq!(resolve_board_net_code(&codes, "HOLD_N"), Some(3));
        assert_eq!(resolve_board_net_code(&codes, "/RUN"), Some(22));
        assert_eq!(resolve_board_net_code(&codes, "MISSING"), None);
    }

    #[test]
    fn file_route_auto_downgrades_only_for_larger_problems() {
        let mut problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.15,
            obstacles: vec![],
            connections: vec![],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 100.0,
                max_y: 100.0,
            },
            clearance: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        assert_eq!(
            file_route_engine(PcbRouterEngine::Auto, &problem),
            PcbRouterEngine::Auto
        );
        problem.connections = (0..21)
            .map(|idx| pcb_model::Connection {
                name: format!("N{idx}"),
                points_to_connect: vec![],
            })
            .collect();
        assert_eq!(
            file_route_engine(PcbRouterEngine::Auto, &problem),
            PcbRouterEngine::Astar
        );
        assert_eq!(
            file_route_engine(PcbRouterEngine::Mesh, &problem),
            PcbRouterEngine::Mesh
        );
    }
}
