//! Interactive editing of saved board files.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use geom::{Point2, Rect};
use serde_json::{Value, json};

use pcb_model::{
    Connection, FailedNet, LayerRef, RoutePoint, RouteResult, RouteSolution, RoutingView, Trace,
    Via, ViaSpan,
};

use gordian_runtime::AgentRuntime;
use gordian_runtime::tool::require_str;

use kicad_board::{BoardSnapshot, FootprintPlacement, ImportedPart};

use crate::board::guard::Guard;
use crate::copper::RetractedCopper;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CopperKind {
    Track,
    Via,
}

#[derive(Debug, Clone)]
struct CopperDeleteRequest {
    at: Point2,
    radius: f64,
    kinds: BTreeSet<CopperKind>,
    net: Option<String>,
    layer: Option<u32>,
    all: bool,
}

#[derive(Debug, Clone, PartialEq)]
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

/// Move one or more saved-board parts in one atomic edit.
pub fn move_parts(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let snapshot = match crate::active_board(ctx) {
        Ok(snapshot) => snapshot,
        Err(err) => return Ok(json!({ "error": err })),
    };
    let mut board = MoveBoard::from_snapshot(
        &snapshot,
        &crate::place::courtyard_extents(&snapshot, ctx),
        &back_side_references(&snapshot),
    );
    let plan = match resolve_move_parts(&input, &mut board) {
        Ok(plan) => plan,
        Err(err) => return Ok(json!({ "error": err })),
    };
    if let Some(refusal) = overlap_error(&board, &plan, snapshot.problem.clearance) {
        return Ok(refusal);
    }
    let retract = retracted_copper(&snapshot, &plan);
    let gate = match Guard::open(
        ctx,
        "move_parts",
        "Move board footprints",
        &[ctx.pcb_path()],
    ) {
        Ok(gate) => gate,
        Err(refusal) => return Ok(refusal),
    };
    if let Err(err) = crate::place::write_placement(ctx, &plan.placements) {
        let error = json!({ "error": format!("move_parts could not write the board: {err}") });
        return Ok(gate.rollback(ctx, error));
    }
    if let Err(err) = crate::copper::write_retained(
        ctx,
        snapshot.problem.layer_count,
        &snapshot.layer_names,
        &retract,
    ) {
        let error = json!({
            "error": format!("move_parts moved the parts but could not retract their copper: {err}"),
        });
        return Ok(gate.rollback(ctx, error));
    }
    Ok(gate.commit(ctx, plan.output(&retract)))
}

/// The parts sitting on the back of the board. KiCAD mirrors a flipped
/// footprint about its y axis, so an asymmetric courtyard is on the other side
/// of the origin there.
fn back_side_references(snapshot: &BoardSnapshot) -> BTreeSet<String> {
    snapshot
        .imported
        .parts
        .iter()
        .filter(|part| part.side == kicad_board::BoardSide::Back)
        .map(|part| part.reference.clone())
        .collect()
}

/// Copper the move invalidates: every net with a trace ending on a pad that
/// moved, retracted whole so no stub is left hanging.
fn retracted_copper(snapshot: &BoardSnapshot, plan: &MovePlan) -> RetractedCopper {
    let moved = plan
        .positions
        .iter()
        .map(|position| position.reference.as_str());
    let pads = crate::copper::pad_extents(&snapshot.problem, moved);
    crate::copper::retract(&snapshot.copper, &pads, &BTreeSet::new())
}

/// Reject a move that would land a part on top of another one: the courtyards
/// plus the board clearance must not overlap. Courtyards are what KiCAD's DRC
/// checks, so a move this accepts cannot leave the board failing
/// `courtyards_overlap`. The refusal carries both rectangles and the measured
/// gap, so a wrong one can be seen for what it is.
fn overlap_error(board: &MoveBoard, plan: &MovePlan, clearance: f64) -> Option<Value> {
    let rect = |r: &Rect| json!([r.min_x, r.min_y, r.max_x, r.max_y]);
    for position in &plan.positions {
        let Some(moved) = board.parts.get(&position.reference) else {
            return Some(json!({
                "error": format!(
                    "move_parts refused: {} has no measurable extent on this board",
                    position.reference
                ),
            }));
        };
        let moved_courtyard = moved.courtyard();
        for (reference, other) in board.parts.iter() {
            if reference == &position.reference {
                continue;
            }
            let other_courtyard = other.courtyard();
            let (ox, oy) = moved_courtyard
                .inflate(clearance / 2.0)
                .axis_penetration(&other_courtyard.inflate(clearance / 2.0));
            if ox <= 0.0 || oy <= 0.0 {
                continue;
            }
            let gap = -ox.min(oy);
            return Some(json!({
                "error": format!(
                    "move_parts refused: {} at [{:.3}, {:.3}] would leave {gap:.3} mm to {} — \
                     their courtyards need {clearance:.3} mm between them",
                    position.reference, position.at.x, position.at.y, reference,
                ),
                "code": "courtyards_overlap",
                // Show the work: a false refusal is only visible if the rects it
                // was computed from are on the transcript.
                "moved": { "reference": position.reference, "courtyard_mm": rect(&moved_courtyard) },
                "blocked_by": { "reference": reference, "courtyard_mm": rect(&other_courtyard) },
                "gap_mm": gap,
                "required_clearance_mm": clearance,
            }));
        }
    }
    None
}

struct MoveBoard {
    bounds: Rect,
    parts: BTreeMap<String, MovePart>,
}

/// One board part as a move reasons about it: where its origin is, how it is
/// turned, which side it is on, and its courtyard RELATIVE TO THAT ORIGIN.
///
/// The local courtyard is kept asymmetric and transformed on demand. A
/// connector's origin is pin 1, not its body centre, so collapsing it to a
/// width and a height centred on the origin invents courtyard on the empty side
/// — the false overlap that refuses a move which really does clear.
#[derive(Debug, Clone)]
struct MovePart {
    at: Point2,
    rotation: f64,
    back: bool,
    local: Rect,
}

