//! Existing-board outline editing.
//!
//! This is the non-destructive PCB geometry path: it edits the current
//! `.kicad_pcb` Edge.Cuts instead of regenerating the board from a schematic.

use anyhow::{Context, Result};
use pcb_model::{Point2, Polygon, Rect};
use serde_json::{Value, json};

use crate::AgentRuntime;

use super::create::req_num;

/// Replace the current board Edge.Cuts with a rectangle or arbitrary polygon.
///
/// Inputs:
/// - `bounds`: `{min_x,max_x,min_y,max_y}` rectangular outline in mm.
/// - `outline`: `[[x,y], ...]` arbitrary closed polygon in mm.
/// - `fit_to_geometry`: when true, derives rectangular bounds from current
///   footprint/copper geometry plus `margin`.
pub fn update_board_outline(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let fit = input
        .get("fit_to_geometry")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let margin = input.get("margin").and_then(Value::as_f64).unwrap_or(2.0);

    let outline = if fit {
        let board = match super::active::board_problem(ctx) {
            Ok(board) => board,
            Err(err) => return Ok(json!({ "error": err })),
        };
        let Some(bounds) = geometry_bounds(&board) else {
            return Ok(json!({
                "error": "cannot fit outline: board has no footprint or copper geometry"
            }));
        };
        Outline::Rect(expand_rect(bounds, margin))
    } else if input.get("outline").is_some() {
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
            "error": "update_board_outline needs `bounds`, `outline`, or fit_to_geometry=true"
        }));
    };

    // If KiCad has the board open, save first so the file edit is applied to the
    // latest state; close after writing so later tools reopen the updated board.
    let _ = ctx.kicad().save_if_open();
    let path = ctx.pcb_path();
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading board {}", path.display()))?;
    let changed = !edge_cuts_match(&text, &outline)?;
    let updated = replace_edge_cuts(&text, &outline)?;
    if changed {
        std::fs::write(&path, updated)
            .with_context(|| format!("writing board {}", path.display()))?;
        ctx.close_kicad_session();
    }

    let bounds = outline.bounds();
    Ok(json!({
        "ok": true,
        "changed": changed,
        "path": path.display().to_string(),
        "bounds": bounds,
        "outline_points": outline.point_count(),
        "note": "updated Edge.Cuts on the existing PCB without regenerating placement or routing",
    }))
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

fn geometry_bounds(board: &kicad_ipc::snapshot::IpcBoardSnapshot) -> Option<Rect> {
    let mut points = Vec::new();
    points.extend(board.imported.parts.iter().map(|part| part.at));
    for trace in &board.copper.traces {
        points.extend(trace.path.iter().copied());
    }
    points.extend(board.copper.vias.iter().map(|via| via.at));
    Rect::bounding(&points)
}

fn expand_rect(mut rect: Rect, margin: f64) -> Rect {
    let margin = margin.max(0.0);
    rect.min_x -= margin;
    rect.max_x += margin;
    rect.min_y -= margin;
    rect.max_y += margin;
    rect
}

fn replace_edge_cuts(board: &str, outline: &Outline) -> Result<String> {
    let stripped = remove_edge_cut_shapes(board)?;
    let insert_at = stripped
        .find("\n\t(footprint ")
        .or_else(|| stripped.rfind("\n)"))
        .context("could not find insertion point in .kicad_pcb")?;
    let mut out = String::with_capacity(stripped.len() + 512);
    out.push_str(&stripped[..insert_at]);
    out.push_str(&edge_cut_sexpr(outline));
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

fn sexpr_point(block: &str, key: &str) -> Option<Point2> {
    let marker = format!("({key} ");
    let rest = block.split_once(&marker)?.1;
    let mut values = rest
        .split(|ch: char| ch.is_ascii_whitespace() || ch == ')')
        .filter(|value| !value.is_empty());
    Some(Point2::new(
        values.next()?.parse().ok()?,
        values.next()?.parse().ok()?,
    ))
}

fn segments_match(a: (Point2, Point2), b: (Point2, Point2)) -> bool {
    (points_match(a.0, b.0) && points_match(a.1, b.1))
        || (points_match(a.0, b.1) && points_match(a.1, b.0))
}

fn points_match(a: Point2, b: Point2) -> bool {
    (a.x - b.x).abs() <= 1e-9 && (a.y - b.y).abs() <= 1e-9
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

fn edge_cut_sexpr(outline: &Outline) -> String {
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
                outline_uuid("rect", 0)
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
        let board = include_str!("../../tests/fixtures/two_res.kicad_pcb");
        let updated = replace_edge_cuts(
            board,
            &Outline::Rect(Rect {
                min_x: 2.0,
                min_y: 3.0,
                max_x: 12.0,
                max_y: 13.0,
            }),
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
        let board = include_str!("../../tests/fixtures/two_res.kicad_pcb");
        let poly = Polygon::new(vec![
            Point2 { x: 0.0, y: 0.0 },
            Point2 { x: 10.0, y: 0.0 },
            Point2 { x: 5.0, y: 8.0 },
        ])
        .unwrap();
        let updated = replace_edge_cuts(board, &Outline::Polygon(poly)).unwrap();
        assert_eq!(updated.matches("(layer \"Edge.Cuts\")").count(), 3);
        assert!(updated.contains("(gr_line"));
        assert!(!updated.contains("(gr_rect"));
    }
}
