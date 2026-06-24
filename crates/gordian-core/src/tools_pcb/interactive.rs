//! Interactive IPC board editing.
//!
//! Once the engine has seeded a board (derive_board → place_board → route_board →
//! export_board), `open_board` launches a live headless KiCAD and the geometry
//! tools edit the REAL board over IPC. This is where the LLM directly controls
//! geometry (the engine is the assist that produced the starting point).
//! `autoroute` (Freerouting) is the heavy-duty routing assist for dense boards.

use anyhow::Result;
use serde_json::{Value, json};

use kicad_cli::cli::KicadCli;
use kicad_ipc::proto::kiapi::board::types::BoardLayer;
use kicad_ipc::{footprint_reference, Session};
use kicad_sexpr::pcb::{read_problem, write_solution};

use crate::tools::{PcbToolCtx, require_str};

use super::create::req_num;
use super::export::is_non_copper;

fn ipc_err(e: kicad_ipc::Error) -> anyhow::Error {
    anyhow::anyhow!(e.to_string())
}

fn mm_to_nm(mm: f64) -> i64 {
    (mm * 1_000_000.0).round() as i64
}

/// Parse a copper-layer name ("F.Cu", "B.Cu", "In1.Cu", "top", "bottom").
fn parse_copper_layer(name: &str) -> std::result::Result<BoardLayer, String> {
    Ok(match name.to_ascii_lowercase().replace('.', "_").as_str() {
        "f_cu" | "top" | "front" => BoardLayer::BlFCu,
        "b_cu" | "bottom" | "back" => BoardLayer::BlBCu,
        "in1_cu" | "in1" => BoardLayer::BlIn1Cu,
        "in2_cu" | "in2" => BoardLayer::BlIn2Cu,
        "in3_cu" | "in3" => BoardLayer::BlIn3Cu,
        "in4_cu" | "in4" => BoardLayer::BlIn4Cu,
        other => return Err(format!("unknown copper layer `{other}` (use F.Cu / B.Cu / In1.Cu …)")),
    })
}

/// Open the exported board in a live headless KiCAD for interactive editing.
pub fn open_board(_input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    let path = ctx.pcb_path();
    if !path.exists() {
        return Ok(json!({
            "error": "no .kicad_pcb yet — seed the board first (derive_board → place_board → route_board → export_board), then open_board"
        }));
    }
    match Session::launch_headless(&path) {
        Ok(s) => *ctx.kicad() = Some(s),
        Err(e) => return Ok(json!({ "error": format!("could not open the board in KiCAD: {e}") })),
    }
    board_state(ctx)
}

