//! Placement: the read-only `get_board` summary, the draft→`PlaceProblem`
//! bridge, the auto edge-affinity plumbing, and the `place_board` handler.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use pcb_place::placement::{Part, PlaceProblem, Placement, Rect};
use pcb_model::{LayerRef, RouteProblem};
use pcb_synth::placefp::part_from_footprint_layers;

use crate::tools::PcbToolCtx;

use super::create::net_pin_counts;
use super::draft::BoardDraft;

// ── get_board ────────────────────────────────────────────────────────────────

pub fn get_board(ctx: &PcbToolCtx) -> Result<Value> {
    let Some(draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — run derive_board first",
        }));
    };

    let net_pins = net_pin_counts(&draft.parts, ctx);
    let nets: Vec<Value> = net_pins
        .iter()
        .map(|(name, &pins)| json!({ "name": name, "pins": pins }))
        .collect();

    let placed = draft.last_placement.is_some();
    // Routed state lives in a separate workspace file (route.json, Task 2);
    // for now report routed=false unless that file exists.
    let routed = ctx.workspace().read_route().is_some();

    // Lean draft view: each part's pad→net map is exactly what the model itself passed to
    // create_board, so echoing it back on every get_board call only re-bloats the context
    // (43–64% of a dense BGA board's draft, re-sent each turn). Replace it with a pad_count;
    // the per-net pin SUMMARY below carries the connectivity view the model actually inspects.
    let mut draft_json = serde_json::to_value(&draft)?;
    if let Some(parts) = draft_json.get_mut("parts").and_then(Value::as_array_mut) {
        for part in parts {
            if let Some(obj) = part.as_object_mut() {
                let n = obj
                    .get("pad_nets")
                    .and_then(Value::as_object)
                    .map_or(0, serde_json::Map::len);
                obj.remove("pad_nets");
                obj.insert("pad_count".into(), json!(n));
            }
        }
    }

    Ok(json!({
        "draft": draft_json,
        "summary": {
            "part_count": draft.parts.len(),
            "net_count": net_pins.len(),
            "nets": nets,
            "keepout_count": draft.keepouts.len(),
            "placed": placed,
            "routed": routed,
        },
    }))
}

// ── draft → engine problem ───────────────────────────────────────────────────

/// Build the [`PlaceProblem`] the placement/routing engine consumes from a
/// board draft. Every part's footprint is resolved through
/// [`part_from_footprint_layers`] (the SAME geometry `create_board` validated and the
/// net-pin counts derive from), then the draft's `locked` position is applied so
/// the placer pins it. Design rules ride along as the problem's clearance /
/// trace width / layer count.
///
/// Keepouts are deliberately NOT carried here: in v1 they affect ROUTING only
/// (they become BLOCKED obstacles in [`route_board`](super::route::route_board)), never placement no-go
/// regions. An unresolvable footprint (e.g. the index lost a lib between
/// `create_board` and now) is returned as an `Err(lib_id)` so the caller can
/// surface a recoverable error naming the part.
pub(super) fn place_problem_from_draft(
    draft: &BoardDraft,
    ctx: &PcbToolCtx,
) -> std::result::Result<PlaceProblem, String> {
    let index = ctx
        .footprint_index()
        .map_err(|e| format!("footprint index unavailable: {e}"))?;
    let mut parts: Vec<Part> = Vec::with_capacity(draft.parts.len());
    for dp in &draft.parts {
        let Some(fp) = index.footprint(&dp.footprint) else {
            return Err(format!(
                "part {}: footprint `{}` is no longer resolvable — re-create the board",
                dp.reference, dp.footprint
            ));
        };
        let mut part =
            part_from_footprint_layers(&fp, &dp.reference, &dp.pad_nets, draft.rules.layer_count);
        // A DSL part `lock` pins the part for the placer (carried through here).
        part.locked = dp.locked.clone();
        parts.push(part);
    }
    // Keep-outs that block a SIGNAL layer (top/bottom) are placement obstacles too:
    // a part dropped inside one has its pads trapped (no track can leave). Inner-only
    // (plane) keep-outs don't constrain placement, so they're excluded here.
    let keepouts: Vec<Rect> = draft
        .keepouts
        .iter()
        .filter(|k| {
            k.layers
                .iter()
                .any(|l| *l == LayerRef::top() || *l == LayerRef::bottom())
        })
        .map(|k| k.rect.clone())
        .collect();
    Ok(PlaceProblem {
        bounds: routing_bounds(draft),
        clearance: draft.rules.clearance,
        layer_count: draft.rules.layer_count,
        min_trace_width: draft.rules.min_trace_width,
        parts,
        keepouts,
        outline: draft.outline.clone(),
    })
}

