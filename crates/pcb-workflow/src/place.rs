//! Placement over the live KiCAD IPC board.

use circuit_graph::netclass::is_ground;
use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use geom::Rect;
use kicad_footprint::{Footprint, FootprintId, FootprintPad, PadTechnology};
use kicad_ipc::FootprintMove;
use pcb_model::{LayerRef, ViaSpan};
use pcb_place::{
    Edge, EdgeDatum, GroupHint, LockedAt, Part, PartPad, PlaceResult, Placement, PlacementHints,
    PlacementView,
};

use gordian_runtime::AgentRuntime;

use kicad_board::{ImportedPad, ImportedPart, IpcBoardSnapshot};

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
    let board = match crate::active_board(ctx) {
        Ok(board) => board,
        Err(err) => return Ok(json!({ "error": err })),
    };

    let net_pins = snapshot_net_pin_counts(&board);
    let nets: Vec<Value> = net_pins
        .iter()
        .map(|(name, &pins)| json!({ "name": name, "pins": pins }))
        .collect();

    let placed = !kicad_board::is_seed_imported_board(&board.imported);
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

/// Each part's COURTYARD extent (mm), by reference.
///
/// KiCAD's DRC checks courtyards, not pads, so a caller reasoning about whether
/// two parts may sit next to each other must use these. Rotation-aware: a part
/// turned a quarter-turn presents its courtyard the other way round. A part
/// whose footprint no longer resolves is simply absent.
pub(super) fn courtyard_extents(
    board: &IpcBoardSnapshot,
    ctx: &AgentRuntime,
) -> std::collections::BTreeMap<String, (f64, f64)> {
    let Ok(catalog) = ctx.footprint_catalog() else {
        return Default::default();
    };
    board
        .imported
        .parts
        .iter()
        .filter_map(|part| {
            let id = FootprintId::parse(&part.lib_id).ok()?;
            let footprint = catalog.footprint(&id).ok()?;
            let (w, h) = enclosing_courtyard(&footprint);
            let swapped = part.rotation.rem_euclid(180) == 90;
            Some((
                part.reference.clone(),
                if swapped { (h, w) } else { (w, h) },
            ))
        })
        .collect()
}

