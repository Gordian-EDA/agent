//! Placement over the live KiCAD IPC board.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use kicad_footprint::{Footprint, FootprintId, FootprintPad, PadTechnology};
use kicad_ipc::{
    FootprintMove,
    snapshot::{ImportedPad, IpcBoardSnapshot},
};
use pcb_model::place::PartPad;
use pcb_model::{LayerRef, ViaSpan};
use pcb_place::placement::{
    EdgeDatum, LockedAt, Part, PlaceProblem, PlaceResult, Placement, PlacementHints, Rect,
};

use crate::AgentRuntime;

pub(super) fn part_from_footprint_layers(
    footprint: &Footprint,
    reference: &str,
    net_map: &BTreeMap<String, String>,
    layer_count: u32,
    locked: Option<LockedAt>,
) -> Part {
    // Paste/mask-only apertures (KiCAD EP footprints carry unnumbered F.Paste
    // stencil pads over the exposed pad) hold no copper: modeling them as pad
    // obstacles walls the EP terminal in and makes every EP net unroutable.
    let pads = footprint
        .pads
        .iter()
        .filter(|pad| has_copper(pad))
        .map(|pad| part_pad(pad, net_map, layer_count))
        .collect();
    let (courtyard_w, courtyard_h) = enclosing_courtyard(footprint);
    Part {
        reference: reference.to_owned(),
        courtyard_w,
        courtyard_h,
        pads,
        edge_datum: footprint.pcb_edge_datum.map(|datum| EdgeDatum {
            start: datum.start,
            end: datum.end,
        }),
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

fn has_copper(pad: &FootprintPad) -> bool {
    matches!(
        pad.technology,
        PadTechnology::ThruHole | PadTechnology::NpThruHole
    ) || pad.layers.iter().any(|l| l.ends_with(".Cu") || l == "*.Cu")
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

pub fn get_board(input: Value, ctx: &AgentRuntime) -> Result<Value> {
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
                "pad_count": part.pads.iter().filter(|pad| pad.net.is_some()).count(),
            })
        })
        .collect();
    let mut board_json = json!({
        "bounds": board.imported.bounds,
        "outline": board.problem.outline,
        "rules": {
            "clearance": board.problem.clearance,
            "min_trace_width": board.problem.min_trace_width,
            "via_diameter": board.problem.via_diameter,
            "via_drill": board.problem.via_drill,
            "layer_count": board.problem.layer_count,
            "net_widths": board.problem.net_widths,
        },
        "parts": parts,
    });
    let include_copper = input
        .get("include_copper")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if include_copper {
        if let Some(layer) = input.get("layer").and_then(Value::as_str)
            && layer_filter_index(layer, board.problem.layer_count).is_none()
        {
            return Ok(json!({
                "error": format!(
                    "unknown copper layer `{layer}` for a {}-layer board",
                    board.problem.layer_count
                )
            }));
        }
        board_json["copper"] = copper_json(&board, &input);
    }
    if let Some(net) = input
        .get("net")
        .and_then(Value::as_str)
        .filter(|net| !net.is_empty())
    {
        board_json["terminals"] = terminals_json(&board, net);
    }

    Ok(json!({
        "board": board_json,
        "summary": {
            "part_count": board.imported.parts.len(),
            "net_count": net_pins.len(),
            "nets": nets,
            "keepout_count": board.imported.keepout_count,
            "placed": placed,
            "routed": routed,
        },
    }))
}

const MAX_FILTERED_TERMINALS: usize = 128;

