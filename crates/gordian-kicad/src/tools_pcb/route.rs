//! Routing orchestration: the `route_board` handler, the plane/escape routing
//! pipeline (`route_with_planes` / `route_planes_core` / `assign_planes` /
//! `assign_inner_escape`), the keepout injection, and the route-result
//! lint/triage (`lint_summary` / `congestion_json` / `escape_bottleneck`).

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use drc_lint::connectivity::Violation as ConnViolation;
use drc_lint::lint::{DrcViolation, lint};
use negotiated_mesh::pathing::global_route;
use negotiated_mesh::pipeline::{RouterKind, metrics, route_auto};
use pcb_place::placement::to_route_problem;
use pcb_model::{
    FailedNet, LayerRef, Obstacle, Point2, RouteProblem, RouteSolution, Via, ViaSpan,
};

use crate::tools::PcbToolCtx;

use super::draft::{BoardDraft, DraftRules, Keepout};
use super::place::{fit_bounds_to_obstacles, place_problem_from_draft};

/// KiCAD's hole-to-hole (drill edge to drill edge) minimum. Applies between ANY two drilled
/// holes regardless of net — two barrels cannot overlap mechanically — so via placement must
/// honour it even for SAME-NET vias (which may share copper but never a hole).
const KICAD_HOLE_CLEAR_MM: f64 = 0.25;
/// HDI blind/micro via-in-pad size for the fine-pitch plane-stitch fallback (increment 3). Smaller
/// than a through via so it fits IN the pad where a 0.5–0.6 through-via has no room between
/// fine-pitch balls, while keeping KiCAD's standard 0.1 mm annular so `kicad-cli pcb drc` accepts
/// it (proven in the feasibility spike). See docs/specs/hdi-microvia-feasibility.md.
const HDI_VIA_DIAMETER: f64 = 0.4;
const HDI_VIA_DRILL: f64 = 0.2;

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
    let Some(draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — run derive_board first",
        }));
    };
    let Some(placements) = draft.last_placement.clone() else {
        return Ok(json!({
            "error": "the board is not placed — run place_board first, then route_board",
        }));
    };

    let problem = match place_problem_from_draft(&draft, ctx) {
        Ok(p) => p,
        Err(msg) => return Ok(json!({ "error": msg })),
    };

    // Placement → RouteProblem, then inject the keepouts as BLOCKED_ALL obstacles
    // (the one place v1 keepouts bite: routing, not placement).
    let mut rp = to_route_problem(&problem, &placements);
    // The fan-out placer can expand the working frame past the declared `bounds` (it
    // sizes to fit the parts), but `to_route_problem` keeps the original bounds — so a
    // part placed beyond them has its pads CLAMPED onto the grid edge and is unroutable
    // (the bga64 fields: balls at x≈91 on a 46 mm frame all collapse onto column 0).
    // Grow the routing bounds to enclose every placed obstacle with an edge margin, so
    // the grid covers the real placement. Export auto-tightens the edge to copper+1 mm
    // regardless, so this only fixes routability; a board already inside its bounds is
    // unchanged (the grow is a max, never a shrink). Skipped for a custom outline, whose
    // shape is authoritative.
    if rp.outline.is_none() {
        fit_bounds_to_obstacles(&mut rp);
    }
    inject_keepouts(&mut rp, &draft.keepouts);
    // Per-net trace widths from the board rules: fat power copper, thin signals. (Plane
    // nets on a 4-layer board are pours, not traces, so a width on them is simply moot.)
    rp.net_widths = draft.rules.net_widths.clone();
    // Via size from the board rules. `to_route_problem` inherits it from the PlaceProblem,
    // which has no via field, so it defaults to 0.6/0.3 — meaning the router's SIGNAL vias
    // ignored rules.via_diameter entirely (only the stitch vias + the .kicad_pro min-via
    // honoured it). On a board with a non-default via that mismatch shipped DRC faults
    // (e.g. a 1.0mm rule sets min-via 0.95 but the router emitted 0.6 vias → via_diameter
    // violations the in-house lint never sees), and made the via-size lever a no-op for
    // signal escape. Propagate it so EVERY via — signal, stitch, fanout — is one size.
    rp.via_diameter = draft.rules.via_diameter;
    rp.via_drill = draft.rules.via_drill;

    // On a multilayer board the highest-fanout power/ground nets become copper
    // PLANES (emitted as zones at export) instead of point-to-point traces — the
    // only way a dense part's many power pins connect. Signals route on the outer
    // pair; each plane pad is stitched up to its plane with a through-via.
    let planes = assign_planes(&draft);
    let (result, planes) = if planes.is_empty() {
        (route_auto(&rp), planes)
    } else {
        let r0 = route_with_planes(rp.clone(), &planes, &draft.rules);
        // Only the ADJACENT inner plane (In1) is reachable by a micro via-in-pad escape, so which
        // power net sits there decides how many fine-pitch balls route (HDI increment 3). If the
        // default assignment STRANDED plane pads and there are exactly two planes, try the other
        // layer assignment and keep whichever lands more plane copper — the DRC oracle gates both,
        // so this only ever trades honest-unrouted for routed, never correctness. The 2× route cost
        // is paid only when the first pass actually left strands (a fully-routed board skips it).
        let stranded = r0
            .failed
            .iter()
            .any(|f| f.connection.contains("plane stitching"));
        if stranded && planes.len() == 2 {
            let swapped = vec![
                (planes[0].0.clone(), planes[1].1),
                (planes[1].0.clone(), planes[0].1),
            ];
            let r1 = route_with_planes(rp.clone(), &swapped, &draft.rules);
            if r1.solution.vias.len() > r0.solution.vias.len() {
                (r1, swapped)
            } else {
                (r0, planes)
            }
        } else {
            (r0, planes)
        }
    };

    // Persist the full solution + failures + router for export (Task 4) and the
    // routed flag (get_board). The solution lives in the workspace; only a small
    // summary goes back to the model.
    let stored = json!({
        "solution": result.solution,
        "failed": result.failed,
        "router": result.router,
        "planes": planes,
    });
    ctx.workspace().write_route(&serde_json::to_string_pretty(&stored)?)?;

    let failed: Vec<Value> = result
        .failed
        .iter()
        .map(|f| json!({ "connection": f.connection, "reason": f.reason }))
        .collect();

    let m = metrics(&result.solution);
    let plane_net_set: std::collections::BTreeSet<String> =
        planes.iter().map(|(n, _)| n.clone()).collect();
    let split = lint_summary(&rp, &result.solution, &result.failed, &plane_net_set);

    let router = match result.router {
        RouterKind::Naive => "naive",
        RouterKind::Detailed => "detailed",
    };

    let mut out = json!({
        "router": router,
        "failed": failed,
        "metrics": {
            "wirelength": m.wirelength,
            "vias": m.via_count,
            "traces": m.trace_count,
        },
        // lint_summary counts only REAL violations (geometry/short/unexpected
        // gaps). Expected connectivity gaps from already-failed nets are NOT here
        // — those are the honest failures the model triages, listed in `failed`.
        "lint_summary": split.by_kind,
    });

    // A non-zero REAL lint on the routed solution means a violation slipped past
    // the router's own oracles — surface it LOUDLY (it is an engine bug, not a
    // board problem the model can fix). An expected gap from a failed net is NOT
    // an engine bug; it flows to the triage branch below.
    if split.real > 0 {
        let real = split.real;
        out["engine_bug"] = json!(true);
        out["note"] = json!(format!(
            "ENGINE BUG: the routed copper has {real} DRC violation(s) that the \
             router's oracles should have caught (over and above the {} expected \
             gap(s) from failed nets) — this is not a board you can fix by triage; \
             report it. Counts by kind are in lint_summary.",
            split.expected_gaps
        ));
    } else if result.failed.is_empty() {
        out["note"] = json!("routed cleanly: zero failed nets, lint clean. Ready to export.");
    } else {
        // Honest failures: enrich with congestion hotspots so the model can triage
        // (move a part off a hot edge, relax a rule, or drop a keepout).
        out["congestion"] = congestion_json(&rp);
        // First distinguish a fine-pitch ESCAPE bottleneck (failures concentrated on ONE part) from
        // scattered congestion — they call for opposite triage, and steering the model to move OTHER
        // parts on an escape limit just burns iterations on an unfixable failure.
        if let Some((r, fp, c, total)) = escape_bottleneck(&draft.parts, &result.failed) {
            out["escape_bottleneck"] =
                json!({ "ref": r, "footprint": fp, "failed_pins": c, "total_failed_pins": total });
            out["note"] = json!(format!(
                "{c} of {total} unrouted pins belong to ONE part — {r} ({fp}). This is a fine-pitch \
                 PIN-ESCAPE bottleneck, not movable congestion: those inner pins have no room to \
                 escape the field, so moving OTHER parts or relaxing clearance will NOT help. \
                 Options, best first: (1) give {r} more open margin (no parts/keepouts hugging it) \
                 and re-place; (2) route fewer of its signals, or split them across more connectors; \
                 (3) choose a coarser-pitch footprint; (4) ACCEPT these as a density limit — the \
                 board is fidelity-clean (0 DRC faults) and these nets are honestly unrouted, so it \
                 is safe to export as-is. NOTE: adding copper layers does not currently relieve a \
                 fine-pitch signal escape (only power/ground pins benefit, via planes)."
            ));
        } else {
            out["note"] = json!(
                "some nets did not route — read each failure `reason` (global:/assign:/cell:/\
                 finisher: provenance) and the congestion hotspots, then triage by RE-AUTHORING \
                 the Board-DSL and design_board: spread parts via `place.groups`, enlarge \
                 `board.outline`, relax `rules`, or replace a blocking `keepout` with a gapped \
                 pair. Re-place and re-route after each change."
            );
        }
        // Layer escalation: on a 2-layer board with unrouted nets, more copper layers
        // are usually the highest-leverage fix — a 4-layer stack adds GND/VCC PLANES
        // (so every power pin connects through a via instead of competing for surface
        // copper) and keeps F/B clear for signals. The engine supports 2 or 4 layers;
        // the agent chooses via create_board rules.layers (it is not auto-escalated, so
        // routing stays deterministic and the board's layer count is an explicit design
        // choice). Surface the option here so the model knows to reach for it.
        if rp.layer_count <= 2 {
            out["layer_suggestion"] = json!(format!(
                "{} net(s) failed on a 2-layer board. Re-run derive_board with rules.layers=4: \
                 it adds GND+VCC power planes (power pins drop straight to a plane via a \
                 drilled via) and frees F.Cu/B.Cu for signals — usually the biggest win on \
                 dense or multi-power-net boards. Then re-place and re-route.",
                result.failed.len()
            ));
        }
    }

    Ok(out)
}