pub(super) fn place_problem_from_snapshot(
    board: &IpcBoardSnapshot,
    ctx: &AgentRuntime,
) -> std::result::Result<PlacementView, String> {
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
        let mut part = part_from_footprint_layers(
            &fp,
            &imported.reference,
            &pad_net_map(&imported.pads),
            board.problem.layer_count,
            imported.locked.then_some(LockedAt {
                at: imported.at,
                rotation: imported.rotation as f64,
            }),
        );
        // A many-padded IC needs routing channels around it, not just legal
        // courtyards: packed passives that satisfy the courtyard margin still
        // wall in its escapes and the router reports enclosure. Reserve
        // breathing room in the courtyard itself; connectors keep their tight
        // envelope so edge seating is unaffected. A FINE-PITCH IC needs a real
        // escape ring — its pins cannot exit between neighbouring pads, so
        // every escape crosses the courtyard boundary where a hugging cap
        // otherwise sits (measured: LQFP-48 boards strand 5-6 ring nets at any
        // canvas or layer count without this).
        if part.pads.len() >= 12 && !is_connector(&imported.lib_id, &imported.reference) {
            let mut pitch = f64::MAX;
            for (i, a) in part.pads.iter().enumerate() {
                for b in &part.pads[i + 1..] {
                    let d = a.offset.dist(b.offset);
                    if d > 1e-6 {
                        pitch = pitch.min(d);
                    }
                }
            }
            let extra = if pitch < 0.66 { 2.4 } else { 0.8 };
            part.courtyard_w += extra;
            part.courtyard_h += extra;
        }
        parts.push(part);
    }

    Ok(PlacementView {
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

fn placement_overlap_pairs(problem: &PlacementView, result: &PlaceResult) -> Vec<Value> {
    let margin = pcb_place::courtyard_margin(problem.clearance) / 2.0;
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
        let ah = pcb_place::rotated_courtyard_half(a, pa.rotation);
        let ar = Rect::from_center_half(pa.at, ah).inflate(margin);
        for b in problem.parts.iter().skip(i + 1) {
            let Some(pb) = positions.get(b.reference.as_str()) else {
                continue;
            };
            let bh = pcb_place::rotated_courtyard_half(b, pb.rotation);
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
        if let Some(value) = object.remove(tool_name)
            && object.insert(sdk_name.to_owned(), value).is_some()
        {
            return Err(format!(
                "use {tool_name}, not both snake_case and camelCase"
            ));
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
                    if let Some(value) = region.remove(tool_name)
                        && region.insert(sdk_name.to_owned(), value).is_some()
                    {
                        return Err(format!(
                            "use region.{tool_name}, not both snake_case and camelCase"
                        ));
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

#[derive(Debug)]
struct Opto817Channel {
    reference: String,
    input_nets: [String; 2],
    emitter_net: String,
    output_net: String,
}

#[derive(Debug, Clone, Copy)]
struct Opto817Requirements {
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, Copy)]
struct PlacementSizeEstimate {
    width: f64,
    height: f64,
    total_area: f64,
}

fn pin_net(component: &sch_check::model::Component, pin: &str) -> Option<String> {
    match component.pins.get(pin) {
        Some(sch_check::model::PinTarget::Net(net)) => Some(net.clone()),
        _ => None,
    }
}

fn same_net(a: &str, b: &str) -> bool {
    a.trim_start_matches('/') == b.trim_start_matches('/')
}

fn is_817(part: &str) -> bool {
    let part = part.to_ascii_uppercase();
    part.contains("PC817") || part.contains("LTV-817") || part.contains("LTV817")
}

fn live_schematic_design(ctx: &AgentRuntime) -> anyhow::Result<sch_check::model::Design> {
    let doc = sch_doc::SchDoc::read(ctx.sch_path())?;
    let netlist = sch_doc::connect::extract(&doc);
    let placed = sch_doc::placed_pins(&doc);
    let mut block = sch_check::model::Block::default();
    for symbol in doc.symbols() {
        let component = block
            .components
            .entry(symbol.refdes().to_string())
            .or_insert_with(|| sch_check::model::Component {
                part: symbol.lib_id.clone(),
                ..Default::default()
            });
        for pin in placed.iter().filter(|pin| pin.owner == symbol.uuid) {
            let target = netlist
                .nets
                .iter()
                .find(|net| {
                    net.pins.iter().any(|candidate| {
                        candidate.refdes == pin.refdes && candidate.pin == pin.number
                    })
                })
                .map(|net| sch_check::model::PinTarget::Net(net.name.clone()))
                .unwrap_or(sch_check::model::PinTarget::NoConnect);
            component.pins.insert(pin.number.clone(), target);
        }
    }
    let mut design = sch_check::model::Design::default();
    design.blocks.insert("main".to_string(), block);
    Ok(design)
}

/// Detect a real 817 bank from the live schematic. Besides the part
/// family, require the corrected common-emitter topology (3=ground, 4=output),
/// then take the physical domains from the actual numbered board pads. KiCad
/// may legitimately rename private authored nets (for example to
/// `Net-(R1-Pad2)`), so authored-to-board net-name equality is not an identity
/// check. The narrow part predicate and corrected draft topology keep this plan
/// off unrelated four-pad devices and electrically reversed schematics.
fn opto817_channels(
    design: &sch_check::model::Design,
    board: &IpcBoardSnapshot,
) -> Vec<Opto817Channel> {
    let imported: BTreeMap<&str, &ImportedPart> = board
        .imported
        .parts
        .iter()
        .map(|part| (part.reference.as_str(), part))
        .collect();
    let mut channels = Vec::new();
    for block in design.blocks.values() {
        for (reference, component) in &block.components {
            if !is_817(&component.part) {
                continue;
            }
            let (Some(_anode), Some(_cathode), Some(authored_emitter), Some(authored_output)) = (
                pin_net(component, "1"),
                pin_net(component, "2"),
                pin_net(component, "3"),
                pin_net(component, "4"),
            ) else {
                continue;
            };
            if !is_ground(&authored_emitter) || is_ground(&authored_output) {
                continue;
            }
            let Some(part) = imported.get(reference.as_str()) else {
                continue;
            };
            let actual_net = |number: &str| {
                part.pads
                    .iter()
                    .find(|pad| pad.number == number)
                    .and_then(|pad| pad.net.as_deref())
                    .map(|net| net.trim_start_matches('/').to_owned())
            };
            let (Some(anode), Some(cathode), Some(emitter), Some(output)) = (
                actual_net("1"),
                actual_net("2"),
                actual_net("3"),
                actual_net("4"),
            ) else {
                continue;
            };
            channels.push(Opto817Channel {
                reference: reference.clone(),
                input_nets: [anode, cathode],
                emitter_net: emitter,
                output_net: output,
            });
        }
    }
    channels.sort_by(|a, b| natural_ref_key(&a.reference).cmp(&natural_ref_key(&b.reference)));
    channels
}

fn natural_ref_key(reference: &str) -> (&str, u32) {
    let split = reference
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(reference.len());
    let (prefix, suffix) = reference.split_at(split);
    (prefix, suffix.parse().unwrap_or(u32::MAX))
}

fn part_nets(part: &ImportedPart) -> Vec<&str> {
    part.pads
        .iter()
        .filter_map(|pad| pad.net.as_deref())
        .collect()
}

fn intersects(nets: &[&str], domain: &std::collections::BTreeSet<String>) -> usize {
    nets.iter()
        .filter(|net| domain.iter().any(|candidate| same_net(net, candidate)))
        .count()
}

/// Pick the quadrant rotation that puts the actual pads carrying pins 1/2 above
/// the actual pads carrying pins 3/4. The footprint library, not a hard-coded
/// SO-4 orientation, owns this decision.
fn opto_pad_bank_centroid_y(part: &Part, nets: &[&str], rotation: f64) -> Option<f64> {
    let points = part
        .pads
        .iter()
        .filter(|pad| {
            pad.net
                .as_deref()
                .is_some_and(|net| nets.iter().any(|candidate| same_net(net, candidate)))
        })
        .map(|pad| pad.offset.rotate(rotation))
        .collect::<Vec<_>>();
    (!points.is_empty())
        .then(|| points.iter().map(|point| point.y).sum::<f64>() / points.len() as f64)
}

fn opto_input_above_rotation(part: &Part, channel: &Opto817Channel) -> Option<f64> {
    let input = [&*channel.input_nets[0], &*channel.input_nets[1]];
    let output = [&*channel.emitter_net, &*channel.output_net];
    [0.0, 90.0, 180.0, 270.0]
        .into_iter()
        .filter_map(|rotation| {
            Some((
                rotation,
                opto_pad_bank_centroid_y(part, &output, rotation)?
                    - opto_pad_bank_centroid_y(part, &input, rotation)?,
            ))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(rotation, _)| rotation)
}

fn opto_bridge_midpoint_y(part: &Part, channel: &Opto817Channel, rotation: f64) -> Option<f64> {
    let input = [&*channel.input_nets[0], &*channel.input_nets[1]];
    let output = [&*channel.emitter_net, &*channel.output_net];
    Some(
        (opto_pad_bank_centroid_y(part, &input, rotation)?
            + opto_pad_bank_centroid_y(part, &output, rotation)?)
            / 2.0,
    )
}

/// Add the narrow, deterministic physical grammar for a repeated 817 isolation
/// bank. Returns `None` without changing hints unless at least eight verified
/// channels are present, preserving generic placement on every other board.
fn add_817_array_hints(
    design: &sch_check::model::Design,
    board: &IpcBoardSnapshot,
    problem: &PlacementView,
    hints: &mut PlacementHints,
) -> Option<Opto817Requirements> {
    let authored_regioned = hints
        .groups
        .iter()
        .filter(|group| group.region.is_some())
        .flat_map(|group| group.members.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    let channels = opto817_channels(design, board);
    if channels.len() < 8 {
        return None;
    }
    let first_part = problem
        .parts
        .iter()
        .find(|part| part.reference == channels[0].reference)?;
    let rotation = opto_input_above_rotation(first_part, &channels[0])?;
    if channels.iter().any(|channel| {
        problem
            .parts
            .iter()
            .find(|part| part.reference == channel.reference)
            .and_then(|part| opto_input_above_rotation(part, channel))
            != Some(rotation)
    }) {
        return None;
    }
    let bridge_midpoint_y = opto_bridge_midpoint_y(first_part, &channels[0], rotation)?;
    if channels.iter().any(|channel| {
        problem
            .parts
            .iter()
            .find(|part| part.reference == channel.reference)
            .and_then(|part| opto_bridge_midpoint_y(part, channel, rotation))
            .is_none_or(|midpoint| (midpoint - bridge_midpoint_y).abs() > geom::EPS)
    }) {
        return None;
    }

    let opto_refs = channels
        .iter()
        .map(|channel| channel.reference.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let input_domain = channels
        .iter()
        .flat_map(|channel| channel.input_nets.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    let mut output_domain = channels
        .iter()
        .flat_map(|channel| [channel.emitter_net.clone(), channel.output_net.clone()])
        .collect::<std::collections::BTreeSet<_>>();

    let mut field_connectors = Vec::new();
    let mut field_aux_connectors = Vec::new();
    let mut logic_connectors = Vec::new();
    let mut logic_aux_connectors = Vec::new();
    for imported in &board.imported.parts {
        if !is_connector(&imported.lib_id, &imported.reference)
            || is_mounting_hole(&imported.lib_id)
        {
            continue;
        }
        let nets = part_nets(imported);
        let (field, logic) = (
            intersects(&nets, &input_domain),
            intersects(&nets, &output_domain),
        );
        if field > logic && field >= 2 {
            field_connectors.push(imported.reference.clone());
        } else if logic > field && logic >= 2 {
            logic_connectors.push(imported.reference.clone());
        }
    }
    // Learn shared logic rails only after the strong signal-bearing headers are
    // fixed. Mutating this domain during the scan makes classification depend on
    // board part order and can incorrectly promote a later GND/VCC-only header.
    for reference in &logic_connectors {
        if let Some(imported) = board
            .imported
            .parts
            .iter()
            .find(|part| &part.reference == reference)
        {
            output_domain.extend(part_nets(imported).into_iter().map(str::to_owned));
        }
    }
    // A separate logic power connector may only expose GND/VCC; after learning
    // those rails from the strong logic headers, place it on the logic edge too.
    for imported in &board.imported.parts {
        if !is_connector(&imported.lib_id, &imported.reference)
            || is_mounting_hole(&imported.lib_id)
            || field_connectors.contains(&imported.reference)
            || logic_connectors.contains(&imported.reference)
        {
            continue;
        }
        let nets = part_nets(imported);
        let (field, logic) = (
            intersects(&nets, &input_domain),
            intersects(&nets, &output_domain),
        );
        if logic > field {
            logic_aux_connectors.push(imported.reference.clone());
        } else if field > logic {
            field_aux_connectors.push(imported.reference.clone());
        }
    }
    field_connectors.sort_by(|a, b| natural_ref_key(a).cmp(&natural_ref_key(b)));
    field_aux_connectors.sort_by(|a, b| natural_ref_key(a).cmp(&natural_ref_key(b)));
    logic_connectors.sort_by(|a, b| natural_ref_key(a).cmp(&natural_ref_key(b)));
    logic_aux_connectors.sort_by(|a, b| natural_ref_key(a).cmp(&natural_ref_key(b)));

    let margin = problem.clearance.max(0.5) + 0.75;
    let bounds = problem.bounds;
    let (mut opto_w, mut opto_h): (f64, f64) = (0.0, 0.0);
    for reference in &opto_refs {
        let part = problem
            .parts
            .iter()
            .find(|part| &part.reference == reference)
            .expect("verified board part");
        let half = pcb_place::rotated_courtyard_half(part, rotation);
        opto_w = opto_w.max(half.0 * 2.0);
        opto_h = opto_h.max(half.1 * 2.0);
    }
    let pitch_x = opto_w + margin;
    // Every package must bridge ONE continuous corridor. Therefore this is one
    // horizontal row, never a compact-looking second row on a board that is too
    // narrow. The explicit dimensional gate below rejects that board before the
    // generic grid code can wrap the bank into multiple rows.
    let array_w = channels.len() as f64 * pitch_x;
    // `apply_grid_hints` derives its column count from region aspect. A region
    // with width `n*pitch` and height `pitch` selects exactly n columns, while
    // the actual rotated courtyard height is reserved separately below.
    let array_h = pitch_x;
    let center = geom::Point2::new(
        (bounds.min_x + bounds.max_x) / 2.0,
        (bounds.min_y + bounds.max_y) / 2.0,
    );
    let center_region = Rect::new(
        center.x - array_w / 2.0,
        center.y - bridge_midpoint_y - array_h / 2.0,
        center.x + array_w / 2.0,
        center.y - bridge_midpoint_y + array_h / 2.0,
    );
    hints.groups.push(GroupHint {
        name: "817 isolation array".into(),
        members: channels
            .iter()
            .map(|channel| channel.reference.clone())
            .collect(),
        region: Some(center_region),
        edge: None,
        grid: true,
        rotation: Some(rotation),
        surround: None,
    });

    let top = Rect::new(
        bounds.min_x + margin,
        bounds.min_y + margin,
        bounds.max_x - margin,
        (center.y - opto_h / 2.0 - margin).max(bounds.min_y + margin),
    );
    let bottom = Rect::new(
        bounds.min_x + margin,
        (center.y + opto_h / 2.0 + margin).min(bounds.max_y - margin),
        bounds.max_x - margin,
        bounds.max_y - margin,
    );
    let connector_refs = field_connectors
        .iter()
        .chain(&field_aux_connectors)
        .chain(&logic_connectors)
        .chain(&logic_aux_connectors)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let eligible_channel_part = |part: &ImportedPart| {
        !opto_refs.contains(&part.reference)
            && !connector_refs.contains(&part.reference)
            && !is_mounting_hole(&part.lib_id)
            && !part.locked
    };
    let mut net_fanout = BTreeMap::<String, usize>::new();
    for imported in board
        .imported
        .parts
        .iter()
        .filter(|part| eligible_channel_part(part))
    {
        let nets = part_nets(imported)
            .into_iter()
            .map(|net| net.trim_start_matches('/').to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        for net in nets {
            *net_fanout.entry(net).or_default() += 1;
        }
    }
    let mut channel_field_groups = Vec::with_capacity(channels.len());
    let mut channel_logic_groups = Vec::with_capacity(channels.len());
    let mut channel_assigned = std::collections::BTreeSet::new();
    for channel in &channels {
        let mut field = board
            .imported
            .parts
            .iter()
            .filter(|part| eligible_channel_part(part))
            .filter(|part| {
                channels
                    .iter()
                    .filter(|candidate| {
                        part_nets(part)
                            .iter()
                            .any(|net| same_net(net, &candidate.input_nets[0]))
                    })
                    .count()
                    == 1
            })
            .filter(|part| {
                part_nets(part)
                    .iter()
                    .any(|net| same_net(net, &channel.input_nets[0]))
            })
            .map(|part| part.reference.clone())
            .collect::<Vec<_>>();
        let direct_logic = board
            .imported
            .parts
            .iter()
            .filter(|part| eligible_channel_part(part))
            .filter(|part| {
                channels
                    .iter()
                    .filter(|candidate| {
                        part_nets(part)
                            .iter()
                            .any(|net| same_net(net, &candidate.output_net))
                    })
                    .count()
                    == 1
            })
            .filter(|part| {
                part_nets(part)
                    .iter()
                    .any(|net| same_net(net, &channel.output_net))
            })
            .collect::<Vec<_>>();
        let expansion_nets = direct_logic
            .iter()
            .flat_map(|part| part_nets(part))
            .map(|net| net.trim_start_matches('/').to_owned())
            .filter(|net| !same_net(net, &channel.output_net))
            .filter(|net| !is_ground(net))
            .filter(|net| net_fanout.get(net).copied().unwrap_or(usize::MAX) <= 3)
            .collect::<std::collections::BTreeSet<_>>();
        let direct_refs = direct_logic
            .iter()
            .map(|part| part.reference.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let mut logic = board
            .imported
            .parts
            .iter()
            .filter(|part| eligible_channel_part(part))
            .filter(|part| {
                direct_refs.contains(part.reference.as_str())
                    || part_nets(part)
                        .iter()
                        .any(|net| expansion_nets.contains(net.trim_start_matches('/')))
            })
            .map(|part| part.reference.clone())
            .collect::<Vec<_>>();
        field.sort_by(|a, b| natural_ref_key(a).cmp(&natural_ref_key(b)));
        logic.sort_by(|a, b| natural_ref_key(a).cmp(&natural_ref_key(b)));
        field.dedup();
        logic.dedup();
        logic.retain(|reference| !field.contains(reference));
        channel_assigned.extend(field.iter().chain(&logic).cloned());
        channel_field_groups.push(field);
        channel_logic_groups.push(logic);
    }
    let mut field_parts = Vec::new();
    let mut logic_parts = Vec::new();
    for imported in &board.imported.parts {
        if opto_refs.contains(&imported.reference)
            || connector_refs.contains(&imported.reference)
            || is_mounting_hole(&imported.lib_id)
            || imported.locked
            || channel_assigned.contains(&imported.reference)
        {
            continue;
        }
        let nets = part_nets(imported);
        let (field, logic) = (
            intersects(&nets, &input_domain),
            intersects(&nets, &output_domain),
        );
        if field > logic {
            field_parts.push(imported.reference.clone());
        } else if logic > field {
            logic_parts.push(imported.reference.clone());
        }
    }
    field_parts.sort_by(|a, b| natural_ref_key(a).cmp(&natural_ref_key(b)));
    logic_parts.sort_by(|a, b| natural_ref_key(a).cmp(&natural_ref_key(b)));
    let logic_cell_width = (bottom.max_x - bottom.min_x) / logic_parts.len().max(1) as f64;
    let (logic_wide_parts, logic_small_parts): (Vec<_>, Vec<_>) =
        logic_parts.into_iter().partition(|reference| {
            problem
                .parts
                .iter()
                .find(|part| &part.reference == reference)
                .is_some_and(|part| part.courtyard_w + margin > logic_cell_width)
        });

    // The field series parts share the opto x cells in one row. This removes
    // the last force/anneal degree of freedom and keeps each input fanout local.
    let field_strip = Rect::new(
        center_region.min_x,
        top.max_y - array_h,
        center_region.max_x,
        top.max_y,
    );
    let field_aux_height = field_parts
        .iter()
        .filter_map(|reference| {
            problem
                .parts
                .iter()
                .find(|part| &part.reference == reference)
        })
        .map(|part| part.courtyard_h)
        .fold(0.0, f64::max)
        + (!field_parts.is_empty() as u8 as f64) * margin;
    let field_aux_top = (field_strip.min_y - field_aux_height).max(top.min_y);
    let field_aux_strip = Rect::new(
        center_region.min_x,
        (field_aux_top + margin).min(field_strip.min_y),
        center_region.max_x,
        field_strip.min_y,
    );
    let field_connector_aux_height = field_aux_connectors
        .iter()
        .filter_map(|reference| {
            problem
                .parts
                .iter()
                .find(|part| &part.reference == reference)
        })
        .map(|part| pcb_place::rotated_courtyard_half(part, 90.0).1 * 2.0)
        .fold(0.0, f64::max);
    let field_connector_aux_strip = Rect::new(
        center_region.min_x,
        (field_aux_top - field_connector_aux_height - margin).max(top.min_y),
        center_region.max_x,
        field_aux_top,
    );
    let connector_top = Rect::new(
        center_region.min_x,
        top.min_y,
        center_region.max_x,
        field_connector_aux_strip.min_y,
    );
    let part_height = |references: &[String]| {
        references
            .iter()
            .filter_map(|reference| {
                problem
                    .parts
                    .iter()
                    .find(|part| &part.reference == reference)
            })
            .map(|part| part.courtyard_h)
            .fold(0.0, f64::max)
    };
    let wide_logic_height = part_height(&logic_wide_parts);
    let logic_height = part_height(&logic_small_parts);
    let channel_logic_part_height = channel_logic_groups
        .iter()
        .flatten()
        .filter_map(|reference| {
            problem
                .parts
                .iter()
                .find(|part| &part.reference == reference)
        })
        .map(|part| part.courtyard_h)
        .fold(0.0, f64::max);
    // Three small support parts in a short, wide channel cell tile as two
    // columns. Their courtyards remain legal, but the long DLED/RLED reference
    // fields nearly touch across adjacent channels. A taller cell makes the
    // regular-grid aspect calculation choose one column and three rows, giving
    // each reference its own horizontal lane without special-casing refdes.
    let channel_logic_height = if channel_logic_part_height > 0.0 {
        (channel_logic_part_height * 2.0 + margin * 2.0).max(pitch_x * 1.5)
    } else {
        0.0
    };
    let channel_logic_strip = Rect::new(
        bottom.min_x,
        bottom.min_y,
        bottom.max_x,
        (bottom.min_y + channel_logic_height).min(bottom.max_y),
    );
    let wide_logic_strip = Rect::new(
        bottom.min_x,
        channel_logic_strip.max_y,
        bottom.max_x,
        (channel_logic_strip.max_y
            + wide_logic_height
            + (!logic_wide_parts.is_empty() as u8 as f64) * margin)
            .min(bottom.max_y),
    );
    let logic_strip = Rect::new(
        bottom.min_x,
        wide_logic_strip.max_y,
        bottom.max_x,
        (wide_logic_strip.max_y
            + logic_height
            + (!logic_small_parts.is_empty() as u8 as f64) * margin)
            .min(bottom.max_y),
    );
    let aux_height = logic_aux_connectors
        .iter()
        .filter_map(|reference| {
            problem
                .parts
                .iter()
                .find(|part| &part.reference == reference)
        })
        .map(|part| pcb_place::rotated_courtyard_half(part, 90.0).1 * 2.0)
        .fold(0.0, f64::max);
    let aux_bottom = Rect::new(
        bottom.min_x,
        logic_strip.max_y,
        bottom.max_x,
        (logic_strip.max_y + aux_height + margin).min(bottom.max_y),
    );
    let header_bottom = Rect::new(bottom.min_x, aux_bottom.max_y, bottom.max_x, bottom.max_y);
    let rotated_depth = |references: &[String]| {
        references
            .iter()
            .filter_map(|reference| {
                problem
                    .parts
                    .iter()
                    .find(|part| &part.reference == reference)
            })
            .map(|part| pcb_place::rotated_courtyard_half(part, 90.0).1 * 2.0)
            .fold(0.0, f64::max)
    };
    let top_depth = margin
        + array_h
        + field_aux_height
        + field_connector_aux_height
        + (!field_aux_connectors.is_empty() as u8 as f64) * margin
        + rotated_depth(&field_connectors)
        + margin;
    let bottom_depth = margin
        + channel_logic_height
        + wide_logic_height
        + (!logic_wide_parts.is_empty() as u8 as f64) * margin
        + logic_height
        + (!logic_small_parts.is_empty() as u8 as f64) * margin
        + aux_height
        + (!logic_aux_connectors.is_empty() as u8 as f64) * margin
        + rotated_depth(&logic_connectors)
        + margin;
    let mut add_group = |name: &str,
                         members: Vec<String>,
                         region: Rect,
                         edge: Option<Edge>,
                         grid: bool,
                         rotation: Option<f64>| {
        if !members.is_empty() {
            hints.groups.push(GroupHint {
                name: name.into(),
                members,
                region: Some(region),
                edge,
                grid,
                rotation,
                surround: None,
            });
        }
    };
    add_group(
        "field connectors",
        field_connectors,
        connector_top,
        Some(Edge::N),
        true,
        Some(90.0),
    );
    add_group(
        "field auxiliary connectors",
        field_aux_connectors,
        field_connector_aux_strip,
        None,
        true,
        Some(90.0),
    );
    add_group(
        "logic connectors",
        logic_connectors,
        header_bottom,
        Some(Edge::S),
        true,
        Some(90.0),
    );
    add_group(
        "logic auxiliary connectors",
        logic_aux_connectors,
        aux_bottom,
        None,
        true,
        Some(90.0),
    );
    for (index, members) in channel_field_groups.into_iter().enumerate() {
        add_group(
            &format!("817 field channel {}", index + 1),
            members,
            Rect::new(
                center_region.min_x + index as f64 * pitch_x,
                field_strip.min_y,
                center_region.min_x + (index + 1) as f64 * pitch_x,
                field_strip.max_y,
            ),
            None,
            true,
            None,
        );
    }
    for (index, members) in channel_logic_groups.into_iter().enumerate() {
        add_group(
            &format!("817 logic channel {}", index + 1),
            members,
            Rect::new(
                center_region.min_x + index as f64 * pitch_x,
                channel_logic_strip.min_y,
                center_region.min_x + (index + 1) as f64 * pitch_x,
                channel_logic_strip.max_y,
            ),
            None,
            true,
            None,
        );
    }
    add_group(
        "field domain",
        field_parts,
        field_aux_strip,
        None,
        true,
        None,
    );
    add_group(
        "logic wide domain",
        logic_wide_parts,
        wide_logic_strip,
        None,
        true,
        None,
    );
    add_group(
        "logic domain",
        logic_small_parts,
        logic_strip,
        None,
        true,
        None,
    );

    // Finish the specialized floorplan by pinning un-authored mounting holes to
    // deterministic, maximally separated corners. Authored locks/regions win.
    let mut holes = board
        .imported
        .parts
        .iter()
        .filter(|part| {
            is_mounting_hole(&part.lib_id)
                && !part.locked
                && !authored_regioned.contains(&part.reference)
        })
        .map(|part| part.reference.clone())
        .collect::<Vec<_>>();
    holes.sort_by(|a, b| natural_ref_key(a).cmp(&natural_ref_key(b)));
    // The wide south logic headers consume both lower corners, so specialized
    // boards use the two clear top corners first.
    let corners = [(false, false), (true, false), (true, true), (false, true)];
    for (reference, (right, bottom_edge)) in holes.into_iter().take(4).zip(corners) {
        let part = problem
            .parts
            .iter()
            .find(|part| part.reference == reference)
            .expect("imported mounting hole has placement part");
        let center = geom::Point2::new(
            if right {
                bounds.max_x - part.courtyard_w / 2.0 - EDGE_CLEAR_MM
            } else {
                bounds.min_x + part.courtyard_w / 2.0 + EDGE_CLEAR_MM
            },
            if bottom_edge {
                bounds.max_y - part.courtyard_h / 2.0 - EDGE_CLEAR_MM
            } else {
                bounds.min_y + part.courtyard_h / 2.0 + EDGE_CLEAR_MM
            },
        );
        add_group(
            &format!("817 mounting {reference}"),
            vec![reference],
            Rect::new(
                center.x - 0.5,
                center.y - 0.5,
                center.x + 0.5,
                center.y + 0.5,
            ),
            None,
            true,
            None,
        );
    }

    // A single row is electrically meaningful: every isolator straddles the
    // same continuous copper-free corridor. Reject a smaller board explicitly
    // instead of allowing the generic grid code to silently wrap into two rows.
    Some(Opto817Requirements {
        width: array_w + 2.0 * margin,
        height: opto_h + top_depth + bottom_depth,
    })
}

fn undersized_817_result(problem: &PlacementView, required: Opto817Requirements) -> Value {
    let current_w = (problem.bounds.max_x - problem.bounds.min_x).max(0.0);
    let current_h = (problem.bounds.max_y - problem.bounds.min_y).max(0.0);
    let estimate = placement_size_estimate(problem, required.width, required.height, true);
    let mut out = json!({
        "placement_applied": false,
        "legal": false,
        "hpwl": 0.0,
        "overlaps_resolved": 0,
        "out_of_bounds_clamps": 0,
        "positions": [],
        "current_bounds_mm": {
            "w": (current_w * 10.0).round() / 10.0,
            "h": (current_h * 10.0).round() / 10.0,
        },
        "suggested_min_bounds_mm": {
            "w": estimate.width.ceil(),
            "h": estimate.height.ceil(),
        },
        "parts_courtyard_area_mm2": (estimate.total_area * 10.0).round() / 10.0,
        "note": "placement is NOT legal, so no footprint positions were written and the board remains at its previous positions. The verified 817 isolation bank requires one continuous single-row barrier; regenerate_board with at least suggested_min_bounds_mm, then run place_board once.",
    });
    out["error"] = Value::String(illegal_placement_error(&out));
    out
}

fn mirror_right_817_bank(problem: &mut PlacementView, hints: &PlacementHints) {
    let center_x = problem.bounds.center().x;
    for group in &hints.groups {
        let mirrored_rotation = match group.name.as_str() {
            "field connectors" | "logic connectors" => 270.0,
            "logic wide domain" => 180.0,
            _ => continue,
        };
        for reference in &group.members {
            let Some(part) = problem
                .parts
                .iter_mut()
                .find(|part| &part.reference == reference)
            else {
                continue;
            };
            let Some(locked) = part.locked.as_mut() else {
                continue;
            };
            if locked.at.x > center_x {
                locked.rotation = mirrored_rotation;
            }
        }
    }
}

fn align_817_field_connector_datums(problem: &mut PlacementView, hints: &PlacementHints) {
    let Some(group) = hints
        .groups
        .iter()
        .find(|group| group.name == "field connectors")
    else {
        return;
    };
    let connector_refs = group
        .members
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut targets = BTreeMap::<String, Vec<f64>>::new();
    for part in &problem.parts {
        if connector_refs.contains(&part.reference) {
            continue;
        }
        let Some(locked) = &part.locked else { continue };
        for pad in &part.pads {
            let Some(net) = &pad.net else { continue };
            targets
                .entry(net.clone())
                .or_default()
                .push(locked.at.x + pad.offset.rotate(locked.rotation).x);
        }
    }
    for reference in &group.members {
        let Some(part) = problem
            .parts
            .iter_mut()
            .find(|part| &part.reference == reference)
        else {
            continue;
        };
        let Some(locked) = part.locked.as_mut() else {
            continue;
        };
        let mut desired_origins = Vec::new();
        for pad in &part.pads {
            let Some(net_targets) = pad.net.as_ref().and_then(|net| targets.get(net)) else {
                continue;
            };
            let target_x = net_targets.iter().sum::<f64>() / net_targets.len() as f64;
            desired_origins.push(target_x - pad.offset.rotate(locked.rotation).x);
        }
        if desired_origins.len() < 4 {
            continue;
        }
        let min_x = part
            .pads
            .iter()
            .map(|pad| pad.offset.x - pad.width / 2.0)
            .fold(f64::INFINITY, f64::min);
        let max_x = part
            .pads
            .iter()
            .map(|pad| pad.offset.x + pad.width / 2.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = part
            .pads
            .iter()
            .map(|pad| pad.offset.y - pad.height / 2.0)
            .fold(f64::INFINITY, f64::min);
        let max_y = part
            .pads
            .iter()
            .map(|pad| pad.offset.y + pad.height / 2.0)
            .fold(f64::NEG_INFINITY, f64::max);
        locked.at.x = desired_origins.iter().sum::<f64>() / desired_origins.len() as f64;
        // These pin headers use pin 1 as their footprint origin. The generic
        // placement model conservatively mirrors that asymmetric courtyard,
        // which would reject the electrically aligned origin as off-board.
        // For this fully prescribed top-edge bank, use its actual pad span plus
        // a 0.5 mm border on each side as the legality envelope.
        part.courtyard_w = max_x - min_x + 1.0;
        part.courtyard_h = max_y - min_y + 1.0;
    }
}

/// A connector is only useful if a cable can reach it. The placer edge-seeks
/// them, but a crowded board can still strand one in the interior — and a legal
/// placement says nothing about that, so the caller never learns why the render
/// looks wrong. Names each connector left more than its own width from any edge,
/// with the `move_parts` call that seats it.
fn connectors_off_edge(
    problem: &PlacementView,
    imported: &[kicad_board::ImportedPart],
    result: &pcb_place::PlaceResult,
) -> Vec<Value> {
    let placed: std::collections::BTreeMap<&str, _> = result
        .placements
        .iter()
        .map(|placement| (placement.reference.as_str(), placement))
        .collect();
    let mut out = Vec::new();
    for part in &problem.parts {
        let lib_id = imported
            .iter()
            .find(|p| p.reference == part.reference)
            .map_or("", |p| p.lib_id.as_str());
        if !is_connector(lib_id, &part.reference) {
            continue;
        }
        let Some(placement) = placed.get(part.reference.as_str()) else {
            continue;
        };
        let half = pcb_place::rotated_courtyard_half(part, placement.rotation);
        let rect = Rect::from_center_half(placement.at, half);
        let gaps = [
            ("left", rect.min_x - problem.bounds.min_x),
            ("right", problem.bounds.max_x - rect.max_x),
            ("top", rect.min_y - problem.bounds.min_y),
            ("bottom", problem.bounds.max_y - rect.max_y),
        ];
        let (edge, gap) = gaps
            .into_iter()
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .expect("four edges");
        // Its own width of clear board behind it means it is not on the rim.
        if gap <= rect.width().min(rect.height()) {
            continue;
        }
        out.push(json!({
            "reference": part.reference,
            "nearest_edge": edge,
            "gap_to_edge_mm": (gap * 10.0).round() / 10.0,
            "fix": format!(
                "move_parts {{\"moves\":[{{\"reference\":\"{}\",\"edge\":\"{edge}\",\"gap\":0.5}}]}}",
                part.reference
            ),
        }));
    }
    out
}

/// The board this placement's parts require, in [`crate::sizing`]'s terms — the
/// same law `regenerate_board` sizes an auto board with, so both tools quote one
/// pair of numbers.
fn board_sizing(
    problem: &PlacementView,
    imported: &[kicad_board::ImportedPart],
    routing: &pcb_model::RoutingView,
) -> crate::sizing::BoardSizing {
    let lib_id = |reference: &str| {
        imported
            .iter()
            .find(|part| part.reference == reference)
            .map_or("", |part| part.lib_id.as_str())
    };
    let extents: Vec<_> = problem
        .parts
        .iter()
        .map(|part| crate::sizing::PartExtent {
            w: part.courtyard_w,
            h: part.courtyard_h,
            edge_seeking: is_connector(lib_id(&part.reference), &part.reference),
        })
        .collect();
    let w = (problem.bounds.max_x - problem.bounds.min_x).max(0.1);
    let h = (problem.bounds.max_y - problem.bounds.min_y).max(0.1);
    crate::sizing::size_board(
        &extents,
        w / h,
        crate::sizing::RoutingDemand {
            clearance: routing.clearance,
            track_width: routing.min_trace_width,
            layer_count: routing.layer_count,
            net_count: routing.connections.len(),
        },
    )
}

/// Estimate a one-retry board size from packing area and the largest footprint.
/// The 817 plan requests a landscape result because its continuous horizontal
/// isolation row is the dominant shape; generic failures preserve the caller's
/// aspect ratio exactly as before.
fn placement_size_estimate(
    problem: &PlacementView,
    minimum_width: f64,
    minimum_height: f64,
    landscape: bool,
) -> PlacementSizeEstimate {
    placement_size_estimate_with_growth(problem, minimum_width, minimum_height, landscape, true)
}

/// `grow_from_current=false` sizes a fresh canvas purely from the parts —
/// used to shrink an oversized-but-legal board back to its packing estimate.
fn placement_size_estimate_with_growth(
    problem: &PlacementView,
    minimum_width: f64,
    minimum_height: f64,
    landscape: bool,
    grow_from_current: bool,
) -> PlacementSizeEstimate {
    let total_area: f64 = problem
        .parts
        .iter()
        .map(|part| part.courtyard_w * part.courtyard_h)
        .sum();
    let max_w = problem
        .parts
        .iter()
        .map(|part| part.courtyard_w)
        .fold(0.0, f64::max);
    let max_h = problem
        .parts
        .iter()
        .map(|part| part.courtyard_h)
        .fold(0.0, f64::max);
    let current_w = (problem.bounds.max_x - problem.bounds.min_x).max(0.1);
    let current_h = (problem.bounds.max_y - problem.bounds.min_y).max(0.1);
    // ~2x courtyard area reserves packing and routing space. Growing at least
    // 1.3x beyond a failed/current outline prevents identical retry loops.
    let min_area = if grow_from_current {
        (total_area * 2.0).max(current_w * current_h * 1.3)
    } else {
        total_area * 2.0
    };
    let aspect = if landscape {
        2.0
    } else {
        current_w / current_h
    };
    let mut height = (min_area / aspect).sqrt();
    let mut width = aspect * height;
    width = width.max(max_w + 2.0).max(minimum_width);
    height = height.max(max_h + 2.0).max(minimum_height);
    if grow_from_current {
        width = width.max(current_w);
        height = height.max(current_h);
    }
    PlacementSizeEstimate {
        width,
        height,
        total_area,
    }
}

fn placement_existing_copper_error(tracks: usize, vias: usize) -> Option<Value> {
    (tracks > 0 || vias > 0).then(|| {
        json!({
            "error": "automatic placement requires a copper-free board",
            "code": "placement_requires_copper_free_board",
            "placement_applied": false,
            "tracks": tracks,
            "vias": vias,
            "note": "Footprints were not moved. Preserve the existing route, or explicitly delete all tracks and vias before running place_board.",
        })
    })
}

#[tracing::instrument(skip_all, fields(project = %ctx.project_dir().display()))]
pub fn place_board(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let board = match crate::active_board(ctx) {
        Ok(board) => board,
        Err(live_err) => return Ok(json!({ "error": live_err })),
    };
    if let Some(error) =
        placement_existing_copper_error(board.copper.traces.len(), board.copper.vias.len())
    {
        return Ok(error);
    }

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

    // A repeated phototransistor-isolator bank is a physical grammar the generic
    // net-force placer cannot infer from a bag of footprints: keep the barrier in
    // the centre, field copper above, and logic copper below.
    let opto817_requirements = live_schematic_design(ctx)
        .ok()
        .and_then(|design| add_817_array_hints(&design, &board, &problem, &mut hints));
    if let Some(required) = opto817_requirements {
        let board_w = problem.bounds.max_x - problem.bounds.min_x;
        let board_h = problem.bounds.max_y - problem.bounds.min_y;
        if board_w + 1e-9 < required.width || board_h + 1e-9 < required.height {
            return Ok(undersized_817_result(&problem, required));
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
    // A fully prescribed 817 floorplan has no optimization choices. Bypass the
    // multi-candidate routing oracle when every part is locked by the structured
    // grids; this makes placement independent of RNG/hash order and completes in
    // one legality pass. Any incomplete specialization keeps the generic path.
    let mut result = if opto817_requirements.is_some() {
        let mut prescribed = problem.clone();
        pcb_place::placement::apply_grid_hints(&mut prescribed, &hints);
        mirror_right_817_bank(&mut prescribed, &hints);
        align_817_field_connector_datums(&mut prescribed, &hints);
        if prescribed.parts.iter().all(|part| part.locked.is_some()) {
            pcb_engine::place_prescribed(&prescribed, &PlacementHints::default())
        } else {
            pcb_engine::place_tuned(&problem, &hints)
        }
    } else {
        pcb_engine::place_tuned(&problem, &hints)
    };
    // The 817 grammar prescribes strips sized for its canonical channel shape;
    // a richer channel (series R + TVS + status LED per input) can overflow
    // them with sub-millimetre collisions no canvas growth fixes. An illegal
    // prescribed result falls back to the generic placer instead of failing.
    if !result.legal && opto817_requirements.is_some() {
        let generic = pcb_engine::place_tuned(&problem, &PlacementHints::default());
        if generic.legal {
            result = generic;
        }
    }

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
                x_nm: kicad_ipc::units::mm_to_nm(p.at.x),
                y_nm: kicad_ipc::units::mm_to_nm(p.at.y),
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
        let pairs = pcb_place::decoupling_pairs(&problem);
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

    // On a failed placement, give the agent a CONCRETE board size so it can retry
    // deterministically instead of guessing — in the one vocabulary
    // `regenerate_board` also uses, grown past the outline that just failed so a
    // retry can never propose it again. `suggested_min_bounds_mm` carries the
    // same recommendation under the name the auto-resize path already reads.
    let mut extra = json!({});
    if !result.legal {
        let cw = (problem.bounds.max_x - problem.bounds.min_x).max(0.1);
        let ch = (problem.bounds.max_y - problem.bounds.min_y).max(0.1);
        let sizing =
            board_sizing(&problem, &board.imported.parts, &board.problem).grown_past(cw, ch);
        extra = json!({
            "overlap_pairs": placement_overlap_pairs(&problem, &result),
            "parts_courtyard_area_mm2": sizing.courtyard_area_mm2,
            "current_bounds_mm": { "w": (cw * 10.0).round() / 10.0, "h": (ch * 10.0).round() / 10.0 },
            "required_bounds": { "width": sizing.required_w, "height": sizing.required_h },
            "recommended_bounds": { "width": sizing.recommended_w, "height": sizing.recommended_h },
            "suggested_min_bounds_mm": { "w": sizing.recommended_w, "h": sizing.recommended_h },
        });
    } else {
        // A legal placement on an oversized canvas reads as wasted board: report
        // the tight courtyard envelope so callers can regenerate at fit_bounds_mm
        // and re-place instead of shipping empty acreage.
        let by_ref: std::collections::BTreeMap<&str, _> = result
            .placements
            .iter()
            .map(|placement| (placement.reference.as_str(), placement))
            .collect();
        let mut envelope: Option<Rect> = None;
        for part in &problem.parts {
            let Some(placement) = by_ref.get(part.reference.as_str()) else {
                continue;
            };
            let half = pcb_place::rotated_courtyard_half(part, placement.rotation);
            let rect = Rect::from_center_half(placement.at, half);
            envelope = Some(match envelope {
                None => rect,
                Some(existing) => Rect::new(
                    existing.min_x.min(rect.min_x),
                    existing.min_y.min(rect.min_y),
                    existing.max_x.max(rect.max_x),
                    existing.max_y.max(rect.max_y),
                ),
            });
        }
        if let Some(envelope) = envelope {
            let cw = (problem.bounds.max_x - problem.bounds.min_x).max(0.1);
            let ch = (problem.bounds.max_y - problem.bounds.min_y).max(0.1);
            // Utilization from courtyard area, not the placed envelope:
            // edge-seeking connectors span the rim of any canvas, so an
            // envelope ratio always reads full even on an oversized board.
            let fresh = placement_size_estimate_with_growth(&problem, 0.0, 0.0, false, false);
            let utilization = (fresh.total_area * 2.0) / (cw * ch);
            extra = json!({
                "utilized_bounds_mm": {
                    "w": (envelope.width() * 10.0).round() / 10.0,
                    "h": (envelope.height() * 10.0).round() / 10.0,
                },
                "current_bounds_mm": { "w": (cw * 10.0).round() / 10.0, "h": (ch * 10.0).round() / 10.0 },
                "fit_bounds_mm": { "w": fresh.width.ceil(), "h": fresh.height.ceil() },
                "canvas_utilization_percent": (utilization * 100.0).round().min(100.0),
                "connectors_off_edge": connectors_off_edge(&problem, &board.imported.parts, &result),
            });
        }
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

/// The refusal a caller can act on in one move: how much copper area the parts
/// need against how much the board offers, and the exact bounds that would fit.
fn illegal_placement_error(result: &Value) -> String {
    let current_w = result
        .pointer("/current_bounds_mm/w")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let current_h = result
        .pointer("/current_bounds_mm/h")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let at = |pointer: &str, fallback: f64| {
        result
            .pointer(pointer)
            .and_then(Value::as_f64)
            .unwrap_or(fallback)
    };
    // The 817 rejection publishes only `suggested_min_bounds_mm`; the generic
    // path publishes all three, with the suggestion equal to the recommendation.
    let suggested_w = at("/suggested_min_bounds_mm/w", current_w);
    let suggested_h = at("/suggested_min_bounds_mm/h", current_h);
    let required_w = at("/required_bounds/width", suggested_w);
    let required_h = at("/required_bounds/height", suggested_h);
    let recommended_w = at("/recommended_bounds/width", suggested_w);
    let recommended_h = at("/recommended_bounds/height", suggested_h);
    let courtyard = at("/parts_courtyard_area_mm2", 0.0);
    let board_area = current_w * current_h;
    format!(
        "placement failed and no positions were written: the placer could not legally pack the \
         selected footprints in {current_w} x {current_h} mm ({board_area:.0} mm² of board for \
         {courtyard:.0} mm² of part courtyards, and packing plus routing needs roughly twice the \
         courtyard area). This board requires at least {required_w} x {required_h} mm; \
         {recommended_w} x {recommended_h} mm is recommended (it leaves routing room). Call \
         regenerate_board with \
         bounds {{\"min_x\":0,\"min_y\":0,\"max_x\":{recommended_w},\"max_y\":{recommended_h}}} \
         — or omit bounds to size it automatically — then place_board once. Choosing smaller \
         footprints is the other way to keep the current board size."
    )
}

/// Write footprint positions to the board: one live IPC commit when the
/// installed KiCAD supports footprint updates, otherwise the equivalent
/// offline s-expression edit.
pub(crate) fn write_placement(
    ctx: &AgentRuntime,
    moves: &[FootprintMove],
) -> std::result::Result<(), String> {
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
    let patched = kicad_board::patch_placements(&text, moves)
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
    use kicad::KicadInstallation;
    use kicad_board::{ImportedBoard, ImportedPad, ImportedPart, IpcBoardSnapshot};
    use kicad_footprint::{FootprintCatalog, PadTechnology};
    use pcb_model::{RouteSolution, RoutingView, Trace, Via};

    fn imported_part(reference: &str, lib_id: &str, pads: Vec<(String, String)>) -> ImportedPart {
        ImportedPart {
            reference: reference.into(),
            lib_id: lib_id.into(),
            at: Point2::new(1.0, 1.0),
            rotation: 0,
            locked: false,
            pads: pads
                .into_iter()
                .map(|(number, net)| ImportedPad {
                    number,
                    net: Some(net),
                    at: Point2::new(1.0, 1.0),
                    layers: vec![LayerRef::top()],
                })
                .collect(),
        }
    }

    fn placement_part(imported: &ImportedPart, opto: bool) -> Part {
        let pad_offset = |number: &str| match number {
            "1" => Point2::new(-3.0, -1.0),
            "2" => Point2::new(-3.0, 1.0),
            "3" => Point2::new(3.0, 1.0),
            "4" => Point2::new(3.0, -1.0),
            _ => Point2::new(0.0, 0.0),
        };
        Part {
            reference: imported.reference.clone(),
            courtyard_w: if opto { 8.0 } else { 5.0 },
            courtyard_h: 5.0,
            pads: imported
                .pads
                .iter()
                .map(|pad| PartPad {
                    number: pad.number.clone(),
                    offset: pad_offset(&pad.number),
                    width: 1.0,
                    height: 1.0,
                    layers: vec![LayerRef::top()],
                    net: pad.net.clone(),
                })
                .collect(),
            edge_datum: None,
            locked: None,
        }
    }

    fn opto817_fixture(
        count: usize,
        reversed: bool,
    ) -> (sch_check::model::Design, IpcBoardSnapshot, PlacementView) {
        let mut design = sch_check::model::Design::default();
        let mut block = sch_check::model::Block::default();
        let mut imported = Vec::new();
        for index in 1..=count {
            let mut component = sch_check::model::Component {
                part: "Isolator:PC817".into(),
                ..Default::default()
            };
            for (pin, net) in [
                ("1", format!("FIELD{index}_LED")),
                ("2", format!("FIELD{index}_RET")),
                (
                    "3",
                    if reversed {
                        format!("OUT{index}")
                    } else {
                        "GND".into()
                    },
                ),
                (
                    "4",
                    if reversed {
                        "GND".into()
                    } else {
                        format!("OUT{index}")
                    },
                ),
            ] {
                component
                    .pins
                    .insert(pin.into(), sch_check::model::PinTarget::Net(net));
            }
            block.components.insert(format!("U{index}"), component);
            imported.push(imported_part(
                &format!("U{index}"),
                "Package_SO:SO-4_4.4x3.6mm_P2.54mm",
                vec![
                    ("1".into(), format!("FIELD{index}_LED")),
                    ("2".into(), format!("FIELD{index}_RET")),
                    (
                        "3".into(),
                        if reversed {
                            format!("OUT{index}")
                        } else {
                            "GND".into()
                        },
                    ),
                    (
                        "4".into(),
                        if reversed {
                            "GND".into()
                        } else {
                            format!("OUT{index}")
                        },
                    ),
                ],
            ));
        }
        design.blocks.insert("channels".into(), block);
        imported.push(imported_part(
            "JF1",
            "Connector_PinHeader:PinHeader_2x04",
            (1..=4)
                .flat_map(|index| {
                    [
                        ("1", format!("FIELD{index}_LED")),
                        ("2", format!("FIELD{index}_RET")),
                    ]
                })
                .map(|(pin, net)| (pin.into(), net))
                .collect(),
        ));
        imported.push(imported_part(
            "JL1",
            "Connector_PinHeader:PinHeader_1x10",
            vec![
                ("1".into(), "OUT1".into()),
                ("2".into(), "OUT2".into()),
                ("3".into(), "GND".into()),
                ("4".into(), "V3V3".into()),
            ],
        ));
        imported.push(imported_part(
            "R1",
            "Resistor_SMD:R_0603",
            vec![
                ("1".into(), "FIELD1_LED".into()),
                ("2".into(), "FIELD1_SRC".into()),
            ],
        ));
        imported.push(imported_part(
            "RN1",
            "Resistor_THT:R_Array",
            vec![
                ("1".into(), "V3V3".into()),
                ("2".into(), "OUT1".into()),
                ("3".into(), "OUT2".into()),
            ],
        ));
        let bounds = Rect::new(0.0, 0.0, 73.0, 58.0);
        let route_problem = RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![],
            bounds,
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
        };
        let parts = imported
            .iter()
            .map(|part| placement_part(part, part.reference.starts_with('U')))
            .collect();
        let board = IpcBoardSnapshot {
            imported: ImportedBoard {
                layer_count: 2,
                bounds,
                parts: imported,
                placement_keepouts: vec![],
                keepout_count: 0,
            },
            problem: route_problem,
            copper: RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            layer_names: vec!["F.Cu".into(), "B.Cu".into()],
        };
        let problem = PlacementView {
            bounds,
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts,
            keepouts: vec![],
            outline: None,
        };
        (design, board, problem)
    }

    #[test]
    fn verified_817_bank_gets_center_barrier_and_opposite_connector_edges() {
        let (design, board, problem) = opto817_fixture(8, false);
        let mut hints = PlacementHints::default();

        let required = add_817_array_hints(&design, &board, &problem, &mut hints).unwrap();
        assert!(required.width <= problem.bounds.max_x - problem.bounds.min_x);
        assert!(required.height <= problem.bounds.max_y - problem.bounds.min_y);

        let array = hints
            .groups
            .iter()
            .find(|group| group.name == "817 isolation array")
            .unwrap();
        assert_eq!(array.members.len(), 8);
        assert!(array.grid);
        assert_eq!(array.rotation, Some(270.0));
        let region = array.region.unwrap();
        assert!((region.center().y - problem.bounds.center().y).abs() < 1e-9);
        let field = hints
            .groups
            .iter()
            .find(|group| group.name == "field connectors")
            .unwrap();
        let logic = hints
            .groups
            .iter()
            .find(|group| group.name == "logic connectors")
            .unwrap();
        assert_eq!(field.members, ["JF1"]);
        assert_eq!(field.edge, Some(Edge::N));
        assert_eq!(logic.members, ["JL1"]);
        assert_eq!(logic.edge, Some(Edge::S));
        assert!(
            hints
                .groups
                .iter()
                .find(|group| group.name == "817 field channel 1")
                .unwrap()
                .members
                .contains(&"R1".into())
        );
        assert!(
            hints
                .groups
                .iter()
                .find(|group| group.name == "logic domain")
                .unwrap()
                .members
                .contains(&"RN1".into())
        );

        let mut locked = problem.clone();
        pcb_place::placement::apply_grid_hints(&mut locked, &hints);
        let optos = locked
            .parts
            .iter()
            .filter(|part| part.reference.starts_with('U'))
            .map(|part| part.locked.as_ref().unwrap())
            .collect::<Vec<_>>();
        assert!(
            optos
                .windows(2)
                .all(|pair| (pair[0].at.y - pair[1].at.y).abs() < 1e-9)
        );
        let pitch = optos[1].at.x - optos[0].at.x;
        assert!(pitch > 5.0);
        assert!(
            optos
                .windows(2)
                .all(|pair| ((pair[1].at.x - pair[0].at.x) - pitch).abs() < 1e-9)
        );
    }

    #[test]
    fn opto817_plan_requires_eight_correct_common_emitter_channels() {
        for (count, reversed) in [(7, false), (8, true)] {
            let (design, board, problem) = opto817_fixture(count, reversed);
            let mut hints = PlacementHints::default();

            assert!(add_817_array_hints(&design, &board, &problem, &mut hints).is_none());
            assert!(hints.groups.is_empty());
        }
    }

    #[test]
    fn opto817_plan_accepts_explicit_logic_ground_domain_names() {
        let (mut design, mut board, problem) = opto817_fixture(8, false);
        for component in design.blocks["channels"].components.values_mut() {
            component.pins.insert(
                "3".into(),
                sch_check::model::PinTarget::Net("LOGIC_GND".into()),
            );
        }
        for imported in board
            .imported
            .parts
            .iter_mut()
            .filter(|part| part.reference.starts_with('U'))
        {
            imported
                .pads
                .iter_mut()
                .filter(|pad| pad.number == "3")
                .for_each(|pad| pad.net = Some("LOGIC_GND".into()));
        }
        let mut hints = PlacementHints::default();

        assert!(add_817_array_hints(&design, &board, &problem, &mut hints).is_some());
    }

    #[test]
    fn opto817_plan_aligns_asymmetric_footprint_gap_to_board_center() {
        let (design, board, mut problem) = opto817_fixture(8, false);
        for part in problem
            .parts
            .iter_mut()
            .filter(|part| part.reference.starts_with('U'))
        {
            for pad in &mut part.pads {
                pad.offset = match pad.number.as_str() {
                    "1" => Point2::new(0.0, 0.0),
                    "2" => Point2::new(2.54, 0.0),
                    "3" => Point2::new(2.54, 7.62),
                    "4" => Point2::new(0.0, 7.62),
                    _ => unreachable!(),
                };
            }
        }
        let mut hints = PlacementHints::default();

        add_817_array_hints(&design, &board, &problem, &mut hints).expect("asymmetric DIP bank");

        let array = hints
            .groups
            .iter()
            .find(|group| group.name == "817 isolation array")
            .unwrap();
        let rotation = array.rotation.unwrap();
        let channel = &opto817_channels(&design, &board)[0];
        let part = problem
            .parts
            .iter()
            .find(|part| part.reference == channel.reference)
            .unwrap();
        let bridge_offset = opto_bridge_midpoint_y(part, channel, rotation).unwrap();
        assert!(
            (array.region.unwrap().center().y + bridge_offset - problem.bounds.center().y).abs()
                < 1e-9
        );
        assert!(
            bridge_offset.abs() > 1.0,
            "fixture must exercise offset origin"
        );
    }

    #[test]
    fn opto817_plan_clusters_one_hop_passives_by_channel() {
        let (design, mut board, mut problem) = opto817_fixture(8, false);
        for index in 1..=8 {
            for (reference, lib_id, nets) in [
                (
                    format!("RIN{index}"),
                    "Resistor_SMD:R",
                    vec![format!("FIELD{index}_LED"), format!("IN{index}")],
                ),
                (
                    format!("RPU{index}"),
                    "Resistor_SMD:R",
                    vec![format!("OUT{index}"), "+5V".into()],
                ),
                (
                    format!("DLED{index}"),
                    "LED_SMD:LED",
                    vec![format!("OUT{index}"), format!("LED_A{index}")],
                ),
                (
                    format!("RLED{index}"),
                    "Resistor_SMD:R",
                    vec![format!("LED_A{index}"), "+5V".into()],
                ),
            ] {
                let imported = imported_part(
                    &reference,
                    lib_id,
                    nets.into_iter()
                        .enumerate()
                        .map(|(pad, net)| ((pad + 1).to_string(), net))
                        .collect(),
                );
                problem.parts.push(placement_part(&imported, false));
                board.imported.parts.push(imported);
            }
        }
        let mut hints = PlacementHints::default();

        add_817_array_hints(&design, &board, &problem, &mut hints)
            .expect("verified bank with passives");

        for index in 1..=8 {
            let field = hints
                .groups
                .iter()
                .find(|group| group.name == format!("817 field channel {index}"))
                .expect("per-channel input group");
            let expected_field = if index == 1 {
                vec!["R1".to_owned(), "RIN1".to_owned()]
            } else {
                vec![format!("RIN{index}")]
            };
            assert_eq!(field.members, expected_field);
            let logic = hints
                .groups
                .iter()
                .find(|group| group.name == format!("817 logic channel {index}"))
                .expect("per-channel output group");
            assert_eq!(
                logic
                    .members
                    .iter()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>(),
                [
                    format!("DLED{index}"),
                    format!("RLED{index}"),
                    format!("RPU{index}"),
                ]
                .into_iter()
                .collect()
            );
            let region = logic.region.expect("channel group has a grid region");
            assert!(
                region.max_y - region.min_y >= (region.max_x - region.min_x) * 1.5 - 1e-9,
                "three support references need separate horizontal lanes"
            );
        }
    }

    #[test]
    fn opto817_field_power_connector_does_not_share_the_passive_grid() {
        let (mut design, mut board, mut problem) = opto817_fixture(8, false);
        let bounds = Rect::new(0.0, 0.0, 122.0, 62.0);
        board.imported.bounds = bounds;
        board.problem.bounds = bounds;
        problem.bounds = bounds;

        for component in design.blocks["channels"].components.values_mut() {
            component.pins.insert(
                "2".into(),
                sch_check::model::PinTarget::Net("FIELD_GND".into()),
            );
        }
        for imported in &mut board.imported.parts {
            for pad in &mut imported.pads {
                if (imported.reference.starts_with('U') && pad.number == "2")
                    || (imported.reference == "JF1"
                        && pad.net.as_deref().is_some_and(|net| net.ends_with("_RET")))
                {
                    pad.net = Some("FIELD_GND".into());
                }
            }
        }

        let ground_header = imported_part(
            "J2",
            "Connector_PinHeader:PinHeader_1x08",
            (1..=8)
                .map(|pin| (pin.to_string(), "FIELD_GND".into()))
                .collect(),
        );
        let mut ground_header_part = placement_part(&ground_header, false);
        ground_header_part.courtyard_w = 2.54;
        ground_header_part.courtyard_h = 20.32;
        board.imported.parts.push(ground_header);
        problem.parts.push(ground_header_part);
        let power = imported_part(
            "JFP",
            "Connector_PinHeader:PinHeader_1x02",
            vec![("1".into(), "V24".into()), ("2".into(), "FIELD_GND".into())],
        );
        let mut power_part = placement_part(&power, false);
        power_part.courtyard_w = 2.54;
        power_part.courtyard_h = 5.08;
        board.imported.parts.push(power);
        problem.parts.push(power_part);
        for index in 25..=32 {
            let imported = imported_part(
                &format!("R{index}"),
                "Resistor_SMD:R_0603",
                vec![("1".into(), "V24".into()), ("2".into(), "FIELD_GND".into())],
            );
            let mut part = placement_part(&imported, false);
            part.courtyard_w = 2.0;
            part.courtyard_h = 1.25;
            board.imported.parts.push(imported);
            problem.parts.push(part);
        }

        let mut hints = PlacementHints::default();
        add_817_array_hints(&design, &board, &problem, &mut hints)
            .expect("verified bank with field power support");
        assert_eq!(
            hints
                .groups
                .iter()
                .find(|group| group.name == "field auxiliary connectors")
                .unwrap()
                .members,
            ["JFP"]
        );
        let field_domain = hints
            .groups
            .iter()
            .find(|group| group.name == "field domain")
            .unwrap();
        assert_eq!(field_domain.members.len(), 8);
        assert!(!field_domain.members.contains(&"JFP".into()));

        pcb_place::placement::apply_grid_hints(&mut problem, &hints);
        mirror_right_817_bank(&mut problem, &hints);
        align_817_field_connector_datums(&mut problem, &hints);
        assert!(problem.parts.iter().all(|part| part.locked.is_some()));
        let result = pcb_place::placement::place(&problem, &PlacementHints::default());
        assert!(
            result.legal,
            "field power support placement should be legal: {:?}",
            placement_overlap_pairs(&problem, &result)
        );
    }

    #[test]
    fn placement_refuses_to_move_footprints_under_existing_copper() {
        let blocked = placement_existing_copper_error(1, 0).unwrap();

        assert_eq!(blocked["code"], "placement_requires_copper_free_board");
        assert_eq!(blocked["placement_applied"], false);
        assert_eq!(blocked["tracks"], 1);
        assert_eq!(blocked["vias"], 0);
        assert!(placement_existing_copper_error(0, 0).is_none());
    }

    #[test]
    fn opto817_plan_uses_actual_anonymous_board_nets() {
        let (design, mut board, mut problem) = opto817_fixture(16, false);
        for imported in board
            .imported
            .parts
            .iter_mut()
            .filter(|part| part.reference.starts_with('U'))
        {
            imported
                .pads
                .iter_mut()
                .find(|pad| pad.number == "1")
                .unwrap()
                .net = Some(format!("Net-({}-Pad1)", imported.reference));
        }
        for part in problem
            .parts
            .iter_mut()
            .filter(|part| part.reference.starts_with('U'))
        {
            part.pads
                .iter_mut()
                .find(|pad| pad.number == "1")
                .unwrap()
                .net = Some(format!("Net-({}-Pad1)", part.reference));
        }

        let channels = opto817_channels(&design, &board);
        assert_eq!(channels.len(), 16);
        assert_eq!(channels[0].input_nets[0], "Net-(U1-Pad1)");
        let mut hints = PlacementHints::default();
        assert!(add_817_array_hints(&design, &board, &problem, &mut hints).is_some());
        assert_eq!(
            hints
                .groups
                .iter()
                .find(|group| group.name == "817 isolation array")
                .unwrap()
                .members
                .len(),
            16
        );
    }

    #[test]
    fn field_headers_align_to_half_banks_while_wide_logic_headers_stay_legal() {
        let (design, mut board, mut problem) = opto817_fixture(16, false);
        let bounds = Rect::new(0.0, 0.0, 117.0, 60.0);
        board.imported.bounds = bounds;
        board.problem.bounds = bounds;
        problem.bounds = bounds;

        let field_header = |reference: &str, channels: std::ops::RangeInclusive<usize>| {
            imported_part(
                reference,
                "Connector_PinHeader:PinHeader_2x08",
                channels
                    .flat_map(|channel| {
                        [format!("FIELD{channel}_LED"), format!("FIELD{channel}_RET")]
                    })
                    .enumerate()
                    .map(|(index, net)| ((index + 1).to_string(), net))
                    .collect(),
            )
        };
        let field1 = field_header("JF1", 1..=8);
        let field2 = field_header("JF2", 9..=16);
        let field_index = board
            .imported
            .parts
            .iter()
            .position(|part| part.reference == "JF1")
            .unwrap();
        board.imported.parts[field_index] = field1.clone();
        board.imported.parts.push(field2.clone());
        let field_part = problem
            .parts
            .iter_mut()
            .find(|part| part.reference == "JF1")
            .unwrap();
        *field_part = placement_part(&field1, false);
        problem.parts.push(placement_part(&field2, false));

        let logic_index = board
            .imported
            .parts
            .iter()
            .position(|part| part.reference == "JL1")
            .unwrap();
        board.imported.parts[logic_index].reference = "JLOG1".into();
        let second = imported_part(
            "JLOG2",
            "Connector_PinHeader:PinHeader_2x10",
            vec![
                ("1".into(), "OUT9".into()),
                ("2".into(), "OUT10".into()),
                ("3".into(), "GND".into()),
                ("4".into(), "V3V3".into()),
            ],
        );
        let power = imported_part(
            "JPWR",
            "Connector_PinHeader:PinHeader_1x02",
            vec![("1".into(), "GND".into()), ("2".into(), "V3V3".into())],
        );
        let rn2 = imported_part(
            "RN2",
            "Resistor_THT:R_Array_SIP9",
            vec![
                ("1".into(), "V3V3".into()),
                ("2".into(), "OUT9".into()),
                ("3".into(), "OUT10".into()),
            ],
        );
        let cap1 = imported_part(
            "C1",
            "Capacitor_SMD:C_0603",
            vec![("1".into(), "V3V3".into()), ("2".into(), "GND".into())],
        );
        let mut cap2 = cap1.clone();
        cap2.reference = "C2".into();
        let hole1 = imported_part("MH1", "MountingHole:MountingHole_3.2mm_M3", vec![]);
        let mut hole2 = hole1.clone();
        hole2.reference = "MH2".into();
        board.imported.parts.push(second.clone());
        board.imported.parts.push(power.clone());
        board.imported.parts.extend([
            rn2.clone(),
            cap1.clone(),
            cap2.clone(),
            hole1.clone(),
            hole2.clone(),
        ]);

        let first_part = problem
            .parts
            .iter_mut()
            .find(|part| part.reference == "JL1")
            .unwrap();
        first_part.reference = "JLOG1".into();
        first_part.courtyard_w = 5.0;
        first_part.courtyard_h = 49.5;
        let mut second_part = placement_part(&second, false);
        second_part.courtyard_w = 5.0;
        second_part.courtyard_h = 49.5;
        problem.parts.push(second_part);
        problem.parts.push(placement_part(&power, false));
        let rn1_part = problem
            .parts
            .iter_mut()
            .find(|part| part.reference == "RN1")
            .unwrap();
        rn1_part.courtyard_w = 43.84;
        rn1_part.courtyard_h = 3.12;
        let mut rn2_part = placement_part(&rn2, false);
        rn2_part.courtyard_w = 43.84;
        rn2_part.courtyard_h = 3.12;
        problem.parts.push(rn2_part);
        for imported in [&cap1, &cap2] {
            let mut part = placement_part(imported, false);
            part.courtyard_w = 2.0;
            part.courtyard_h = 2.0;
            problem.parts.push(part);
        }
        for imported in [&hole1, &hole2] {
            let mut part = placement_part(imported, false);
            part.courtyard_w = 6.9;
            part.courtyard_h = 6.9;
            problem.parts.push(part);
        }

        let mut hints = PlacementHints::default();
        add_817_array_hints(&design, &board, &problem, &mut hints).expect("verified 817 plan");
        let strong = hints
            .groups
            .iter()
            .find(|group| group.name == "logic connectors")
            .unwrap();
        let auxiliary = hints
            .groups
            .iter()
            .find(|group| group.name == "logic auxiliary connectors")
            .unwrap();
        let field = hints
            .groups
            .iter()
            .find(|group| group.name == "field connectors")
            .unwrap();
        assert_eq!(field.members, ["JF1", "JF2"]);
        assert_eq!(strong.members, ["JLOG1", "JLOG2"]);
        assert_eq!(auxiliary.members, ["JPWR"]);

        let mut shuffled_board = board.clone();
        shuffled_board.imported.parts.reverse();
        let mut shuffled_problem = problem.clone();
        shuffled_problem.parts.reverse();
        let mut shuffled_hints = PlacementHints::default();
        add_817_array_hints(
            &design,
            &shuffled_board,
            &shuffled_problem,
            &mut shuffled_hints,
        )
        .expect("order-independent 817 plan");
        pcb_place::placement::apply_grid_hints(&mut problem, &hints);
        pcb_place::placement::apply_grid_hints(&mut shuffled_problem, &shuffled_hints);
        mirror_right_817_bank(&mut problem, &hints);
        mirror_right_817_bank(&mut shuffled_problem, &shuffled_hints);
        align_817_field_connector_datums(&mut problem, &hints);
        align_817_field_connector_datums(&mut shuffled_problem, &shuffled_hints);
        assert!(problem.parts.iter().all(|part| part.locked.is_some()));
        assert!(
            shuffled_problem
                .parts
                .iter()
                .all(|part| part.locked.is_some())
        );
        let locked_by_ref = |problem: &PlacementView| {
            problem
                .parts
                .iter()
                .map(|part| (part.reference.clone(), part.locked.clone().unwrap()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(locked_by_ref(&problem), locked_by_ref(&shuffled_problem));
        let connector = |reference: &str| {
            problem
                .parts
                .iter()
                .find(|part| part.reference == reference)
                .unwrap()
        };
        let (jf1, jf2) = (connector("JF1"), connector("JF2"));
        let (jlog1, jlog2, jpwr) = (connector("JLOG1"), connector("JLOG2"), connector("JPWR"));
        assert_eq!(jf1.locked.as_ref().unwrap().rotation, 90.0);
        assert_eq!(jf2.locked.as_ref().unwrap().rotation, 270.0);
        assert_eq!(jlog1.locked.as_ref().unwrap().rotation, 90.0);
        assert_eq!(jlog2.locked.as_ref().unwrap().rotation, 270.0);
        let channels = opto817_channels(&design, &board);
        let bank_centroid = |channels: &[Opto817Channel]| {
            channels
                .iter()
                .map(|channel| connector(&channel.reference).locked.as_ref().unwrap().at.x)
                .sum::<f64>()
                / channels.len() as f64
        };
        let left_bank = bank_centroid(&channels[..8]);
        let right_bank = bank_centroid(&channels[8..]);
        assert!(jf1.locked.as_ref().unwrap().at.x < jf2.locked.as_ref().unwrap().at.x);
        assert!((left_bank - 33.5).abs() < 1e-9);
        assert!((right_bank - 83.5).abs() < 1e-9);
        assert!((jlog1.locked.as_ref().unwrap().at.x - 29.875).abs() < 1e-9);
        assert!((jlog2.locked.as_ref().unwrap().at.x - 87.125).abs() < 1e-9);
        assert!((jpwr.locked.as_ref().unwrap().at.x - 58.5).abs() < 1e-9);
        assert!(jpwr.locked.as_ref().unwrap().at.y < jlog1.locked.as_ref().unwrap().at.y);
        let rn1 = connector("RN1");
        let rn2 = connector("RN2");
        assert_eq!(rn1.locked.as_ref().unwrap().rotation, 0.0);
        assert_eq!(rn2.locked.as_ref().unwrap().rotation, 180.0);
        assert!((rn2.locked.as_ref().unwrap().at.x - rn1.locked.as_ref().unwrap().at.x) > 50.0);
        let mh1 = connector("MH1").locked.as_ref().unwrap();
        let mh2 = connector("MH2").locked.as_ref().unwrap();
        assert!((mh1.at.y - 3.95).abs() < 1e-9);
        assert!((mh2.at.y - 3.95).abs() < 1e-9);
        let placed_rect = |part: &Part| {
            let locked = part.locked.as_ref().unwrap();
            Rect::from_center_half(
                locked.at,
                pcb_place::rotated_courtyard_half(part, locked.rotation),
            )
            .inflate(pcb_place::courtyard_margin(problem.clearance) / 2.0)
        };
        for (a, b) in [(jlog1, jlog2), (jlog1, jpwr), (jlog2, jpwr)] {
            let (x, y) = placed_rect(a).axis_penetration(&placed_rect(b));
            assert!(
                x <= geom::EPS || y <= geom::EPS,
                "{} overlaps {}",
                a.reference,
                b.reference
            );
        }

        let started = std::time::Instant::now();
        let result = pcb_place::placement::place(&problem, &PlacementHints::default());
        assert!(
            result.legal,
            "117x60 specialized placement should be legal: {:?}",
            placement_overlap_pairs(&problem, &result)
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let shuffled_result =
            pcb_place::placement::place(&shuffled_problem, &PlacementHints::default());
        assert!(shuffled_result.legal);
        let positions_by_ref = |result: &PlaceResult| {
            result
                .placements
                .iter()
                .map(|placement| {
                    (
                        placement.reference.clone(),
                        (placement.at.x, placement.at.y, placement.rotation),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            positions_by_ref(&result),
            positions_by_ref(&shuffled_result)
        );
    }

    #[test]
    fn real_817_bank_rejects_73mm_and_forms_one_oriented_row_at_90mm() {
        let Some(env) = KicadInstallation::detect() else {
            eprintln!("SKIP: KiCad libraries not installed");
            return;
        };
        let catalog =
            FootprintCatalog::from_root(env.footprint_dir()).expect("installed footprint catalog");
        let id =
            FootprintId::parse("Package_SO:SO-4_4.4x3.6mm_P2.54mm").expect("valid footprint id");
        let footprint = catalog.footprint(&id).expect("installed SO-4 footprint");
        let (design, mut board, mut problem) = opto817_fixture(16, false);
        let bounds = Rect::new(0.0, 0.0, 90.0, 58.0);
        board.imported.bounds = bounds;
        board.problem.bounds = bounds;
        problem.bounds = bounds;
        for part in &mut problem.parts {
            if !part.reference.starts_with('U') {
                continue;
            }
            let imported = board
                .imported
                .parts
                .iter()
                .find(|candidate| candidate.reference == part.reference)
                .unwrap();
            let nets = imported
                .pads
                .iter()
                .filter_map(|pad| Some((pad.number.clone(), pad.net.clone()?)))
                .collect();
            *part = part_from_footprint_layers(&footprint, &part.reference, &nets, 2, None);
        }

        let mut hints = PlacementHints::default();
        let required =
            add_817_array_hints(&design, &board, &problem, &mut hints).expect("verified 817 plan");
        assert!(required.width > 73.0 && required.width <= 90.0);
        assert!(required.height <= 58.0);

        let mut undersized = problem.clone();
        undersized.bounds = Rect::new(0.0, 0.0, 73.0, 58.0);
        let rejected = undersized_817_result(&undersized, required);
        assert_eq!(rejected["legal"], json!(false));
        assert_eq!(rejected["placement_applied"], json!(false));
        assert_eq!(rejected["positions"], json!([]));
        assert!(rejected["suggested_min_bounds_mm"]["w"].as_f64().unwrap() >= required.width);
        assert!(
            rejected["error"]
                .as_str()
                .unwrap()
                .contains("no positions were written")
        );

        pcb_place::placement::apply_grid_hints(&mut problem, &hints);
        let channels = opto817_channels(&design, &board);
        let optos = channels
            .iter()
            .map(|channel| {
                let part = problem
                    .parts
                    .iter()
                    .find(|part| part.reference == channel.reference)
                    .unwrap();
                (channel, part, part.locked.as_ref().unwrap())
            })
            .collect::<Vec<_>>();
        assert_eq!(optos.len(), 16);
        assert!(optos.iter().all(|(_, _, locked)| locked.rotation == 270.0));
        assert!(
            optos
                .iter()
                .all(|(_, _, locked)| (locked.at.y - 29.0).abs() < 1e-9)
        );
        assert!((optos[0].2.at.x - 4.875).abs() < 1e-9);
        assert!((optos[15].2.at.x - 85.125).abs() < 1e-9);
        assert!(
            optos
                .windows(2)
                .all(|pair| { ((pair[1].2.at.x - pair[0].2.at.x) - 5.35).abs() < 1e-9 })
        );
        for (channel, part, locked) in optos {
            let average_y = |nets: &[&str]| {
                let pads = part
                    .pads
                    .iter()
                    .filter(|pad| {
                        pad.net.as_deref().is_some_and(|net| {
                            nets.iter().any(|candidate| same_net(net, candidate))
                        })
                    })
                    .collect::<Vec<_>>();
                pads.iter()
                    .map(|pad| locked.at.y + pad.offset.rotate(locked.rotation).y)
                    .sum::<f64>()
                    / pads.len() as f64
            };
            let input = [&*channel.input_nets[0], &*channel.input_nets[1]];
            let output = [&*channel.emitter_net, &*channel.output_net];
            assert!(average_y(&input) < average_y(&output));
        }
    }

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
        assert_eq!(group.edge, Some(pcb_place::Edge::W));
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
        let problem = RoutingView {
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
        let problem = RoutingView {
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
            "parts_courtyard_area_mm2": 1580.0,
        }));

        assert!(message.contains("no positions were written"), "{message}");
        assert!(message.contains("smaller footprints"), "{message}");
        // Area budget: what the parts need against what the board offers.
        assert!(message.contains("1350 mm² of board"), "{message}");
        assert!(message.contains("1580 mm² of part courtyards"), "{message}");
        // The one call that fixes it, spelled out.
        assert!(message.contains("\"max_x\":69,\"max_y\":46"), "{message}");
    }

    /// The nine-part board from the failure report: a USB-C receptacle, a
    /// SOT-23-5 LDO and seven 0603 passives. `regenerate_board` sizes a board
    /// like this before anything is placed, and the size it picks must place
    /// legally on the FIRST call — otherwise the caller is back to guessing.
    /// A legal placement says nothing about whether a cable can reach the
    /// connectors. One stranded in the interior is the single most common
    /// complaint about these boards, and the caller never learns of it.
    #[test]
    fn a_connector_stranded_in_the_board_interior_is_named_with_its_fix() {
        let part = |reference: &str| Part {
            reference: reference.to_string(),
            courtyard_w: 3.0,
            courtyard_h: 3.0,
            pads: vec![],
            edge_datum: None,
            locked: None,
        };
        let problem = PlacementView {
            bounds: Rect::new(0.0, 0.0, 40.0, 40.0),
            clearance: 0.15,
            layer_count: 2,
            min_trace_width: 0.15,
            parts: vec![part("J1"), part("J2"), part("R1")],
            keepouts: vec![],
            outline: None,
        };
        let at = |reference: &str, x: f64, y: f64| pcb_place::Placement {
            reference: reference.to_string(),
            at: Point2::new(x, y),
            rotation: 0.0,
        };
        let result = pcb_place::PlaceResult {
            // J1 sits on the left edge; J2 is stranded mid-board; R1 is not a
            // connector and is never asked to seek an edge.
            placements: vec![
                at("J1", 2.0, 20.0),
                at("J2", 20.0, 20.0),
                at("R1", 30.0, 30.0),
            ],
            legal: true,
            report: pcb_place::PlaceReport::default(),
        };

        let off = connectors_off_edge(&problem, &[], &result);

        assert_eq!(off.len(), 1, "{off:?}");
        assert_eq!(off[0]["reference"], "J2");
        assert_eq!(off[0]["gap_to_edge_mm"], 18.5);
        assert!(
            off[0]["fix"]
                .as_str()
                .unwrap()
                .contains("\"reference\":\"J2\""),
            "{off:?}"
        );
    }

    #[test]
    fn an_auto_sized_nine_part_board_places_legally_on_the_first_try() {
        let part = |reference: &str, w: f64, h: f64| Part {
            reference: reference.to_string(),
            courtyard_w: w,
            courtyard_h: h,
            pads: vec![],
            edge_datum: None,
            locked: None,
        };
        let mut parts = vec![part("J1", 9.2, 7.6), part("U1", 3.0, 3.0)];
        for i in 1..=7 {
            parts.push(part(&format!("C{i}"), 2.0, 1.5));
        }
        let extents: Vec<_> = parts
            .iter()
            .map(|p| crate::sizing::PartExtent {
                w: p.courtyard_w,
                h: p.courtyard_h,
                edge_seeking: is_connector("", &p.reference),
            })
            .collect();
        let sizing = crate::sizing::size_board(
            &extents,
            1.0,
            crate::sizing::RoutingDemand {
                clearance: 0.15,
                track_width: 0.15,
                layer_count: 2,
                net_count: 8,
            },
        );

        assert!(
            sizing.required_w < sizing.recommended_w && sizing.required_h < sizing.recommended_h,
            "recommended must leave routing room over required: {sizing:?}"
        );

        let problem = PlacementView {
            bounds: Rect::new(0.0, 0.0, sizing.recommended_w, sizing.recommended_h),
            clearance: 0.15,
            layer_count: 2,
            min_trace_width: 0.15,
            parts,
            keepouts: vec![],
            outline: None,
        };
        assert!(
            pcb_engine::place_tuned(&problem, &PlacementHints::default()).legal,
            "auto-sized {} x {} mm did not place legally",
            sizing.recommended_w,
            sizing.recommended_h
        );
    }

    #[test]
    fn run19_like_resize_estimate_jumps_from_50x40_to_one_step_legal_bounds() {
        let starts = [
            Point2::new(23.0, 12.0),
            Point2::new(97.0, 12.0),
            Point2::new(23.0, 48.0),
            Point2::new(97.0, 48.0),
        ];
        let parts = starts
            .iter()
            .enumerate()
            .map(|(index, &at)| Part {
                reference: format!("BANK{index}"),
                courtyard_w: 45.0,
                courtyard_h: 20.0,
                pads: vec![],
                edge_datum: None,
                locked: Some(LockedAt { at, rotation: 0.0 }),
            })
            .collect();
        let mut problem = PlacementView {
            bounds: Rect::new(0.0, 0.0, 50.0, 40.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts,
            keepouts: vec![],
            outline: None,
        };

        let required = Opto817Requirements {
            width: 88.1,
            height: 23.8,
        };
        let estimate = placement_size_estimate(&problem, required.width, required.height, true);
        assert_eq!(estimate.width.ceil(), 120.0);
        assert_eq!(estimate.height.ceil(), 60.0);
        let rejected = undersized_817_result(&problem, required);
        assert_eq!(
            rejected["suggested_min_bounds_mm"],
            json!({"w": 120.0, "h": 60.0})
        );
        problem.bounds = Rect::new(0.0, 0.0, estimate.width.ceil(), estimate.height.ceil());
        let second_call = pcb_engine::place_tuned(&problem, &PlacementHints::default());
        assert!(second_call.legal);
    }

    #[test]
    fn real_palconn_usb_c_datum_locks_exactly_to_every_board_edge() {
        let Some(env) = KicadInstallation::detect() else {
            eprintln!("SKIP: KiCad libraries not installed");
            return;
        };
        let catalog =
            FootprintCatalog::from_root(env.footprint_dir()).expect("installed footprint catalog");
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
        let mut problem = PlacementView {
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
            pcb_place::Edge::N,
            pcb_place::Edge::E,
            pcb_place::Edge::S,
            pcb_place::Edge::W,
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
            let copper_box = pcb_place::rotated_copper_bbox(part, locked.rotation);
            let copper_center = Point2::new(
                locked.at.x + copper_box.center().x,
                locked.at.y + copper_box.center().y,
            );
            let datum_center = Point2::new(
                (world_start.x + world_end.x) / 2.0,
                (world_start.y + world_end.y) / 2.0,
            );
            match edge {
                pcb_place::Edge::N => {
                    assert!((world_start.y - bounds.min_y).abs() < 1e-9);
                    assert!((world_end.y - bounds.min_y).abs() < 1e-9);
                    assert!(
                        copper_center.y > datum_center.y,
                        "north copper must point inward"
                    );
                }
                pcb_place::Edge::S => {
                    assert!((world_start.y - bounds.max_y).abs() < 1e-9);
                    assert!((world_end.y - bounds.max_y).abs() < 1e-9);
                    assert!(
                        copper_center.y < datum_center.y,
                        "south copper must point inward"
                    );
                }
                pcb_place::Edge::W => {
                    assert!((world_start.x - bounds.min_x).abs() < 1e-9);
                    assert!((world_end.x - bounds.min_x).abs() < 1e-9);
                    assert!(
                        copper_center.x > datum_center.x,
                        "west copper must point inward"
                    );
                }
                pcb_place::Edge::E => {
                    assert!((world_start.x - bounds.max_x).abs() < 1e-9);
                    assert!((world_end.x - bounds.max_x).abs() < 1e-9);
                    assert!(
                        copper_center.x < datum_center.x,
                        "east copper must point inward"
                    );
                }
            }
            positions.push(locked.at);
            half.push(pcb_place::rotated_courtyard_half(part, locked.rotation));
            copper.push(copper_box);
        }
        assert!(
            pcb_place::is_legal(
                &problem,
                &half,
                &copper,
                pcb_place::courtyard_margin(problem.clearance),
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
        let result = pcb_engine::place_tuned(&problem, &hints);
        assert!(result.legal, "real Palconn auto-placement must be legal");
        let auto_positions: Vec<Point2> = result.placements.iter().map(|p| p.at).collect();
        let auto_half: Vec<_> = problem
            .parts
            .iter()
            .zip(&result.placements)
            .map(|(part, placed)| {
                let half = pcb_place::rotated_courtyard_half(part, placed.rotation);
                assert!(
                    pcb_place::part_edge_distance(
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
            .map(|(part, placed)| pcb_place::rotated_copper_bbox(part, placed.rotation))
            .collect();
        assert!(pcb_place::is_legal(
            &problem,
            &auto_half,
            &auto_copper,
            pcb_place::courtyard_margin(problem.clearance),
            &auto_positions,
        ));
    }
}