fn terminals_json(board: &IpcBoardSnapshot, net: &str) -> Value {
    let matching = board.imported.parts.iter().flat_map(|part| {
        part.pads
            .iter()
            .filter_map(move |pad| (pad.net.as_deref() == Some(net)).then_some((part, pad)))
    });
    let total = matching.clone().count();
    let items: Vec<Value> = matching
        .take(MAX_FILTERED_TERMINALS)
        .map(|(part, pad)| {
            json!({
                "ref": part.reference,
                "pad": pad.number,
                "net": net,
                "x": pad.at.x,
                "y": pad.at.y,
                "layers": pad.layers.iter().map(|layer| layer_name(layer, board.problem.layer_count)).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "items": items,
        "count": total,
        "truncated": total > MAX_FILTERED_TERMINALS,
    })
}

fn copper_json(board: &IpcBoardSnapshot, input: &Value) -> Value {
    let net_filter = input
        .get("net")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let layer_filter = input
        .get("layer")
        .and_then(Value::as_str)
        .and_then(|layer| layer_filter_index(layer, board.problem.layer_count));
    let include_tracks = copper_kind_enabled(input, "track");
    let include_vias = copper_kind_enabled(input, "via");

    let tracks: Vec<Value> = if include_tracks {
        board
            .copper
            .traces
            .iter()
            .filter(|trace| net_filter.is_none_or(|net| trace.connection == net))
            .filter(|trace| {
                layer_filter
                    .is_none_or(|layer| trace.layer.index(board.problem.layer_count) == Some(layer))
            })
            .map(|trace| {
                json!({
                    "net": trace.connection,
                    "layer": layer_name(&trace.layer, board.problem.layer_count),
                    "width": trace.width,
                    "path": trace.path.iter().map(|p| json!([p.x, p.y])).collect::<Vec<_>>(),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    let vias: Vec<Value> = if include_vias {
        board
            .copper
            .vias
            .iter()
            .filter(|via| net_filter.is_none_or(|net| via.connection == net))
            .filter(|via| {
                layer_filter.is_none_or(|layer| {
                    via_span_indices(&via.span, board.problem.layer_count).contains(&layer)
                })
            })
            .map(|via| {
                let layers: Vec<Value> = via_span_indices(&via.span, board.problem.layer_count)
                    .into_iter()
                    .map(|idx| json!(layer_name_from_index(idx, board.problem.layer_count)))
                    .collect();
                json!({
                    "net": via.connection,
                    "at": [via.at.x, via.at.y],
                    "diameter": via.diameter,
                    "drill": via.drill,
                    "layers": layers,
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    json!({
        "tracks": tracks,
        "vias": vias,
        "track_count": tracks.len(),
        "via_count": vias.len(),
    })
}

fn copper_kind_enabled(input: &Value, kind: &str) -> bool {
    input
        .get("kinds")
        .and_then(Value::as_array)
        .map(|kinds| kinds.iter().any(|v| v.as_str() == Some(kind)))
        .unwrap_or(true)
}

fn layer_filter_index(layer: &str, layer_count: u32) -> Option<u32> {
    canonical_layer_name(layer)
        .as_deref()
        .and_then(|layer| LayerRef::resolve(layer, layer_count))
        .map(|(idx, _)| idx)
}

fn canonical_layer_name(name: &str) -> Option<String> {
    let norm = name.trim().to_ascii_lowercase().replace(['.', '-'], "_");
    match norm.as_str() {
        "f_cu" | "top" | "front" => return Some("F.Cu".to_owned()),
        "b_cu" | "bottom" | "back" => return Some("B.Cu".to_owned()),
        _ => {}
    }
    if let Some(num) = norm
        .strip_prefix("inner")
        .and_then(|s| s.parse::<u32>().ok())
    {
        return Some(format!("inner{num}"));
    }
    let rest = norm.strip_prefix("in")?;
    let num = rest.strip_suffix("_cu").unwrap_or(rest);
    num.parse::<u32>().ok().map(|idx| format!("In{idx}.Cu"))
}

fn layer_name(layer: &LayerRef, layer_count: u32) -> String {
    layer
        .index(layer_count)
        .map(|idx| layer_name_from_index(idx, layer_count))
        .unwrap_or_else(|| layer.0.clone())
}

fn layer_name_from_index(idx: u32, layer_count: u32) -> String {
    if idx == 0 {
        "F.Cu".to_owned()
    } else if idx + 1 == layer_count.max(1) {
        "B.Cu".to_owned()
    } else {
        format!("In{idx}.Cu")
    }
}

fn via_span_indices(span: &ViaSpan, layer_count: u32) -> Vec<u32> {
    match *span {
        ViaSpan::Through => (0..layer_count.max(1)).collect(),
        ViaSpan::Partial { from, to, .. } => {
            let (lo, hi) = (from.min(to), from.max(to));
            (lo..=hi).filter(|idx| *idx < layer_count.max(1)).collect()
        }
    }
}

fn snapshot_net_pin_counts(board: &IpcBoardSnapshot) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for part in &board.imported.parts {
        for pad in &part.pads {
            if let Some(net) = &pad.net {
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
        bounds: board.problem.bounds,
        clearance: board.problem.clearance,
        layer_count: board.problem.layer_count,
        min_trace_width: board.problem.min_trace_width,
        parts,
        keepouts: board.imported.placement_keepouts.clone(),
        outline: board.problem.outline.clone(),
    })
}

fn pad_net_map(pads: &[ImportedPad]) -> BTreeMap<String, String> {
    pads.iter()
        .filter_map(|pad| {
            pad.net
                .as_ref()
                .map(|net| (pad.number.clone(), net.clone()))
        })
        .collect()
}

/// KiCAD's copper-to-board-edge clearance (its default). Copper closer than this to the
/// Edge.Cuts is a `copper_edge_clearance` fault.
const EDGE_CLEAR_MM: f64 = 0.5;

/// The bounds the placer + router actually work inside. The production seed
/// writer emits Edge.Cuts at the requested bounds for both rectangular and
/// polygon boards, so copper must stay inside KiCad's 0.5 mm edge clearance in
/// both cases. (Inset the bbox; the lint also checks distance to polygon edges,
/// catching non-bbox edges of a non-rectangular outline.)
pub(super) fn routing_bounds(bounds: &Rect, _outline: Option<&pcb_model::Polygon>) -> Rect {
    // Never invert a small board: clamp the inset so min stays < max.
    let max_x_inset = ((bounds.max_x - bounds.min_x) / 2.0 - 0.1).max(0.0);
    let max_y_inset = ((bounds.max_y - bounds.min_y) / 2.0 - 0.1).max(0.0);
    let inset = EDGE_CLEAR_MM.min(max_x_inset).min(max_y_inset);
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

fn placement_overlap_pairs(problem: &PlaceProblem, result: &PlaceResult) -> Vec<Value> {
    let margin = pcb_model::place::courtyard_margin(problem.clearance) / 2.0;
    let positions: BTreeMap<_, _> = result
        .placements
        .iter()
        .map(|placement| (placement.reference.as_str(), placement))
        .collect();
    let mut overlaps = Vec::new();
    for (i, a) in problem.parts.iter().enumerate() {
        let Some(pa) = positions.get(a.reference.as_str()) else {
            continue;
        };
        let ah = pcb_model::place::rotated_courtyard_half(a, pa.rotation);
        let ar = Rect::from_center_half(pa.at, ah).inflate(margin);
        for b in problem.parts.iter().skip(i + 1) {
            let Some(pb) = positions.get(b.reference.as_str()) else {
                continue;
            };
            let bh = pcb_model::place::rotated_courtyard_half(b, pb.rotation);
            let br = Rect::from_center_half(pb.at, bh).inflate(margin);
            let (x, y) = ar.axis_penetration(&br);
            if x > geom::EPS && y > geom::EPS {
                overlaps.push(json!({
                    "a": a.reference,
                    "b": b.reference,
                    "overlap_x_mm": (x * 100.0).round() / 100.0,
                    "overlap_y_mm": (y * 100.0).round() / 100.0,
                }));
            }
        }
    }
    overlaps
}

// ── place_board ──────────────────────────────────────────────────────────────

/// Whether a part is a board-edge part (connector / header / terminal block /
/// mounting hole) that should hug the perimeter. Detected from the footprint
/// library id, with the conventional `J` reference prefix as a fallback.
pub(super) fn is_connector(footprint: &str, reference: &str) -> bool {
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
pub(super) fn is_mounting_hole(footprint: &str) -> bool {
    footprint.to_ascii_lowercase().contains("mountinghole")
}

fn placement_hints_from_input(mut input: Value) -> std::result::Result<PlacementHints, String> {
    let object = input
        .as_object_mut()
        .ok_or_else(|| "place_board input must be an object".to_owned())?;

    // Tool inputs consistently use snake_case. The neutral placement SDK uses
    // camelCase serde names so external engine payloads remain language-neutral.
    // Normalize only that boundary here and keep the engine type canonical.
    for (tool_name, sdk_name) in [("edge_seek", "edgeSeek"), ("corner_seek", "cornerSeek")] {
        if let Some(value) = object.remove(tool_name) {
            if object.insert(sdk_name.to_owned(), value).is_some() {
                return Err(format!(
                    "use {tool_name}, not both snake_case and camelCase"
                ));
            }
        }
    }
    if let Some(groups) = object.get_mut("groups").and_then(Value::as_array_mut) {
        for group in groups {
            if let Some(region) = group
                .as_object_mut()
                .and_then(|group| group.get_mut("region"))
                .and_then(Value::as_object_mut)
            {
                for (tool_name, sdk_name) in [
                    ("min_x", "minX"),
                    ("min_y", "minY"),
                    ("max_x", "maxX"),
                    ("max_y", "maxY"),
                ] {
                    if let Some(value) = region.remove(tool_name) {
                        if region.insert(sdk_name.to_owned(), value).is_some() {
                            return Err(format!(
                                "use region.{tool_name}, not both snake_case and camelCase"
                            ));
                        }
                    }
                }
            }
        }
    }
    let hints: PlacementHints =
        serde_json::from_value(input).map_err(|e| format!("invalid placement hints: {e}"))?;
    if let Some(rotation) = hints.groups.iter().find_map(|group| {
        group
            .rotation
            .filter(|rotation| ![0.0, 90.0, 180.0, 270.0].contains(rotation))
    }) {
        return Err(format!(
            "invalid placement hints: grid rotation {rotation} must be 0, 90, 180, or 270"
        ));
    }
    Ok(hints)
}

pub fn place_board(input: Value, ctx: &AgentRuntime) -> Result<Value> {
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
    let mut hints = match placement_hints_from_input(input) {
        Ok(hints) => hints,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let explicitly_edged: std::collections::BTreeSet<&str> = hints
        .groups
        .iter()
        .filter(|g| g.edge.is_some())
        .flat_map(|g| g.members.iter().map(String::as_str))
        .collect();
    let explicitly_regioned: std::collections::BTreeSet<&str> = hints
        .groups
        .iter()
        .filter(|g| g.region.is_some())
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
            // An authored region is more deliberate than the generic corner
            // heuristic (for example, a 5 mm centre leaves useful edge stock
            // around an M3 pad). Preserve it instead of snapping to the
            // courtyard-tight mathematical corner.
            if explicitly_regioned.contains(p.reference.as_str()) {
                continue;
            }
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
    // overrides any of this with the interactive geometry tools (move_parts/route_track).
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
                rotation_deg: Some(p.rotation),
            })
            .collect();
        if !moves.is_empty()
            && let Err(e) = write_placement(ctx, &moves)
        {
            return Ok(json!({ "error": format!("could not write placement: {e}") }));
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
                        "{ic_ref} has {} decoupling caps the placer scattered. Render the board \
                         and, if the cluster is still poor, use `move_parts` for deliberate live \
                         refinement rather than regenerating unchanged.",
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
            "overlap_pairs": placement_overlap_pairs(&problem, &result),
            "parts_courtyard_area_mm2": (total_area * 10.0).round() / 10.0,
            "current_bounds_mm": { "w": (cw * 10.0).round() / 10.0, "h": (ch * 10.0).round() / 10.0 },
            "suggested_min_bounds_mm": { "w": sw.ceil(), "h": sh.ceil() },
        });
    }

    let mut out = json!({
        "placement_applied": result.legal,
        "legal": result.legal,
        "hpwl": result.report.hpwl,
        "overlaps_resolved": result.report.overlaps_resolved,
        "out_of_bounds_clamps": result.report.out_of_bounds_clamps,
        "positions": positions,
        "note": if result.legal {
            "placement is legal (no courtyard overlap, all parts in bounds). \
             Call route_board next, or render_board to see it."
        } else {
            "placement is NOT legal, so no footprint positions were written and the board remains at \
             its previous (usually regeneration-seed) positions. To keep the requested board size, \
             choose smaller appropriate footprints; otherwise regenerate_board with bounds at least \
             suggested_min_bounds_mm, then run place_board once."
        },
    });
    if let (Value::Object(o), Value::Object(e)) = (&mut out, extra) {
        o.extend(e);
    }
    if !hint_suggestions.is_empty() {
        out["hint_suggestions"] = Value::Array(hint_suggestions);
    }
    if !result.legal {
        out["error"] = Value::String(illegal_placement_error(&out));
    }
    Ok(out)
}

fn illegal_placement_error(result: &Value) -> String {
    let current_w = result
        .pointer("/current_bounds_mm/w")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let current_h = result
        .pointer("/current_bounds_mm/h")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let suggested_w = result
        .pointer("/suggested_min_bounds_mm/w")
        .and_then(Value::as_f64)
        .unwrap_or(current_w);
    let suggested_h = result
        .pointer("/suggested_min_bounds_mm/h")
        .and_then(Value::as_f64)
        .unwrap_or(current_h);
    format!(
        "placement failed and no positions were written: the placer could not legally pack the \
         selected footprints in {current_w} x {current_h} mm. Choose smaller appropriate \
         footprints to preserve that board size, or regenerate with at least \
         {suggested_w} x {suggested_h} mm, then run place_board once."
    )
}

fn write_placement(ctx: &AgentRuntime, moves: &[FootprintMove]) -> std::result::Result<(), String> {
    let path = ctx.pcb_path();
    let live = ctx.kicad().with_session(&path, |session| {
        session.kicad().move_footprints(moves)?;
        session.kicad().save()
    });
    let Err(live_err) = live else { return Ok(()) };
    // Headless / pre-9.0.3 fallback: apply the same moves to the board file
    // as s-expression edits. Any open session now holds stale state — drop it
    // so the next read reopens from disk.
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("{live_err}; offline fallback could not read the board: {e}"))?;
    let patched = super::patch::patch_placements(&text, moves)
        .map_err(|e| format!("{live_err}; offline fallback failed: {e}"))?;
    std::fs::write(&path, patched)
        .map_err(|e| format!("{live_err}; offline fallback could not write the board: {e}"))?;
    ctx.close_kicad_session();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::Point2;
    use kicad_env::KicadEnv;
    use kicad_footprint::{FootprintCatalog, PadTechnology};
    use kicad_ipc::snapshot::{ImportedBoard, ImportedPad, ImportedPart, IpcBoardSnapshot};
    use pcb_model::{RouteProblem, RouteSolution, Trace, Via};

    #[test]
    fn placement_tool_accepts_snake_case_visual_hints() {
        let hints = placement_hints_from_input(json!({
            "groups": [{
                "name": "power",
                "members": ["U1", "C1", "C2"],
                "region": { "min_x": 2.0, "min_y": 3.0, "max_x": 18.0, "max_y": 12.0 },
                "edge": "w",
                "grid": true,
                "rotation": 180,
                "surround": "U1"
            }],
            "edge_seek": ["J1"],
            "corner_seek": ["H1", "H2"]
        }))
        .expect("valid tool hints");

        assert_eq!(hints.edge_seek, ["J1"]);
        assert_eq!(hints.corner_seek, ["H1", "H2"]);
        assert_eq!(hints.groups.len(), 1);
        let group = &hints.groups[0];
        assert_eq!(group.members, ["U1", "C1", "C2"]);
        assert_eq!(group.region, Some(Rect::new(2.0, 3.0, 18.0, 12.0)));
        assert_eq!(group.edge, Some(pcb_place::placement::Edge::W));
        assert!(group.grid);
        assert_eq!(group.rotation, Some(180.0));
        assert_eq!(group.surround.as_deref(), Some("U1"));
    }

    #[test]
    fn placement_tool_rejects_unknown_or_mixed_case_hints() {
        assert!(placement_hints_from_input(json!({ "scatter": ["R1"] })).is_err());
        assert!(
            placement_hints_from_input(json!({
                "groups": [{"name": "bad", "members": ["R1"], "grid": true, "rotation": 45}]
            }))
            .is_err()
        );
        assert!(
            placement_hints_from_input(json!({
                "edge_seek": ["J1"],
                "edgeSeek": ["J2"]
            }))
            .is_err()
        );
    }

    #[test]
    fn routing_bounds_reserve_kicad_edge_clearance_for_rectangles() {
        let bounds = Rect {
            min_x: 0.0,
            min_y: 1.0,
            max_x: 20.0,
            max_y: 11.0,
        };

        assert_eq!(
            routing_bounds(&bounds, None),
            Rect {
                min_x: 0.5,
                min_y: 1.5,
                max_x: 19.5,
                max_y: 10.5,
            }
        );
    }

    #[test]
    fn routing_bounds_never_expand_degenerate_boards() {
        let bounds = Rect {
            min_x: 4.0,
            min_y: 7.0,
            max_x: 4.1,
            max_y: 7.1,
        };

        assert_eq!(routing_bounds(&bounds, None), bounds);
    }

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

    #[test]
    fn copper_json_filters_tracks_and_vias_for_inspection() {
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
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
            plane_nets: Default::default(),
        };
        let board = IpcBoardSnapshot {
            imported: ImportedBoard {
                layer_count: 2,
                bounds: problem.bounds,
                parts: vec![],
                placement_keepouts: vec![],
                keepout_count: 0,
            },
            problem,
            copper: RouteSolution {
                traces: vec![
                    Trace {
                        connection: "SIG".to_owned(),
                        layer: LayerRef::top(),
                        width: 0.2,
                        path: vec![Point2::new(1.0, 1.0), Point2::new(5.0, 1.0)],
                    },
                    Trace {
                        connection: "GND".to_owned(),
                        layer: LayerRef::bottom(),
                        width: 0.4,
                        path: vec![Point2::new(1.0, 2.0), Point2::new(5.0, 2.0)],
                    },
                ],
                vias: vec![Via {
                    connection: "SIG".to_owned(),
                    at: Point2::new(5.0, 1.0),
                    diameter: 0.6,
                    drill: 0.3,
                    span: ViaSpan::Through,
                }],
            },
            net_codes: Default::default(),
            layer_names: vec!["F.Cu".to_owned(), "B.Cu".to_owned()],
        };

        let out = copper_json(
            &board,
            &json!({ "include_copper": true, "net": "SIG", "layer": "F.Cu" }),
        );

        assert_eq!(out["track_count"], json!(1));
        assert_eq!(out["via_count"], json!(1));
        assert_eq!(out["tracks"][0]["net"], json!("SIG"));
        assert_eq!(out["tracks"][0]["layer"], json!("F.Cu"));
        assert_eq!(out["vias"][0]["net"], json!("SIG"));

        let tracks_only = copper_json(
            &board,
            &json!({ "include_copper": true, "kinds": ["track"] }),
        );
        assert_eq!(tracks_only["track_count"], json!(2));
        assert_eq!(tracks_only["via_count"], json!(0));
    }

    #[test]
    fn filtered_terminals_report_electrical_pad_anchors() {
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
            plane_nets: Default::default(),
        };
        let board = IpcBoardSnapshot {
            imported: ImportedBoard {
                layer_count: 2,
                bounds: problem.bounds,
                parts: vec![ImportedPart {
                    reference: "U1".to_owned(),
                    lib_id: "Package:Test".to_owned(),
                    at: Point2::new(10.0, 10.0),
                    rotation: 0,
                    locked: false,
                    pads: vec![
                        ImportedPad {
                            number: "A4".to_owned(),
                            net: Some("VBUS".to_owned()),
                            at: Point2::new(8.75, 9.5),
                            layers: vec![LayerRef::top()],
                        },
                        ImportedPad {
                            number: "A6".to_owned(),
                            net: Some("D+".to_owned()),
                            at: Point2::new(8.75, 10.0),
                            layers: vec![LayerRef::top()],
                        },
                    ],
                }],
                placement_keepouts: vec![],
                keepout_count: 0,
            },
            problem,
            copper: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            net_codes: Default::default(),
            layer_names: vec!["F.Cu".to_owned(), "B.Cu".to_owned()],
        };

        let out = terminals_json(&board, "VBUS");

        assert_eq!(out["count"], json!(1));
        assert_eq!(out["truncated"], json!(false));
        assert_eq!(out["items"][0]["ref"], json!("U1"));
        assert_eq!(out["items"][0]["pad"], json!("A4"));
        assert_eq!(out["items"][0]["x"], json!(8.75));
        assert_eq!(out["items"][0]["y"], json!(9.5));
        assert_eq!(out["items"][0]["layers"], json!(["F.Cu"]));
    }

    #[test]
    fn illegal_placement_error_says_nothing_was_written_and_gives_both_recoveries() {
        let message = illegal_placement_error(&json!({
            "current_bounds_mm": {"w": 45.0, "h": 30.0},
            "suggested_min_bounds_mm": {"w": 69.0, "h": 46.0},
        }));

        assert!(message.contains("no positions were written"));
        assert!(message.contains("smaller appropriate footprints"));
        assert!(message.contains("69 x 46 mm"));
    }

    #[test]
    fn real_palconn_usb_c_datum_locks_exactly_to_every_board_edge() {
        let Some(env) = KicadEnv::detect() else {
            eprintln!("SKIP: KiCad libraries not installed");
            return;
        };
        let catalog = FootprintCatalog::from_env(&env).expect("installed footprint catalog");
        let id = FootprintId::parse("Connector_USB:USB_C_Receptacle_Palconn_UTC16-G")
            .expect("valid footprint id");
        let footprint = catalog.footprint(&id).expect("installed Palconn footprint");
        let datum = footprint.pcb_edge_datum.expect("Palconn PCB Edge datum");
        assert!((datum.start.y - 4.34).abs() < 1e-9);
        assert!((datum.end.y - 4.34).abs() < 1e-9);

        let bounds = Rect::new(0.0, 0.0, 45.0, 30.0);
        let parts: Vec<Part> = (1..=4)
            .map(|n| {
                part_from_footprint_layers(&footprint, &format!("J{n}"), &BTreeMap::new(), 2, None)
            })
            .collect();
        let mut problem = PlaceProblem {
            bounds,
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts,
            keepouts: vec![],
            outline: None,
        };
        let refs = ["J1", "J2", "J3", "J4"].map(str::to_owned);
        pcb_place::placement::apply_edge_lock(&mut problem, &refs);

        let expected_edges = [
            pcb_place::placement::Edge::N,
            pcb_place::placement::Edge::E,
            pcb_place::placement::Edge::S,
            pcb_place::placement::Edge::W,
        ];
        let mut positions = Vec::new();
        let mut half = Vec::new();
        let mut copper = Vec::new();
        for (part, edge) in problem.parts.iter().zip(expected_edges) {
            let locked = part.locked.as_ref().expect("edge-locked connector");
            let placed_datum = part
                .edge_datum
                .expect("carried edge datum")
                .rotated(locked.rotation);
            let world_start = Point2::new(
                locked.at.x + placed_datum.start.x,
                locked.at.y + placed_datum.start.y,
            );
            let world_end = Point2::new(
                locked.at.x + placed_datum.end.x,
                locked.at.y + placed_datum.end.y,
            );
            let copper_box = pcb_model::place::rotated_copper_bbox(part, locked.rotation);
            let copper_center = Point2::new(
                locked.at.x + copper_box.center().x,
                locked.at.y + copper_box.center().y,
            );
            let datum_center = Point2::new(
                (world_start.x + world_end.x) / 2.0,
                (world_start.y + world_end.y) / 2.0,
            );
            match edge {
                pcb_place::placement::Edge::N => {
                    assert!((world_start.y - bounds.min_y).abs() < 1e-9);
                    assert!((world_end.y - bounds.min_y).abs() < 1e-9);
                    assert!(
                        copper_center.y > datum_center.y,
                        "north copper must point inward"
                    );
                }
                pcb_place::placement::Edge::S => {
                    assert!((world_start.y - bounds.max_y).abs() < 1e-9);
                    assert!((world_end.y - bounds.max_y).abs() < 1e-9);
                    assert!(
                        copper_center.y < datum_center.y,
                        "south copper must point inward"
                    );
                }
                pcb_place::placement::Edge::W => {
                    assert!((world_start.x - bounds.min_x).abs() < 1e-9);
                    assert!((world_end.x - bounds.min_x).abs() < 1e-9);
                    assert!(
                        copper_center.x > datum_center.x,
                        "west copper must point inward"
                    );
                }
                pcb_place::placement::Edge::E => {
                    assert!((world_start.x - bounds.max_x).abs() < 1e-9);
                    assert!((world_end.x - bounds.max_x).abs() < 1e-9);
                    assert!(
                        copper_center.x < datum_center.x,
                        "east copper must point inward"
                    );
                }
            }
            positions.push(locked.at);
            half.push(pcb_model::place::rotated_courtyard_half(
                part,
                locked.rotation,
            ));
            copper.push(copper_box);
        }
        assert!(
            pcb_model::place::is_legal(
                &problem,
                &half,
                &copper,
                pcb_model::place::courtyard_margin(problem.clearance),
                &positions,
            ),
            "datum-aligned connector bodies may overhang, but all pad copper must remain legal"
        );

        // Exercise the same top-level placement pipeline used by `place_board`,
        // not just the edge-lock helper. The oracle must prefer a mechanically
        // exact edge candidate over an otherwise-routable inboard connector.
        for part in &mut problem.parts {
            part.locked = None;
        }
        let hints = PlacementHints {
            edge_seek: refs.to_vec(),
            ..PlacementHints::default()
        };
        let result = pcb_place::placement::place_board(&problem, &hints);
        assert!(result.legal, "real Palconn auto-placement must be legal");
        let auto_positions: Vec<Point2> = result.placements.iter().map(|p| p.at).collect();
        let auto_half: Vec<_> = problem
            .parts
            .iter()
            .zip(&result.placements)
            .map(|(part, placed)| {
                let half = pcb_model::place::rotated_courtyard_half(part, placed.rotation);
                assert!(
                    pcb_model::place::part_edge_distance(
                        part,
                        placed.rotation,
                        placed.at,
                        &problem.bounds,
                        half,
                    ) < 1e-9,
                    "{} PCB Edge datum must not be left inboard",
                    part.reference,
                );
                half
            })
            .collect();
        let auto_copper: Vec<_> = problem
            .parts
            .iter()
            .zip(&result.placements)
            .map(|(part, placed)| pcb_model::place::rotated_copper_bbox(part, placed.rotation))
            .collect();
        assert!(pcb_model::place::is_legal(
            &problem,
            &auto_half,
            &auto_copper,
            pcb_model::place::courtyard_margin(problem.clearance),
            &auto_positions,
        ));
    }
}