// ── stored route ──────────────────────────────────────────────────────────────

/// Stored route: the full `route.json` shape — solution + failures + router
/// tag. This mirrors the JSON `route_board` persists so we can recover the
/// rendered state without re-routing.
#[derive(Debug, Deserialize)]
pub(super) struct StoredRoute {
    pub(super) solution: RouteSolution,
    pub(super) failed: Vec<FailedNet>,
    // router field is present in the JSON but we only need it for the key;
    // its value is a RouterKind enum that serde handles fine.
    #[allow(dead_code)]
    router: serde_json::Value,
    /// Power-plane assignment chosen at route time: (net name, copper layer index).
    /// Empty for ordinary (2-layer / no-plane) boards. Export emits these as zones.
    #[serde(default)]
    pub(super) planes: Vec<(String, u32)>,
}

// ── plane assignment + plane routing ───────────────────────────────────────────

/// Minimum pin count for a net to become a copper plane (a power/ground rail) on
/// a multilayer board, rather than being routed as point-to-point traces.
const PLANE_MIN_PINS: usize = 5;

/// Choose copper-plane nets for a multilayer board: the highest-fanout nets
/// (>= [`PLANE_MIN_PINS`] pins) get the inner copper layers (In1, In2, …), so a
/// dense part's many power/ground pins connect through a plane instead of traces
/// the grid router cannot fan out. Returns `(net, layer_index)`; empty unless the
/// board has >= 4 copper layers (2-layer boards keep the all-traces flow).
fn assign_planes(draft: &BoardDraft) -> Vec<(String, u32)> {
    if draft.rules.layer_count < 4 {
        return Vec::new();
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for p in &draft.parts {
        for net in p.pad_nets.values() {
            if !net.is_empty() {
                *counts.entry(net.clone()).or_default() += 1;
            }
        }
    }
    let mut nets: Vec<(String, usize)> =
        counts.into_iter().filter(|(_, c)| *c >= PLANE_MIN_PINS).collect();
    // Highest fanout first; ties by name for determinism.
    nets.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    // Up to 2 planes (GND/VCC), placed on the CENTRED plane layers for this stackup
    // (4-layer → In1,In2; 6-layer → In2,In3) so the other inner layers stay signal.
    // Must match the router's `plane_layers`, or the router would route on a plane.
    let plane_idx = grid_astar::router::plane_layers(draft.rules.layer_count as usize);
    nets.into_iter()
        .take(plane_idx.len())
        .enumerate()
        .map(|(i, (net, _))| (net, plane_idx[i]))
        .collect()
}

/// Distance from point `p` to segment `a`–`b` (mm) — used to keep a plane
/// stitching via clear of routed signal tracks.
fn seg_point_dist(a: &Point2, b: &Point2, p: &Point2) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-12 {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0);
    let (cx, cy) = (a.x + t * dx, a.y + t * dy);
    ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt()
}

