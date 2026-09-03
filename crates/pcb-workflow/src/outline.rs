//! Existing-board outline editing.
//!
//! This is the non-destructive PCB geometry path: it edits the current
//! `.kicad_pcb` Edge.Cuts instead of regenerating the board from a schematic.

use anyhow::{Context, Result};
use kicad_board::{sexpr_end, sexpr_point};
use pcb_model::{PlaceReport, PlaceResult, Placement, PlacementHints, Point2, Polygon, Rect};
use serde_json::{Value, json};

use gordian_runtime::AgentRuntime;

use crate::board::guard::{Edit, Guard};

use super::create::req_num;

/// Replace the current board Edge.Cuts with a rectangle or arbitrary polygon.
///
/// Inputs:
/// - `bounds`: `{min_x,max_x,min_y,max_y}` rectangular outline in mm.
/// - `outline`: `[[x,y], ...]` arbitrary closed polygon in mm.
/// - `fit`: re-place an unrouted board on its compact rule-derived frame.
pub fn update_board_outline(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let fit = input.get("fit").and_then(Value::as_bool).unwrap_or(false)
        || input
            .get("fit_to_geometry")
            .and_then(Value::as_bool)
            .unwrap_or(false);

    if fit {
        return refit_existing_board(ctx);
    }

    let outline = if input.get("outline").is_some() {
        match parse_outline(input.get("outline")) {
            Ok(poly) => Outline::Polygon(poly),
            Err(msg) => return Ok(json!({ "error": msg })),
        }
    } else if input.get("bounds").is_some() {
        match parse_bounds(input.get("bounds")) {
            Ok(bounds) => Outline::Rect(bounds),
            Err(msg) => return Ok(json!({ "error": msg })),
        }
    } else {
        return Ok(json!({
            "error": "update_board_outline needs `bounds`, `outline`, or fit=true"
        }));
    };

    let path = ctx.pcb_path();
    let gate = match Guard::open(
        ctx,
        Edit::new("update_board_outline", std::slice::from_ref(&path)),
    ) {
        Ok(gate) => gate,
        Err(refusal) => return Ok(refusal),
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading board {}", path.display()))?;
    let changed = !edge_cuts_match(&text, &outline)?;
    let updated = replace_edge_cuts(&text, &outline, None)?;
    if changed {
        crate::route::write_board_atomically(&path, updated.as_bytes())
            .with_context(|| format!("writing board {}", path.display()))?;
    }

    let bounds = outline.bounds();
    Ok(gate.commit(ctx, json!({
        "ok": true,
        "changed": changed,
        "path": path.display().to_string(),
        "bounds": bounds,
        "outline_points": outline.point_count(),
        "note": "updated Edge.Cuts on the existing PCB without regenerating placement or routing",
    })))
}

fn refit_existing_board(ctx: &AgentRuntime) -> Result<Value> {
    let board = match crate::active_board(ctx) {
        Ok(board) => board,
        Err(err) => return Ok(json!({ "error": err })),
    };
    if !board.copper.traces.is_empty() || !board.copper.vias.is_empty() {
        return Ok(json!({
            "ok": true,
            "changed": false,
            "bounds": {
                "min_x": board.imported.bounds.min_x,
                "min_y": board.imported.bounds.min_y,
                "max_x": board.imported.bounds.max_x,
                "max_y": board.imported.bounds.max_y,
            },
            "outline_refit": "skipped (routed board)",
            "note": "the board has copper, so its outline and footprint poses were left unchanged",
        }));
    }
    let placement = match super::place::place_problem_from_snapshot(&board, ctx) {
        Ok(placement) => placement,
        Err(err) => return Ok(json!({ "error": err })),
    };
    let path = ctx.pcb_path();
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading board {}", path.display()))?;
    if let Err(error) = managed_outline_bounds(&text) {
        return Ok(json!({ "error": error }));
    }
    let mut hints = PlacementHints::default();
    for part in &board.imported.parts {
        if super::place::is_connector(&part.lib_id, &part.reference) {
            hints.edge_seek.push(part.reference.clone());
        }
    }
    let current = PlaceResult {
        placements: board
            .imported
            .parts
            .iter()
            .map(|part| Placement {
                reference: part.reference.clone(),
                at: part.at,
                rotation: part.rotation as f64,
            })
            .collect(),
        legal: true,
        report: PlaceReport::default(),
    };
    let Some(plan) = super::place::plan_outline_refit(
        &placement,
        &board.imported.parts,
        &board.problem,
        &hints,
        &current,
    ) else {
        // Nothing smaller is legal, so there is nothing to do. That is an
        // answer, not a failure: the board keeps the outline it has.
        return Ok(json!({
            "ok": true,
            "changed": false,
            "bounds": {
                "min_x": board.imported.bounds.min_x,
                "min_y": board.imported.bounds.min_y,
                "max_x": board.imported.bounds.max_x,
                "max_y": board.imported.bounds.max_y,
            },
            "note": "the current outline is already the smallest legal rectangle these \
                     footprints and rules allow; nothing was changed",
        }));
    };
    // A refit answers for the placed board only. Locked parts keep their pose,
    // and staged parts are not on the board yet: moving them would clear the
    // annotation that keeps them out of the guard, DRC and fab export.
    let locked: std::collections::BTreeSet<String> = board
        .imported
        .parts
        .iter()
        .filter(|part| part.locked)
        .map(|part| part.reference.clone())
        .collect();
    let staged = crate::staging::staged_references(&board);
    let gate = match Guard::open(
        ctx,
        Edit::new("update_board_outline", std::slice::from_ref(&path)),
    ) {
        Ok(gate) => gate,
        Err(refusal) => return Ok(refusal),
    };
    let moves: Vec<kicad_board::FootprintPlacement> = plan
        .result
        .placements
        .iter()
        .filter(|placement| {
            !locked.contains(&placement.reference) && !staged.contains(&placement.reference)
        })
        .map(|placement| kicad_board::FootprintPlacement {
            reference: placement.reference.clone(),
            at: placement.at,
            rotation_deg: Some(placement.rotation),
        })
        .collect();
    if let Err(error) = super::place::write_placement(ctx, &moves) {
        return Ok(gate.rollback(
            ctx,
            json!({ "error": format!("could not write compact placement: {error}") }),
        ));
    }
    let proposed = Polygon::new(vec![
        Point2::new(plan.to.min_x, plan.to.min_y),
        Point2::new(plan.to.max_x, plan.to.min_y),
        Point2::new(plan.to.max_x, plan.to.max_y),
        Point2::new(plan.to.min_x, plan.to.max_y),
    ])
    .expect("a refit rectangle is a polygon");
    let realised = match crate::active_board(ctx) {
        Ok(realised) => realised,
        Err(error) => {
            return Ok(gate.rollback(
                ctx,
                json!({ "error": format!("could not verify fitted placement: {error}") }),
            ));
        }
    };
    let containment = crate::board::guard::outline_containment_against(&realised, &proposed);
    if !containment.is_clear() {
        return Ok(gate.rollback(
            ctx,
            json!({
                "error": "the compact outline did not contain the realised placement; the previous outline was kept",
                "code": "outline_refit_outside",
                "outside_outline": containment.outside_outline,
                "copper_outside_outline": containment.copper_outside_outline > 0,
            }),
        ));
    }
    let placed = std::fs::read_to_string(&path)
        .with_context(|| format!("reading placed board {}", path.display()))?;
    let updated = match replace_managed_outline(&placed, plan.to) {
        Ok(updated) => updated,
        Err(error) => return Ok(gate.rollback(ctx, json!({ "error": error.to_string() }))),
    };
    if let Err(error) = crate::route::write_board_atomically(&path, updated.as_bytes()) {
        return Ok(gate.rollback(
            ctx,
            json!({ "error": format!("could not write fitted outline: {error}") }),
        ));
    }
    Ok(gate.commit(
        ctx,
        json!({
            "ok": true,
            "changed": plan.from != plan.to,
            "path": path.display().to_string(),
            "bounds": plan.to,
            "outline_refit": outline_refit_json(&plan),
            "skipped_locked": locked.iter().collect::<Vec<_>>(),
            "skipped_staged": staged.iter().collect::<Vec<_>>(),
            "note": "re-placed the unrouted board on its compact rule-derived managed outline; \
                     locked and staged footprints kept their pose",
        }),
    ))
}

enum Outline {
    Rect(Rect),
    Polygon(Polygon),
}

impl Outline {
    fn bounds(&self) -> Rect {
        match self {
            Outline::Rect(rect) => *rect,
            Outline::Polygon(poly) => poly.bbox(),
        }
    }

    fn point_count(&self) -> usize {
        match self {
            Outline::Rect(_) => 4,
            Outline::Polygon(poly) => poly.points().len(),
        }
    }
}

fn parse_bounds(v: Option<&Value>) -> std::result::Result<Rect, String> {
    let Some(obj) = v else {
        return Err("missing required `bounds` ({min_x, max_x, min_y, max_y} in mm)".into());
    };
    let rect = Rect {
        min_x: req_num(obj, "min_x", "bounds")?,
        max_x: req_num(obj, "max_x", "bounds")?,
        min_y: req_num(obj, "min_y", "bounds")?,
        max_y: req_num(obj, "max_y", "bounds")?,
    };
    if rect.min_x >= rect.max_x || rect.min_y >= rect.max_y {
        return Err("bounds must satisfy min_x < max_x and min_y < max_y".into());
    }
    Ok(rect)
}

fn parse_outline(v: Option<&Value>) -> std::result::Result<Polygon, String> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Err("outline must be an array of >= 3 [x,y] points".into());
    };
    if arr.len() < 3 {
        return Err("outline must be an array of >= 3 [x,y] points".into());
    }
    let mut points = Vec::with_capacity(arr.len());
    for point in arr {
        let Some(xy) = point.as_array() else {
            return Err("outline point must be a [x, y] pair".into());
        };
        if xy.len() != 2 {
            return Err("outline point must be a [x, y] pair".into());
        }
        let (Some(x), Some(y)) = (xy[0].as_f64(), xy[1].as_f64()) else {
            return Err("outline point must be [x, y] numbers".into());
        };
        points.push(Point2 { x, y });
    }
    Polygon::new(points)
}

