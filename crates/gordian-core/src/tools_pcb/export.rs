//! Export + DRC: the `export_board` handler, the synthesis wiring
//! (`synth_parts_from_draft`, the tight-outline `content_bounds`, the sibling
//! `.kicad_pro`), and the copper-zone builders (`plane_zones` / `pour_zones` /
//! `prune_islands` / `resolve_pour_layer`) the export consumes.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use kicad_cli::cli::{KicadCli, Violation};
use kicad_sexpr::pcb::{read_problem, write_solution};
use pcb_synth::synth::{
    plane_fill_rects, BoardModel, KeepoutZone, KicadV9Synth, NetClass, SynthPart, Synthesizer,
    ZoneSpec,
};
use pcb_model::{Bounds, Point2, RouteProblem, RouteSolution};
use pcb_place::placement::Placement;

use crate::tools::PcbToolCtx;

use super::draft::{BoardDraft, DraftRules, Keepout, PourSpec};
use super::route::StoredRoute;

// ── copper-zone builders ───────────────────────────────────────────────────────

/// Drop floating copper ISLANDS from a precomputed plane/pour fill: keep only the
/// rectangles reachable (by shared-edge adjacency) from one that covers a same-net
/// ANCHOR (a pad, via, or this net's own trace point). KiCAD treats edge-sharing
/// `filled_polygon`s as one connected pour, so an island with no anchor is copper that
/// connects to nothing — KiCAD would delete it on fill and DRC reports `isolated_copper`.
/// Pruning it up front keeps the pour professional (no floating fills) and cannot affect
/// connectivity: a removed island, by definition, carried no net connection. Empty
/// `anchors` ⇒ no-op (never drop a pour we cannot anchor).
fn prune_islands(rects: Vec<[f64; 4]>, anchors: &[Point2]) -> Vec<[f64; 4]> {
    const EPS: f64 = 1e-6;
    let n = rects.len();
    if n == 0 || anchors.is_empty() {
        return rects;
    }
    let covers = |r: &[f64; 4], a: &Point2| {
        a.x >= r[0] - EPS && a.x <= r[2] + EPS && a.y >= r[1] - EPS && a.y <= r[3] + EPS
    };
    // Two tiling rects connect iff they share a positive-length edge (abut on one axis,
    // overlap on the other).
    let touch = |a: &[f64; 4], b: &[f64; 4]| {
        let xov = a[2].min(b[2]) - a[0].max(b[0]);
        let yov = a[3].min(b[3]) - a[1].max(b[1]);
        (yov.abs() < EPS && xov > EPS) || (xov.abs() < EPS && yov > EPS)
    };
    let mut keep = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for (i, r) in rects.iter().enumerate() {
        if anchors.iter().any(|a| covers(r, a)) {
            keep[i] = true;
            stack.push(i);
        }
    }
    while let Some(i) = stack.pop() {
        for j in 0..n {
            if !keep[j] && touch(&rects[i], &rects[j]) {
                keep[j] = true;
                stack.push(j);
            }
        }
    }
    rects
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, r)| r)
        .collect()
}