/// Would a stitch via at `at` (radius `via_r`) on net `net` clear every FOREIGN pad,
/// via, and routed track? (Same-net copper is fine to touch.)
#[allow(clippy::too_many_arguments)] // internal clearance helper; flat args keep the hot loop readable
fn stitch_via_clears(
    at: &Point2,
    net: &str,
    obstacles: &[Obstacle],
    vias: &[Via],
    traces: &[pcb_model::Trace],
    via_r: f64,
    via_drill: f64,
    clearance: f64,
) -> bool {
    let min_via2 = (2.0 * via_r + clearance).powi(2);
    obstacles.iter().all(|ob| {
        ob.connected_to.iter().any(|n| n == net) || {
            let dx = (at.x - ob.center.x).abs() - ob.width / 2.0;
            let dy = (at.y - ob.center.y).abs() - ob.height / 2.0;
            dx.max(0.0).powi(2) + dy.max(0.0).powi(2) >= (via_r + clearance).powi(2)
        }
    }) && vias.iter().all(|v| {
        let c2 = (at.x - v.at.x).powi(2) + (at.y - v.at.y).powi(2);
        // Drill-to-drill (hole) clearance applies to EVERY via pair — two barrels cannot
        // overlap, same net or not (else a KiCAD hole-to-hole / holes_co_located fault, which
        // the net-independent lint catches and then drops the WHOLE plane net, disconnecting
        // all its power). A FOREIGN via additionally needs full copper clearance (min_via2,
        // already hole-aware via clr_via); a same-net via may share copper but never a hole.
        let hole2 = (via_drill / 2.0 + v.drill / 2.0 + KICAD_HOLE_CLEAR_MM).powi(2);
        let need2 = if v.connection == net { hole2 } else { min_via2.max(hole2) };
        c2 >= need2
    }) && traces.iter().all(|t| {
            t.connection == net || {
                let need = via_r + t.width / 2.0 + clearance;
                !t.path.windows(2).any(|w| seg_point_dist(&w[0], &w[1], at) < need)
            }
        })
}