fn replace_edge_cuts(board: &str, outline: &Outline, managed: Option<bool>) -> Result<String> {
    let stripped = remove_edge_cut_shapes(board)?;
    let insert_at = stripped
        .find("\n\t(footprint ")
        .or_else(|| stripped.rfind("\n)"))
        .context("could not find insertion point in .kicad_pcb")?;
    let mut out = String::with_capacity(stripped.len() + 512);
    out.push_str(&stripped[..insert_at]);
    out.push_str(&edge_cut_sexpr(outline, managed));
    out.push_str(&stripped[insert_at..]);
    Ok(out)
}

fn remove_edge_cut_shapes(board: &str) -> Result<String> {
    let mut out = String::with_capacity(board.len());
    let mut pos = 0usize;
    while let Some(rel) = board[pos..].find("(gr_") {
        let start = pos + rel;
        out.push_str(&board[pos..start]);
        let end = sexpr_end(board, start).context("could not parse board graphic shape")?;
        let block = &board[start..end];
        if !block.contains("(layer \"Edge.Cuts\")") {
            out.push_str(block);
        }
        pos = end;
    }
    out.push_str(&board[pos..]);
    Ok(out)
}

fn edge_cut_blocks(board: &str) -> Result<Vec<String>> {
    let mut blocks = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = board[pos..].find("(gr_") {
        let start = pos + rel;
        let end = sexpr_end(board, start).context("could not parse board graphic shape")?;
        let block = &board[start..end];
        if block.contains("(layer \"Edge.Cuts\")") {
            blocks.push(block.to_owned());
        }
        pos = end;
    }
    Ok(blocks)
}