/// Build the copper-plane zones for export: one pour per plane net, filling the
/// board minus an anti-pad keep-out around every FOREIGN copper item that reaches
/// that inner layer — every via on another net (which passes through the plane)
/// and every foreign through-hole pad (whose barrel sits on the inner layer).
/// Same-net vias/pads are NOT carved, so the plane's own stitching vias connect.
fn plane_zones(
    planes: &[(String, u32)],
    board: &RouteProblem,
    solution: &RouteSolution,
    bounds: &Bounds,
    rules: &DraftRules,
    user_keepouts: &[Keepout],
    outline: Option<&[Point2]>,
) -> Vec<ZoneSpec> {
    let layer_count = rules.layer_count;
    let via_half = rules.via_diameter / 2.0 + rules.clearance + PLANE_ANTIPAD_MARGIN_MM;
    planes
        .iter()
        .map(|(net, layer_idx)| {
            let mut keepouts: Vec<(Point2, f64, f64)> = Vec::new();
            for v in &solution.vias {
                if &v.connection != net {
                    keepouts.push((v.at.clone(), via_half, via_half));
                }
            }
            for ob in &board.obstacles {
                let on_layer = ob.layers.iter().any(|lr| lr.index(layer_count) == Some(*layer_idx));
                let foreign = !ob.connected_to.iter().any(|n| n == net);
                if on_layer && foreign {
                    let half =
                        ob.width.max(ob.height) / 2.0 + rules.clearance + PLANE_ANTIPAD_MARGIN_MM;
                    keepouts.push((ob.center.clone(), half, half));
                }
            }
            // A user keepout on this plane's layer is a NO-COPPER region — the
            // plane must carve it out (it was previously filled over: planes don't
            // route, so the routing-only keepout never reached the pour).
            for ko in user_keepouts {
                let on_layer = ko.layers.iter().any(|lr| lr.index(layer_count) == Some(*layer_idx));
                if on_layer {
                    let cx = (ko.rect.min_x + ko.rect.max_x) / 2.0;
                    let cy = (ko.rect.min_y + ko.rect.max_y) / 2.0;
                    let hx = (ko.rect.max_x - ko.rect.min_x) / 2.0 + rules.clearance;
                    let hy = (ko.rect.max_y - ko.rect.min_y) / 2.0 + rules.clearance;
                    keepouts.push((Point2 { x: cx, y: cy }, hx, hy));
                }
            }
            // Same-net anchors (this plane's stitching vias + its own pads on the layer):
            // fill islands not reachable from one are floating copper — prune them.
            let mut anchors: Vec<Point2> = Vec::new();
            for v in &solution.vias {
                if &v.connection == net {
                    anchors.push(v.at.clone());
                }
            }
            for ob in &board.obstacles {
                let on_layer = ob.layers.iter().any(|lr| lr.index(layer_count) == Some(*layer_idx));
                if on_layer && ob.connected_to.iter().any(|n| n == net) {
                    anchors.push(ob.center.clone());
                }
            }
            let fill = prune_islands(
                plane_fill_rects(bounds, BOARD_EDGE_MARGIN_MM, &keepouts, outline),
                &anchors,
            );
            ZoneSpec {
                net_name: net.clone(),
                layer_name: format!("In{layer_idx}.Cu"),
                fill_rects: fill,
                clearance: rules.clearance,
                min_thickness: rules.min_trace_width.min(ZONE_MIN_THICKNESS_CAP_MM),
            }
        })
        .collect()
}

/// Safety margin added to a plane anti-pad beyond bare (radius + clearance) — a
/// bare keep-out lands exactly on the clearance limit and KiCAD flags it.
const PLANE_ANTIPAD_MARGIN_MM: f64 = 0.15;

/// Cap on a copper zone's `min_thickness`. KiCAD's zone fill removes slivers thinner than
/// `min_thickness` by a deflate/inflate (≈ min_thickness/2 each), and that inflate can grow
/// the fill back INTO a via's anti-pad, eating its clearance. Tying min_thickness to a large
/// `min_trace_width` (e.g. all-0.4mm traces) made the inflate (0.2) exceed the anti-pad margin
/// (0.15) → KiCAD's re-fill produced via-to-plane clearance faults the in-house lint never sees
/// (it doesn't model plane copper). Capping min_thickness keeps the inflate < the anti-pad
/// margin so the pour stays clearance-correct at any trace width; 0.25mm is KiCAD's own default
/// zone minimum, so normal boards (min_trace ≤ 0.25) are unchanged.
const ZONE_MIN_THICKNESS_CAP_MM: f64 = 0.25;

/// Map a pour layer string to its (copper-layer index, KiCAD layer name) on an
/// `lc`-layer board — see [`pcb_model::LayerRef::resolve`]. (A pour on a GND/VCC
/// PLANE layer is resolvable here but rejected at `create_board` — a plane is
/// already full copper.) This is what lets a GND fill sit on an inner SIGNAL layer
/// (In1/In4 on a 6-layer board) for shielding / impedance reference, not just
/// top/bottom.
pub(super) fn resolve_pour_layer(layer: &str, lc: u32) -> Option<(u32, String)> {
    pcb_model::LayerRef::resolve(layer, lc)
}