/// Would a straight fanout trace `a`→`b` (half-width `hw`) on net `net` clear every
/// FOREIGN pad, VIA, and routed track? Sampled densely along the (short) segment.
/// Checking foreign vias here is what the first fanout attempt missed (the bga100
/// clearance faults were the trace grazing a via).
#[allow(clippy::too_many_arguments)] // internal clearance helper; flat args keep the hot loop readable
fn fanout_seg_clears(
    a: &Point2,
    b: &Point2,
    net: &str,
    obstacles: &[Obstacle],
    vias: &[Via],
    traces: &[pcb_model::Trace],
    hw: f64,
    clearance: f64,
) -> bool {
    let len = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
    let n = ((len / 0.1).ceil() as usize).max(8);
    for i in 0..=n {
        let t = i as f64 / n as f64;
        let p = Point2 { x: a.x + t * (b.x - a.x), y: a.y + t * (b.y - a.y) };
        let pad_ok = obstacles.iter().all(|ob| {
            ob.connected_to.iter().any(|nn| nn == net) || {
                let dx = (p.x - ob.center.x).abs() - ob.width / 2.0;
                let dy = (p.y - ob.center.y).abs() - ob.height / 2.0;
                dx.max(0.0).powi(2) + dy.max(0.0).powi(2) >= (hw + clearance).powi(2)
            }
        });
        if !pad_ok {
            return false;
        }
        let via_ok = vias.iter().all(|v| {
            v.connection == net || {
                let need = hw + v.diameter / 2.0 + clearance;
                (p.x - v.at.x).powi(2) + (p.y - v.at.y).powi(2) >= need * need
            }
        });
        if !via_ok {
            return false;
        }
        let trk_ok = traces.iter().all(|tr| {
            tr.connection == net || {
                let need = hw + tr.width / 2.0 + clearance;
                !tr.path.windows(2).any(|w| seg_point_dist(&w[0], &w[1], &p) < need)
            }
        });
        if !trk_ok {
            return false;
        }
    }
    true
}

