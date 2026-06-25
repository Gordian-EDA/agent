//! Routing orchestration for the active KiCAD board: route generation, IPC copper
//! write-back, and route-result lint/triage.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use drc_lint::connectivity::Violation as ConnViolation;
use drc_lint::lint::{DrcViolation, lint};
use kicad_ipc::proto::kiapi::board::types::{
    BoardLayer, DrillProperties, DrillShape, Net as KiNet, PadStack, PadStackLayer, PadStackShape,
    PadStackType, Track as KiTrack, UnconnectedLayerRemoval, Via as KiVia, ViaType,
};
use kicad_ipc::proto::kiapi::common::types::{Distance, Vector2};
use negotiated_mesh::pathing::global_route;
use negotiated_mesh::pipeline::route_auto;
use pcb_model::{FailedNet, LayerRef, Obstacle, Point2, RouteProblem, RouteSolution, ViaSpan};

use crate::tools::PcbToolCtx;

use super::draft::Keepout;

// ── route_board ──────────────────────────────────────────────────────────────

/// Inject the draft's keepouts into a [`RouteProblem`] as BLOCKED_ALL obstacles.
///
/// We extend the engine's `to_route_problem` output at the agent layer (rather
/// than forking `to_route_problem`): each keepout becomes an [`Obstacle`] with an
/// EMPTY `connected_to`, which the lint/router treat as unowned copper — blocking
/// EVERY net on the listed layers. A keepout rect maps to a centered rect
/// obstacle per the rect's geometry.
pub(super) fn inject_keepouts(rp: &mut RouteProblem, keepouts: &[Keepout]) {
    for ko in keepouts {
        let center = Point2 {
            x: (ko.rect.min_x + ko.rect.max_x) / 2.0,
            y: (ko.rect.min_y + ko.rect.max_y) / 2.0,
        };
        rp.obstacles.push(Obstacle {
            kind: "rect".to_owned(),
            layers: ko.layers.clone(),
            center,
            width: ko.rect.max_x - ko.rect.min_x,
            height: ko.rect.max_y - ko.rect.min_y,
            // No owner ⇒ blocks all nets (a true keepout).
            connected_to: Vec::new(),
        });
    }
}

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
    parts: &[super::draft::DraftPart],
    failed: &[FailedNet],
) -> Option<(String, String, usize, usize)> {
    let failed_nets: std::collections::BTreeSet<&str> =
        failed.iter().map(|f| f.connection.as_str()).collect();
    let mut per_part: Vec<(String, String, usize)> = Vec::new();
    let mut total = 0usize;
    for p in parts {
        let c = p
            .pad_nets
            .values()
            .filter(|n| failed_nets.contains(n.as_str()))
            .count();
        if c > 0 {
            per_part.push((p.reference.clone(), p.footprint.clone(), c));
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
    let path = ctx.pcb_path();
    ctx.kicad()
        .with_session(&path, |session| session.kicad().save())
        .map_err(|e| format!("could not open/save live KiCAD board: {e}"))?;
    let draft = super::active::draft_from_live(ctx)?;
    if draft.last_placement.is_none() {
        return Err("board has only the initial seed-row footprint positions — run place_board before route_board".to_owned());
    }
    let board = super::active::board_problem(ctx)?;

    let existing = ctx
        .kicad()
        .with_session(&path, |session| {
            let tracks = session.kicad().tracks()?;
            let vias = session.kicad().vias()?;
            Ok((tracks.len(), vias.len()))
        })
        .map_err(|e| format!("could not inspect live KiCAD copper: {e}"))?;
    if existing.0 > 0 || existing.1 > 0 {
        return Err(format!(
            "live board already has {} tracks and {} vias; refusing to replace copper until generated-item tagging is implemented",
            existing.0, existing.1
        ));
    }

    let mut rp = board.problem.clone();
    inject_keepouts(&mut rp, &draft.keepouts);
    let result = route_auto(&rp);
    let split = lint_summary(&rp, &result.solution, &result.failed, &Default::default());
    if split.real > 0 {
        return Err(format!(
            "router produced {} real DRC violation(s); refusing to write copper to live KiCAD board",
            split.real
        ));
    }

    ctx.kicad()
        .with_session(&path, |session| {
            let nets: BTreeMap<String, KiNet> = session
                .kicad()
                .net_list()?
                .into_iter()
                .map(|n| (n.name.clone(), n))
                .collect();
            let items = route_items_for_ipc(&rp, &result.solution, &board.layer_names, &nets)
                .map_err(|e| {
                    kicad_ipc::Error::Spawn(format!("could not build IPC route items: {e}"))
                })?;
            session
                .kicad()
                .commit("route board", |k| k.create_items(items))?;
            session.kicad().save()
        })
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
    let escape = escape_bottleneck(&draft.parts, &result.failed).map(
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
        "congestion": congestion,
        "escape_bottleneck": escape,
        "note": if result.failed.is_empty() {
            "routed live KiCAD board cleanly and saved it"
        } else {
            "routed live KiCAD board with honest failed nets and saved routed copper"
        },
    }))
}