/// Build copper-POUR zones on SIGNAL layers (top/bottom) for the requested nets — a
/// GND flood for HF return paths / shielding, or a 2-layer ground plane. Unlike an
/// inner PLANE, a signal layer carries traces, so the anti-pad keep-out is carved
/// around foreign VIAS, foreign PADS, foreign TRACES (per segment), and user keepouts
/// on that layer. Same-net copper is NOT carved, so the pour ties the net's own pads/
/// traces/vias together (and, on a 4-layer board, to the inner plane through the net's
/// existing stitching vias).
fn pour_zones(
    pours: &[PourSpec],
    board: &RouteProblem,
    solution: &RouteSolution,
    bounds: &Bounds,
    rules: &DraftRules,
    user_keepouts: &[Keepout],
    outline: Option<&[Point2]>,
) -> Vec<ZoneSpec> {
    let lc = rules.layer_count;
    let via_half = rules.via_diameter / 2.0 + rules.clearance + PLANE_ANTIPAD_MARGIN_MM;
    pours
        .iter()
        .filter_map(|p| {
            let (idx, kname) = resolve_pour_layer(&p.layer, lc)?;
            let net = &p.net;
            let mut ko: Vec<(Point2, f64, f64)> = Vec::new();
            // Foreign vias (a through via reaches every layer).
            for v in &solution.vias {
                if &v.connection != net {
                    ko.push((v.at.clone(), via_half, via_half));
                }
            }
            // Foreign pads on this layer.
            for ob in &board.obstacles {
                let on = ob.layers.iter().any(|l| l.index(lc) == Some(idx));
                if on && !ob.connected_to.iter().any(|n| n == net) {
                    let h = ob.width.max(ob.height) / 2.0 + rules.clearance + PLANE_ANTIPAD_MARGIN_MM;
                    ko.push((ob.center.clone(), h, h));
                }
            }
            // Foreign traces on this layer — carve each segment's bbox + half-width.
            for t in &solution.traces {
                if t.layer.index(lc) == Some(idx) && &t.connection != net {
                    let inf = t.width / 2.0 + rules.clearance + PLANE_ANTIPAD_MARGIN_MM;
                    for w in t.path.windows(2) {
                        let (a, b) = (&w[0], &w[1]);
                        ko.push((
                            Point2 { x: (a.x + b.x) / 2.0, y: (a.y + b.y) / 2.0 },
                            (a.x - b.x).abs() / 2.0 + inf,
                            (a.y - b.y).abs() / 2.0 + inf,
                        ));
                    }
                }
            }
            // User keepouts on this layer.
            for k in user_keepouts {
                if k.layers.iter().any(|l| l.index(lc) == Some(idx)) {
                    let cx = (k.rect.min_x + k.rect.max_x) / 2.0;
                    let cy = (k.rect.min_y + k.rect.max_y) / 2.0;
                    ko.push((
                        Point2 { x: cx, y: cy },
                        (k.rect.max_x - k.rect.min_x) / 2.0 + rules.clearance,
                        (k.rect.max_y - k.rect.min_y) / 2.0 + rules.clearance,
                    ));
                }
            }
            // Same-net anchors on this layer (pads + vias + this net's own traces):
            // any fill island not reachable from one is floating copper — prune it.
            let mut anchors: Vec<Point2> = Vec::new();
            for v in &solution.vias {
                if &v.connection == net {
                    anchors.push(v.at.clone());
                }
            }
            for ob in &board.obstacles {
                let on = ob.layers.iter().any(|l| l.index(lc) == Some(idx));
                if on && ob.connected_to.iter().any(|n| n == net) {
                    anchors.push(ob.center.clone());
                }
            }
            for t in &solution.traces {
                if t.layer.index(lc) == Some(idx) && &t.connection == net {
                    anchors.extend(t.path.iter().cloned());
                }
            }
            let fill = prune_islands(plane_fill_rects(bounds, BOARD_EDGE_MARGIN_MM, &ko, outline), &anchors);
            Some(ZoneSpec {
                net_name: net.clone(),
                layer_name: kname,
                fill_rects: fill,
                clearance: rules.clearance,
                min_thickness: rules.min_trace_width.min(ZONE_MIN_THICKNESS_CAP_MM),
            })
        })
        .collect()
}

// ── export_board ─────────────────────────────────────────────────────────────