fn edge_cuts_match(board: &str, outline: &Outline) -> Result<bool> {
    let current = edge_cut_segments(board)?;
    let desired = outline_segments(outline);
    if current.len() != desired.len() {
        return Ok(false);
    }
    let mut used = vec![false; current.len()];
    for desired_segment in desired {
        let Some((idx, _)) = current.iter().enumerate().find(|(idx, current_segment)| {
            !used[*idx] && segments_match(**current_segment, desired_segment)
        }) else {
            return Ok(false);
        };
        used[idx] = true;
    }
    Ok(true)
}

fn edge_cut_segments(board: &str) -> Result<Vec<(Point2, Point2)>> {
    let mut segments = Vec::new();
    for block in edge_cut_blocks(board)? {
        let start = sexpr_point(&block, "start")
            .with_context(|| format!("Edge.Cuts shape has no start point: {block}"))?;
        let end = sexpr_point(&block, "end")
            .with_context(|| format!("Edge.Cuts shape has no end point: {block}"))?;
        if block.starts_with("(gr_rect") {
            let a = Point2::new(start.x, start.y);
            let b = Point2::new(end.x, start.y);
            let c = Point2::new(end.x, end.y);
            let d = Point2::new(start.x, end.y);
            segments.extend([(a, b), (b, c), (c, d), (d, a)]);
        } else if block.starts_with("(gr_line") {
            segments.push((start, end));
        } else {
            return Ok(Vec::new());
        }
    }
    Ok(segments)
}

