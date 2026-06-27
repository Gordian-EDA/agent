//! Interactive IPC board editing.
//!
//! Once the engine has seeded a board (regenerate_board → place_board → route_board),
//! `open_board` launches or inspects the live KiCAD session and the geometry
//! tools edit the REAL board over IPC. This is where the LLM directly controls
//! geometry (the engine is the assist that produced the starting point).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use geom::{Point2, Rect};
use serde_json::{Value, json};

use kicad_ipc::{
    FootprintMove, proto::kiapi::board::types::BoardLayer, snapshot::IpcBoardSnapshot,
};

use crate::AgentRuntime;
use crate::tools::require_str;

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
        other => {
            return Err(format!(
                "unknown copper layer `{other}` (use F.Cu / B.Cu / In1.Cu …)"
            ));
        }
    })
}

/// Open the project board in a live headless KiCAD for interactive editing.
pub fn open_board(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let path = ctx.pcb_path();
    match ctx.kicad().open(&path) {
        Ok(()) => {}
        Err(e) => return Ok(json!({ "error": format!("could not open the board in KiCAD: {e}") })),
    };
    super::place::get_board(ctx)
}

/// Move one or more live-board parts in a single KiCAD IPC commit.
pub fn move_parts(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let path = ctx.pcb_path();
    match ctx.kicad().with_session(&path, |session| {
        let snapshot = session.kicad().board_snapshot()?;
        let mut board = MoveBoard::from_snapshot(&snapshot);
        let plan = match resolve_move_parts(&input, &mut board) {
            Ok(plan) => plan,
            Err(err) => return Ok(Err(err)),
        };
        session.kicad().move_footprints(&plan.ipc_moves)?;
        Ok(Ok(plan.output()))
    }) {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(err)) => Ok(json!({ "error": err })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

#[derive(Debug, Clone)]
struct MoveBoard {
    bounds: Rect,
    parts: BTreeMap<String, MovePart>,
}

#[derive(Debug, Clone)]
struct MovePart {
    at: Point2,
    rotation: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone)]
struct MovePlan {
    ipc_moves: Vec<FootprintMove>,
    positions: Vec<ResolvedPosition>,
}

#[derive(Debug, Clone)]
struct ResolvedPosition {
    reference: String,
    at: Point2,
    rotation: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
    Above,
    Below,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl MoveBoard {
    fn from_snapshot(snapshot: &IpcBoardSnapshot) -> Self {
        let mut sizes = BTreeMap::new();
        for part in &snapshot.imported.parts {
            let points: Vec<Point2> = snapshot
                .problem
                .obstacles
                .iter()
                .filter(|ob| ob.kind == format!("pad:{}", part.reference))
                .flat_map(|ob| {
                    [
                        Point2::new(ob.center.x - ob.width / 2.0, ob.center.y - ob.height / 2.0),
                        Point2::new(ob.center.x + ob.width / 2.0, ob.center.y + ob.height / 2.0),
                    ]
                })
                .collect();
            let (width, height) = Rect::bounding(&points)
                .map(|r| (r.max_x - r.min_x, r.max_y - r.min_y))
                .unwrap_or((1.0, 1.0));
            sizes.insert(part.reference.clone(), (width.max(1.0), height.max(1.0)));
        }
        let parts = snapshot
            .imported
            .parts
            .iter()
            .map(|part| {
                let (width, height) = sizes.get(&part.reference).copied().unwrap_or((1.0, 1.0));
                (
                    part.reference.clone(),
                    MovePart {
                        at: part.at,
                        rotation: part.rotation as f64,
                        width,
                        height,
                    },
                )
            })
            .collect();
        Self {
            bounds: snapshot.imported.bounds,
            parts,
        }
    }
}

impl MovePlan {
    fn output(&self) -> Value {
        let positions: Vec<Value> = self
            .positions
            .iter()
            .map(|p| {
                json!({
                    "reference": p.reference,
                    "x": p.at.x,
                    "y": p.at.y,
                    "rotation": p.rotation,
                })
            })
            .collect();
        json!({
            "ok": true,
            "moved": self.positions.len(),
            "positions": positions,
        })
    }
}

fn resolve_move_parts(
    input: &Value,
    board: &mut MoveBoard,
) -> std::result::Result<MovePlan, String> {
    let moves = input
        .get("moves")
        .and_then(Value::as_array)
        .ok_or_else(|| "move_parts needs `moves` as a non-empty array".to_owned())?;
    if moves.is_empty() {
        return Err("move_parts needs at least one move".to_owned());
    }

    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    let mut finals = BTreeMap::new();
    for (idx, mv) in moves.iter().enumerate() {
        let ctx = format!("moves[{idx}]");
        let obj = mv
            .as_object()
            .ok_or_else(|| format!("{ctx}: move must be an object"))?;
        let reference = obj
            .get("reference")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{ctx}: missing required string `reference`"))?
            .to_owned();
        if seen.insert(reference.clone()) {
            order.push(reference.clone());
        }

        let current = board
            .parts
            .get(&reference)
            .cloned()
            .ok_or_else(|| format!("{ctx}: unknown footprint `{reference}`"))?;

        let has_to = obj.contains_key("to");
        let has_by = obj.contains_key("by");
        let has_near = obj.contains_key("near");
        let has_edge = obj.contains_key("edge");
        let mode_count = [has_to, has_by, has_near, has_edge]
            .into_iter()
            .filter(|has| *has)
            .count();
        if mode_count > 1 {
            return Err(format!(
                "{ctx}: specify only one movement mode (`to`, `by`, `near`, or `edge`)"
            ));
        }

        let has_horizontal_offset = obj.contains_key("horizontal_offset");
        let has_vertical_offset = obj.contains_key("vertical_offset");
        let rotation = optional_num(mv, "rotation", &ctx)?;
        if mode_count == 0 && rotation.is_none() && !has_horizontal_offset && !has_vertical_offset {
            return Err(format!(
                "{ctx}: move needs a movement mode, `rotation`, or an offset"
            ));
        }

        let mut at = match (has_to, has_by, has_near, has_edge) {
            (true, false, false, false) => parse_point(mv, "to", &ctx)?,
            (false, true, false, false) => {
                let by = parse_point(mv, "by", &ctx)?;
                Point2::new(current.at.x + by.x, current.at.y + by.y)
            }
            (false, false, true, false) => {
                let target_ref = obj
                    .get("near")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("{ctx}: `near` must be a reference string"))?;
                let target = board
                    .parts
                    .get(target_ref)
                    .ok_or_else(|| format!("{ctx}: unknown `near` footprint `{target_ref}`"))?;
                let side = parse_side(mv, "side", &ctx)?;
                let gap = req_num(mv, "gap", &ctx)?;
                near_position(&current, target, side, gap)
            }
            (false, false, false, true) => {
                let edge = parse_edge(mv, "edge", &ctx)?;
                let gap = req_num(mv, "gap", &ctx)?;
                edge_position(&current, board.bounds, edge, gap)
            }
            (false, false, false, false) => current.at,
            _ => unreachable!("mode_count guards multiple movement modes"),
        };

        at.x += optional_num(mv, "horizontal_offset", &ctx)?.unwrap_or(0.0);
        at.y += optional_num(mv, "vertical_offset", &ctx)?.unwrap_or(0.0);
        let final_rotation = rotation.unwrap_or(current.rotation);

        let mut updated = current;
        if rotation_swaps_extents(updated.rotation, final_rotation) {
            std::mem::swap(&mut updated.width, &mut updated.height);
        }
        updated.at = at;
        updated.rotation = final_rotation;
        board.parts.insert(reference.clone(), updated);

        finals.insert(
            reference.clone(),
            ResolvedPosition {
                reference,
                at,
                rotation: final_rotation,
            },
        );
    }

    let positions: Vec<ResolvedPosition> = order
        .into_iter()
        .filter_map(|reference| finals.get(&reference).cloned())
        .collect();
    let ipc_moves = positions
        .iter()
        .map(|p| FootprintMove {
            reference: p.reference.clone(),
            x_nm: mm_to_nm(p.at.x),
            y_nm: mm_to_nm(p.at.y),
            rotation_deg: Some(p.rotation),
        })
        .collect();
    Ok(MovePlan {
        ipc_moves,
        positions,
    })
}

fn parse_point(input: &Value, key: &str, ctx: &str) -> std::result::Result<Point2, String> {
    let values = input
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{ctx}: `{key}` must be [x, y] in mm"))?;
    if values.len() != 2 {
        return Err(format!("{ctx}: `{key}` must be exactly [x, y] in mm"));
    }
    let x = values[0]
        .as_f64()
        .ok_or_else(|| format!("{ctx}: `{key}[0]` must be numeric"))?;
    let y = values[1]
        .as_f64()
        .ok_or_else(|| format!("{ctx}: `{key}[1]` must be numeric"))?;
    Ok(Point2::new(x, y))
}

fn optional_num(input: &Value, key: &str, ctx: &str) -> std::result::Result<Option<f64>, String> {
    input
        .get(key)
        .map(|v| {
            v.as_f64()
                .ok_or_else(|| format!("{ctx}: `{key}` must be numeric"))
        })
        .transpose()
}

fn req_num(input: &Value, key: &str, ctx: &str) -> std::result::Result<f64, String> {
    optional_num(input, key, ctx)?.ok_or_else(|| format!("{ctx}: missing numeric `{key}`"))
}

fn parse_side(input: &Value, key: &str, ctx: &str) -> std::result::Result<Side, String> {
    match input.get(key).and_then(Value::as_str) {
        Some("left") => Ok(Side::Left),
        Some("right") => Ok(Side::Right),
        Some("above") => Ok(Side::Above),
        Some("below") => Ok(Side::Below),
        Some(other) => Err(format!(
            "{ctx}: unknown `{key}` `{other}` (use left/right/above/below)"
        )),
        None => Err(format!("{ctx}: missing `{key}` for `near` placement")),
    }
}

fn parse_edge(input: &Value, key: &str, ctx: &str) -> std::result::Result<Edge, String> {
    match input.get(key).and_then(Value::as_str) {
        Some("left") => Ok(Edge::Left),
        Some("right") => Ok(Edge::Right),
        Some("top") => Ok(Edge::Top),
        Some("bottom") => Ok(Edge::Bottom),
        Some(other) => Err(format!(
            "{ctx}: unknown `{key}` `{other}` (use left/right/top/bottom)"
        )),
        None => Err(format!("{ctx}: `edge` must be left/right/top/bottom")),
    }
}

fn near_position(part: &MovePart, target: &MovePart, side: Side, gap: f64) -> Point2 {
    match side {
        Side::Left => Point2::new(
            target.at.x - target.width / 2.0 - gap - part.width / 2.0,
            target.at.y,
        ),
        Side::Right => Point2::new(
            target.at.x + target.width / 2.0 + gap + part.width / 2.0,
            target.at.y,
        ),
        Side::Above => Point2::new(
            target.at.x,
            target.at.y - target.height / 2.0 - gap - part.height / 2.0,
        ),
        Side::Below => Point2::new(
            target.at.x,
            target.at.y + target.height / 2.0 + gap + part.height / 2.0,
        ),
    }
}

fn edge_position(part: &MovePart, bounds: Rect, edge: Edge, gap: f64) -> Point2 {
    match edge {
        Edge::Left => Point2::new(
            bounds.min_x + gap + part.width / 2.0,
            (bounds.min_y + bounds.max_y) / 2.0,
        ),
        Edge::Right => Point2::new(
            bounds.max_x - gap - part.width / 2.0,
            (bounds.min_y + bounds.max_y) / 2.0,
        ),
        Edge::Top => Point2::new(
            (bounds.min_x + bounds.max_x) / 2.0,
            bounds.min_y + gap + part.height / 2.0,
        ),
        Edge::Bottom => Point2::new(
            (bounds.min_x + bounds.max_x) / 2.0,
            bounds.max_y - gap - part.height / 2.0,
        ),
    }
}

fn rotation_swaps_extents(from: f64, to: f64) -> bool {
    let turns = (to - from) / 90.0;
    let rounded = turns.round();
    if (turns - rounded).abs() > 1e-6 {
        return false;
    }
    (rounded as i64).rem_euclid(2) == 1
}

/// Route a straight track segment: start `[x,y]`, end `[x,y]` (mm), width (mm),
/// layer (F.Cu/…), optional net.
pub fn route_track(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let start = input.get("start").and_then(|v| v.as_array());
    let end = input.get("end").and_then(|v| v.as_array());
    let (Some(s), Some(e)) = (start, end) else {
        return Ok(json!({ "error": "route_track needs `start` and `end` as [x,y] mm arrays" }));
    };
    let coord = |a: &[Value], i: usize| a.get(i).and_then(Value::as_f64);
    let (Some(sx), Some(sy), Some(ex), Some(ey)) =
        (coord(s, 0), coord(s, 1), coord(e, 0), coord(e, 1))
    else {
        return Ok(json!({ "error": "start/end must be [x,y] numbers (mm)" }));
    };
    let width = input.get("width").and_then(Value::as_f64).unwrap_or(0.2);
    let layer =
        match parse_copper_layer(input.get("layer").and_then(Value::as_str).unwrap_or("F.Cu")) {
            Ok(l) => l,
            Err(err) => return Ok(json!({ "error": err })),
        };
    let net = input.get("net").and_then(Value::as_str);
    match ctx.kicad().with_session(&ctx.pcb_path(), |session| {
        session.kicad().add_track(
            (mm_to_nm(sx), mm_to_nm(sy)),
            (mm_to_nm(ex), mm_to_nm(ey)),
            mm_to_nm(width),
            layer,
            net,
        )
    }) {
        Ok(()) => Ok(json!({ "ok": true })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

/// Set (or update) a net class with a track width + clearance (mm) and assign
/// nets to it — "wide copper for power". (Note: also achievable per-track via
/// route_track width.)
pub fn set_net_width(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let name = require_str(&input, "name")?;
    let width = input.get("width").and_then(Value::as_f64).unwrap_or(0.5);
    let clearance = input
        .get("clearance")
        .and_then(Value::as_f64)
        .unwrap_or(0.2);
    let nets: Vec<String> = input
        .get("nets")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let net_refs: Vec<&str> = nets.iter().map(String::as_str).collect();
    match ctx.kicad().with_session(&ctx.pcb_path(), |session| {
        session
            .kicad()
            .set_net_class(&name, mm_to_nm(width), mm_to_nm(clearance), &net_refs)
    }) {
        Ok(()) => Ok(json!({ "ok": true, "net_class": name, "width": width, "nets": nets })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

/// Save the live KiCAD board to disk if a session is open. Returns whether it saved.
pub fn save_session_if_open(ctx: &AgentRuntime) -> Result<bool> {
    ctx.kicad().save_if_open().map_err(ipc_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_board() -> MoveBoard {
        MoveBoard {
            bounds: Rect::new(0.0, 0.0, 100.0, 50.0),
            parts: [
                (
                    "U1".to_owned(),
                    MovePart {
                        at: Point2::new(50.0, 25.0),
                        rotation: 0.0,
                        width: 10.0,
                        height: 8.0,
                    },
                ),
                (
                    "C1".to_owned(),
                    MovePart {
                        at: Point2::new(10.0, 10.0),
                        rotation: 0.0,
                        width: 2.0,
                        height: 1.0,
                    },
                ),
                (
                    "R1".to_owned(),
                    MovePart {
                        at: Point2::new(20.0, 20.0),
                        rotation: 0.0,
                        width: 4.0,
                        height: 2.0,
                    },
                ),
                (
                    "J1".to_owned(),
                    MovePart {
                        at: Point2::new(30.0, 30.0),
                        rotation: 90.0,
                        width: 6.0,
                        height: 10.0,
                    },
                ),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn resolve(input: Value) -> MovePlan {
        let mut board = fixture_board();
        resolve_move_parts(&input, &mut board).expect("move resolution succeeds")
    }

    fn pos(plan: &MovePlan, reference: &str) -> ResolvedPosition {
        plan.positions
            .iter()
            .find(|p| p.reference == reference)
            .cloned()
            .expect("position exists")
    }

    #[test]
    fn resolves_absolute_move_with_rotation() {
        let plan = resolve(json!({
            "moves": [{ "reference": "U1", "to": [25.0, 20.0], "rotation": 180.0 }]
        }));

        let got = pos(&plan, "U1");
        assert_eq!(got.at, Point2::new(25.0, 20.0));
        assert_eq!(got.rotation, 180.0);
        assert_eq!(plan.ipc_moves[0].x_nm, 25_000_000);
        assert_eq!(plan.ipc_moves[0].y_nm, 20_000_000);
        assert_eq!(plan.ipc_moves[0].rotation_deg, Some(180.0));
    }

    #[test]
    fn resolves_nudge_and_rotate_only() {
        let plan = resolve(json!({
            "moves": [
                { "reference": "R1", "by": [0.0, -2.0], "rotation": 90.0 },
                { "reference": "J1", "rotation": 180.0 }
            ]
        }));

        let r1 = pos(&plan, "R1");
        assert_eq!(r1.at, Point2::new(20.0, 18.0));
        assert_eq!(r1.rotation, 90.0);
        let j1 = pos(&plan, "J1");
        assert_eq!(j1.at, Point2::new(30.0, 30.0));
        assert_eq!(j1.rotation, 180.0);
    }

    #[test]
    fn resolves_near_with_offsets() {
        let plan = resolve(json!({
            "moves": [{
                "reference": "C1",
                "near": "U1",
                "side": "above",
                "gap": 1.5,
                "horizontal_offset": 3.0,
                "vertical_offset": -1.0
            }]
        }));

        let c1 = pos(&plan, "C1");
        assert_eq!(c1.at, Point2::new(53.0, 18.0));
        assert_eq!(c1.rotation, 0.0);
    }

    #[test]
    fn resolves_edge_with_offset_and_rotation() {
        let plan = resolve(json!({
            "moves": [{
                "reference": "J1",
                "edge": "left",
                "gap": 2.0,
                "vertical_offset": -8.0,
                "rotation": 180.0
            }]
        }));

        let j1 = pos(&plan, "J1");
        assert_eq!(j1.at, Point2::new(5.0, 17.0));
        assert_eq!(j1.rotation, 180.0);
    }

    #[test]
    fn sequential_moves_see_earlier_positions_and_duplicate_refs_collapse() {
        let plan = resolve(json!({
            "moves": [
                { "reference": "U1", "to": [40.0, 20.0] },
                { "reference": "C1", "near": "U1", "side": "right", "gap": 2.0 },
                { "reference": "U1", "by": [1.0, 1.0], "rotation": 90.0 }
            ]
        }));

        assert_eq!(plan.positions.len(), 2);
        let u1 = pos(&plan, "U1");
        assert_eq!(u1.at, Point2::new(41.0, 21.0));
        assert_eq!(u1.rotation, 90.0);
        let c1 = pos(&plan, "C1");
        assert_eq!(c1.at, Point2::new(48.0, 20.0));
    }

    #[test]
    fn rejects_invalid_input_without_moves() {
        let mut board = fixture_board();
        assert!(resolve_move_parts(&json!({ "moves": [] }), &mut board).is_err());
    }

    #[test]
    fn rejects_unknown_reference_and_near_target() {
        let mut board = fixture_board();
        assert!(
            resolve_move_parts(
                &json!({ "moves": [{ "reference": "NOPE", "to": [1.0, 2.0] }] }),
                &mut board
            )
            .unwrap_err()
            .contains("unknown footprint")
        );

        let mut board = fixture_board();
        assert!(
            resolve_move_parts(
                &json!({
                    "moves": [{ "reference": "C1", "near": "NOPE", "side": "left", "gap": 1.0 }]
                }),
                &mut board
            )
            .unwrap_err()
            .contains("unknown `near` footprint")
        );
    }

    #[test]
    fn rejects_multiple_modes_and_empty_noop() {
        let mut board = fixture_board();
        assert!(
            resolve_move_parts(
                &json!({
                    "moves": [{ "reference": "C1", "to": [1.0, 2.0], "by": [1.0, 0.0] }]
                }),
                &mut board
            )
            .unwrap_err()
            .contains("only one movement mode")
        );

        let mut board = fixture_board();
        assert!(
            resolve_move_parts(&json!({ "moves": [{ "reference": "C1" }] }), &mut board)
                .unwrap_err()
                .contains("needs a movement mode")
        );
    }
}