/// DRC findings KiCAD raises that are independent of the routed copper: the
/// inline footprint bodies do not byte-match the installed library copies. This
/// is the same inert library-bookkeeping warning the e2e tests carve out (see
/// `placed_board_e2e.rs`); it never reflects a copper or connectivity fault.
const NON_COPPER_WARNINGS: &[&str] = &[
    // Inert library-bookkeeping diff between the board footprint and its library.
    "lib_footprint_mismatch",
    "lib_footprint_issues",
    // Silkscreen warnings — these are silk-layer aesthetics, never a copper or
    // connectivity fault. Keeping silkscreen on a dense board (so it reads like a
    // real PCB) inevitably produces some silk-over-copper / silk overlap; that is
    // a fab note, not a copper violation, so it must not gate the copper count.
    "silk_over_copper",
    "silk_overlap",
    "silk_edge_clearance",
    "silk_over_silk",
];

pub(super) fn is_non_copper(v: &Violation) -> bool {
    v.severity == "warning" && NON_COPPER_WARNINGS.contains(&v.kind.as_str())
}

/// The KiCAD major version, or 0 if unparseable / no real install.
fn kicad_major(ctx: &PcbToolCtx) -> u32 {
    ctx.env()
        .cli_version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}

/// Build the [`SynthPart`]s for the draft from the resolved footprint sources and
/// the stored placement. Returns a model-readable error string if a footprint's
/// source text can no longer be loaded, or a part has no placement.
fn synth_parts_from_draft(
    draft: &BoardDraft,
    placements: &[Placement],
    ctx: &PcbToolCtx,
) -> std::result::Result<Vec<SynthPart>, String> {
    let index = ctx
        .footprint_index()
        .map_err(|e| format!("footprint index unavailable: {e}"))?;
    let by_ref: BTreeMap<&str, &Placement> =
        placements.iter().map(|p| (p.reference.as_str(), p)).collect();

    let mut parts = Vec::with_capacity(draft.parts.len());
    for dp in &draft.parts {
        let source = index.footprint_source(&dp.footprint).ok_or_else(|| {
            format!(
                "part {}: footprint `{}` source is no longer readable — re-create the board",
                dp.reference, dp.footprint
            )
        })?;
        let placement = by_ref.get(dp.reference.as_str()).ok_or_else(|| {
            format!(
                "part {} has no placement — re-run place_board before export",
                dp.reference
            )
        })?;
        parts.push(SynthPart {
            reference: dp.reference.clone(),
            lib_id: dp.footprint.clone(),
            source,
            pad_nets: dp.pad_nets.clone(),
            placement: (*placement).clone(),
        });
    }
    Ok(parts)
}

/// Edge-of-board margin (mm) left around the copper when the export tightens the
/// board outline to the parts. Comfortably clears the default copper-to-edge
/// clearance and leaves a clean visual border.
const BOARD_EDGE_MARGIN_MM: f64 = 1.0;

/// A board outline that snugly fits all copper (pads, traces, vias) plus
/// `margin` of edge clearance on every side. Returns `budget` unchanged when
/// there is no copper to bound. The outline is `content ± margin` *without*
/// clamping to `budget`: every piece of copper is inside by exactly `margin`, so
/// the board can never clip copper or trip a copper-to-edge clearance rule. The
/// requested `budget` is only a routing area; the finished outline may extend up
/// to `margin` beyond it so a part routed to the budget edge still clears it.
fn content_bounds(
    problem: &RouteProblem,
    solution: &RouteSolution,
    budget: &Bounds,
    keepouts: &[Keepout],
    margin: f64,
) -> Bounds {
    let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
    let mut acc = |x0: f64, y0: f64, x1: f64, y1: f64| {
        min_x = min_x.min(x0);
        max_x = max_x.max(x1);
        min_y = min_y.min(y0);
        max_y = max_y.max(y1);
    };
    for o in &problem.obstacles {
        acc(
            o.center.x - o.width / 2.0,
            o.center.y - o.height / 2.0,
            o.center.x + o.width / 2.0,
            o.center.y + o.height / 2.0,
        );
    }
    for t in &solution.traces {
        let hw = t.width / 2.0;
        for p in &t.path {
            acc(p.x - hw, p.y - hw, p.x + hw, p.y + hw);
        }
    }
    for v in &solution.vias {
        let r = v.diameter / 2.0;
        acc(v.at.x - r, v.at.y - r, v.at.x + r, v.at.y + r);
    }
    // Keepouts are ROUTING obstacles (copper is kept out of them), NOT board-defining
    // features. Including them inflated the outline whenever a keepout sat in otherwise
    // empty space (e.g. planes-keepout: parts in the upper-left, a keepout on the far
    // right → a board twice as wide as the copper, scored "vastly oversized" by the
    // critic). The outline tightens to actual COPPER; a keepout in dead space no longer
    // bloats the board. (No copper ever sits inside a keepout, so this can't clip anything.)
    let _ = keepouts;
    if !min_x.is_finite() {
        return budget.clone();
    }
    Bounds {
        min_x: min_x - margin,
        max_x: max_x + margin,
        min_y: min_y - margin,
        max_y: max_y + margin,
    }
}