/// The largest fine-pitch ball pitch (mm) at which a through-via centred ON a 0.5 mm ball
/// pad still clears its orthogonal neighbour ball: `pitch ≥ pad_r + via_r + clearance`.
/// Below this the via-in-pad does not fit and the deeper balls need an HDI microvia (a
/// separate frontier) — so the structured inner-layer escape is gated to `pitch ≥` this.
const VIA_IN_PAD_MIN_PITCH_MM: f64 = 0.75;

/// Assign each ENCLOSED fine-pitch ball an inner SIGNAL layer to escape onto, by
/// (ring, quadrant) depth, so each layer drains a disjoint wedge of the field and the
/// radial escapes never collide (the structured BGA fan-out a free maze can't find).
///
/// Returns `net → inner copper layer`. Only nets whose ball sits in a dense fine-pitch
/// field (≥ [`VIA_IN_PAD_MIN_PITCH_MM`] pitch — coarse enough for a via-in-pad — and ≥ 16
/// balls) and that are NOT plane nets are assigned; the router fires the via-in-pad only
/// on the ones it finds actually enclosed. Empty when the board has no inner signal layer
/// (a 4-layer board's inner pair are both planes) or no qualifying field.
fn assign_inner_escape(
    rp: &RouteProblem,
    planes: &[(String, u32)],
    layer_count: usize,
) -> std::collections::BTreeMap<String, u32> {
    let plane_layers = grid_astar::router::plane_layers(layer_count);
    // Inner SIGNAL layers = inner copper layers that are not planes. Ordered for a
    // deterministic round-robin.
    let inner_sig: Vec<u32> = (1..layer_count as u32 - 1)
        .filter(|l| !plane_layers.contains(l))
        .collect();
    if inner_sig.is_empty() {
        return Default::default();
    }
    let plane_nets: std::collections::BTreeSet<&str> =
        planes.iter().map(|(n, _)| n.as_str()).collect();

    // Candidate balls: small square top-only SMD pads owning a single net.
    let balls: Vec<&Obstacle> = rp
        .obstacles
        .iter()
        .filter(|ob| {
            ob.layers == vec![LayerRef::top()]
                && ob.connected_to.len() == 1
                && ob.width <= 0.6
                && ob.height <= 0.6
                && (ob.width - ob.height).abs() < 0.15
        })
        .collect();
    if balls.len() < 16 {
        return Default::default();
    }

    // Ball pitch = the smallest centre-to-centre distance among balls.
    let mut pitch = f64::INFINITY;
    for i in 0..balls.len() {
        for j in (i + 1)..balls.len() {
            let d = ((balls[i].center.x - balls[j].center.x).powi(2)
                + (balls[i].center.y - balls[j].center.y).powi(2))
            .sqrt();
            if d > 0.05 {
                pitch = pitch.min(d);
            }
        }
    }
    if !pitch.is_finite() || pitch < VIA_IN_PAD_MIN_PITCH_MM {
        return Default::default();
    }

    // Colour the ball lattice so NO TWO orthogonally- or diagonally-adjacent balls share
    // an inner layer: at 0.8 mm pitch two escaped via barrels one pitch apart leave only a
    // 0.2 mm copper gap — unroutable — so adjacent balls MUST escape on different layers.
    // Snap each ball to integer lattice (row, col) from the field origin and assign
    // `inner_sig[(col + 2·row) mod n]`; with n ≥ 4 inner layers this gives every cell a
    // colour distinct from all 8 neighbours (a generalized brick/knight colouring), so on
    // any one layer the same-layer balls sit ≥ √5·pitch apart and the radial escapes have
    // room. Deterministic; the origin is the min-corner ball.
    let min_x = balls.iter().map(|b| b.center.x).fold(f64::INFINITY, f64::min);
    let min_y = balls.iter().map(|b| b.center.y).fold(f64::INFINITY, f64::min);
    let n = inner_sig.len() as i64;
    let mut out: std::collections::BTreeMap<String, u32> = Default::default();
    for b in &balls {
        let net = b.connected_to[0].as_str();
        if plane_nets.contains(net) || net == "GND" || net == "VCC" {
            continue;
        }
        let col = ((b.center.x - min_x) / pitch).round() as i64;
        let row = ((b.center.y - min_y) / pitch).round() as i64;
        let idx = (col + 2 * row).rem_euclid(n) as usize;
        out.insert(net.to_owned(), inner_sig[idx]);
    }
    out
}

