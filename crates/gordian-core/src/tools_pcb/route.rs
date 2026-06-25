//! Routing orchestration for the active KiCAD board: route generation, IPC copper
//! write-back, and route-result lint/triage.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde_json::{Value, json};

use drc_lint::connectivity::Violation as ConnViolation;
use drc_lint::lint::{DrcViolation, lint};
use kicad_ipc::proto::kiapi::board::types::Net as KiNet;
use kicad_ipc::snapshot::ImportedPart;
use negotiated_mesh::pathing::global_route;
use negotiated_mesh::pipeline::route_auto;
use pcb_model::{
    FailedNet, LayerRef, Point2, RouteProblem, RouteResult, RouteSolution, Trace, ViaSpan,
};

use crate::tools::PcbToolCtx;

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

pub fn route_board(_input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    match route_live_board(ctx) {
        Ok(out) => Ok(out),
        Err(err) => Ok(json!({ "error": err })),
    }
}

fn route_live_board(ctx: &PcbToolCtx) -> std::result::Result<Value, String> {
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
    let mut result = route_auto(&rp);
    let _used_direct_fallback = apply_direct_two_pin_fallback(&rp, &mut result);
    let original_solution = result.solution.clone();
    let mut pruned_spurs = prune_dangling_spurs(&rp, &mut result.solution);
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
        "congestion": congestion,
        "escape_bottleneck": escape,
        "note": if result.failed.is_empty() {
            "routed and saved the KiCAD board cleanly"
        } else {
            "routed and saved the KiCAD board with honest failed nets"
        },
    }))
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
    let mut applied = false;
    for conn in &rp.connections {
        if !failed.contains(&conn.name) || conn.points_to_connect.len() != 2 {
            continue;
        }
        let a = conn.points_to_connect[0].point();
        let b = conn.points_to_connect[1].point();
        result.solution.traces.push(Trace {
            connection: conn.name.clone(),
            layer: LayerRef::top(),
            width: rp.net_width(&conn.name),
            path: vec![a, b],
        });
        applied = true;
    }
    result.failed.retain(|f| !failed.contains(&f.connection));
    if result.engine != "direct-two-pin" {
        result.engine = format!("{}+direct-two-pin", result.engine);
    }
    applied
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
    ctx: &PcbToolCtx,
    rp: &RouteProblem,
    solution: &RouteSolution,
    layer_names: &[String],
) -> std::result::Result<(), kicad_ipc::Error> {
    let path = ctx.pcb_path();
    let nets = ctx.kicad().with_session(&path, |session| {
        let nets: BTreeMap<String, KiNet> = session
            .kicad()
            .net_list()?
            .into_iter()
            .map(|n| (n.name.clone(), n))
            .collect();
        session.kicad().save()?;
        Ok(nets)
    })?;
    ctx.close_kicad_session();

    let items = route_items_for_file(rp, solution, layer_names, &nets)
        .map_err(|e| kicad_ipc::Error::Spawn(format!("could not build route S-expr: {e}")))?;
    append_route_items_to_board(&path, &items)
        .map_err(|e| kicad_ipc::Error::Spawn(format!("could not write routed board file: {e}")))
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

fn route_items_for_file(
    problem: &RouteProblem,
    solution: &RouteSolution,
    layer_names: &[String],
    nets: &BTreeMap<String, KiNet>,
) -> std::result::Result<String, String> {
    let mut out = String::new();
    let mut uuid_idx = 1usize;
    for trace in &solution.traces {
        let layer = layer_name(&trace.layer, problem.layer_count, layer_names);
        let net = net_code(nets, &trace.connection)?;
        for segment in trace.path.windows(2) {
            out.push_str("  ");
            push_segment_sexpr(
                &mut out,
                &segment[0],
                &segment[1],
                trace.width,
                layer,
                net,
                uuid_idx,
            );
            uuid_idx += 1;
            out.push('\n');
        }
    }
    for via in &solution.vias {
        let net = net_code(nets, &via.connection)?;
        let (from, to) = via_span_layer_names(&via.span, layer_names);
        out.push_str("  ");
        push_via_sexpr(&mut out, via, from, to, net, uuid_idx);
        uuid_idx += 1;
        out.push('\n');
    }
    Ok(out)
}

fn append_route_items_to_board(
    path: &std::path::Path,
    items: &str,
) -> std::result::Result<(), String> {
    if items.trim().is_empty() {
        return Ok(());
    }
    let mut text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let insert_at = text
        .rfind("\n)")
        .ok_or_else(|| format!("{} is not a KiCAD board S-expression", path.display()))?;
    let mut insert = String::new();
    if !text[..insert_at].ends_with('\n') {
        insert.push('\n');
    }
    insert.push_str(items);
    text.insert_str(insert_at, &insert);
    std::fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn push_segment_sexpr(
    out: &mut String,
    start: &Point2,
    end: &Point2,
    width: f64,
    layer: &str,
    net: i32,
    uuid_idx: usize,
) {
    out.push_str("(segment");
    push_xy(out, "start", start);
    push_xy(out, "end", end);
    out.push_str(&format!(
        " (width {}) (layer \"{}\") (net {}) (uuid \"{}\"))",
        fmt_mm(width),
        sexpr_escape(layer),
        net,
        route_uuid(uuid_idx),
    ));
}

fn push_via_sexpr(
    out: &mut String,
    via: &pcb_model::Via,
    from_layer: &str,
    to_layer: &str,
    net: i32,
    uuid_idx: usize,
) {
    let via_kind = match via.span {
        ViaSpan::Through => "",
        ViaSpan::Partial { micro: true, .. } => " micro",
        ViaSpan::Partial { micro: false, .. } => " blind",
    };
    out.push_str(&format!("(via{via_kind}"));
    push_xy(out, "at", &via.at);
    out.push_str(&format!(
        " (size {}) (drill {}) (layers \"{}\" \"{}\") (net {}) (uuid \"{}\"))",
        fmt_mm(via.diameter),
        fmt_mm(via.drill),
        sexpr_escape(from_layer),
        sexpr_escape(to_layer),
        net,
        route_uuid(uuid_idx),
    ));
}

fn push_xy(out: &mut String, tag: &str, p: &Point2) {
    out.push_str(&format!(" ({tag} {} {})", fmt_mm(p.x), fmt_mm(p.y)));
}

fn route_uuid(idx: usize) -> String {
    format!("00000000-0000-4000-8000-{idx:012x}")
}

fn fmt_mm(v: f64) -> String {
    let mut s = format!("{v:.6}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.push('0');
    }
    s
}

fn sexpr_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn net_code(nets: &BTreeMap<String, KiNet>, name: &str) -> std::result::Result<i32, String> {
    nets.get(name)
        .and_then(|net| net.code.as_ref().map(|code| code.value))
        .ok_or_else(|| format!("KiCAD board has no net code for `{name}`"))
}

fn layer_name<'a>(layer: &LayerRef, layer_count: u32, layer_names: &'a [String]) -> &'a str {
    let idx = layer.index(layer_count).unwrap_or(0) as usize;
    layer_names.get(idx).map(String::as_str).unwrap_or("F.Cu")
}