/// Derive the board's net classes from the per-net widths the router already carries
/// (`rules.net_widths`), grouping the board's nets by trace width: every net at the
/// board minimum forms the `Default` class; each distinct fat (over-minimum) width
/// forms a `Power` class (suffixed `_<n>mm` when several fat widths coexist, so the
/// names stay distinct and self-describing). Clearance / via geometry are the board's
/// own — only the trace width varies — so a class is purely additive metadata that
/// cannot change a routing/clearance outcome. Returns the classes ordered with
/// `Default` first; a board with no fat nets yields a single `Default` class.
///
/// This invents no data the board lacks: the only per-net intent present is the width,
/// so the classes are exactly the width groups. `all_nets` is the union of the board's
/// pad nets (so even an unrouted net at default width still lands in `Default`).
fn net_classes_from_rules(rules: &DraftRules, all_nets: &std::collections::BTreeSet<String>) -> Vec<NetClass> {
    use std::collections::BTreeMap;
    let width_of = |net: &str| rules.net_widths.get(net).copied().unwrap_or(rules.min_trace_width);
    // net width (scaled to integer µm so f64 keys group exactly) → member nets.
    let key = |w: f64| (w * 1000.0).round() as i64;
    let mut by_width: BTreeMap<i64, (f64, Vec<String>)> = BTreeMap::new();
    for net in all_nets {
        let w = width_of(net);
        by_width.entry(key(w)).or_insert_with(|| (w, Vec::new())).1.push(net.clone());
    }
    let default_key = key(rules.min_trace_width);
    let fat_count = by_width.keys().filter(|k| **k != default_key).count();
    let mk = |name: String, description: String, trace_width: f64, members: Vec<String>| NetClass {
        name,
        description,
        clearance: rules.clearance,
        trace_width,
        via_diameter: rules.via_diameter,
        via_drill: rules.via_drill,
        members,
    };
    let mut classes = Vec::new();
    // Default first (the board minimum width), so KiCAD shows it as the base class.
    if let Some((w, members)) = by_width.get(&default_key) {
        classes.push(mk("Default".into(), "board default — thin signal".into(), *w, members.clone()));
    }
    for (k, (w, members)) in &by_width {
        if *k == default_key {
            continue;
        }
        // Disambiguate names only when several fat widths coexist.
        let name = if fat_count > 1 { format!("Power_{}mm", kicad_sexpr::fmt_num(*w)) } else { "Power".into() };
        classes.push(mk(name, format!("fat power/high-current — {} mm", kicad_sexpr::fmt_num(*w)), *w, members.clone()));
    }
    classes
}

/// The union of every part's bound pad nets — the board's real net names.
fn all_board_nets(parts: &[SynthPart]) -> std::collections::BTreeSet<String> {
    let mut nets = std::collections::BTreeSet::new();
    for p in parts {
        for net in p.pad_nets.values() {
            if !net.is_empty() {
                nets.insert(net.clone());
            }
        }
    }
    nets
}