fn outline_segments(outline: &Outline) -> Vec<(Point2, Point2)> {
    match outline {
        Outline::Rect(rect) => {
            let a = Point2::new(rect.min_x, rect.min_y);
            let b = Point2::new(rect.max_x, rect.min_y);
            let c = Point2::new(rect.max_x, rect.max_y);
            let d = Point2::new(rect.min_x, rect.max_y);
            vec![(a, b), (b, c), (c, d), (d, a)]
        }
        Outline::Polygon(poly) => {
            let points = poly.points();
            (0..points.len())
                .map(|idx| (points[idx], points[(idx + 1) % points.len()]))
                .collect()
        }
    }
}

fn segments_match(a: (Point2, Point2), b: (Point2, Point2)) -> bool {
    (points_match(a.0, b.0) && points_match(a.1, b.1))
        || (points_match(a.0, b.1) && points_match(a.1, b.0))
}

fn points_match(a: Point2, b: Point2) -> bool {
    a.near_eq(b, geom::STRICT_EPS)
}

fn edge_cut_sexpr(outline: &Outline, managed: Option<bool>) -> String {
    match outline {
        Outline::Rect(rect) => {
            let x0 = super::fmt_num(rect.min_x);
            let y0 = super::fmt_num(rect.min_y);
            let x1 = super::fmt_num(rect.max_x);
            let y1 = super::fmt_num(rect.max_y);
            format!(
                "\n\t(gr_rect\n\t\t(start {x0} {y0})\n\t\t(end {x1} {y1})\n\
                 \t\t(stroke\n\t\t\t(width 0.1)\n\t\t\t(type default)\n\t\t)\n\
                 \t\t(fill no)\n\t\t(layer \"Edge.Cuts\")\n\t\t(uuid \"{}\")\n\t)\n",
                match managed {
                    Some(explicit) => managed_rect_uuid(rect, explicit),
                    None => outline_uuid("rect", 0),
                }
            )
        }
        Outline::Polygon(poly) => {
            let mut out = String::new();
            let points = poly.points();
            for idx in 0..points.len() {
                let a = points[idx];
                let b = points[(idx + 1) % points.len()];
                let x0 = super::fmt_num(a.x);
                let y0 = super::fmt_num(a.y);
                let x1 = super::fmt_num(b.x);
                let y1 = super::fmt_num(b.y);
                out.push_str(&format!(
                    "\n\t(gr_line\n\t\t(start {x0} {y0})\n\t\t(end {x1} {y1})\n\
                     \t\t(stroke\n\t\t\t(width 0.1)\n\t\t\t(type default)\n\t\t)\n\
                     \t\t(layer \"Edge.Cuts\")\n\t\t(uuid \"{}\")\n\t)\n",
                    outline_uuid("poly", idx)
                ));
            }
            out
        }
    }
}

/// A seeded rectangle's identity: its coordinates and whether the caller fixed them
/// (`explicit`) or the seeder sized them (`auto`). Re-fit rewrites only the latter.
fn managed_rect_uuid(rect: &Rect, explicit: bool) -> String {
    let kind = if explicit { "explicit" } else { "auto" };
    seed_uuid(&format!(
        "edge:{kind}:{}:{}:{}:{}",
        super::fmt_num(rect.min_x),
        super::fmt_num(rect.min_y),
        super::fmt_num(rect.max_x),
        super::fmt_num(rect.max_y)
    ))
}