/// Read the live board: footprints (ref + position mm), track/net counts.
pub fn board_state(ctx: &PcbToolCtx) -> Result<Value> {
    let mut guard = ctx.kicad();
    let Some(session) = guard.as_mut() else {
        return Ok(json!({ "error": "no board open — call open_board first" }));
    };
    let k = session.kicad();
    let fps = k.footprints().map_err(ipc_err)?;
    let tracks = k.tracks().map_err(ipc_err)?;
    let nets = k.nets().map_err(ipc_err)?;
    let parts: Vec<Value> = fps
        .iter()
        .map(|f| {
            let p = f.position.clone().unwrap_or_default();
            json!({
                "reference": footprint_reference(f),
                "x": p.x_nm as f64 / 1e6,
                "y": p.y_nm as f64 / 1e6,
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "footprints": parts.len(),
        "parts": parts,
        "tracks": tracks.len(),
        "nets": nets,
    }))
}

/// Move a part (reference) to (x,y) mm, optional rotation degrees.
pub fn move_part(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    let reference = require_str(&input, "reference")?;
    let x = match req_num(&input, "x", "move_part") { Ok(v) => v, Err(e) => return Ok(json!({ "error": e })) };
    let y = match req_num(&input, "y", "move_part") { Ok(v) => v, Err(e) => return Ok(json!({ "error": e })) };
    let rot = input.get("rotation").and_then(Value::as_f64);
    let mut guard = ctx.kicad();
    let Some(session) = guard.as_mut() else {
        return Ok(json!({ "error": "no board open — call open_board first" }));
    };
    match session.kicad().move_footprint(&reference, mm_to_nm(x), mm_to_nm(y), rot) {
        Ok(()) => Ok(json!({ "ok": true, "reference": reference, "x": x, "y": y })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

/// Route a straight track segment: start `[x,y]`, end `[x,y]` (mm), width (mm),
/// layer (F.Cu/…), optional net.
pub fn route_track(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    let start = input.get("start").and_then(|v| v.as_array());
    let end = input.get("end").and_then(|v| v.as_array());
    let (Some(s), Some(e)) = (start, end) else {
        return Ok(json!({ "error": "route_track needs `start` and `end` as [x,y] mm arrays" }));
    };
    let coord = |a: &[Value], i: usize| a.get(i).and_then(Value::as_f64);
    let (Some(sx), Some(sy), Some(ex), Some(ey)) = (coord(s, 0), coord(s, 1), coord(e, 0), coord(e, 1)) else {
        return Ok(json!({ "error": "start/end must be [x,y] numbers (mm)" }));
    };
    let width = input.get("width").and_then(Value::as_f64).unwrap_or(0.2);
    let layer = match parse_copper_layer(input.get("layer").and_then(Value::as_str).unwrap_or("F.Cu")) {
        Ok(l) => l,
        Err(err) => return Ok(json!({ "error": err })),
    };
    let net = input.get("net").and_then(Value::as_str);
    let mut guard = ctx.kicad();
    let Some(session) = guard.as_mut() else {
        return Ok(json!({ "error": "no board open — call open_board first" }));
    };
    match session.kicad().add_track(
        (mm_to_nm(sx), mm_to_nm(sy)),
        (mm_to_nm(ex), mm_to_nm(ey)),
        mm_to_nm(width),
        layer,
        net,
    ) {
        Ok(()) => Ok(json!({ "ok": true })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

/// Set (or update) a net class with a track width + clearance (mm) and assign
/// nets to it — "wide copper for power". (Note: also achievable per-track via
/// route_track width.)
pub fn set_net_width(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    let name = require_str(&input, "name")?;
    let width = input.get("width").and_then(Value::as_f64).unwrap_or(0.5);
    let clearance = input.get("clearance").and_then(Value::as_f64).unwrap_or(0.2);
    let nets: Vec<String> = input
        .get("nets")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let net_refs: Vec<&str> = nets.iter().map(String::as_str).collect();
    let mut guard = ctx.kicad();
    let Some(session) = guard.as_mut() else {
        return Ok(json!({ "error": "no board open — call open_board first" }));
    };
    match session.kicad().set_net_class(&name, mm_to_nm(width), mm_to_nm(clearance), &net_refs) {
        Ok(()) => Ok(json!({ "ok": true, "net_class": name, "width": width, "nets": nets })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

/// Auto-route the exported board with the Freerouting autorouter (the heavy-duty
/// assist for dense boards the in-house router can't escape). Routes from scratch
/// at the board's design rules, writes the routed copper back to the project
/// `.kicad_pcb`, and reports DRC. Requires an exported (placed) board.
pub fn autoroute(_input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    let board_path = ctx.pcb_path();
    if !board_path.exists() {
        return Ok(json!({ "error": "no .kicad_pcb — run export_board first (derive_board → place_board → export_board → autoroute)" }));
    }
    let problem = match read_problem(&board_path) {
        Ok(p) => p,
        Err(e) => return Ok(json!({ "error": format!("could not read the board: {e}") })),
    };
    let rules = specctra::RouteRules::from_board(&problem);
    let geo = match specctra::freeroute_with_rules(&board_path, rules) {
        Ok(g) => g,
        Err(e) => return Ok(json!({ "error": format!("freerouting failed: {e}") })),
    };
    let (wires, vias) = (geo.wires.len(), geo.vias.len());
    let solution = geo.to_solution(&problem);
    if let Err(e) = write_solution(&board_path, &solution, &problem) {
        return Ok(json!({ "error": format!("could not write the routed board: {e}") }));
    }
    let _ = specctra::write_net_settings(&board_path, rules);

    // DRC via KiCAD (the external authority); copper faults only.
    let mut copper_violations = None;
    let mut unconnected = None;
    if let Ok(report) = KicadCli::new(ctx.env()).drc(&board_path) {
        copper_violations = Some(report.violations.iter().filter(|v| !is_non_copper(v)).count());
        unconnected = Some(report.unconnected_items.len());
    }
    Ok(json!({
        "ok": true,
        "router": "freerouting",
        "wires": wires,
        "vias": vias,
        "copper_violations": copper_violations,
        "unconnected_items": unconnected,
        "note": "routed with Freerouting and written to the board. open_board to inspect/refine, \
                 or render_board. A few honest unconnected nets on a dense board are acceptable.",
    }))
}

/// Save the live KiCAD board to disk if a session is open. Returns whether it saved.
pub fn save_session_if_open(ctx: &PcbToolCtx) -> Result<bool> {
    let mut guard = ctx.kicad();
    if let Some(session) = guard.as_mut() {
        session.kicad().save().map_err(ipc_err)?;
        Ok(true)
    } else {
        Ok(false)
    }
}