/// KiCAD's copper-to-board-edge clearance (its default). Copper closer than this to the
/// Edge.Cuts is a `copper_edge_clearance` fault.
const EDGE_CLEAR_MM: f64 = 0.5;

/// The bounds the placer + router actually work inside. For a board with NO custom outline the
/// export auto-tightens the edge to copper + 1 mm, so any copper is already ≥ 1 mm from the
/// finished edge — no inset needed. But a CUSTOM outline is exported verbatim, so copper routed
/// to the raw `bounds` lands right on that edge and trips KiCAD's 0.5 mm copper-to-edge rule
/// (a dense custom-outline board shipped 42 such faults). Inset the working bounds by the edge
/// clearance so place + route keep copper off the edge; the exported Edge.Cuts stays the user's
/// real outline. (Inset the bbox; the lint also checks distance to the outline POLYGON edges,
/// catching the non-bbox edges of a non-rectangular outline.)
fn routing_bounds(draft: &BoardDraft) -> Rect {
    if draft.outline.is_none() {
        return draft.bounds.clone();
    }
    let b = &draft.bounds;
    // Never invert a small board: clamp the inset so min stays < max.
    let inset = EDGE_CLEAR_MM.min((b.max_x - b.min_x) / 2.0 - 0.1).min((b.max_y - b.min_y) / 2.0 - 0.1);
    Rect {
        min_x: b.min_x + inset,
        max_x: b.max_x - inset,
        min_y: b.min_y + inset,
        max_y: b.max_y - inset,
    }
}

/// Grow `rp.bounds` ONLY when a placed obstacle falls OUTSIDE the declared frame, so the
/// routing grid covers a placement the fan-out expanded past `bounds` (else its pads are
/// clamped onto the grid edge and unroutable — the bga64 fields: balls at x≈91 on a 46 mm
/// frame). A board whose obstacles already fit is left BYTE-IDENTICAL (the grow runs only
/// per-axis when that axis overflows), so no in-bounds board's grid shifts. The overflow
/// side grows to the obstacle edge + a board-edge margin (copper off the finished edge).
pub(super) fn fit_bounds_to_obstacles(rp: &mut RouteProblem) {
    let (mut mnx, mut mxx, mut mny, mut mxy) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for ob in &rp.obstacles {
        mnx = mnx.min(ob.center.x - ob.width / 2.0);
        mxx = mxx.max(ob.center.x + ob.width / 2.0);
        mny = mny.min(ob.center.y - ob.height / 2.0);
        mxy = mxy.max(ob.center.y + ob.height / 2.0);
    }
    if !mnx.is_finite() {
        return; // no obstacles
    }
    let b = &mut rp.bounds;
    if mnx < b.min_x {
        b.min_x = mnx - EDGE_CLEAR_MM;
    }
    if mxx > b.max_x {
        b.max_x = mxx + EDGE_CLEAR_MM;
    }
    if mny < b.min_y {
        b.min_y = mny - EDGE_CLEAR_MM;
    }
    if mxy > b.max_y {
        b.max_y = mxy + EDGE_CLEAR_MM;
    }
}

/// JSON shape for one placed part, returned by `place_board` (and reused as the
/// model's view of `last_placement`).
fn placement_json(p: &Placement) -> Value {
    json!({
        "reference": p.reference,
        "x": p.at.x,
        "y": p.at.y,
        "rotation": p.rotation,
    })
}