fn seed_uuid(key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    "gordian-seed-a".hash(&mut h1);
    key.hash(&mut h1);
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    "gordian-seed-b".hash(&mut h2);
    key.hash(&mut h2);
    let a = h1.finish();
    let b = h2.finish();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        (a as u16 & 0x0fff) | 0x5000,
        ((b >> 48) as u16 & 0x3fff) | 0x8000,
        b & 0x0000_ffff_ffff_ffff
    )
}

/// The single seeded rectangle on Edge.Cuts, and whether its bounds were explicit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ManagedOutline {
    pub rect: Rect,
    pub explicit: bool,
}

pub(crate) fn managed_outline_bounds(board: &str) -> std::result::Result<ManagedOutline, String> {
    let blocks = edge_cut_blocks(board).map_err(|error| error.to_string())?;
    let [block] = blocks.as_slice() else {
        return Err(managed_outline_error());
    };
    if !block.starts_with("(gr_rect") {
        return Err(managed_outline_error());
    }
    let start = sexpr_point(block, "start").ok_or_else(managed_outline_error)?;
    let end = sexpr_point(block, "end").ok_or_else(managed_outline_error)?;
    let rect = Rect::new(
        start.x.min(end.x),
        start.y.min(end.y),
        start.x.max(end.x),
        start.y.max(end.y),
    );
    if rect.width() <= geom::EPS || rect.height() <= geom::EPS {
        return Err(managed_outline_error());
    }
    let actual_uuid = block
        .split_once("(uuid \"")
        .and_then(|(_, tail)| tail.split_once("\")"))
        .map(|(uuid, _)| uuid);
    let explicit = [false, true]
        .into_iter()
        .find(|&explicit| actual_uuid == Some(managed_rect_uuid(&rect, explicit).as_str()));
    match explicit {
        Some(explicit) => Ok(ManagedOutline { rect, explicit }),
        None => Err(
            "cannot re-fit outline: the rectangular Edge.Cuts was hand-edited after seeding; automatic re-fit only changes its matching managed seed rectangle".to_owned(),
        ),
    }
}

fn managed_outline_error() -> String {
    "cannot re-fit outline: Edge.Cuts is not the single managed seed rectangle; non-rectangular or user-drawn outlines are never changed automatically".to_owned()
}

pub(crate) fn replace_managed_outline(board: &str, rect: Rect) -> Result<String> {
    let managed = managed_outline_bounds(board).map_err(anyhow::Error::msg)?;
    replace_edge_cuts(board, &Outline::Rect(rect), Some(managed.explicit))
}

pub(crate) fn outline_refit_json(plan: &super::place::OutlineRefitPlan) -> Value {
    let rect_json = |rect: Rect| {
        json!({
            "min_x": rect.min_x,
            "min_y": rect.min_y,
            "max_x": rect.max_x,
            "max_y": rect.max_y,
            "width": rect.width(),
            "height": rect.height(),
        })
    };
    json!({
        "from": rect_json(plan.from),
        "to": rect_json(plan.to),
        "routing_headroom_mm": {
            "west": plan.headroom.west,
            "east": plan.headroom.east,
            "north": plan.headroom.north,
            "south": plan.headroom.south,
        },
        "nets_per_edge": {
            "west": plan.edge_net_counts[0],
            "east": plan.edge_net_counts[1],
            "north": plan.edge_net_counts[2],
            "south": plan.edge_net_counts[3],
        },
    })
}

fn outline_uuid(kind: &str, idx: usize) -> String {
    // Stable UUID-shaped values; uniqueness only needs to hold within this board.
    format!("0f1e0000-0000-4000-8000-{idx:08x}{}", suffix(kind))
}