fn via_span_layer_names<'a>(span: &ViaSpan, layer_names: &'a [String]) -> (&'a str, &'a str) {
    match *span {
        ViaSpan::Through => {
            let top = layer_names.first().map(String::as_str).unwrap_or("F.Cu");
            let bottom = layer_names.last().map(String::as_str).unwrap_or("B.Cu");
            (top, bottom)
        }
        ViaSpan::Partial { from, to, .. } => {
            let a = layer_names.get(from as usize).map(String::as_str);
            let b = layer_names.get(to as usize).map(String::as_str);
            (a.unwrap_or("F.Cu"), b.unwrap_or("B.Cu"))
        }
    }
}

#[cfg(test)]
mod escape_bottleneck_tests {
    use super::*;
    use kicad_ipc::proto::kiapi::board::types::NetCode;

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
    fn via_routes_are_emitted_as_native_kicad_pcb_items() {
        let mut nets = BTreeMap::new();
        nets.insert(
            "GND".to_string(),
            KiNet {
                code: Some(NetCode { value: 7 }),
                name: "GND".to_string(),
            },
        );
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 20.0,
                max_y: 20.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };
        let solution = RouteSolution {
            traces: vec![Trace {
                connection: "GND".to_string(),
                layer: LayerRef::top(),
                width: 0.25,
                path: vec![Point2 { x: 1.0, y: 2.0 }, Point2 { x: 3.0, y: 4.0 }],
            }],
            vias: vec![pcb_model::Via {
                connection: "GND".to_string(),
                at: Point2 { x: 3.0, y: 4.0 },
                diameter: 0.6,
                drill: 0.3,
                span: ViaSpan::Through,
            }],
        };

        let sexpr =
            route_items_for_file(&problem, &solution, &["F.Cu".into(), "B.Cu".into()], &nets)
                .unwrap();

        assert!(sexpr.contains("(segment (start 1.0 2.0) (end 3.0 4.0)"));
        assert!(sexpr.contains("(width 0.25) (layer \"F.Cu\") (net 7)"));
        assert!(sexpr.contains("(via (at 3.0 4.0) (size 0.6) (drill 0.3)"));
        assert!(sexpr.contains("(layers \"F.Cu\" \"B.Cu\") (net 7)"));
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
                    width: 0.25,
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
}