fn route_items_for_ipc(
    problem: &RouteProblem,
    solution: &RouteSolution,
    layer_names: &[String],
    nets: &BTreeMap<String, KiNet>,
) -> std::result::Result<Vec<prost_types::Any>, String> {
    let mut items = Vec::new();
    for trace in &solution.traces {
        let layer = board_layer(&trace.layer, problem.layer_count, layer_names);
        let net = nets
            .get(trace.connection.as_str())
            .cloned()
            .ok_or_else(|| format!("KiCAD board has no net `{}`", trace.connection))?;
        for segment in trace.path.windows(2) {
            items.push(
                prost_types::Any::from_msg(&KiTrack {
                    start: Some(mm_point(&segment[0])),
                    end: Some(mm_point(&segment[1])),
                    width: Some(mm_distance(trace.width)),
                    layer: layer as i32,
                    net: Some(net.clone()),
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?,
            );
        }
    }
    for via in &solution.vias {
        let (start_layer, end_layer, via_type) = match via.span {
            ViaSpan::Through => (BoardLayer::BlFCu, BoardLayer::BlBCu, ViaType::VtThrough),
            ViaSpan::Partial { from, to, micro } => (
                board_layer_by_index(from, layer_names),
                board_layer_by_index(to, layer_names),
                if micro {
                    ViaType::VtMicro
                } else {
                    ViaType::VtBlindBuried
                },
            ),
        };
        let net = nets
            .get(via.connection.as_str())
            .cloned()
            .ok_or_else(|| format!("KiCAD board has no net `{}`", via.connection))?;
        let via_layers = via_span_layers(&via.span, layer_names);
        let copper_layers: Vec<PadStackLayer> = via_layers
            .iter()
            .map(|layer| PadStackLayer {
                layer: *layer as i32,
                shape: PadStackShape::PssCircle as i32,
                size: Some(Vector2 {
                    x_nm: mm_to_nm(via.diameter),
                    y_nm: mm_to_nm(via.diameter),
                }),
                ..Default::default()
            })
            .collect();
        items.push(
            prost_types::Any::from_msg(&KiVia {
                position: Some(mm_point(&via.at)),
                pad_stack: Some(PadStack {
                    r#type: PadStackType::PstNormal as i32,
                    layers: via_layers.iter().map(|layer| *layer as i32).collect(),
                    drill: Some(DrillProperties {
                        start_layer: start_layer as i32,
                        end_layer: end_layer as i32,
                        diameter: Some(Vector2 {
                            x_nm: mm_to_nm(via.drill),
                            y_nm: mm_to_nm(via.drill),
                        }),
                        shape: DrillShape::DsCircle as i32,
                    }),
                    unconnected_layer_removal: UnconnectedLayerRemoval::UlrKeep as i32,
                    copper_layers,
                    ..Default::default()
                }),
                net: Some(net),
                r#type: via_type as i32,
                ..Default::default()
            })
            .map_err(|e| e.to_string())?,
        );
    }
    Ok(items)
}

fn via_span_layers(span: &ViaSpan, layer_names: &[String]) -> Vec<BoardLayer> {
    match *span {
        ViaSpan::Through => (0..layer_names.len() as u32)
            .map(|idx| board_layer_by_index(idx, layer_names))
            .collect(),
        ViaSpan::Partial { from, to, .. } => {
            let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
            (lo..=hi)
                .map(|idx| board_layer_by_index(idx, layer_names))
                .collect()
        }
    }
}

fn mm_to_nm(mm: f64) -> i64 {
    (mm * 1_000_000.0).round() as i64
}

fn mm_point(p: &Point2) -> Vector2 {
    Vector2 {
        x_nm: mm_to_nm(p.x),
        y_nm: mm_to_nm(p.y),
    }
}

fn mm_distance(mm: f64) -> Distance {
    Distance {
        value_nm: mm_to_nm(mm),
    }
}

fn board_layer(layer: &LayerRef, layer_count: u32, layer_names: &[String]) -> BoardLayer {
    layer
        .index(layer_count)
        .map(|idx| board_layer_by_index(idx as u32, layer_names))
        .unwrap_or(BoardLayer::BlFCu)
}

fn board_layer_by_index(index: u32, layer_names: &[String]) -> BoardLayer {
    let last = layer_names.len().saturating_sub(1) as u32;
    if index == 0 {
        BoardLayer::BlFCu
    } else if index == last {
        BoardLayer::BlBCu
    } else {
        match index {
            1 => BoardLayer::BlIn1Cu,
            2 => BoardLayer::BlIn2Cu,
            3 => BoardLayer::BlIn3Cu,
            4 => BoardLayer::BlIn4Cu,
            5 => BoardLayer::BlIn5Cu,
            6 => BoardLayer::BlIn6Cu,
            7 => BoardLayer::BlIn7Cu,
            8 => BoardLayer::BlIn8Cu,
            9 => BoardLayer::BlIn9Cu,
            10 => BoardLayer::BlIn10Cu,
            11 => BoardLayer::BlIn11Cu,
            12 => BoardLayer::BlIn12Cu,
            13 => BoardLayer::BlIn13Cu,
            14 => BoardLayer::BlIn14Cu,
            15 => BoardLayer::BlIn15Cu,
            16 => BoardLayer::BlIn16Cu,
            17 => BoardLayer::BlIn17Cu,
            18 => BoardLayer::BlIn18Cu,
            19 => BoardLayer::BlIn19Cu,
            20 => BoardLayer::BlIn20Cu,
            21 => BoardLayer::BlIn21Cu,
            22 => BoardLayer::BlIn22Cu,
            23 => BoardLayer::BlIn23Cu,
            24 => BoardLayer::BlIn24Cu,
            25 => BoardLayer::BlIn25Cu,
            26 => BoardLayer::BlIn26Cu,
            27 => BoardLayer::BlIn27Cu,
            28 => BoardLayer::BlIn28Cu,
            29 => BoardLayer::BlIn29Cu,
            30 => BoardLayer::BlIn30Cu,
            _ => BoardLayer::BlBCu,
        }
    }
}

#[cfg(test)]
mod escape_bottleneck_tests {
    use super::super::draft::DraftPart;
    use super::*;
    use std::collections::BTreeMap;

    fn part(reference: &str, footprint: &str, nets: &[(&str, &str)]) -> DraftPart {
        DraftPart {
            reference: reference.to_owned(),
            footprint: footprint.to_owned(),
            pad_nets: nets
                .iter()
                .map(|(p, n)| (p.to_string(), n.to_string()))
                .collect::<BTreeMap<_, _>>(),
            locked: None,
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
}