fn route_with_planes(
    rp: RouteProblem,
    planes: &[(String, u32)],
    rules: &DraftRules,
) -> negotiated_mesh::pipeline::RouteResult {
    // Inner-layer escape assignment for dense BGA fields, computed on the FULL stack —
    // each enclosed ball gets an inner signal layer to drop to (via-in-pad) so it escapes
    // where F/B are walled in. Empty on a 4-layer board (no inner signal layer) or a board
    // with no dense fine-pitch field. When non-empty, route BOTH with the escape (full
    // stack, per-net layer restriction) and without (the old F/B-only flow) and keep the
    // one that connects more pads — so the escape is a strict capability ADD: it can only
    // help, never regress a board where the field can't actually use it.
    let escape = assign_inner_escape(&rp, planes, rules.layer_count as usize);
    if escape.is_empty() {
        let mut base = rp;
        base.layer_count = 2;
        return route_planes_core(base, planes, rules);
    }
    let mut esc_rp = rp.clone();
    esc_rp.escape_layers = escape;
    let with_escape = route_planes_core(esc_rp, planes, rules);
    let mut base = rp;
    base.layer_count = 2;
    let without = route_planes_core(base, planes, rules);
    // Tie → prefer WITHOUT escape (fewer vias, the established flow). Use the escape only
    // when it strictly connects more REAL nets (a stranded-stitch pseudo-failure is not a
    // net, so it doesn't sway the choice).
    if real_failed_count(&with_escape) < real_failed_count(&without) {
        with_escape
    } else {
        without
    }
}

/// Count of REAL failed nets in a finished plane route — the `<N plane stitching vias>`
/// pseudo-failure is not a net, so it is excluded. Used to keep the better of {escape,
/// no-escape}; the relative ordering of two routes of the SAME board is what matters.
fn real_failed_count(r: &negotiated_mesh::pipeline::RouteResult) -> usize {
    r.failed
        .iter()
        .filter(|f| !f.connection.contains("plane stitching"))
        .count()
}