/// Write a sibling `.kicad_pro` for `board_path` declaring the board's net classes (the
/// same width groups [`net_classes_from_rules`] emits into the `.kicad_pcb`), so KiCAD DRC
/// (and any downstream tool that opens the board) checks copper against the engine's
/// clearance / trace width / via — NOT KiCAD's built-in 0.2 mm netclass default, which
/// false-flags a finer-pitch board whose footprint pads are inherently closer than 0.2 mm.
/// Each non-Default class's nets are assigned via `net_settings.netclass_assignments`, so
/// the board setup dialog shows the fat-power vs thin-signal grouping. KiCAD loads the
/// same-stem project.
fn write_kicad_project(
    board_path: &std::path::Path,
    rules: &DraftRules,
    classes: &[NetClass],
) -> std::io::Result<()> {
    let stem = board_path.file_stem().and_then(|s| s.to_str()).unwrap_or("board");
    let pro = board_path.with_extension("kicad_pro");
    let vmin = (rules.via_diameter - 0.05).max(0.1);
    // IMPORTANT (verified empirically Jun 19): `kicad-cli pcb drc` reads the NET_SETTINGS classes
    // below — so the netclass clearance / track width / via size DO gate DRC against the engine's
    // rules — but it does NOT enforce this `design_settings.rules` MINIMUMS block: setting
    // min_via_diameter to 2.0 here did not flag 0.6 mm vias. These minimums are therefore for the
    // KiCAD GUI only; kicad-cli falls back to its built-in constraint defaults (via ≥ 0.5, drill ≥
    // 0.3, annular ≥ 0.1, hole-to-hole ≥ 0.25, copper-to-edge ≥ 0.5). That is SAFE because
    // create_board's via pre-checks (KICAD_MIN_VIA_*) and the in-house lint (HOLE_CLEAR / EDGE_CLEAR
    // = 0.25 / 0.5) are set to MATCH those same defaults — the DRC gate is meaningful via the
    // netclass + matching pre-checks, NOT this block. Don't add a rule here expecting kicad-cli to
    // honour it (it won't); enforce new minimums in the in-house lint + a create_board pre-check.
    // One project class per synth class (same width groups), plus a guaranteed "Default"
    // (KiCAD requires it) when the derived classes carry none. Non-Default classes get a
    // net→class assignment so board setup shows the grouping.
    let class_obj = |c: &NetClass| {
        json!({
            "name": c.name,
            "clearance": c.clearance, "track_width": c.trace_width,
            "via_diameter": c.via_diameter, "via_drill": c.via_drill,
            "microvia_diameter": 0.3, "microvia_drill": 0.1,
            "diff_pair_gap": 0.25, "diff_pair_width": 0.2,
            "bus_width": 12.0, "line_style": 0, "wire_width": 6.0,
            "pcb_color": "rgba(0, 0, 0, 0.000)", "schematic_color": "rgba(0, 0, 0, 0.000)"
        })
    };
    let mut class_json: Vec<Value> = classes.iter().map(class_obj).collect();
    if !classes.iter().any(|c| c.name == "Default") {
        class_json.insert(0, class_obj(&NetClass {
            name: "Default".into(), description: String::new(),
            clearance: rules.clearance, trace_width: rules.min_trace_width,
            via_diameter: rules.via_diameter, via_drill: rules.via_drill, members: Vec::new(),
        }));
    }
    let mut assignments = serde_json::Map::new();
    for c in classes.iter().filter(|c| c.name != "Default") {
        for net in &c.members {
            assignments.insert(net.clone(), Value::String(c.name.clone()));
        }
    }
    let doc = json!({
        "board": {"design_settings": {"rules": {
            "min_clearance": 0.0, "min_track_width": 0.0,
            "min_via_diameter": vmin, "min_through_hole_diameter": 0.1
        }}},
        "meta": {"filename": format!("{stem}.kicad_pro"), "version": 1},
        "net_settings": {
            "classes": class_json,
            "netclass_assignments": Value::Object(assignments),
            "meta": {"version": 3}
        }
    });
    std::fs::write(pro, serde_json::to_string_pretty(&doc).unwrap_or_default())
}

