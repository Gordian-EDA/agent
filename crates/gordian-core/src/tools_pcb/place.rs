//! Placement: the read-only `get_board` summary, the draft→`PlaceProblem`
//! bridge, the auto edge-affinity plumbing, and the `place_board` handler.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use kicad_ipc::FootprintMove;
use kicad_sexpr::footlib::{BBox, Footprint, FootprintPad, PadTechnology};
use pcb_model::LayerRef;
use pcb_model::place::PartPad;
use pcb_place::placement::{Part, PlaceProblem, Placement, Rect};

use crate::tools::PcbToolCtx;

use super::create::net_pin_counts;
use super::draft::BoardDraft;

fn part_from_footprint_layers(
    footprint: &Footprint,
    reference: &str,
    net_map: &BTreeMap<String, String>,
    layer_count: u32,
) -> Part {
    let pads = footprint
        .pads
        .iter()
        .map(|pad| part_pad(pad, net_map, layer_count))
        .collect();
    let (courtyard_w, courtyard_h) = enclosing_courtyard(footprint);
    Part {
        reference: reference.to_owned(),
        courtyard_w,
        courtyard_h,
        pads,
        locked: None,
    }
}

fn part_pad(pad: &FootprintPad, net_map: &BTreeMap<String, String>, layer_count: u32) -> PartPad {
    PartPad {
        number: pad.number.clone(),
        offset: pcb_model::Point2 {
            x: pad.at[0],
            y: pad.at[1],
        },
        width: pad.size[0],
        height: pad.size[1],
        layers: pad_layers(pad, layer_count),
        net: net_map.get(&pad.number).cloned(),
    }
}

fn all_copper_layers(layer_count: u32) -> Vec<LayerRef> {
    let n = layer_count.max(2);
    let mut layers = vec![LayerRef::top()];
    for i in 1..=(n.saturating_sub(2)) {
        layers.push(LayerRef(format!("inner{i}")));
    }
    layers.push(LayerRef::bottom());
    layers
}

fn pad_layers(pad: &FootprintPad, layer_count: u32) -> Vec<LayerRef> {
    let spans_all = matches!(
        pad.technology,
        PadTechnology::ThruHole | PadTechnology::NpThruHole
    ) || pad.layers.iter().any(|l| l == "*.Cu");
    if spans_all {
        return all_copper_layers(layer_count);
    }
    let on_front = pad.layers.iter().any(|l| l == "F.Cu");
    let on_back = pad.layers.iter().any(|l| l == "B.Cu");
    match (on_front, on_back) {
        (true, true) => all_copper_layers(layer_count),
        (false, true) => vec![LayerRef::bottom()],
        _ => vec![LayerRef::top()],
    }
}

fn enclosing_courtyard(footprint: &Footprint) -> (f64, f64) {
    let (mut hw, mut hh) = abs_half(&footprint.courtyard);
    if let Some(pad_bbox) = pad_bbox(&footprint.pads) {
        let (pw, ph) = abs_half(&pad_bbox);
        hw = hw.max(pw);
        hh = hh.max(ph);
    }
    (hw * 2.0, hh * 2.0)
}

fn abs_half(b: &BBox) -> (f64, f64) {
    (
        b.min_x.abs().max(b.max_x.abs()),
        b.min_y.abs().max(b.max_y.abs()),
    )
}

fn pad_bbox(pads: &[FootprintPad]) -> Option<BBox> {
    let mut it = pads.iter();
    let first = it.next()?;
    let mut bbox = pad_aabb(first);
    for pad in it {
        let p = pad_aabb(pad);
        bbox.min_x = bbox.min_x.min(p.min_x);
        bbox.min_y = bbox.min_y.min(p.min_y);
        bbox.max_x = bbox.max_x.max(p.max_x);
        bbox.max_y = bbox.max_y.max(p.max_y);
    }
    Some(bbox)
}

fn pad_aabb(pad: &FootprintPad) -> BBox {
    let [cx, cy] = pad.at;
    let (hw, hh) = geom::rotated_aabb_half(pad.size[0], pad.size[1], pad.rotation);
    BBox {
        min_x: cx - hw,
        min_y: cy - hh,
        max_x: cx + hw,
        max_y: cy + hh,
    }
}

// ── get_board ────────────────────────────────────────────────────────────────

pub fn get_board(ctx: &PcbToolCtx) -> Result<Value> {
    let draft = match super::active::draft_from_live(ctx) {
        Ok(draft) => draft,
        Err(err) => return Ok(json!({ "error": err })),
    };

    let net_pins = net_pin_counts(&draft.parts, ctx);
    let nets: Vec<Value> = net_pins
        .iter()
        .map(|(name, &pins)| json!({ "name": name, "pins": pins }))
        .collect();

    let placed = draft.last_placement.is_some();
    // Routed state is read from the active KiCAD board.
    let routed = ctx
        .kicad()
        .with_session(&ctx.pcb_path(), |session| {
            Ok(session.kicad().tracks()?.len() > 0)
        })
        .unwrap_or(false);

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
    let inset = EDGE_CLEAR_MM
        .min((b.max_x - b.min_x) / 2.0 - 0.1)
        .min((b.max_y - b.min_y) / 2.0 - 0.1);
    Rect {
        min_x: b.min_x + inset,
        max_x: b.max_x - inset,
        min_y: b.min_y + inset,
        max_y: b.max_y - inset,
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
    let draft = match super::active::draft_from_live(ctx) {
        Ok(draft) => draft,
        Err(live_err) => return Ok(json!({ "error": live_err })),
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

    let moves: Vec<FootprintMove> = result
        .placements
        .iter()
        .map(|p| FootprintMove {
            reference: p.reference.clone(),
            x_nm: (p.at.x * 1_000_000.0).round() as i64,
            y_nm: (p.at.y * 1_000_000.0).round() as i64,
            rotation_deg: Some(p.rotation as f64),
        })
        .collect();
    if let Err(e) = ctx.kicad().with_session(&ctx.pcb_path(), |session| {
        session.kicad().move_footprints(&moves)?;
        session.kicad().save()
    }) {
        return Ok(json!({ "error": format!("could not write placement to KiCAD: {e}") }));
    }
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
                let cap_refs: Vec<String> = caps
                    .iter()
                    .map(|&c| problem.parts[c].reference.clone())
                    .collect();
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
        let max_w = problem
            .parts
            .iter()
            .map(|p| p.courtyard_w)
            .fold(0.0, f64::max);
        let max_h = problem
            .parts
            .iter()
            .map(|p| p.courtyard_h)
            .fold(0.0, f64::max);
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