fn route_planes_core(
    mut rp: RouteProblem,
    planes: &[(String, u32)],
    rules: &DraftRules,
) -> negotiated_mesh::pipeline::RouteResult {
    let plane_names: std::collections::BTreeSet<String> =
        planes.iter().map(|(n, _)| n.clone()).collect();
    // net → its copper-plane layer index (1 = In1 …) for the HDI via-in-pad fallback below.
    let plane_layer: std::collections::BTreeMap<&str, u32> =
        planes.iter().map(|(n, l)| (n.as_str(), *l)).collect();
    // Stitch points: one through-via per SMD plane pad, dropping it to its inner
    // plane. Collected BEFORE retagging layers so we can still tell a THROUGH-HOLE
    // plane pad — which already spans the inner planes and needs NO via (adding one
    // drills a hole co-located with the pad's own barrel: a KiCAD holes_co_located
    // defect) — from an SMD pad on one face, which does need the via. A pad belongs
    // to exactly one plane net.
    let stitches: Vec<(String, Point2, bool)> = rp
        .obstacles
        .iter()
        .filter_map(|ob| {
            ob.connected_to
                .iter()
                .find(|n| plane_names.contains(*n))
                .map(|n| {
                    let thru = ob.layers.contains(&LayerRef::top())
                        && ob.layers.contains(&LayerRef::bottom());
                    (n.clone(), ob.center.clone(), thru)
                })
        })
        .collect();
    // Retag plane-net pads onto both signal faces so signals route around them.
    for ob in &mut rp.obstacles {
        if ob.connected_to.iter().any(|n| plane_names.contains(n)) {
            ob.layers = vec![LayerRef::top(), LayerRef::bottom()];
        }
    }
    rp.connections.retain(|c| !plane_names.contains(&c.name));

    let mut result = route_auto(&rp);
    // Add a through-via per plane pad, but ONLY where it clears its foreign
    // neighbours — a fine-pitch part (0.5mm BGA) has no room for a standard
    // through-via between balls, and an overhanging via would short adjacent nets.
    // A via that doesn't fit is SKIPPED (that pad's power stays honestly unrouted,
    // surfaced as a KiCAD unconnected), never shipped as a DRC fault: the plane
    // path must be as connectivity-honest as the router. (Fine-pitch parts need
    // via-in-pad / microvias, a documented v1 limitation.)
    let via_r = rules.via_diameter / 2.0;
    let clr = rules.clearance;
    // A FANOUT via lands in fresh board, so unlike the in-place stitch it can sit near
    // another drilled hole. KiCAD's hole-to-hole rule (0.25mm) is on DRILL edges, but
    // our clearance is on COPPER edges; the via's drill is `via_annular` inside its
    // copper, so a copper gap of `clr` only buys a hole gap of `clr + via_annular` when
    // the neighbour has ~0 annular. Bump the fanout via's clearance so the hole gap
    // clears 0.25 with margin — this is what makes the fanout DRC-clean WITHOUT a
    // drill-aware obstacle model (the deficit was always just this arithmetic).
    let via_annular = (rules.via_diameter - rules.via_drill) / 2.0;
    let clr_via = clr.max(KICAD_HOLE_CLEAR_MM - via_annular + 0.05);
    const DIRS: [(f64, f64); 8] = [
        (1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0),
        (0.707, 0.707), (-0.707, 0.707), (0.707, -0.707), (-0.707, -0.707),
    ];
    let step = rules.via_diameter + clr;
    let mut skipped = 0usize;
    for (net, at, thru) in stitches {
        // A through-hole plane pad already connects to its inner plane (its barrel
        // spans every copper layer); a stitching via here would only co-locate a
        // second drill with the pad's own hole.
        if thru {
            continue;
        }
        // 1) In place: drop the stitch via on the pad if it clears. Uses the
        //    hole-clearance-aware clr_via (not bare clr) — the via's drill must clear a
        //    neighbour's drill by KiCAD's 0.25mm, same as a fanout via; at a fine
        //    clearance (e.g. 0.1mm) bare clr left only a clr+via_annular hole gap < 0.25
        //    (a latent fault the 0.13mm boards passed only by luck). For the default
        //    0.2mm clearance clr_via == clr, so those boards are unchanged.
        if stitch_via_clears(
            &at, &net, &rp.obstacles, &result.solution.vias, &result.solution.traces, via_r,
            rules.via_drill, clr_via,
        ) {
            result.solution.vias.push(Via {
                connection: net,
                at,
                diameter: rules.via_diameter,
                drill: rules.via_drill,
                span: ViaSpan::Through,
            });
            continue;
        }
        // 2) FANOUT: a fine-pitch pad has no room for a via between neighbours, but
        //    open board nearby does. Route a short outward trace to the first open
        //    point where the via AND the trace clear, then stitch there — the dog-bone
        //    escape that routes a perimeter power pin an in-place via can't. Stays
        //    inside the board/outline; conservative so it never ships a DRC fault.
        let tw = rp.net_width(&net);
        let mut placed = false;
        'search: for k in 1..=4 {
            let r = k as f64 * step;
            for (dx, dy) in DIRS {
                let cand = Point2 { x: at.x + dx * r, y: at.y + dy * r };
                let in_board = cand.x - via_r >= rp.bounds.min_x + clr
                    && cand.x + via_r <= rp.bounds.max_x - clr
                    && cand.y - via_r >= rp.bounds.min_y + clr
                    && cand.y + via_r <= rp.bounds.max_y - clr
                    && rp
                        .outline
                        .as_ref()
                        .is_none_or(|poly| pcb_model::point_in_polygon(&cand, poly));
                if in_board
                    && stitch_via_clears(
                        &cand, &net, &rp.obstacles, &result.solution.vias,
                        &result.solution.traces, via_r, rules.via_drill, clr_via,
                    )
                    && fanout_seg_clears(
                        &at, &cand, &net, &rp.obstacles, &result.solution.vias,
                        &result.solution.traces, tw / 2.0, clr,
                    )
                {
                    result.solution.traces.push(pcb_model::Trace {
                        connection: net.clone(),
                        layer: LayerRef::top(),
                        width: tw,
                        path: vec![at.clone(), cand.clone()],
                    });
                    result.solution.vias.push(Via {
                        connection: net.clone(),
                        at: cand,
                        diameter: rules.via_diameter,
                        drill: rules.via_drill,
                        span: ViaSpan::Through,
                    });
                    placed = true;
                    break 'search;
                }
            }
        }
        // 3) HDI VIA-IN-PAD fallback: a smaller blind/micro via reaches this pad's own plane
        //    where a full through-via has no room between fine-pitch balls. `micro` when the
        //    plane is the layer directly below the top (adjacent, F→In1), `blind` for a deeper
        //    inner plane (F→In2…). It sits IN the pad (same net), so KiCAD's zone fill connects
        //    it in its own plane and carves an anti-pad in any foreign plane it pierces — no
        //    short. The DRC oracle (drop_violating_copper + kicad-cli) still gates it: a via that
        //    can't clear is dropped and reported unrouted, never shipped failing.
        if !placed {
            // Only a true laser MICROVIA helps the room problem: KiCAD relaxes the size rule for
            // micro vias (down to the netclass microvia min) but holds blind/buried vias to the
            // FULL through-via minimum — so a blind via is never smaller than the through-via that
            // already failed to fit here, and would only re-trip the size/room limit. We therefore
            // emit a micro via only when the pad's plane is the layer DIRECTLY below the top
            // (adjacent F→In1): the via-in-pad drops straight onto its own plane (same net → KiCAD's
            // zone fill connects it; nothing foreign is pierced → no anti-pad, no short). A deeper
            // inner plane (In2…) would need STACKED micro vias; those are kicad-cli-DRC-valid
            // (verified) but their co-located holes trip the in-house lint's hole-clearance check,
            // which then drops the whole net — so the stack needs a span-aware hole exemption first
            // (see docs/specs/hdi-microvia-feasibility.md). The DRC oracle still gates this; a via
            // that can't clear is reported unrouted, never shipped failing.
            if plane_layer.get(net.as_str()) == Some(&1)
                && stitch_via_clears(
                    &at, &net, &rp.obstacles, &result.solution.vias,
                    &result.solution.traces, HDI_VIA_DIAMETER / 2.0, HDI_VIA_DRILL, clr_via,
                )
            {
                result.solution.vias.push(Via {
                    connection: net.clone(),
                    at: at.clone(),
                    diameter: HDI_VIA_DIAMETER,
                    drill: HDI_VIA_DRILL,
                    span: ViaSpan::Partial { from: 0, to: 1, micro: true },
                });
                placed = true;
            }
        }
        if !placed {
            skipped += 1;
        }
    }
    if skipped > 0 {
        result.failed.push(FailedNet {
            connection: format!("<{skipped} plane stitching vias>"),
            reason: "no room for a through-via between fine-pitch pads — that power \
                     pin stays on the plane unrouted (needs via-in-pad / microvias)"
                .to_owned(),
        });
    }
    // Final fidelity pass — close the structural gap that the stitch/fanout vias above
    // are added in the AGENT layer, OUTSIDE the engine's route_auto reconcile, so they
    // never saw the DRC oracle. Route the COMPLETE copper (engine route + plane stitches)
    // back through the SAME geometry lint and drop any net whose copper still violates
    // clearance/via/width/bounds. The engine must never EMIT a DRC-failing via — even a
    // hand-rolled stitch check that slips at an unusual via/clearance is caught here, and
    // the net is reported honestly unrouted instead of shipping a fault. SAFE: a clean
    // board lints to zero so nothing is dropped (verified byte-for-byte on the 57-board
    // suite); only a would-fault config trades copper for an honest failure.
    let dropped = drc_lint::lint::drop_violating_copper(&rp, &mut result.solution);
    for net in dropped {
        if !result.failed.iter().any(|f| f.connection == net) {
            result.failed.push(FailedNet {
                connection: net,
                reason: "DRC oracle: dropped after stitching — copper could not clear \
                         at this via/clearance (reported unrouted, never shipped failing)"
                    .to_owned(),
            });
        }
    }
    result
}

#[cfg(test)]
mod escape_bottleneck_tests {
    use super::*;
    use super::super::draft::DraftPart;
    use std::collections::BTreeMap;

    fn part(reference: &str, footprint: &str, nets: &[(&str, &str)]) -> DraftPart {
        DraftPart {
            reference: reference.to_owned(),
            footprint: footprint.to_owned(),
            pad_nets: nets.iter().map(|(p, n)| (p.to_string(), n.to_string())).collect::<BTreeMap<_, _>>(),
            locked: None,
        }
    }
    fn failed(nets: &[&str]) -> Vec<FailedNet> {
        nets.iter().map(|n| FailedNet { connection: n.to_string(), reason: String::new() }).collect()
    }

    #[test]
    fn concentrated_failures_on_one_part_are_an_escape_bottleneck() {
        // A BGA whose 4 inner pins fail + a cap with 1 unrelated fail → 4/5 on U1 (≥60%) → flagged.
        let parts = vec![
            part("U1", "Package_BGA:BGA-100", &[("A1", "S1"), ("A2", "S2"), ("A3", "S3"), ("A4", "S4")]),
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
        assert!(escape_bottleneck(&parts, &failed(&["<7 plane stitching vias>"]) ).is_none());
    }
}