// ── place_board ──────────────────────────────────────────────────────────────

/// Whether a part is a board-edge part (connector / header / terminal block /
/// mounting hole) that should hug the perimeter. Detected from the footprint
/// library id, with the conventional `J` reference prefix as a fallback.
fn is_connector(footprint: &str, reference: &str) -> bool {
    let fp = footprint.to_ascii_lowercase();
    fp.contains("connector")
        || fp.contains("pinheader")
        || fp.contains("pinsocket")
        || fp.contains("terminalblock")
        || fp.contains("screwterminal")
        || fp.contains("mountinghole")
        || fp.contains("usb")
        || fp.contains("barreljack")
        || reference.starts_with('J')
}

/// A mounting hole / mechanical fixing: pulled to a board CORNER (not just an
/// edge), where a screw clears the components. Checked before [`is_connector`]
/// (which also matches mounting holes) so these corner-seek rather than edge-seek.
fn is_mounting_hole(footprint: &str) -> bool {
    footprint.to_ascii_lowercase().contains("mountinghole")
}

pub fn place_board(_input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    let Some(mut draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — run derive_board first",
        }));
    };

    let problem = match place_problem_from_draft(&draft, ctx) {
        Ok(p) => p,
        Err(msg) => return Ok(json!({ "error": msg })),
    };

    // Auto edge-affinity: pull connectors/headers to their nearest board edge so
    // they land at the perimeter (where a cable or the enclosure reaches them),
    // not stranded in the interior with copper wrapping around them. Skip any
    // part the model already steered with an explicit group `edge` hint.
    let mut hints = draft.hints.clone();
    let explicitly_edged: std::collections::BTreeSet<&str> = hints
        .groups
        .iter()
        .filter(|g| g.edge.is_some())
        .flat_map(|g| g.members.iter().map(String::as_str))
        .collect();
    for p in &draft.parts {
        if explicitly_edged.contains(p.reference.as_str()) {
            continue;
        }
        // Mounting holes corner-seek (mechanical fixings at the board corners) AND
        // edge-seek (so any hole the corner post-pass can't seat — a corner taken
        // or blocked by a part — falls back to the perimeter, not the interior).
        // Other connectors/headers edge-seek (a cable/enclosure reaches the edge).
        if is_mounting_hole(&p.footprint) {
            if !hints.corner_seek.contains(&p.reference) {
                hints.corner_seek.push(p.reference.clone());
            }
            if !hints.edge_seek.contains(&p.reference) {
                hints.edge_seek.push(p.reference.clone());
            }
        } else if is_connector(&p.footprint, &p.reference)
            && !hints.edge_seek.contains(&p.reference)
        {
            hints.edge_seek.push(p.reference.clone());
        }
    }

    // UNIFIED FAN-OUT placement (the routing-neatness + compactness lever): for a
    // board with a dominant fine-pitch IC, lay the WHOLE board as a radial fan-out —
    // IC centred, caps then series resistors (in IC-pad order) then passives on
    // density-aware concentric rings, connectors on the outer frame — overlap-free
    // by construction, with bounds sized to fit. Escapes route radially (short,
    // parallel) and the board is compact. Falls back to cap-ring auto-surround when
    // there's no clear dominant IC.
    // Run the placement PIPELINE — structured fan-out fast-path → optimize fallback.
    // The stages, the $NO_UNIFIED override, and the legality fallback all live in (and
    // are documented on) the single visible entry pcb_place::place_board. The agent
    // overrides any of this with the interactive geometry tools (move_part/route_track).
    let result = pcb_place::placement::place_board(&problem, &hints);

    // Persist the placement into the draft so route_board / render_board / a
    // later get_board can read it without re-running the placer.
    draft.last_placement = Some(result.placements.clone());
    draft.last_place_illegal = !result.legal;
    draft.save(ctx)?;

    let positions: Vec<Value> = result.placements.iter().map(placement_json).collect();

    // Discoverability: if a legal placement has a decoupling-heavy IC whose caps the
    // annealer scattered (>=4 bypass caps, none locked/pinned), suggest the `surround`
    // hint so the agent can ring them into a tidy decoupling cluster.
    let mut hint_suggestions: Vec<Value> = Vec::new();
    if result.legal {
        let pairs = pcb_place::placement::decoupling_pairs(&problem);
        let mut by_ic: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (cap, ic) in pairs {
            by_ic.entry(ic).or_default().push(cap);
        }
        for (ic, caps) in by_ic {
            if caps.len() >= 4 && caps.iter().all(|&c| problem.parts[c].locked.is_none()) {
                let ic_ref = problem.parts[ic].reference.clone();
                let cap_refs: Vec<String> =
                    caps.iter().map(|&c| problem.parts[c].reference.clone()).collect();
                hint_suggestions.push(json!({
                    "type": "surround",
                    "target": ic_ref,
                    "members": cap_refs,
                    "note": format!(
                        "{ic_ref} has {} decoupling caps the placer scattered. For a tidy ring, \
                         add a `place.groups` entry to the DSL — {{<name>: {{members: [<the caps>], \
                         surround: {ic_ref}}}}} — and design_board again, then place_board.",
                        caps.len()
                    ),
                }));
            }
        }
    }

    // On a failed placement, give the agent a CONCRETE minimum board size so it can
    // retry deterministically instead of guessing. Estimate from the parts' total
    // courtyard area (with packing + routing overhead) and the largest single part.
    let mut extra = json!({});
    if !result.legal {
        let total_area: f64 = problem
            .parts
            .iter()
            .map(|p| p.courtyard_w * p.courtyard_h)
            .sum();
        let max_w = problem.parts.iter().map(|p| p.courtyard_w).fold(0.0, f64::max);
        let max_h = problem.parts.iter().map(|p| p.courtyard_h).fold(0.0, f64::max);
        let cw = (problem.bounds.max_x - problem.bounds.min_x).max(0.1);
        let ch = (problem.bounds.max_y - problem.bounds.min_y).max(0.1);
        // ~2x the courtyard area leaves room for spacing, refdes gaps, and routing;
        // but ALSO grow 1.3x past the current bounds, so if the caller already gave
        // generous-but-still-failing bounds (large parts the legalizer can't
        // separate) each retry with the suggestion converges instead of looping.
        // Keep the caller's aspect ratio; never below the biggest part + a margin.
        let min_area = (total_area * 2.0).max(cw * ch * 1.3);
        let aspect = cw / ch;
        let mut sh = (min_area / aspect).sqrt();
        let mut sw = aspect * sh;
        sw = sw.max(max_w + 2.0);
        sh = sh.max(max_h + 2.0);
        extra = json!({
            "parts_courtyard_area_mm2": (total_area * 10.0).round() / 10.0,
            "current_bounds_mm": { "w": (cw * 10.0).round() / 10.0, "h": (ch * 10.0).round() / 10.0 },
            "suggested_min_bounds_mm": { "w": sw.ceil(), "h": sh.ceil() },
        });
    }

    let mut out = json!({
        "legal": result.legal,
        "hpwl": result.report.hpwl,
        "overlaps_resolved": result.report.overlaps_resolved,
        "out_of_bounds_clamps": result.report.out_of_bounds_clamps,
        "positions": positions,
        "note": if result.legal {
            "placement is legal (no courtyard overlap, all parts in bounds). \
             Call route_board next, or render_board to see it."
        } else {
            "placement is NOT legal — the board is too tight for these parts. The fix is more \
             room, not rearrangement: enlarge `board.outline` in the DSL to at least \
             suggested_min_bounds_mm and design_board again, then re-run place_board. \
             Locking/nudging parts won't help when the board is simply too small for the courtyards."
        },
    });
    if let (Value::Object(o), Value::Object(e)) = (&mut out, extra) {
        o.extend(e);
    }
    if !hint_suggestions.is_empty() {
        out["hint_suggestions"] = Value::Array(hint_suggestions);
    }
    Ok(out)
}