fn suffix(kind: &str) -> &'static str {
    match kind {
        "poly" => "0001",
        _ => "0000",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_rectangular_edge_cuts() {
        let board = include_str!("../tests/fixtures/two_res.kicad_pcb");
        let updated = replace_edge_cuts(
            board,
            &Outline::Rect(Rect {
                min_x: 2.0,
                min_y: 3.0,
                max_x: 12.0,
                max_y: 13.0,
            }),
            None,
        )
        .unwrap();
        assert!(updated.contains("(start 2 3)"));
        assert!(updated.contains("(end 12 13)"));
        assert!(!updated.contains("(end 30 20)"));
        assert_eq!(updated.matches("(layer \"Edge.Cuts\")").count(), 1);
    }

    #[test]
    fn outline_match_ignores_uuid_and_shape_representation() {
        let board = r#"(kicad_pcb
            (gr_line (start 12 13) (end 2 13) (layer "Edge.Cuts") (uuid "a"))
            (gr_line (start 2 3) (end 12 3) (layer "Edge.Cuts") (uuid "b"))
            (gr_line (start 2 13) (end 2 3) (layer "Edge.Cuts") (uuid "c"))
            (gr_line (start 12 3) (end 12 13) (layer "Edge.Cuts") (uuid "d"))
        )"#;
        let outline = Outline::Rect(Rect {
            min_x: 2.0,
            min_y: 3.0,
            max_x: 12.0,
            max_y: 13.0,
        });

        assert!(edge_cuts_match(board, &outline).unwrap());
    }

    #[test]
    fn emits_polygon_edge_cuts_as_lines() {
        let board = include_str!("../tests/fixtures/two_res.kicad_pcb");
        let poly = Polygon::new(vec![
            Point2 { x: 0.0, y: 0.0 },
            Point2 { x: 10.0, y: 0.0 },
            Point2 { x: 5.0, y: 8.0 },
        ])
        .unwrap();
        let updated = replace_edge_cuts(board, &Outline::Polygon(poly), None).unwrap();
        assert_eq!(updated.matches("(layer \"Edge.Cuts\")").count(), 3);
        assert!(updated.contains("(gr_line"));
        assert!(!updated.contains("(gr_rect"));
    }

    #[test]
    fn managed_rectangle_round_trips_and_rejects_coordinate_edits() {
        let original = Rect::new(2.0, 3.0, 12.0, 13.0);
        let board = format!(
            "(kicad_pcb{})",
            edge_cut_sexpr(&Outline::Rect(original), Some(false))
        );

        assert_eq!(managed_outline_bounds(&board).unwrap().rect, original);
        let changed = board.replacen("(end 12 13)", "(end 12.5 13)", 1);
        assert!(
            managed_outline_bounds(&changed)
                .unwrap_err()
                .contains("hand-edited")
        );
    }

    #[test]
    fn managed_outline_rejects_user_drawn_edges() {
        let board = r#"(kicad_pcb
            (gr_line (start 0 0) (end 10 0) (layer "Edge.Cuts") (uuid "a"))
            (gr_line (start 10 0) (end 0 0) (layer "Edge.Cuts") (uuid "b"))
        )"#;

        assert!(
            managed_outline_bounds(board)
                .unwrap_err()
                .contains("user-drawn")
        );
    }

    #[test]
    fn routed_board_refit_is_a_reported_no_op() {
        let Some(ctx) = AgentRuntime::detect_for_test() else {
            eprintln!("SKIP: KiCad is not installed");
            return;
        };
        let board = include_str!("../tests/fixtures/two_res.kicad_pcb");
        let close = board.rfind(')').unwrap();
        let routed = format!(
            "{}\t(segment (start 7.0875 10) (end 22 10.9125) (width 0.25) \
             (layer \"F.Cu\") (net 2))\n{}",
            &board[..close],
            &board[close..],
        );
        std::fs::write(ctx.pcb_path(), &routed).unwrap();

        let result = update_board_outline(json!({ "fit_to_geometry": true }), &ctx).unwrap();

        assert_eq!(result["ok"], json!(true), "{result:#}");
        assert_eq!(result["changed"], json!(false), "{result:#}");
        assert_eq!(result["outline_refit"], json!("skipped (routed board)"));
        assert_eq!(std::fs::read_to_string(ctx.pcb_path()).unwrap(), routed);
    }
}