impl MovePart {
    /// Where this part's courtyard actually is on the board.
    fn courtyard(&self) -> Rect {
        crate::place::courtyard_at(self.local, self.at, self.rotation, self.back)
    }
}

#[derive(Debug, Clone)]
struct MovePlan {
    placements: Vec<FootprintPlacement>,
    positions: Vec<ResolvedPosition>,
    changed: usize,
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
    /// `courtyards` is the KiCAD courtyard extent per reference — what DRC
    /// actually checks. Pads alone underestimate it, so a move judged by pads
    /// could report success and leave the board failing `courtyards_overlap`.
    /// A part missing from the map falls back to its pad bounding box.
    fn from_snapshot(
        snapshot: &BoardSnapshot,
        courtyards: &BTreeMap<String, Rect>,
        back: &BTreeSet<String>,
    ) -> Self {
        let parts = snapshot
            .imported
            .parts
            .iter()
            .map(|part| {
                let rotation = part.rotation as f64;
                let on_back = back.contains(&part.reference);
                // Without a resolvable footprint the pads are all we know. They
                // come in board coordinates, so undo the pose to get the same
                // footprint-local rectangle a courtyard would have given —
                // exact, since a board rotation is a quadrant.
                let local = courtyards.get(&part.reference).copied().unwrap_or_else(|| {
                    let local_point = |x: f64, y: f64| {
                        let turned = Point2::new(x - part.at.x, y - part.at.y).rotate(-rotation);
                        if on_back {
                            Point2::new(-turned.x, turned.y)
                        } else {
                            turned
                        }
                    };
                    let points: Vec<Point2> = snapshot
                        .problem
                        .obstacles
                        .iter()
                        .filter(|ob| ob.kind == format!("pad:{}", part.reference))
                        .flat_map(|ob| {
                            [
                                local_point(
                                    ob.center.x - ob.width / 2.0,
                                    ob.center.y - ob.height / 2.0,
                                ),
                                local_point(
                                    ob.center.x + ob.width / 2.0,
                                    ob.center.y + ob.height / 2.0,
                                ),
                            ]
                        })
                        .collect();
                    Rect::bounding(&points).unwrap_or(Rect::new(-0.5, -0.5, 0.5, 0.5))
                });
                (
                    part.reference.clone(),
                    MovePart {
                        at: part.at,
                        rotation,
                        back: on_back,
                        local,
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
    fn output(&self, retract: &RetractedCopper) -> Value {
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
        let mut out = json!({
            "ok": true,
            "moved": self.positions.len(),
            "changed": self.changed,
            "positions": positions,
            "retracted_tracks": retract.count,
            "retracted_nets": retract.nets.len(),
            "nets_to_reroute": retract.nets.iter().collect::<Vec<_>>(),
        });
        // A move is a LOCAL edit, so name the local repair. Routing the whole
        // board instead throws away every route the move did not invalidate.
        if !retract.nets.is_empty() {
            out["next_tool"] = json!("route_board");
            out["note"] = json!(format!(
                "only these nets lost copper: call route_board {{\"nets\": {}}} to repair just \
                 them, or route_board {{\"bbox\": …}} for the area you edited. Routing the whole \
                 board would discard the routes this move left standing.",
                serde_json::to_string(&retract.nets).unwrap_or_else(|_| "[…]".to_owned())
            ));
        }
        out
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

    let original_parts = board.parts.clone();
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
    let placements = positions
        .iter()
        .map(|p| FootprintPlacement {
            reference: p.reference.clone(),
            at: p.at,
            rotation_deg: Some(p.rotation),
        })
        .collect();
    let changed = positions
        .iter()
        .filter(|position| {
            original_parts
                .get(&position.reference)
                .is_none_or(|original| {
                    (original.at.x - position.at.x).abs() > 1e-9
                        || (original.at.y - position.at.y).abs() > 1e-9
                        || (original.rotation - position.rotation).abs() > 1e-9
                })
        })
        .count();
    Ok(MovePlan {
        placements,
        positions,
        changed,
    })
}

/// A millimetre point, written either as `[x, y]` or as the `{x, y}` object the
/// board queries report positions in.
fn parse_point(input: &Value, key: &str, ctx: &str) -> std::result::Result<Point2, String> {
    let malformed = || format!("{ctx}: `{key}` must be [x, y] or {{x, y}} in mm");
    match input.get(key) {
        Some(Value::Array(values)) => {
            let [x, y] = values.as_slice() else {
                return Err(malformed());
            };
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => Ok(Point2::new(x, y)),
                _ => Err(malformed()),
            }
        }
        Some(Value::Object(fields)) => {
            match (
                fields.get("x").and_then(Value::as_f64),
                fields.get("y").and_then(Value::as_f64),
            ) {
                (Some(x), Some(y)) => Ok(Point2::new(x, y)),
                _ => Err(malformed()),
            }
        }
        _ => Err(malformed()),
    }
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

/// The origin that seats `part` `gap` mm clear of `target` on the named side.
///
/// Solved on the courtyards, not on centres: the answer is the origin that puts
/// the part's own courtyard edge where it belongs, which is not the same thing
/// for a footprint whose origin sits off-centre.
fn near_position(part: &MovePart, target: &MovePart, side: Side, gap: f64) -> Point2 {
    let (own, theirs) = (part.courtyard(), target.courtyard());
    let (dx, dy) = (part.at.x - own.min_x, part.at.y - own.min_y);
    match side {
        Side::Left => Point2::new(theirs.min_x - gap - own.width() + dx, target.at.y),
        Side::Right => Point2::new(theirs.max_x + gap + dx, target.at.y),
        Side::Above => Point2::new(target.at.x, theirs.min_y - gap - own.height() + dy),
        Side::Below => Point2::new(target.at.x, theirs.max_y + gap + dy),
    }
}

/// The origin that seats `part` `gap` mm inside the named board edge, centred
/// on the other axis.
fn edge_position(part: &MovePart, bounds: Rect, edge: Edge, gap: f64) -> Point2 {
    let own = part.courtyard();
    let (dx, dy) = (part.at.x - own.min_x, part.at.y - own.min_y);
    let mid_x = (bounds.min_x + bounds.max_x) / 2.0 - own.width() / 2.0 + dx;
    let mid_y = (bounds.min_y + bounds.max_y) / 2.0 - own.height() / 2.0 + dy;
    match edge {
        Edge::Left => Point2::new(bounds.min_x + gap + dx, mid_y),
        Edge::Right => Point2::new(bounds.max_x - gap - own.width() + dx, mid_y),
        Edge::Top => Point2::new(mid_x, bounds.min_y + gap + dy),
        Edge::Bottom => Point2::new(mid_x, bounds.max_y - gap - own.height() + dy),
    }
}

/// Route a single saved-board connection with grid-A* obstacle avoidance.
pub fn route_track(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let prepared = crate::active_board(ctx).and_then(|snapshot| {
        let request =
            parse_route_track_request(&input, &snapshot.problem, &snapshot.imported.parts)?;
        let (problem, solution) = manual_route_solution(&snapshot.problem, &request)?;
        Ok((problem, solution, request, snapshot.layer_names))
    });
    match prepared {
        Ok((problem, solution, request, layer_names)) => {
            let gate = match Guard::open(
                ctx,
                "route_track",
                "Route one board connection",
                &[ctx.pcb_path()],
            ) {
                Ok(gate) => gate,
                Err(refusal) => return Ok(refusal),
            };
            if let Err(err) =
                super::route::write_route_file(ctx, &problem, &solution, &layer_names)
            {
                let error =
                    json!({ "error": format!("route_track could not write copper: {err}") });
                return Ok(gate.rollback(ctx, error));
            }
            Ok(gate.commit(ctx, route_track_output(&problem, &solution, &request)))
        }
        Err(err) => Ok(json!({ "error": err })),
    }
}

/// Delete saved-board track/via copper near a click point.
pub fn delete_copper(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let path = ctx.pcb_path();
    let snapshot = match crate::active_board(ctx) {
        Ok(snapshot) => snapshot,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let selection = match parse_delete_copper_request(&input, snapshot.layer_names.len() as u32) {
        Ok(request) => request,
        Err(error) => return Ok(json!({ "error": error })),
    };
    let gate = match Guard::open(
        ctx,
        "delete_copper",
        "Delete board copper",
        std::slice::from_ref(&path),
    ) {
        Ok(gate) => gate,
        Err(refusal) => return Ok(refusal),
    };
    let deleted = delete_copper_file(ctx, &snapshot, &selection);
    match deleted {
        Ok(hits) => Ok(gate.commit(ctx, delete_copper_output(&selection, &hits))),
        Err(error) => Ok(gate.rollback(ctx, json!({ "error": error }))),
    }
}

/// Set one board net's track width in the saved project files.
pub fn set_net_width(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let net = require_str(&input, "net")?;
    if net.is_empty() {
        return Ok(json!({ "error": "net must not be empty" }));
    }
    let width = input
        .get("width")
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow::anyhow!("width must be a number in mm"))?;
    let clearance = input
        .get("clearance")
        .and_then(Value::as_f64)
        .unwrap_or(0.2);
    if !width.is_finite() || width <= 0.0 {
        return Ok(json!({ "error": format!("width must be greater than zero, got {width}") }));
    }
    if !clearance.is_finite() || clearance < 0.0 {
        return Ok(json!({ "error": format!("clearance must be non-negative, got {clearance}") }));
    }
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Width_{}", crate::fmt_num(width).replace('.', "_")));
    if name.is_empty() || name == "Default" {
        return Ok(json!({ "error": "name must be non-empty and not Default" }));
    }
    let path = ctx.pcb_path();
    let project_path = ctx.sch_path().with_extension("kicad_pro");
    let gate = match Guard::open(
        ctx,
        "set_net_width",
        "Set a board net class",
        &[path.clone(), project_path.clone()],
    ) {
        Ok(gate) => gate,
        Err(refusal) => return Ok(refusal),
    };
    let update = kicad_board::NetClassUpdate {
        name: name.clone(),
        width,
        clearance,
        nets: vec![net.clone()],
    };
    match write_net_width_file(&path, &project_path, &update) {
        Ok(report) => Ok(gate.commit(ctx, net_width_output(name, net, width, clearance, report))),
        Err(error) => Ok(gate.rollback(ctx, json!({ "error": error }))),
    }
}

fn net_width_output(
    name: String,
    net: String,
    width: f64,
    clearance: f64,
    report: kicad_board::NetClassUpdateReport,
) -> Value {
    json!({
        "ok": true,
        "write_path": "file",
        "changed": report.changed,
        "board_changed": report.board_changed,
        "project_changed": report.project_changed,
        "net_class": name,
        "width": width,
        "clearance": clearance,
        "nets": [net],
        "changed_nets": report.nets,
        "changed_classes": report.classes,
    })
}

fn delete_copper_file(
    ctx: &AgentRuntime,
    snapshot: &BoardSnapshot,
    selection: &DeleteCopperSelection,
) -> std::result::Result<Vec<CopperHit>, String> {
    #[derive(Clone, Copy)]
    enum Selected {
        Trace(usize),
        Via(usize),
    }

    let request = &selection.request;
    let layer_count = snapshot.problem.layer_count;
    let mut matches = Vec::<(Selected, CopperHit)>::new();
    if request.kinds.contains(&CopperKind::Track) {
        for (index, trace) in snapshot.copper.traces.iter().enumerate() {
            if request
                .net
                .as_deref()
                .is_some_and(|net| net != trace.connection)
                || request
                    .layer
                    .is_some_and(|layer| trace.layer.index(layer_count) != Some(layer))
            {
                continue;
            }
            for pair in trace.path.windows(2) {
                if selection
                    .bbox
                    .is_some_and(|bbox| !trace_segment_hits_bbox(trace, pair, &bbox))
                {
                    continue;
                }
                let distance = (geom::Segment::new(pair[0], pair[1]).dist_to_point(request.at)
                    - trace.width / 2.0)
                    .max(0.0);
                matches.push((
                    Selected::Trace(index),
                    CopperHit {
                        kind: CopperKind::Track,
                        distance,
                        net: Some(trace.connection.clone()),
                        layer: trace
                            .layer
                            .index(layer_count)
                            .and_then(|layer| snapshot.layer_names.get(layer as usize).cloned()),
                        layers: Vec::new(),
                        at: None,
                        start: Some(pair[0]),
                        end: Some(pair[1]),
                    },
                ));
            }
        }
    }
    if request.kinds.contains(&CopperKind::Via) {
        for (index, via) in snapshot.copper.vias.iter().enumerate() {
            if request
                .net
                .as_deref()
                .is_some_and(|net| net != via.connection)
            {
                continue;
            }
            let indices: Vec<u32> = match via.span {
                ViaSpan::Through => (0..layer_count).collect(),
                ViaSpan::Partial { from, to, .. } => (from.min(to)..=from.max(to)).collect(),
            };
            if request.layer.is_some_and(|layer| !indices.contains(&layer)) {
                continue;
            }
            if selection
                .bbox
                .is_some_and(|bbox| !via_hits_bbox(via, &bbox))
            {
                continue;
            }
            matches.push((
                Selected::Via(index),
                CopperHit {
                    kind: CopperKind::Via,
                    distance: (via.at.dist(request.at) - via.diameter / 2.0).max(0.0),
                    net: Some(via.connection.clone()),
                    layer: None,
                    layers: indices
                        .iter()
                        .filter_map(|index| snapshot.layer_names.get(*index as usize).cloned())
                        .collect(),
                    at: Some(via.at),
                    start: None,
                    end: None,
                },
            ));
        }
    }
    if selection.bbox.is_none() {
        matches.retain(|(_, hit)| hit.distance <= request.radius + geom::EPS);
    }
    matches.sort_by(|a, b| {
        a.1.distance
            .total_cmp(&b.1.distance)
            .then_with(|| a.1.kind.cmp(&b.1.kind))
    });
    if !request.all {
        matches.truncate(1);
    }
    if matches.is_empty() {
        return Ok(Vec::new());
    }
    let trace_indices: BTreeSet<usize> = matches
        .iter()
        .filter_map(|(selected, _)| match selected {
            Selected::Trace(index) => Some(*index),
            Selected::Via(_) => None,
        })
        .collect();
    let via_indices: BTreeSet<usize> = matches
        .iter()
        .filter_map(|(selected, _)| match selected {
            Selected::Via(index) => Some(*index),
            Selected::Trace(_) => None,
        })
        .collect();
    let retained = RouteSolution {
        traces: snapshot
            .copper
            .traces
            .iter()
            .enumerate()
            .filter(|(index, _)| !trace_indices.contains(index))
            .map(|(_, trace)| trace.clone())
            .collect(),
        vias: snapshot
            .copper
            .vias
            .iter()
            .enumerate()
            .filter(|(index, _)| !via_indices.contains(index))
            .map(|(_, via)| via.clone())
            .collect(),
    };
    let path = ctx.pcb_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read the board: {error}"))?;
    let (stripped, _, _) = kicad_board::strip_copper(&text)?;
    let updated = kicad_board::append_copper(
        &stripped,
        &retained,
        snapshot.problem.layer_count,
        &snapshot.layer_names,
    )?;
    crate::route::write_board_atomically(&path, updated.as_bytes())
        .map_err(|error| format!("could not replace the board: {error}"))?;
    Ok(matches.into_iter().map(|(_, hit)| hit).collect())
}

fn trace_segment_hits_bbox(trace: &Trace, pair: &[Point2], bbox: &Rect) -> bool {
    geom::Segment::new(pair[0], pair[1]).dist_to_rect(bbox) <= trace.width / 2.0 + geom::EPS
}

fn via_hits_bbox(via: &Via, bbox: &Rect) -> bool {
    bbox.dist_to_point(via.at) <= via.diameter / 2.0 + geom::EPS
}

fn write_net_width_file(
    board_path: &std::path::Path,
    project_path: &std::path::Path,
    update: &kicad_board::NetClassUpdate,
) -> std::result::Result<kicad_board::NetClassUpdateReport, String> {
    kicad_board::write_net_class_update(board_path, project_path, update)
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

/// A `route_track` endpoint: a point in mm, or the `"R1.1"` pad reference that
/// `route_board`'s `unrouted` report hands back, so a repair is copy-paste.
fn parse_endpoint(
    input: &Value,
    key: &str,
    ctx: &str,
    parts: &[ImportedPart],
) -> std::result::Result<(Point2, Option<LayerRef>), String> {
    let Some(Value::String(reference)) = input.get(key) else {
        return parse_point(input, key, ctx).map(|at| (at, None));
    };
    let (refdes, pad) = reference.split_once('.').ok_or_else(|| {
        format!("{ctx}: `{key}` = \"{reference}\" is not a pad reference; use \"REF.PAD\" (e.g. \"U1.3\") or [x, y] in mm")
    })?;
    parts
        .iter()
        .find(|part| part.reference == refdes)
        // A pad knows which layer it is on, so a copy-pasted repair for a
        // bottom-side pad must not silently lay copper on the top.
        .and_then(|part| part.pads.iter().find(|p| p.number == pad))
        .map(|p| (p.at, p.layers.first().cloned()))
        .ok_or_else(|| format!("{ctx}: no pad {reference} on this board"))
}

fn parse_route_track_request(
    input: &Value,
    problem: &RoutingView,
    parts: &[ImportedPart],
) -> std::result::Result<RouteTrackRequest, String> {
    let ctx = "route_track";
    if input.get("start").is_some() || input.get("end").is_some() || input.get("layer").is_some() {
        return Err(
            "route_track now uses `from`, `to`, `net`, `from_layer`, and `to_layer`; legacy `start`/`end`/`layer` are not accepted"
                .to_owned(),
        );
    }
    let (from, from_pad_layer) = parse_endpoint(input, "from", ctx, parts)?;
    let (to, to_pad_layer) = parse_endpoint(input, "to", ctx, parts)?;
    let net = input
        .get("net")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "route_track needs non-empty string `net`".to_owned())?
        .to_owned();
    let from_layer = match input.get("from_layer").and_then(Value::as_str) {
        Some(layer) => parse_route_layer_ref(layer, problem.layer_count)?,
        None => from_pad_layer.unwrap_or_else(LayerRef::top),
    };
    let to_layer = match input.get("to_layer").and_then(Value::as_str) {
        Some(layer) => parse_route_layer_ref(layer, problem.layer_count)?,
        None => to_pad_layer.unwrap_or_else(|| from_layer.clone()),
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
    base: &RoutingView,
    request: &RouteTrackRequest,
) -> std::result::Result<(RoutingView, RouteSolution), String> {
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
    base: &RoutingView,
    net: &str,
    width: f64,
    from: Point2,
    from_layer: LayerRef,
    to: Point2,
    to_layer: LayerRef,
) -> std::result::Result<RouteSolution, String> {
    // A via-only stitch has zero-length legs on either side of its explicit
    // anchor. Treat those as already connected on that layer; asking a router
    // to solve a point-to-itself connection reports a misleading dropped net.
    if from.near_eq(to, geom::EPS) && from_layer == to_layer {
        return Ok(RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        });
    }
    let exact_problem = single_connection_problem(
        base,
        net,
        width,
        from,
        from_layer.clone(),
        to,
        to_layer.clone(),
    );
    // Exact-coordinate visibility/dogleg routing avoids manufacturing an
    // artificial blocked endpoint when a wide pad center snaps into an
    // occupied grid cell. Fall back to grid A* for genuinely maze-like legs.
    let mut direct = RouteResult {
        solution: RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        },
        failed: vec![FailedNet {
            connection: net.to_owned(),
            reason: "manual direct route pending".to_owned(),
        }],
        engine: "manual".to_owned(),
    };
    if super::route::apply_direct_rescue_fallback(&exact_problem, &mut direct) {
        validate_manual_solution(&exact_problem, &direct.solution)?;
        return Ok(direct.solution);
    }
    let grid_from = snapped_route_point(&exact_problem, from);
    let grid_to = snapped_route_point(&exact_problem, to);
    let grid_problem =
        single_connection_problem(base, net, width, grid_from, from_layer, grid_to, to_layer);
    let result = pcb_engine::route_grid(&grid_problem);
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

fn snapped_route_point(problem: &RoutingView, point: Point2) -> Point2 {
    let pitch = problem.grid_pitch();
    Point2::new(
        route_cell_center(problem.bounds.min_x, point.x, pitch),
        route_cell_center(problem.bounds.min_y, point.y, pitch),
    )
}

fn single_connection_problem(
    base: &RoutingView,
    net: &str,
    width: f64,
    from: Point2,
    from_layer: LayerRef,
    to: Point2,
    to_layer: LayerRef,
) -> RoutingView {
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

fn validation_problem(base: &RoutingView, request: &RouteTrackRequest) -> RoutingView {
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

fn add_manual_terminal_stubs(problem: &RoutingView, solution: &mut RouteSolution) {
    let pitch = problem.grid_pitch();
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
    problem: &RoutingView,
    solution: &RouteSolution,
) -> std::result::Result<(), String> {
    let target_net = problem
        .connections
        .first()
        .map(|connection| connection.name.as_str())
        .unwrap_or_default();
    let baseline = pcb_engine::check(
        problem,
        &RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        },
    );
    let violations = pcb_engine::check(problem, solution);
    let mut baseline_counts = BTreeMap::<String, usize>::new();
    for violation in baseline {
        let key = serde_json::to_string(&violation).unwrap_or_else(|_| format!("{violation:?}"));
        *baseline_counts.entry(key).or_default() += 1;
    }
    let mut introduced = Vec::new();
    for violation in violations {
        // The validation problem contains only the requested manual connection.
        // Its own Unconnected defect is never acceptable merely because the
        // empty baseline has the same expected finding. Other connectivity
        // findings (for example two intentional solder-jumper pads represented
        // as preexisting obstacles) remain baseline-subtractable. A new
        // cross-net merge still has no baseline match and is rejected below.
        if matches!(
            &violation,
            pcb_model::Finding::Connectivity {
                violation: pcb_model::Violation::Unconnected { connection, .. }
            } if connection == target_net
        ) {
            introduced.push(violation);
            continue;
        }
        let key = serde_json::to_string(&violation).unwrap_or_else(|_| format!("{violation:?}"));
        let available = baseline_counts.entry(key).or_default();
        if *available > 0 {
            *available -= 1;
        } else {
            introduced.push(violation);
        }
    }
    if introduced.is_empty() {
        return Ok(());
    }
    let first = introduced
        .first()
        .and_then(|v| serde_json::to_string(v).ok())
        .unwrap_or_else(|| "unknown violation".to_owned());
    Err(format!(
        "route_track validation failed with {} DRC/connectivity violation(s); first: {first}",
        introduced.len()
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
    problem: &RoutingView,
    solution: &RouteSolution,
    request: &RouteTrackRequest,
) -> Value {
    let metrics = solution.metrics();
    json!({
        "ok": true,
        "router": "pcb-route-grid",
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

#[derive(Debug, Clone)]
struct DeleteCopperSelection {
    request: CopperDeleteRequest,
    bbox: Option<Rect>,
}

fn parse_delete_copper_request(
    input: &Value,
    layer_count: u32,
) -> std::result::Result<DeleteCopperSelection, String> {
    let ctx = "delete_copper";
    let at = input
        .get("at")
        .map(|_| parse_point(input, "at", ctx))
        .transpose()?;
    let bbox = crate::selection::parse_bbox(input)?;
    if at.is_some() && bbox.is_some() {
        return Err("delete_copper accepts either `at` or `bbox`, not both".to_owned());
    }
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
    if at.is_none() && net.is_none() {
        return Err("delete_copper needs `at`, or a `net` with optional `bbox`".to_owned());
    }
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
    let all = at.is_none() || input.get("all").and_then(Value::as_bool).unwrap_or(false);
    Ok(DeleteCopperSelection {
        request: CopperDeleteRequest {
            at: at
                .or_else(|| bbox.map(|bbox| bbox.center()))
                .unwrap_or_else(|| Point2::new(0.0, 0.0)),
            radius,
            kinds,
            net,
            layer,
            all,
        },
        bbox,
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

fn delete_copper_output(selection: &DeleteCopperSelection, hits: &[CopperHit]) -> Value {
    let request = &selection.request;
    let matches: Vec<Value> = hits.iter().map(copper_hit_json).collect();
    json!({
        "ok": true,
        "deleted": hits.len(),
        "all": request.all,
        "net": request.net,
        "bbox": selection.bbox,
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

fn layer_name_from_index(idx: u32, layer_names: &[String]) -> String {
    layer_names.get(idx as usize).cloned().unwrap_or_else(|| {
        if idx == 0 {
            "F.Cu".to_owned()
        } else {
            format!("In{idx}.Cu")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The move a real 555 board refused: J1 — a pin header whose origin is
    /// pin 1, not its body centre — to (10.77, 35.5), with R2 — an axial
    /// resistor whose origin is its first lead — sitting at (25.25, 35.5).
    /// There is over 10 mm of clear board between them; only a courtyard box
    /// centred on each origin, twice too wide on the empty side, overlaps.
    #[test]
    fn a_pin_one_origin_header_clears_an_axial_resistor_ten_millimetres_away() {
        let Some(ctx) = gordian_runtime::AgentRuntime::detect_for_test() else {
            eprintln!("SKIP: no KiCAD detected");
            return;
        };
        let Ok(catalog) = ctx.footprint_catalog() else {
            eprintln!("SKIP: no footprint catalog");
            return;
        };
        let envelope = |id: &str| {
            let id = kicad_footprint::FootprintId::parse(id).expect("a library id");
            crate::place::placement_envelope(&catalog.footprint(&id).expect("a footprint"))
        };
        let header = envelope("Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical");
        let resistor = envelope("Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal");
        assert!(
            header.min_y.abs() < header.max_y.abs() && resistor.min_x.abs() < resistor.max_x.abs(),
            "both footprints put their origin off centre: {header:?} {resistor:?}"
        );

        let part = |at: Point2, local: Rect| MovePart {
            at,
            rotation: 0.0,
            back: false,
            local,
        };
        let mut board = MoveBoard {
            bounds: Rect::new(0.0, 0.0, 60.0, 60.0),
            parts: [
                ("J1".to_owned(), part(Point2::new(2.27, 35.5), header)),
                ("R2".to_owned(), part(Point2::new(25.25, 35.5), resistor)),
            ]
            .into_iter()
            .collect(),
        };
        let plan = resolve_move_parts(
            &json!({ "moves": [{ "reference": "J1", "to": [10.77, 35.5] }] }),
            &mut board,
        )
        .expect("the move resolves");
        assert_eq!(
            overlap_error(&board, &plan, 0.2),
            None,
            "J1 {:?} and R2 {:?} do not touch",
            board.parts["J1"].courtyard(),
            board.parts["R2"].courtyard(),
        );

        // Slid up against the resistor, the same check still refuses — and says
        // which rectangles it measured.
        let plan = resolve_move_parts(
            &json!({ "moves": [{ "reference": "J1", "to": [24.0, 35.5] }] }),
            &mut board,
        )
        .expect("the move resolves");
        let refused = overlap_error(&board, &plan, 0.2).expect("an overlap");
        assert_eq!(refused["code"], "courtyards_overlap");
        assert!(refused["gap_mm"].as_f64().is_some_and(|gap| gap < 0.0));
    }

    /// KiCAD's DRC checks courtyards, which are wider than the pads inside them.
    /// A move judged by pads alone reported success and left the board failing
    /// `courtyards_overlap`, so the guard must measure what DRC measures.
    #[test]
    fn a_move_is_judged_by_courtyards_not_by_the_pads_inside_them() {
        let pad = |number: &str, x: f64| pcb_model::Obstacle {
            kind: "pad:C1".to_owned(),
            layers: vec![LayerRef::top()],
            center: Point2::new(x, 5.0),
            width: 0.9,
            height: 1.0,
            connected_to: vec![number.to_owned()],
        };
        let snapshot = BoardSnapshot {
            imported: kicad_board::ImportedBoard {
                layer_count: 2,
                bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
                parts: vec![ImportedPart {
                    reference: "C1".to_owned(),
                    lib_id: "Capacitor_SMD:C_0603_1608Metric".to_owned(),
                    at: Point2::new(5.0, 5.0),
                    rotation: 0,
                    side: kicad_board::BoardSide::Front,
                    locked: false,
                    courtyard: None,
                    pads: vec![],
                    properties: Default::default(),
                }],
                placement_keepouts: vec![],
                keepout_count: 0,
            },
            problem: route_problem(vec![pad("GND", 4.2), pad("VCC", 5.8)]),
            copper: RouteSolution::default(),
            layer_names: vec!["F.Cu".to_owned(), "B.Cu".to_owned()],
        };

        let pads_only = MoveBoard::from_snapshot(&snapshot, &BTreeMap::new(), &BTreeSet::new());
        let c1 = pads_only.parts["C1"].courtyard();
        assert!((c1.width() - 2.5).abs() < 1e-6, "pad bbox: {c1:?}");

        let courtyards = BTreeMap::from([("C1".to_owned(), Rect::new(-1.55, -0.9, 1.55, 0.9))]);
        let with_courtyards = MoveBoard::from_snapshot(&snapshot, &courtyards, &BTreeSet::new());
        let c1 = with_courtyards.parts["C1"].courtyard();
        assert!((c1.width() - 3.1).abs() < 1e-6, "courtyard: {c1:?}");
        assert!((c1.height() - 1.8).abs() < 1e-6, "courtyard: {c1:?}");
    }

    fn fixture_board() -> MoveBoard {
        MoveBoard {
            bounds: Rect::new(0.0, 0.0, 100.0, 50.0),
            parts: [
                (
                    "U1".to_owned(),
                    MovePart {
                        at: Point2::new(50.0, 25.0),
                        rotation: 0.0,
                        back: false,
                        local: Rect::new(-5.0, -4.0, 5.0, 4.0),
                    },
                ),
                (
                    "C1".to_owned(),
                    MovePart {
                        at: Point2::new(10.0, 10.0),
                        rotation: 0.0,
                        back: false,
                        local: Rect::new(-1.0, -0.5, 1.0, 0.5),
                    },
                ),
                (
                    "R1".to_owned(),
                    MovePart {
                        at: Point2::new(20.0, 20.0),
                        rotation: 0.0,
                        back: false,
                        local: Rect::new(-2.0, -1.0, 2.0, 1.0),
                    },
                ),
                (
                    "J1".to_owned(),
                    MovePart {
                        at: Point2::new(30.0, 30.0),
                        rotation: 90.0,
                        back: false,
                        local: Rect::new(-5.0, -3.0, 5.0, 3.0),
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
        assert_eq!(plan.placements[0].at, Point2::new(25.0, 20.0));
        assert_eq!(plan.placements[0].rotation_deg, Some(180.0));
        assert_eq!(plan.changed, 1);
    }

    #[test]
    fn reports_an_explicit_move_to_the_current_pose_as_unchanged() {
        let plan = resolve(json!({
            "moves": [{ "reference": "U1", "to": [50.0, 25.0], "rotation": 0.0 }]
        }));

        assert_eq!(plan.positions.len(), 1);
        assert_eq!(plan.changed, 0);
        assert_eq!(
            plan.output(&RetractedCopper::default())["changed"],
            json!(0)
        );
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

    fn route_problem(obstacles: Vec<pcb_model::Obstacle>) -> RoutingView {
        RoutingView {
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
            fixed_copper: Default::default(),
            nets: None,
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
            &[],
        )
        .unwrap();

        let (routed_problem, solution) = manual_route_solution(&problem, &request).unwrap();

        assert!(
            pcb_engine::check(&routed_problem, &solution).is_empty(),
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
            &[],
        )
        .unwrap();

        let (routed_problem, solution) = manual_route_solution(&problem, &request).unwrap();

        assert!(
            pcb_engine::check(&routed_problem, &solution).is_empty(),
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
    fn manual_route_supports_a_via_only_stitch() {
        let problem = route_problem(vec![]);
        let request = parse_route_track_request(
            &json!({
                "from": [5.0, 1.0],
                "to": [5.0, 1.0],
                "net": "GND",
                "from_layer": "F.Cu",
                "to_layer": "B.Cu",
                "vias": [{ "at": [5.0, 1.0], "to_layer": "B.Cu" }]
            }),
            &problem,
            &[],
        )
        .unwrap();

        let (routed_problem, solution) = manual_route_solution(&problem, &request).unwrap();

        assert!(pcb_engine::check(&routed_problem, &solution).is_empty());
        assert!(solution.traces.is_empty());
        assert_eq!(solution.vias.len(), 1);
        assert_eq!(solution.vias[0].span, ViaSpan::Through);
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
            &[],
        )
        .unwrap();

        let err = manual_route_solution(&problem, &request).unwrap_err();

        assert!(err.contains("could not route"), "{err}");
    }

    #[test]
    fn manual_route_validation_never_hides_target_connectivity() {
        let base = route_problem(Vec::new());
        let problem = single_connection_problem(
            &base,
            "SIG",
            0.2,
            Point2::new(1.0, 1.0),
            LayerRef::top(),
            Point2::new(9.0, 1.0),
            LayerRef::top(),
        );
        let empty = RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        };

        let err = validate_manual_solution(&problem, &empty).unwrap_err();
        assert!(err.contains("unconnected"), "{err}");
    }

    #[test]
    fn manual_route_validation_subtracts_unrelated_baseline_connectivity() {
        let mut a = obstacle(Point2::new(5.0, 4.0), 1.0, 1.0, vec![LayerRef::top()]);
        a.kind = "pad:JP1".to_owned();
        a.connected_to = vec!["CANH".to_owned()];
        let mut b = a.clone();
        b.connected_to = vec!["CAN_TERM".to_owned()];
        let base = route_problem(vec![a, b]);
        let problem = single_connection_problem(
            &base,
            "GND",
            0.2,
            Point2::new(1.0, 1.0),
            LayerRef::top(),
            Point2::new(9.0, 1.0),
            LayerRef::top(),
        );
        let solution = RouteSolution {
            traces: vec![Trace {
                connection: "GND".to_owned(),
                layer: LayerRef::top(),
                width: 0.2,
                path: vec![Point2::new(1.0, 1.0), Point2::new(9.0, 1.0)],
            }],
            vias: Vec::new(),
        };
        let empty = RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        };
        assert!(
            pcb_engine::check(&problem, &empty)
                .iter()
                .any(|finding| matches!(
                    finding,
                    pcb_model::Finding::Connectivity {
                        violation: pcb_model::Violation::CrossNetMerge { .. }
                    }
                ))
        );

        validate_manual_solution(&problem, &solution).unwrap();
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
            &[],
        )
        .unwrap_err();

        assert!(err.contains("legacy `start`/`end`/`layer`"), "{err}");
    }

    /// `route_board`'s `unrouted` report hands back `"U1.3"`-style pad handles;
    /// a repair is only copy-paste if `route_track` takes them as they are.
    #[test]
    fn route_track_takes_the_pad_handles_the_unrouted_report_hands_back() {
        let parts = vec![ImportedPart {
            reference: "U1".to_owned(),
            lib_id: "Package_TO_SOT_SMD:SOT-23-5".to_owned(),
            at: Point2::new(0.0, 0.0),
            rotation: 0,
            side: kicad_board::BoardSide::Front,
            locked: false,
            courtyard: None,
            pads: vec![
                kicad_board::ImportedPad {
                    number: "3".to_owned(),
                    net: Some("SIG".to_owned()),
                    at: Point2::new(4.0, 2.0),
                    layers: vec![LayerRef::top()],
                    shape: "rect".to_owned(),
                    size: Point2::new(0.0, 0.0),
                    drill: None,
                },
                kicad_board::ImportedPad {
                    number: "5".to_owned(),
                    net: Some("SIG".to_owned()),
                    at: Point2::new(9.0, 2.0),
                    layers: vec![LayerRef::top()],
                    shape: "rect".to_owned(),
                    size: Point2::new(0.0, 0.0),
                    drill: None,
                },
            ],
            properties: Default::default(),
        }];
        let problem = route_problem(vec![]);

        let request = parse_route_track_request(
            &json!({ "from": "U1.3", "to": "U1.5", "net": "SIG" }),
            &problem,
            &parts,
        )
        .unwrap();

        assert_eq!(request.from, Point2::new(4.0, 2.0));
        assert_eq!(request.to, Point2::new(9.0, 2.0));

        let err = parse_route_track_request(
            &json!({ "from": "U1.9", "to": "U1.5", "net": "SIG" }),
            &problem,
            &parts,
        )
        .unwrap_err();
        assert!(err.contains("no pad U1.9 on this board"), "{err}");
    }

    #[test]
    fn delete_copper_accepts_net_wide_and_bounded_selection() {
        let whole = parse_delete_copper_request(&json!({ "net": "GND" }), 2).unwrap();
        assert_eq!(whole.request.net.as_deref(), Some("GND"));
        assert!(whole.request.all);
        assert!(whole.bbox.is_none());

        let bounded = parse_delete_copper_request(
            &json!({
                "net": "GND",
                "bbox": { "min_x": 4.0, "min_y": 4.0, "max_x": 6.0, "max_y": 6.0 }
            }),
            2,
        )
        .unwrap();
        assert_eq!(bounded.bbox, Some(Rect::new(4.0, 4.0, 6.0, 6.0)));
        assert!(parse_delete_copper_request(&json!({}), 2).is_err());
        assert!(
            parse_delete_copper_request(
                &json!({
                    "at": [5.0, 5.0],
                    "bbox": { "min_x": 4.0, "min_y": 4.0, "max_x": 6.0, "max_y": 6.0 }
                }),
                2,
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_copper_selection_hits_crossing_tracks_and_touching_vias() {
        let bbox = Rect::new(4.0, 4.0, 6.0, 6.0);
        let crossing = Trace {
            connection: "GND".to_owned(),
            layer: LayerRef::top(),
            width: 0.2,
            path: vec![Point2::new(2.0, 5.0), Point2::new(8.0, 5.0)],
        };
        let outside = Trace {
            path: vec![Point2::new(2.0, 2.0), Point2::new(8.0, 2.0)],
            ..crossing.clone()
        };
        let touching = Via {
            connection: "GND".to_owned(),
            at: Point2::new(6.3, 5.0),
            diameter: 0.6,
            drill: 0.3,
            span: ViaSpan::Through,
        };

        assert!(trace_segment_hits_bbox(&crossing, &crossing.path, &bbox));
        assert!(!trace_segment_hits_bbox(&outside, &outside.path, &bbox));
        assert!(via_hits_bbox(&touching, &bbox));
    }

    #[test]
    fn offline_net_width_write_updates_board_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let board_path = dir.path().join("design.kicad_pcb");
        let project_path = dir.path().join("design.kicad_pro");
        std::fs::write(
            &board_path,
            "(kicad_pcb\n\t(net 0 \"\")\n\t(net 1 \"SIG\")\n\t(net_class \"Default\" \"default\"\n\t\t(clearance 0.2)\n\t\t(trace_width 0.25)\n\t\t(add_net \"SIG\")\n\t)\n)\n",
        )
        .unwrap();
        std::fs::write(
            &project_path,
            "{\n  \"net_settings\": {\n    \"classes\": [{\"name\": \"Default\", \"priority\": 2147483647, \"clearance\": 0.2, \"track_width\": 0.25}],\n    \"netclass_assignments\": null\n  }\n}\n",
        )
        .unwrap();
        let update = kicad_board::NetClassUpdate {
            name: "Width_0_5".into(),
            width: 0.5,
            clearance: 0.2,
            nets: vec!["SIG".into()],
        };

        let report = write_net_width_file(&board_path, &project_path, &update).unwrap();

        assert_eq!(report.nets, vec!["SIG"]);
        assert_eq!(report.classes, vec!["Width_0_5"]);
        let board = std::fs::read_to_string(board_path).unwrap();
        let project = std::fs::read_to_string(project_path).unwrap();
        assert_eq!(kicad_board::board_net_widths(&board).unwrap()["SIG"], 0.5);
        assert_eq!(
            kicad_board::project_net_widths(&project).unwrap()["SIG"],
            0.5
        );
    }
}