/// Synthesize the routed board into a `.kicad_pcb`: requires a placed + routed
/// draft, writes the board (footprints from the engine placement + the stored
/// copper) and, when a recent enough KiCAD is available, runs `kicad-cli pcb
/// drc` and reports the counts.
pub fn export_board(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    let Some(draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — run derive_board first",
        }));
    };
    let Some(placements) = draft.last_placement.clone() else {
        return Ok(json!({
            "error": "the board is not placed — run place_board (then route_board) before export",
        }));
    };
    // Never export an ILLEGAL placement — it would ship a board with overlapping
    // courtyards / out-of-bounds parts that fails DRC. The engine's contract is to
    // emit only DRC-clean copper or fail honestly: refuse with an actionable fix.
    if draft.last_place_illegal {
        return Ok(json!({
            "error": "the last placement is NOT legal (courtyard overlap / out of bounds) — \
                      the board is too tight for these parts. Enlarge `bounds` or remove parts, \
                      then re-run place_board and route_board before export.",
        }));
    }
    let Some(raw_route) = ctx.workspace().read_route() else {
        return Ok(json!({
            "error": "the board is not routed — run route_board before export \
                      (export writes the routed copper)",
        }));
    };
    let stored: StoredRoute = match serde_json::from_str(&raw_route) {
        Ok(s) => s,
        Err(e) => {
            return Ok(json!({
                "error": format!("route.json is corrupt or schema-mismatch: {e}"),
            }));
        }
    };

    // Build the synthesis inputs and assemble the board text. The first pass is a
    // minimal model (parts + bounds + stackup) we read back only for its net-code /
    // layer map; zones/outline/net-classes are added in the second pass below.
    let parts = match synth_parts_from_draft(&draft, &placements, ctx) {
        Ok(p) => p,
        Err(msg) => return Ok(json!({ "error": msg })),
    };
    let model = BoardModel::new(parts.clone(), draft.bounds.clone(), draft.rules.layer_count);
    let board_text = match KicadV9Synth.emit(&model) {
        Ok(t) => t,
        Err(e) => {
            return Ok(json!({
                "error": format!("board synthesis failed: {e}"),
            }));
        }
    };

    // Resolve the output path (default = the project's <stem>.kicad_pcb).
    let path = match input.get("path").and_then(Value::as_str) {
        Some(p) => std::path::PathBuf::from(p),
        None => ctx.pcb_path(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating export dir {}", parent.display()))?;
    }
    std::fs::write(&path, board_text.as_bytes())
        .with_context(|| format!("writing synthesized board to {}", path.display()))?;

    // Read the just-written board back for its net-code / layer map, then splice
    // the routed copper onto it. (write_solution validates by re-parse before it
    // promotes the file, so a malformed splice never lands.)
    let board = read_problem(&path)
        .with_context(|| format!("re-reading synthesized board {}", path.display()))?;

    // Tighten the board outline to fit the actual copper. The requested bounds
    // are a routing *budget*; a finished board should be cropped to the parts +
    // copper (+ an edge margin) so it reads as professional instead of a small
    // cluster stranded on an oversized blank. This is a pure edge-cuts change
    // computed AFTER routing — the copper geometry is untouched, so it cannot
    // create a DRC regression (the outline only ever shrinks toward the copper,
    // never clips it). Re-synthesize the outline at the tight bounds.
    let tight = content_bounds(&board.problem, &stored.solution, &draft.bounds, &draft.keepouts, BOARD_EDGE_MARGIN_MM);
    // Copper-plane zones (power pours) for a multilayer board, computed from the
    // foreign copper reaching each inner layer, at the final (tight) bounds. The
    // obstacle positions are absolute, so the first board's read is reusable here.
    // Planes/pours fill the BOARD: for a custom outline that's the outline's bbox (the
    // fill is then clipped to the polygon), not the content-tight bbox — otherwise a pour
    // shrinks to a square around the parts instead of flooding the shaped board.
    let fill_bounds = if draft.outline.is_some() { &draft.bounds } else { &tight };
    let outline = draft.outline.as_deref();
    let mut zones = plane_zones(
        &stored.planes,
        &board.problem,
        &stored.solution,
        fill_bounds,
        &draft.rules,
        &draft.keepouts,
        outline,
    );
    // Signal-layer copper pours (GND flood for HF return / 2-layer ground plane).
    zones.extend(pour_zones(
        &draft.rules.pours,
        &board.problem,
        &stored.solution,
        fill_bounds,
        &draft.rules,
        &draft.keepouts,
        outline,
    ));
    // Export each routing keep-out as a KiCAD rule area so the finished board carries the
    // design intent the placer/router worked around — and KiCAD independently confirms no
    // track/via landed inside it. Resolve each keep-out's layers to KiCAD names; drop a
    // keep-out whose layers don't resolve rather than emit a malformed zone.
    let keepout_zones: Vec<KeepoutZone> = draft
        .keepouts
        .iter()
        .map(|k| {
            let layers: Vec<String> = k
                .layers
                .iter()
                .filter_map(|l| resolve_pour_layer(&l.0, draft.rules.layer_count).map(|(_, n)| n))
                .collect();
            KeepoutZone {
                layers,
                min: [k.rect.min_x, k.rect.min_y],
                max: [k.rect.max_x, k.rect.max_y],
            }
        })
        .filter(|k| !k.layers.is_empty())
        .collect();
    // Net classes from the per-net widths the router already carries: fat power vs thin
    // signal, made legible/editable in KiCAD. Only emit them into the `.kicad_pcb` when
    // there is a real fat/thin distinction (>1 class) — a uniform board needs no blocks
    // (its single Default rule already lives in the `.kicad_pro`).
    let net_classes = net_classes_from_rules(&draft.rules, &all_board_nets(&parts));
    let pcb_classes: Vec<NetClass> = if net_classes.len() > 1 { net_classes.clone() } else { Vec::new() };
    let board = if tight != draft.bounds
        || !zones.is_empty()
        || !keepout_zones.is_empty()
        || draft.outline.is_some()
        || !pcb_classes.is_empty()
    {
        // The full board model: parts on the tight outline, with the copper zones,
        // routing keep-outs, custom outline, and net classes. KicadV9Synth emits it.
        let full = BoardModel {
            parts: parts.clone(),
            bounds: tight.clone(),
            layer_count: draft.rules.layer_count,
            zones,
            keepouts: keepout_zones,
            outline: draft.outline.clone(),
            net_classes: pcb_classes,
        };
        match KicadV9Synth.emit(&full) {
            Ok(t) => {
                std::fs::write(&path, t.as_bytes())
                    .with_context(|| format!("writing tight-outline board to {}", path.display()))?;
                read_problem(&path)
                    .with_context(|| format!("re-reading tight-outline board {}", path.display()))?
            }
            Err(_) => board,
        }
    } else {
        board
    };
    write_solution(&path, &stored.solution, &board)
        .with_context(|| format!("writing routed copper onto {}", path.display()))?;
    // Sibling .kicad_pro declaring the board's design rules, so kicad-cli DRC (and any
    // downstream tool) checks against the SAME clearance/width/via the router used — not
    // KiCAD's 0.2 mm netclass default, which false-flags a finer-pitch board. KiCAD loads
    // the same-stem project when opening the board.
    let _ = write_kicad_project(&path, &draft.rules, &net_classes);

    let mut out = json!({
        "ok": true,
        "path": path.display().to_string(),
        "part_count": draft.parts.len(),
        "failed_nets": stored.failed.len(),
        "traces": stored.solution.traces.len(),
        "vias": stored.solution.vias.len(),
    });

    // DRC, when a real KiCAD ≥ 8 is on PATH (kicad-cli pcb drc lands in 8).
    if kicad_major(ctx) >= 8 {
        match KicadCli::new(ctx.env()).drc(&path) {
            Ok(report) => {
                let copper: Vec<&Violation> = report
                    .violations
                    .iter()
                    .filter(|v| !is_non_copper(v))
                    .collect();
                let tolerated = report.violations.len() - copper.len();
                out["drc"] = json!({
                    "ran": true,
                    "kicad_version": ctx.env().cli_version,
                    "copper_violations": copper.len(),
                    // Error-severity copper faults (shorts/clearance/width) only —
                    // the hard "never ship a DRC fault" metric; must be 0.
                    "copper_errors": report.copper_error_count(),
                    "unconnected_items": report.unconnected_items.len(),
                    "tolerated_footprint_warnings": tolerated,
                    "errors": report.error_count(),
                    "carve_out": "lib_footprint_mismatch warnings are tolerated — they \
                                  are an inert library-bookkeeping diff, not a copper fault",
                });
                out["note"] = json!(if copper.is_empty()
                    && report.unconnected_items.is_empty()
                    && report.error_count() == 0
                {
                    "board exported and DRC-clean (0 copper violations, 0 unconnected)."
                } else {
                    "board exported, but KiCAD DRC found copper/connectivity issues — \
                     inspect drc, re-route or triage, then export again."
                });
            }
            Err(e) => {
                out["drc"] = json!({ "ran": false, "error": e.to_string() });
                out["note"] = json!("board exported; DRC could not run (see drc.error).");
            }
        }
    } else {
        out["drc"] = json!({
            "ran": false,
            "skipped": "no KiCAD >= 8 on PATH (kicad-cli pcb drc needs KiCAD 8+)",
        });
        out["note"] = json!("board exported; DRC skipped (no KiCAD CLI available).");
    }

    Ok(out)
}
