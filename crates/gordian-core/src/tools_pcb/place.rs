//! Placement over the live KiCAD IPC board.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use kicad_footprint::{Footprint, FootprintId, FootprintPad, PadTechnology};
use kicad_ipc::{FootprintMove, snapshot::IpcBoardSnapshot};
use pcb_model::LayerRef;
use pcb_model::place::PartPad;
use pcb_place::placement::{LockedAt, Part, PlaceProblem, Placement, PlacementHints, Rect};

use crate::AgentRuntime;

use super::seed::BoardSeed;

fn part_from_footprint_layers(
    footprint: &Footprint,
    reference: &str,
    net_map: &BTreeMap<String, String>,
    layer_count: u32,
    locked: Option<LockedAt>,
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
        locked,
    }
}

fn part_pad(pad: &FootprintPad, net_map: &BTreeMap<String, String>, layer_count: u32) -> PartPad {
    let half =
        geom::Point2::new(pad.size.x / 2.0, pad.size.y / 2.0).rotated_half_extents(pad.rotation);
    PartPad {
        number: pad.number.clone(),
        offset: pad.at,
        width: half.x * 2.0,
        height: half.y * 2.0,
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

fn abs_half(b: &Rect) -> (f64, f64) {
    (
        b.min_x.abs().max(b.max_x.abs()),
        b.min_y.abs().max(b.max_y.abs()),
    )
}

fn pad_bbox(pads: &[FootprintPad]) -> Option<Rect> {
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

fn pad_aabb(pad: &FootprintPad) -> Rect {
    let half =
        geom::Point2::new(pad.size.x / 2.0, pad.size.y / 2.0).rotated_half_extents(pad.rotation);
    Rect::new(
        pad.at.x - half.x,
        pad.at.y - half.y,
        pad.at.x + half.x,
        pad.at.y + half.y,
    )
}

// ── get_board ────────────────────────────────────────────────────────────────

pub fn get_board(ctx: &AgentRuntime) -> Result<Value> {
    let board = match super::active::board_problem(ctx) {
        Ok(board) => board,
        Err(err) => return Ok(json!({ "error": err })),
    };

    let net_pins = snapshot_net_pin_counts(&board);
    let nets: Vec<Value> = net_pins
        .iter()
        .map(|(name, &pins)| json!({ "name": name, "pins": pins }))
        .collect();

    let placed = !super::active::is_seed_imported_board(&board.imported);
    let routed = !board.copper.traces.is_empty() || !board.copper.vias.is_empty();
    let parts: Vec<Value> = board
        .imported
        .parts
        .iter()
        .map(|part| {
            json!({
                "reference": part.reference,
                "footprint": part.lib_id,
                "x": part.at.x,
                "y": part.at.y,
                "rotation": part.rotation,
                "pad_count": part.pads.iter().filter(|(_, net)| net.is_some()).count(),
            })
        })
        .collect();
    let board_json = json!({
        "bounds": board.imported.bounds,
        "outline": board.problem.outline,
        "rules": {
            "clearance": board.problem.clearance,
            "minTraceWidth": board.problem.min_trace_width,
            "viaDiameter": board.problem.via_diameter,
            "viaDrill": board.problem.via_drill,
            "layerCount": board.problem.layer_count,
            "netWidths": board.problem.net_widths,
        },
        "parts": parts,
    });

    Ok(json!({
        "board": board_json,
        "summary": {
            "part_count": board.imported.parts.len(),
            "net_count": net_pins.len(),
            "nets": nets,
            "keepout_count": 0,
            "placed": placed,
            "routed": routed,
        },
    }))
}

fn snapshot_net_pin_counts(board: &IpcBoardSnapshot) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for part in &board.imported.parts {
        for (_, net) in &part.pads {
            if let Some(net) = net {
                *counts.entry(net.clone()).or_insert(0) += 1;
            }
        }
    }
    counts
}

// ── IPC snapshot to engine problem ───────────────────────────────────────────

pub(super) fn place_problem_from_snapshot(
    board: &IpcBoardSnapshot,
    ctx: &AgentRuntime,
) -> std::result::Result<PlaceProblem, String> {
    let catalog = ctx
        .footprint_catalog()
        .map_err(|e| format!("footprint catalog unavailable: {e}"))?;
    let mut parts: Vec<Part> = Vec::with_capacity(board.imported.parts.len());
    for imported in &board.imported.parts {
        let id = FootprintId::parse(&imported.lib_id).map_err(|e| {
            format!(
                "part {}: invalid footprint id `{}`: {e}",
                imported.reference, imported.lib_id
            )
        })?;
        let fp = catalog.footprint(&id).map_err(|e| {
            format!(
                "part {}: footprint `{}` is no longer resolvable: {e}",
                imported.reference, imported.lib_id
            )
        })?;
        parts.push(part_from_footprint_layers(
            &fp,
            &imported.reference,
            &pad_net_map(&imported.pads),
            board.problem.layer_count,
            imported.locked.then_some(LockedAt {
                at: imported.at,
                rotation: imported.rotation as f64,
            }),
        ));
    }

    Ok(PlaceProblem {
        bounds: routing_bounds(&board.problem.bounds, board.problem.outline.as_ref()),
        clearance: board.problem.clearance,
        layer_count: board.problem.layer_count,
        min_trace_width: board.problem.min_trace_width,
        parts,
        keepouts: Vec::new(),
        outline: board.problem.outline.clone(),
    })
}

fn pad_net_map(pads: &[(String, Option<String>)]) -> BTreeMap<String, String> {
    pads.iter()
        .filter_map(|(pad, net)| net.as_ref().map(|net| (pad.clone(), net.clone())))
        .collect()
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
fn routing_bounds(bounds: &Rect, outline: Option<&pcb_model::Polygon>) -> Rect {
    if outline.is_none() {
        return bounds.clone();
    }
    // Never invert a small board: clamp the inset so min stays < max.
    let inset = EDGE_CLEAR_MM
        .min((bounds.max_x - bounds.min_x) / 2.0 - 0.1)
        .min((bounds.max_y - bounds.min_y) / 2.0 - 0.1);
    Rect {
        min_x: bounds.min_x + inset,
        max_x: bounds.max_x - inset,
        min_y: bounds.min_y + inset,
        max_y: bounds.max_y - inset,
    }
}

/// JSON shape for one placed part returned by `place_board`.
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

pub fn place_board(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    if let Some(seed) = read_board_seed(ctx) {
        return Ok(place_seed_board(ctx, &seed));
    }

    let board = match super::active::board_problem(ctx) {
        Ok(board) => board,
        Err(live_err) => return Ok(json!({ "error": live_err })),
    };

    let problem = match place_problem_from_snapshot(&board, ctx) {
        Ok(p) => p,
        Err(msg) => return Ok(json!({ "error": msg })),
    };

    // Auto edge-affinity: pull connectors/headers to their nearest board edge so
    // they land at the perimeter (where a cable or the enclosure reaches them),
    // not stranded in the interior with copper wrapping around them. Skip any
    // part the model already steered with an explicit group `edge` hint.
    let mut hints = PlacementHints::default();
    let explicitly_edged: std::collections::BTreeSet<&str> = hints
        .groups
        .iter()
        .filter(|g| g.edge.is_some())
        .flat_map(|g| g.members.iter().map(String::as_str))
        .collect();
    for p in &board.imported.parts {
        if explicitly_edged.contains(p.reference.as_str()) {
            continue;
        }
        // Mounting holes corner-seek (mechanical fixings at the board corners) AND
        // edge-seek (so any hole the corner post-pass can't seat — a corner taken
        // or blocked by a part — falls back to the perimeter, not the interior).
        // Other connectors/headers edge-seek (a cable/enclosure reaches the edge).
        if is_mounting_hole(&p.lib_id) {
            if !hints.corner_seek.contains(&p.reference) {
                hints.corner_seek.push(p.reference.clone());
            }
            if !hints.edge_seek.contains(&p.reference) {
                hints.edge_seek.push(p.reference.clone());
            }
        } else if is_connector(&p.lib_id, &p.reference) && !hints.edge_seek.contains(&p.reference) {
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

    if result.legal {
        let locked_refs: std::collections::BTreeSet<&str> = board
            .imported
            .parts
            .iter()
            .filter(|p| p.locked)
            .map(|p| p.reference.as_str())
            .collect();
        let moves: Vec<FootprintMove> = result
            .placements
            .iter()
            .filter(|p| !locked_refs.contains(p.reference.as_str()))
            .map(|p| FootprintMove {
                reference: p.reference.clone(),
                x_nm: (p.at.x * 1_000_000.0).round() as i64,
                y_nm: (p.at.y * 1_000_000.0).round() as i64,
                rotation_deg: Some(p.rotation as f64),
            })
            .collect();
        if !moves.is_empty()
            && let Err(e) = write_placement(ctx, &moves)
        {
            return Ok(json!({ "error": format!("could not write placement to KiCAD: {e}") }));
        }
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
                         surround: {ic_ref}}}}} — then apply_design, regenerate_board, and place_board.",
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
             room, not rearrangement: enlarge the board outline or regenerate_board bounds to at least \
             suggested_min_bounds_mm, regenerate_board again, then re-run place_board. \
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

fn read_board_seed(ctx: &AgentRuntime) -> Option<BoardSeed> {
    let text = std::fs::read_to_string(super::create::board_seed_path(ctx)).ok()?;
    serde_json::from_str(&text).ok()
}

fn place_seed_board(ctx: &AgentRuntime, seed: &BoardSeed) -> Value {
    let problem = match debug_place_problem_from_seed(seed, ctx) {
        Ok(problem) => problem,
        Err(msg) => return json!({ "error": msg }),
    };
    let hints = placement_hints_for_seed(seed);
    let result = pcb_place::placement::place_board(&problem, &hints);
    if result.legal {
        if let Err(msg) = persist_placements(ctx, &result.placements) {
            return json!({ "error": msg });
        }
        let moves: Vec<FootprintMove> = result
            .placements
            .iter()
            .map(|p| FootprintMove {
                reference: p.reference.clone(),
                x_nm: (p.at.x * 1_000_000.0).round() as i64,
                y_nm: (p.at.y * 1_000_000.0).round() as i64,
                rotation_deg: Some(p.rotation),
            })
            .collect();
        if let Err(msg) = write_placement_to_board_file(ctx, &moves) {
            return json!({ "error": msg });
        }
    }
    let positions: Vec<Value> = result.placements.iter().map(placement_json).collect();
    json!({
        "legal": result.legal,
        "hpwl": result.report.hpwl,
        "overlaps_resolved": result.report.overlaps_resolved,
        "out_of_bounds_clamps": result.report.out_of_bounds_clamps,
        "positions": positions,
        "note": if result.legal {
            "placement is legal and was written directly to the board file. Call route_board next, or render_board to see it."
        } else {
            "placement is NOT legal — enlarge the board with regenerate_board bounds and retry."
        },
    })
}

fn placement_hints_for_seed(seed: &BoardSeed) -> PlacementHints {
    let mut hints = seed.hints.clone();
    for part in &seed.parts {
        if is_mounting_hole(&part.footprint) {
            if !hints.corner_seek.contains(&part.reference) {
                hints.corner_seek.push(part.reference.clone());
            }
            if !hints.edge_seek.contains(&part.reference) {
                hints.edge_seek.push(part.reference.clone());
            }
        } else if is_connector(&part.footprint, &part.reference)
            && !hints.edge_seek.contains(&part.reference)
        {
            hints.edge_seek.push(part.reference.clone());
        }
    }
    hints
}

pub(super) fn board_placement_path(ctx: &AgentRuntime) -> std::path::PathBuf {
    ctx.project_dir()
        .join(".gordian")
        .join("board.placement.json")
}

fn persist_placements(
    ctx: &AgentRuntime,
    placements: &[Placement],
) -> std::result::Result<(), String> {
    let text = serde_json::to_string_pretty(placements)
        .map_err(|e| format!("could not serialize board placement metadata: {e}"))?;
    std::fs::write(board_placement_path(ctx), format!("{text}\n"))
        .map_err(|e| format!("could not write board placement metadata: {e}"))
}

pub fn debug_place_problem_from_seed(
    seed: &BoardSeed,
    ctx: &AgentRuntime,
) -> std::result::Result<PlaceProblem, String> {
    let catalog = ctx
        .footprint_catalog()
        .map_err(|e| format!("footprint catalog unavailable: {e}"))?;
    let mut parts = Vec::with_capacity(seed.parts.len());
    for part in &seed.parts {
        let id = FootprintId::parse(&part.footprint).map_err(|e| {
            format!(
                "part {}: invalid footprint id `{}`: {e}",
                part.reference, part.footprint
            )
        })?;
        let footprint = catalog.footprint(&id).map_err(|e| {
            format!(
                "part {}: footprint `{}` is not resolvable: {e}",
                part.reference, part.footprint
            )
        })?;
        parts.push(part_from_footprint_layers(
            &footprint,
            &part.reference,
            &part.pad_nets,
            seed.rules.layer_count,
            part.locked.clone(),
        ));
    }
    Ok(PlaceProblem {
        bounds: seed.bounds.clone(),
        clearance: seed.rules.clearance,
        layer_count: seed.rules.layer_count,
        min_trace_width: seed.rules.min_trace_width,
        parts,
        keepouts: seed.keepouts.iter().map(|k| k.rect.clone()).collect(),
        outline: seed.outline.clone(),
    })
}

fn write_placement_to_board_file(
    ctx: &AgentRuntime,
    moves: &[FootprintMove],
) -> std::result::Result<(), String> {
    let path = ctx.pcb_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let updated = update_footprint_positions(&text, moves)?;
    std::fs::write(&path, updated).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn update_footprint_positions(
    board: &str,
    moves: &[FootprintMove],
) -> std::result::Result<String, String> {
    let by_ref: BTreeMap<&str, &FootprintMove> =
        moves.iter().map(|m| (m.reference.as_str(), m)).collect();
    let mut out = String::with_capacity(board.len());
    let mut pos = 0usize;
    while let Some(rel) = board[pos..].find("(footprint ") {
        let start = pos + rel;
        out.push_str(&board[pos..start]);
        let Some(end) = sexpr_end(board, start) else {
            return Err("could not parse footprint block in board file".to_owned());
        };
        let block = &board[start..end];
        if let Some(reference) = footprint_reference_in_block(block)
            && let Some(mv) = by_ref.get(reference.as_str())
        {
            out.push_str(&replace_first_at(block, mv));
        } else {
            out.push_str(block);
        }
        pos = end;
    }
    out.push_str(&board[pos..]);
    Ok(out)
}

fn sexpr_end(text: &str, start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

fn footprint_reference_in_block(block: &str) -> Option<String> {
    let marker = "(property \"Reference\" \"";
    let start = block.find(marker)? + marker.len();
    let rest = &block[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

fn replace_first_at(block: &str, mv: &FootprintMove) -> String {
    let x = mv.x_nm as f64 / 1e6;
    let y = mv.y_nm as f64 / 1e6;
    let rotation = mv.rotation_deg.unwrap_or(0.0);
    let mut replaced = false;
    let mut out = String::with_capacity(block.len());
    for line in block.lines() {
        if !replaced && line.trim_start().starts_with("(at ") {
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push_str(&format!(
                "{indent}(at {} {} {})\n",
                super::fmt_num(x),
                super::fmt_num(y),
                super::fmt_num(rotation)
            ));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !block.ends_with('\n') {
        out.pop();
    }
    out
}

fn write_placement(
    ctx: &AgentRuntime,
    moves: &[FootprintMove],
) -> std::result::Result<(), kicad_ipc::Error> {
    let path = ctx.pcb_path();
    ctx.kicad().with_session(&path, |session| {
        session.kicad().move_footprints(moves)?;
        session.kicad().save()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::Point2;
    use kicad_footprint::PadTechnology;

    #[test]
    fn footprint_pad_rotation_affects_route_obstacle_size() {
        let pad = FootprintPad {
            number: "2".to_string(),
            at: Point2 { x: -3.81, y: 6.25 },
            rotation: 90.0,
            size: Point2 { x: 2.5, y: 1.2 },
            shape: "rect".to_string(),
            layers: vec!["F.Cu".to_string()],
            technology: PadTechnology::Smd,
            drill: None,
        };
        let mut nets = BTreeMap::new();
        nets.insert("2".to_string(), "GND".to_string());

        let got = part_pad(&pad, &nets, 2);

        assert!((got.width - 1.2).abs() < 1e-9);
        assert!((got.height - 2.5).abs() < 1e-9);
        assert_eq!(got.net.as_deref(), Some("GND"));
    }
}
