//! Interactive IPC board editing.
//!
//! Once the engine has seeded a board (regenerate_board → place_board → route_board),
//! `open_board` launches or inspects the live KiCAD session and the geometry
//! tools edit the REAL board over IPC. This is where the LLM directly controls
//! geometry (the engine is the assist that produced the starting point).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use geom::{Point2, Rect, Segment};
use serde_json::{Value, json};

use kicad_ipc::{
    FootprintMove,
    proto::kiapi::{
        board::types::{BoardLayer, PadStackShape, Track, Via as IpcVia},
        common::types::{KiCadObjectType, Vector2},
    },
    snapshot::IpcBoardSnapshot,
};
use pcb_model::{
    Connection, LayerRef, RoutePoint, RouteProblem, RouteSolution, Router, Trace, Via, ViaSpan,
};

use crate::AgentRuntime;
use crate::tools::require_str;

fn ipc_err(e: kicad_ipc::Error) -> anyhow::Error {
    anyhow::anyhow!(e.to_string())
}

fn mm_to_nm(mm: f64) -> i64 {
    (mm * 1_000_000.0).round() as i64
}

/// Open the project board in a live headless KiCAD for interactive editing.
pub fn open_board(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let path = ctx.pcb_path();
    match ctx.kicad().open(&path) {
        Ok(()) => {}
        Err(e) => return Ok(json!({ "error": format!("could not open the board in KiCAD: {e}") })),
    };
    super::place::get_board(json!({}), ctx)
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
        session.kicad().save()?;
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

/// Route a single live-board connection with grid-A* obstacle avoidance.
pub fn route_track(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let path = ctx.pcb_path();
    match ctx.kicad().with_session(&path, |session| {
        let snapshot = session.kicad().board_snapshot()?;
        let request = match parse_route_track_request(&input, &snapshot.problem) {
            Ok(request) => request,
            Err(err) => return Ok(Err(err)),
        };
        let (problem, solution) = match manual_route_solution(&snapshot.problem, &request) {
            Ok(route) => route,
            Err(err) => return Ok(Err(err)),
        };
        session
            .kicad()
            .create_route_solution(&problem, &solution, &snapshot.layer_names)?;
        Ok(Ok(route_track_output(&problem, &solution, &request)))
    }) {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(err)) => Ok(json!({ "error": err })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

/// Delete live-board track/via copper near a click point.
pub fn delete_copper(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let path = ctx.pcb_path();
    match ctx.kicad().with_session(&path, |session| {
        let snapshot = session.kicad().board_snapshot()?;
        let request = match parse_delete_copper_request(&input, snapshot.layer_names.len() as u32) {
            Ok(request) => request,
            Err(err) => return Ok(Err(err)),
        };
        let mut types = Vec::new();
        if request.kinds.contains(&CopperKind::Track) {
            types.push(KiCadObjectType::KotPcbTrace);
        }
        if request.kinds.contains(&CopperKind::Via) {
            types.push(KiCadObjectType::KotPcbVia);
        }
        let packed = session.kicad().get_items(&types)?;
        let plan = resolve_delete_copper(&request, &packed, &snapshot.layer_names);
        if !plan.items.is_empty() {
            session
                .kicad()
                .commit("delete copper", |k| k.delete_packed_items(&plan.items))?;
            session.kicad().save()?;
        }
        Ok(Ok(delete_copper_output(&request, &plan)))
    }) {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(err)) => Ok(json!({ "error": err })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

/// Set (or update) a net class with a track width + clearance (mm) and assign
/// nets to it — "wide copper for power". (Note: also achievable per-track via
/// route_track width.)
pub fn set_net_width(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    if !net_class_update_supported(&ctx.env().cli_version) {
        return Ok(json!({
            "error": format!(
                "unsupported KiCAD IPC operation: KiCAD {} does not reliably support SetNetClasses; set_net_width requires KiCAD 9.0.3+ or KiCAD 10",
                ctx.env().cli_version
            )
        }));
    }
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
            .set_net_class(&name, mm_to_nm(width), mm_to_nm(clearance), &net_refs)?;
        session.kicad().save()
    }) {
        Ok(()) => Ok(json!({ "ok": true, "net_class": name, "width": width, "nets": nets })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

fn net_class_update_supported(version: &str) -> bool {
    let mut components = version.split('.').map(|part| {
        part.chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });
    let major = components.next().unwrap_or(0);
    let minor = components.next().unwrap_or(0);
    let patch = components.next().unwrap_or(0);
    major >= 10 || (major == 9 && (minor > 0 || patch >= 3))
}

/// Save the live KiCAD board to disk if a session is open. Returns whether it saved.
pub fn save_session_if_open(ctx: &AgentRuntime) -> Result<bool> {
    ctx.kicad().save_if_open().map_err(ipc_err)
}

#[derive(Debug, Clone)]
struct RouteTrackRequest {
    from: Point2,
    to: Point2,
    net: String,
    from_layer: LayerRef,
    to_layer: LayerRef,
    width: f64,
    vias: Vec<RouteViaAnchor>,
}

#[derive(Debug, Clone)]
struct RouteViaAnchor {
    at: Point2,
    to_layer: LayerRef,
}

fn parse_route_track_request(
    input: &Value,
    problem: &RouteProblem,
) -> std::result::Result<RouteTrackRequest, String> {
    let ctx = "route_track";
    if input.get("start").is_some() || input.get("end").is_some() || input.get("layer").is_some() {
        return Err(
            "route_track now uses `from`, `to`, `net`, `from_layer`, and `to_layer`; legacy `start`/`end`/`layer` are not accepted"
                .to_owned(),
        );
    }
    let from = parse_point(input, "from", ctx)?;
    let to = parse_point(input, "to", ctx)?;
    let net = input
        .get("net")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "route_track needs non-empty string `net`".to_owned())?
        .to_owned();
    let from_layer = parse_route_layer_ref(
        input
            .get("from_layer")
            .and_then(Value::as_str)
            .unwrap_or("F.Cu"),
        problem.layer_count,
    )?;
    let to_layer = match input.get("to_layer").and_then(Value::as_str) {
        Some(layer) => parse_route_layer_ref(layer, problem.layer_count)?,
        None => from_layer.clone(),
    };
    let width = optional_num(input, "width", ctx)?.unwrap_or_else(|| problem.net_width(&net));
    if width <= 0.0 {
        return Err("route_track `width` must be positive".to_owned());
    }
    if width + geom::EPS < problem.min_trace_width {
        return Err(format!(
            "route_track width {width} mm is below board minimum trace width {} mm",
            problem.min_trace_width
        ));
    }

    let vias = input
        .get("vias")
        .map(|v| {
            let array = v
                .as_array()
                .ok_or_else(|| "route_track `vias` must be an array".to_owned())?;
            array
                .iter()
                .enumerate()
                .map(|(idx, via)| {
                    let via_ctx = format!("vias[{idx}]");
                    let at = parse_point(via, "at", &via_ctx)?;
                    let to_layer = via
                        .get("to_layer")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("{via_ctx}: missing string `to_layer`"))
                        .and_then(|layer| parse_route_layer_ref(layer, problem.layer_count))?;
                    Ok(RouteViaAnchor { at, to_layer })
                })
                .collect::<std::result::Result<Vec<_>, String>>()
        })
        .transpose()?
        .unwrap_or_default();

    Ok(RouteTrackRequest {
        from,
        to,
        net,
        from_layer,
        to_layer,
        width,
        vias,
    })
}

fn manual_route_solution(
    base: &RouteProblem,
    request: &RouteTrackRequest,
) -> std::result::Result<(RouteProblem, RouteSolution), String> {
    let mut solution = RouteSolution {
        traces: Vec::new(),
        vias: Vec::new(),
    };
    let mut current_at = request.from;
    let mut current_layer = request.from_layer.clone();

    for anchor in &request.vias {
        if current_layer == anchor.to_layer {
            return Err(format!(
                "explicit via at [{}, {}] does not change layers",
                anchor.at.x, anchor.at.y
            ));
        }
        let leg = route_leg(
            base,
            &request.net,
            request.width,
            current_at,
            current_layer.clone(),
            anchor.at,
            current_layer.clone(),
        )?;
        extend_solution(&mut solution, leg);
        solution.vias.push(Via {
            connection: request.net.clone(),
            at: anchor.at,
            diameter: base.via_diameter,
            drill: base.via_drill,
            span: via_span_between(&current_layer, &anchor.to_layer, base.layer_count)?,
        });
        current_at = anchor.at;
        current_layer = anchor.to_layer.clone();
    }

    let leg = route_leg(
        base,
        &request.net,
        request.width,
        current_at,
        current_layer,
        request.to,
        request.to_layer.clone(),
    )?;
    extend_solution(&mut solution, leg);

    let problem = validation_problem(base, request);
    validate_manual_solution(&problem, &solution)?;
    Ok((problem, solution))
}

fn route_leg(
    base: &RouteProblem,
    net: &str,
    width: f64,
    from: Point2,
    from_layer: LayerRef,
    to: Point2,
    to_layer: LayerRef,
) -> std::result::Result<RouteSolution, String> {
    let exact_problem = single_connection_problem(
        base,
        net,
        width,
        from,
        from_layer.clone(),
        to,
        to_layer.clone(),
    );
    let grid_from = snapped_route_point(&exact_problem, from);
    let grid_to = snapped_route_point(&exact_problem, to);
    let grid_problem =
        single_connection_problem(base, net, width, grid_from, from_layer, grid_to, to_layer);
    let router = grid_astar::router::GridAStarRouter;
    let result = router.route(&grid_problem);
    if !result.failed.is_empty() {
        let reasons = result
            .failed
            .iter()
            .map(|f| f.reason.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("route_track could not route `{net}`: {reasons}"));
    }
    let mut solution = result.solution;
    add_manual_terminal_stubs(&exact_problem, &mut solution);
    validate_manual_solution(&exact_problem, &solution)?;
    Ok(solution)
}

fn snapped_route_point(problem: &RouteProblem, point: Point2) -> Point2 {
    let pitch = grid_astar::grid::grid_pitch(problem);
    Point2::new(
        route_cell_center(problem.bounds.min_x, point.x, pitch),
        route_cell_center(problem.bounds.min_y, point.y, pitch),
    )
}

fn single_connection_problem(
    base: &RouteProblem,
    net: &str,
    width: f64,
    from: Point2,
    from_layer: LayerRef,
    to: Point2,
    to_layer: LayerRef,
) -> RouteProblem {
    let mut problem = base.clone();
    problem.connections = vec![Connection {
        name: net.to_owned(),
        points_to_connect: vec![
            RoutePoint {
                x: from.x,
                y: from.y,
                layer: from_layer,
            },
            RoutePoint {
                x: to.x,
                y: to.y,
                layer: to_layer,
            },
        ],
    }];
    problem.net_widths.insert(net.to_owned(), width);
    problem.escape_layers.clear();
    problem
}

fn validation_problem(base: &RouteProblem, request: &RouteTrackRequest) -> RouteProblem {
    single_connection_problem(
        base,
        &request.net,
        request.width,
        request.from,
        request.from_layer.clone(),
        request.to,
        request.to_layer.clone(),
    )
}

fn extend_solution(dst: &mut RouteSolution, src: RouteSolution) {
    dst.traces.extend(src.traces);
    dst.vias.extend(src.vias);
}

fn add_manual_terminal_stubs(problem: &RouteProblem, solution: &mut RouteSolution) {
    let pitch = grid_astar::grid::grid_pitch(problem);
    for conn in &problem.connections {
        let width = problem.net_width(&conn.name);
        for point in &conn.points_to_connect {
            let exact = Point2::new(point.x, point.y);
            let center = Point2::new(
                route_cell_center(problem.bounds.min_x, point.x, pitch),
                route_cell_center(problem.bounds.min_y, point.y, pitch),
            );
            if exact.near_eq(center, geom::EPS) {
                continue;
            }
            solution.traces.push(Trace {
                connection: conn.name.clone(),
                layer: point.layer.clone(),
                width,
                path: vec![exact, center],
            });
        }
    }
}

fn route_cell_center(min: f64, value: f64, pitch: f64) -> f64 {
    let idx = ((value - min) / pitch).floor().max(0.0);
    min + (idx + 0.5) * pitch
}

fn validate_manual_solution(
    problem: &RouteProblem,
    solution: &RouteSolution,
) -> std::result::Result<(), String> {
    let violations = drc_lint::lint::lint(problem, solution);
    if violations.is_empty() {
        return Ok(());
    }
    let first = violations
        .first()
        .and_then(|v| serde_json::to_string(v).ok())
        .unwrap_or_else(|| "unknown violation".to_owned());
    Err(format!(
        "route_track validation failed with {} DRC/connectivity violation(s); first: {first}",
        violations.len()
    ))
}

fn via_span_between(
    from: &LayerRef,
    to: &LayerRef,
    layer_count: u32,
) -> std::result::Result<ViaSpan, String> {
    let from = from
        .index(layer_count)
        .ok_or_else(|| format!("invalid via start layer `{}`", from.0))?;
    let to = to
        .index(layer_count)
        .ok_or_else(|| format!("invalid via target layer `{}`", to.0))?;
    if from == to {
        return Err("via start and target layers are the same".to_owned());
    }
    let (lo, hi) = (from.min(to), from.max(to));
    if lo == 0 && hi + 1 == layer_count.max(1) {
        Ok(ViaSpan::Through)
    } else {
        Ok(ViaSpan::Partial {
            from,
            to,
            micro: false,
        })
    }
}

fn route_track_output(
    problem: &RouteProblem,
    solution: &RouteSolution,
    request: &RouteTrackRequest,
) -> Value {
    let metrics = solution.metrics();
    json!({
        "ok": true,
        "router": "grid-astar",
        "net": request.net,
        "from_layer": route_layer_name(&request.from_layer, problem.layer_count),
        "to_layer": route_layer_name(&request.to_layer, problem.layer_count),
        "width": request.width,
        "tracks": solution.traces.len(),
        "vias": solution.vias.len(),
        "metrics": {
            "wirelength": metrics.wirelength,
            "vias": metrics.via_count,
            "traces": metrics.trace_count,
        },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CopperKind {
    Track,
    Via,
}

#[derive(Debug, Clone)]
struct DeleteCopperRequest {
    at: Point2,
    radius: f64,
    kinds: BTreeSet<CopperKind>,
    net: Option<String>,
    layer: Option<u32>,
    all: bool,
}

#[derive(Debug, Clone)]
struct DeleteCopperPlan {
    items: Vec<prost_types::Any>,
    hits: Vec<CopperHit>,
}

#[derive(Debug, Clone)]
struct CopperHit {
    kind: CopperKind,
    distance: f64,
    net: Option<String>,
    layer: Option<String>,
    layers: Vec<String>,
    at: Option<Point2>,
    start: Option<Point2>,
    end: Option<Point2>,
}

fn parse_delete_copper_request(
    input: &Value,
    layer_count: u32,
) -> std::result::Result<DeleteCopperRequest, String> {
    let ctx = "delete_copper";
    let at = parse_point(input, "at", ctx)?;
    let radius = optional_num(input, "radius", ctx)?.unwrap_or(0.4);
    if radius < 0.0 {
        return Err("delete_copper `radius` must be non-negative".to_owned());
    }
    let kinds = parse_copper_kinds(input)?;
    let net = input
        .get("net")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let layer = input
        .get("layer")
        .and_then(Value::as_str)
        .map(|layer| {
            parse_route_layer_ref(layer, layer_count).and_then(|layer_ref| {
                layer_ref
                    .index(layer_count)
                    .ok_or_else(|| format!("unknown copper layer `{layer}`"))
            })
        })
        .transpose()?;
    let all = input.get("all").and_then(Value::as_bool).unwrap_or(false);
    Ok(DeleteCopperRequest {
        at,
        radius,
        kinds,
        net,
        layer,
        all,
    })
}

fn parse_copper_kinds(input: &Value) -> std::result::Result<BTreeSet<CopperKind>, String> {
    let Some(kinds) = input.get("kinds") else {
        return Ok([CopperKind::Track, CopperKind::Via].into_iter().collect());
    };
    let array = kinds
        .as_array()
        .ok_or_else(|| "delete_copper `kinds` must be an array".to_owned())?;
    if array.is_empty() {
        return Err("delete_copper `kinds` must not be empty".to_owned());
    }
    let mut out = BTreeSet::new();
    for kind in array {
        match kind.as_str() {
            Some("track") => {
                out.insert(CopperKind::Track);
            }
            Some("via") => {
                out.insert(CopperKind::Via);
            }
            Some(other) => {
                return Err(format!(
                    "delete_copper unknown kind `{other}` (use track/via)"
                ));
            }
            None => return Err("delete_copper `kinds` entries must be strings".to_owned()),
        }
    }
    Ok(out)
}

fn resolve_delete_copper(
    request: &DeleteCopperRequest,
    packed: &[prost_types::Any],
    layer_names: &[String],
) -> DeleteCopperPlan {
    let mut matches: Vec<(prost_types::Any, CopperHit)> = packed
        .iter()
        .filter_map(|any| copper_hit(any, request, layer_names).map(|hit| (any.clone(), hit)))
        .filter(|(_, hit)| hit.distance <= request.radius + geom::EPS)
        .collect();
    matches.sort_by(|a, b| {
        a.1.distance
            .total_cmp(&b.1.distance)
            .then_with(|| a.1.kind.cmp(&b.1.kind))
    });
    if !request.all {
        matches.truncate(1);
    }
    let (items, hits): (Vec<_>, Vec<_>) = matches.into_iter().unzip();
    DeleteCopperPlan { items, hits }
}

fn copper_hit(
    any: &prost_types::Any,
    request: &DeleteCopperRequest,
    layer_names: &[String],
) -> Option<CopperHit> {
    if request.kinds.contains(&CopperKind::Track)
        && let Ok(track) = any.to_msg::<Track>()
    {
        return track_hit(&track, request, layer_names);
    }
    if request.kinds.contains(&CopperKind::Via)
        && let Ok(via) = any.to_msg::<IpcVia>()
    {
        return via_hit(&via, request, layer_names);
    }
    None
}

fn track_hit(
    track: &Track,
    request: &DeleteCopperRequest,
    layer_names: &[String],
) -> Option<CopperHit> {
    let (Some(start), Some(end)) = (&track.start, &track.end) else {
        return None;
    };
    let net = track
        .net
        .as_ref()
        .map(|n| n.name.clone())
        .filter(|n| !n.is_empty());
    if let Some(filter) = &request.net
        && net.as_deref() != Some(filter.as_str())
    {
        return None;
    }
    let layer_idx = board_layer_index(track.layer, layer_names)?;
    if let Some(filter) = request.layer
        && filter != layer_idx
    {
        return None;
    }
    let start = point_from_ipc(start);
    let end = point_from_ipc(end);
    let width = nm_to_mm(track.width.as_ref().map(|w| w.value_nm).unwrap_or(0));
    let distance = (Segment::new(start, end).dist_to_point(request.at) - width / 2.0).max(0.0);
    Some(CopperHit {
        kind: CopperKind::Track,
        distance,
        net,
        layer: Some(layer_name_from_index(layer_idx, layer_names)),
        layers: Vec::new(),
        at: None,
        start: Some(start),
        end: Some(end),
    })
}

fn via_hit(
    via: &IpcVia,
    request: &DeleteCopperRequest,
    layer_names: &[String],
) -> Option<CopperHit> {
    let position = via.position.as_ref()?;
    let net = via
        .net
        .as_ref()
        .map(|n| n.name.clone())
        .filter(|n| !n.is_empty());
    if let Some(filter) = &request.net
        && net.as_deref() != Some(filter.as_str())
    {
        return None;
    }
    let indices = via_layer_indices(via, layer_names);
    if let Some(filter) = request.layer
        && !indices.contains(&filter)
    {
        return None;
    }
    let at = point_from_ipc(position);
    let diameter = via_diameter(via);
    let distance = (at.dist(request.at) - diameter / 2.0).max(0.0);
    Some(CopperHit {
        kind: CopperKind::Via,
        distance,
        net,
        layer: None,
        layers: indices
            .iter()
            .map(|idx| layer_name_from_index(*idx, layer_names))
            .collect(),
        at: Some(at),
        start: None,
        end: None,
    })
}

fn delete_copper_output(request: &DeleteCopperRequest, plan: &DeleteCopperPlan) -> Value {
    let matches: Vec<Value> = plan.hits.iter().map(copper_hit_json).collect();
    json!({
        "ok": true,
        "deleted": plan.items.len(),
        "all": request.all,
        "matches": matches,
    })
}

fn copper_hit_json(hit: &CopperHit) -> Value {
    let kind = match hit.kind {
        CopperKind::Track => "track",
        CopperKind::Via => "via",
    };
    json!({
        "kind": kind,
        "distance": hit.distance,
        "net": hit.net,
        "layer": hit.layer,
        "layers": hit.layers,
        "at": hit.at.map(|p| json!([p.x, p.y])),
        "start": hit.start.map(|p| json!([p.x, p.y])),
        "end": hit.end.map(|p| json!([p.x, p.y])),
    })
}

fn parse_route_layer_ref(name: &str, layer_count: u32) -> std::result::Result<LayerRef, String> {
    let canonical = canonical_layer_name(name).ok_or_else(|| {
        format!("unknown copper layer `{name}` (use F.Cu / B.Cu / In1.Cu / top / bottom)")
    })?;
    let (idx, _) = LayerRef::resolve(&canonical, layer_count).ok_or_else(|| {
        format!(
            "copper layer `{name}` is not available on a {}-layer board",
            layer_count.max(1)
        )
    })?;
    Ok(layer_ref_from_index(idx, layer_count))
}

fn canonical_layer_name(name: &str) -> Option<String> {
    let raw = name.trim();
    let norm = raw.to_ascii_lowercase().replace(['.', '-'], "_");
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
    let inner = norm
        .strip_prefix("in")?
        .strip_suffix("_cu")
        .unwrap_or_else(|| {
            // `in1` has no suffix; `in1_cu` was handled by strip_suffix above.
            norm.strip_prefix("in").unwrap_or("")
        });
    inner.parse::<u32>().ok().map(|idx| format!("In{idx}.Cu"))
}

fn layer_ref_from_index(idx: u32, layer_count: u32) -> LayerRef {
    if idx == 0 {
        LayerRef::top()
    } else if idx + 1 == layer_count.max(1) {
        LayerRef::bottom()
    } else {
        LayerRef(format!("inner{idx}"))
    }
}

fn route_layer_name(layer: &LayerRef, layer_count: u32) -> String {
    layer
        .index(layer_count)
        .map(|idx| layer_name_from_index(idx, &copper_layer_names(layer_count)))
        .unwrap_or_else(|| layer.0.clone())
}

fn copper_layer_names(layer_count: u32) -> Vec<String> {
    let count = layer_count.max(2);
    let mut names = Vec::with_capacity(count as usize);
    names.push("F.Cu".to_owned());
    for idx in 1..count.saturating_sub(1) {
        names.push(format!("In{idx}.Cu"));
    }
    names.push("B.Cu".to_owned());
    names
}

fn board_layer_index(layer: i32, layer_names: &[String]) -> Option<u32> {
    let name = board_layer_name(layer);
    layer_names
        .iter()
        .position(|n| n == &name)
        .map(|i| i as u32)
}

fn board_layer_name(layer: i32) -> String {
    match BoardLayer::try_from(layer).ok() {
        Some(BoardLayer::BlFCu) => "F.Cu".to_owned(),
        Some(BoardLayer::BlBCu) => "B.Cu".to_owned(),
        Some(inner)
            if (BoardLayer::BlIn1Cu as i32..=BoardLayer::BlIn30Cu as i32)
                .contains(&(inner as i32)) =>
        {
            format!("In{}.Cu", inner as i32 - BoardLayer::BlFCu as i32)
        }
        _ => format!("layer:{layer}"),
    }
}

fn layer_name_from_index(idx: u32, layer_names: &[String]) -> String {
    layer_names.get(idx as usize).cloned().unwrap_or_else(|| {
        if idx == 0 {
            "F.Cu".to_owned()
        } else {
            format!("In{idx}.Cu")
        }
    })
}

fn via_layer_indices(via: &IpcVia, layer_names: &[String]) -> Vec<u32> {
    let Some(stack) = &via.pad_stack else {
        return (0..layer_names.len().max(2) as u32).collect();
    };
    let Some(drill) = &stack.drill else {
        return (0..layer_names.len().max(2) as u32).collect();
    };
    let Some(start) = board_layer_index(drill.start_layer, layer_names) else {
        return (0..layer_names.len().max(2) as u32).collect();
    };
    let Some(end) = board_layer_index(drill.end_layer, layer_names) else {
        return (0..layer_names.len().max(2) as u32).collect();
    };
    let (lo, hi) = (start.min(end), start.max(end));
    (lo..=hi).collect()
}

fn via_diameter(via: &IpcVia) -> f64 {
    via.pad_stack
        .as_ref()
        .and_then(|s| {
            s.copper_layers
                .iter()
                .find(|l| {
                    matches!(
                        PadStackShape::try_from(l.shape),
                        Ok(PadStackShape::PssCircle
                            | PadStackShape::PssRectangle
                            | PadStackShape::PssOval
                            | PadStackShape::PssRoundrect)
                    )
                })
                .or_else(|| s.copper_layers.iter().find(|l| l.size.is_some()))
        })
        .and_then(|l| l.size.as_ref())
        .map(|s| nm_to_mm(s.x_nm.max(s.y_nm)))
        .unwrap_or(0.6)
}

fn point_from_ipc(v: &Vector2) -> Point2 {
    Point2::new(nm_to_mm(v.x_nm), nm_to_mm(v.y_nm))
}

fn nm_to_mm(nm: i64) -> f64 {
    nm as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_ipc::proto::kiapi::{
        board::types::{
            DrillProperties, DrillShape, Net, NetCode, PadStack, PadStackLayer, PadStackType,
            ViaType,
        },
        common::types::{Distance, Kiid},
    };

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

    #[test]
    fn net_class_update_version_gate_rejects_known_hanging_kicad() {
        assert!(!net_class_update_supported("9.0.2+dfsg-1"));
        assert!(net_class_update_supported("9.0.3"));
        assert!(net_class_update_supported("9.1.0"));
        assert!(net_class_update_supported("10.0.0"));
    }

    fn route_problem(obstacles: Vec<pcb_model::Obstacle>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections: vec![],
            bounds: pcb_model::Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 5.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
        }
    }

    fn obstacle(
        center: Point2,
        width: f64,
        height: f64,
        layers: Vec<LayerRef>,
    ) -> pcb_model::Obstacle {
        pcb_model::Obstacle {
            kind: "keepout".to_owned(),
            layers,
            center,
            width,
            height,
            connected_to: vec!["OTHER".to_owned()],
        }
    }

    #[test]
    fn manual_route_avoids_foreign_obstacle() {
        let problem = route_problem(vec![obstacle(
            Point2::new(5.0, 1.0),
            0.8,
            2.0,
            vec![LayerRef::top(), LayerRef::bottom()],
        )]);
        let request = parse_route_track_request(
            &json!({
                "from": [1.0, 1.0],
                "to": [9.0, 1.0],
                "net": "SIG",
                "from_layer": "F.Cu",
                "width": 0.2
            }),
            &problem,
        )
        .unwrap();

        let (routed_problem, solution) = manual_route_solution(&problem, &request).unwrap();

        assert!(
            drc_lint::lint::lint(&routed_problem, &solution).is_empty(),
            "manual route must validate cleanly"
        );
        assert!(
            solution.metrics().wirelength > 8.0,
            "route should detour around the obstacle: {:?}",
            solution.metrics()
        );
    }

    #[test]
    fn manual_route_places_explicit_via_anchor() {
        let problem = route_problem(vec![]);
        let request = parse_route_track_request(
            &json!({
                "from": [1.0, 1.0],
                "to": [9.0, 1.0],
                "net": "SIG",
                "from_layer": "F.Cu",
                "to_layer": "B.Cu",
                "vias": [{ "at": [5.0, 1.0], "to_layer": "B.Cu" }]
            }),
            &problem,
        )
        .unwrap();

        let (routed_problem, solution) = manual_route_solution(&problem, &request).unwrap();

        assert!(
            drc_lint::lint::lint(&routed_problem, &solution).is_empty(),
            "explicit-via route must validate cleanly"
        );
        let via = solution
            .vias
            .iter()
            .find(|via| via.at.near_eq(Point2::new(5.0, 1.0), 1e-9))
            .expect("explicit via at requested coordinate");
        assert_eq!(via.span, ViaSpan::Through);
    }

    #[test]
    fn manual_route_reports_unroutable_without_solution() {
        let problem = route_problem(vec![obstacle(
            Point2::new(5.0, 2.5),
            0.8,
            5.0,
            vec![LayerRef::top(), LayerRef::bottom()],
        )]);
        let request = parse_route_track_request(
            &json!({
                "from": [1.0, 2.0],
                "to": [9.0, 2.0],
                "net": "SIG"
            }),
            &problem,
        )
        .unwrap();

        let err = manual_route_solution(&problem, &request).unwrap_err();

        assert!(err.contains("could not route"), "{err}");
    }

    #[test]
    fn route_track_rejects_legacy_start_end_layer_input() {
        let problem = route_problem(vec![]);

        let err = parse_route_track_request(
            &json!({
                "start": [1.0, 1.0],
                "end": [9.0, 1.0],
                "layer": "F.Cu",
                "net": "SIG"
            }),
            &problem,
        )
        .unwrap_err();

        assert!(err.contains("legacy `start`/`end`/`layer`"), "{err}");
    }

    fn ipc_point(x: f64, y: f64) -> Vector2 {
        Vector2 {
            x_nm: mm_to_nm(x),
            y_nm: mm_to_nm(y),
        }
    }

    fn ipc_net(name: &str) -> Net {
        Net {
            code: Some(NetCode { value: 1 }),
            name: name.to_owned(),
        }
    }

    fn packed_track(
        id: &str,
        net: &str,
        layer: BoardLayer,
        start: Point2,
        end: Point2,
    ) -> prost_types::Any {
        prost_types::Any::from_msg(&Track {
            id: Some(Kiid {
                value: id.to_owned(),
            }),
            start: Some(ipc_point(start.x, start.y)),
            end: Some(ipc_point(end.x, end.y)),
            width: Some(Distance {
                value_nm: mm_to_nm(0.2),
            }),
            layer: layer as i32,
            net: Some(ipc_net(net)),
            ..Default::default()
        })
        .unwrap()
    }

    fn packed_via(id: &str, net: &str, at: Point2) -> prost_types::Any {
        prost_types::Any::from_msg(&IpcVia {
            id: Some(Kiid {
                value: id.to_owned(),
            }),
            position: Some(ipc_point(at.x, at.y)),
            pad_stack: Some(PadStack {
                r#type: PadStackType::PstNormal as i32,
                layers: vec![BoardLayer::BlFCu as i32, BoardLayer::BlBCu as i32],
                drill: Some(DrillProperties {
                    start_layer: BoardLayer::BlFCu as i32,
                    end_layer: BoardLayer::BlBCu as i32,
                    diameter: Some(ipc_point(0.3, 0.3)),
                    shape: DrillShape::DsCircle as i32,
                }),
                copper_layers: vec![PadStackLayer {
                    layer: BoardLayer::BlFCu as i32,
                    shape: PadStackShape::PssCircle as i32,
                    size: Some(ipc_point(0.6, 0.6)),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            net: Some(ipc_net(net)),
            r#type: ViaType::VtThrough as i32,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn delete_copper_picks_nearest_unless_all_is_set() {
        let layer_names = vec!["F.Cu".to_owned(), "B.Cu".to_owned()];
        let items = vec![
            packed_track(
                "track-1",
                "SIG",
                BoardLayer::BlFCu,
                Point2::new(0.0, 0.0),
                Point2::new(10.0, 0.0),
            ),
            packed_via("via-1", "SIG", Point2::new(5.0, 1.0)),
        ];
        let nearest =
            parse_delete_copper_request(&json!({ "at": [5.0, 0.05], "radius": 1.0 }), 2).unwrap();

        let plan = resolve_delete_copper(&nearest, &items, &layer_names);

        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.hits[0].kind, CopperKind::Track);

        let all = parse_delete_copper_request(
            &json!({ "at": [5.0, 0.05], "radius": 1.0, "all": true }),
            2,
        )
        .unwrap();
        let plan = resolve_delete_copper(&all, &items, &layer_names);

        assert_eq!(plan.items.len(), 2);
    }

    #[test]
    fn delete_copper_honors_kind_net_and_layer_filters() {
        let layer_names = vec!["F.Cu".to_owned(), "B.Cu".to_owned()];
        let items = vec![
            packed_track(
                "track-1",
                "SIG",
                BoardLayer::BlFCu,
                Point2::new(0.0, 0.0),
                Point2::new(10.0, 0.0),
            ),
            packed_track(
                "track-2",
                "OTHER",
                BoardLayer::BlBCu,
                Point2::new(0.0, 0.0),
                Point2::new(10.0, 0.0),
            ),
            packed_via("via-1", "SIG", Point2::new(5.0, 0.0)),
        ];
        let request = parse_delete_copper_request(
            &json!({
                "at": [5.0, 0.0],
                "radius": 0.2,
                "kinds": ["track"],
                "net": "SIG",
                "layer": "F.Cu",
                "all": true
            }),
            2,
        )
        .unwrap();

        let plan = resolve_delete_copper(&request, &items, &layer_names);

        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.hits[0].kind, CopperKind::Track);
        assert_eq!(plan.hits[0].net.as_deref(), Some("SIG"));
        assert_eq!(plan.hits[0].layer.as_deref(), Some("F.Cu"));
    }
}
