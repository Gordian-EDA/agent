//! `render_board` - render the live KiCad board to a PNG for the model and user.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kicad::KicadInstallation;
use pcb_model::{Point2, Polygon, Rect};
use serde_json::{Value, json};

use gordian_runtime::AgentRuntime;
use gordian_runtime::render::{CoordinateOverlayStyle, RenderBounds};

const BOARD_RENDER_LAYERS: &str = "F.Cu,B.Cu,F.SilkS,B.SilkS";
const BOARD_FRONT_DETAIL_LAYERS: &str = "F.Cu,F.SilkS";
const BOARD_BACK_DETAIL_LAYERS: &str = "B.Cu,B.SilkS";
const BOARD_RENDER_BG: &str = "#050b12";
const BOARD_OUTLINE_INNER: &str = "#00e5ff";
const BOARD_AXIS: &str = "#f8fafc";
const BOARD_AXIS_GRID: &str = "#94a3b8";
const BOARD_AXIS_X: &str = "#fb7185";
const BOARD_AXIS_Y: &str = "#60a5fa";

/// Render the board to a PNG using KiCad's own PCB SVG exporter, save under
/// `.gordian/renders/`, and attach via `IMAGE_PATH_KEY`.
pub fn render_board(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let pcb_path = ctx.pcb_path();
    if !pcb_path.exists() {
        return Ok(json!({
            "error": "no board exists yet — run sync_board first"
        }));
    }
    // Mutating board tools save every successful operation. If this process already
    // owns a live session, flush it without opening or launching anything; otherwise
    // the on-disk board is immediately authoritative. This avoids the old render-only
    // board_snapshot + save path, which could launch pcbnew and spend tens of seconds
    // before the sub-second `kicad-cli` export even began.
    if ctx.kicad().save_if_open().is_err() {
        ctx.close_kicad_session();
    }
    let source = match board_render_source(&pcb_path) {
        Ok(source) => source,
        Err(file_err) => match crate::active_board(ctx) {
            Ok(board) => BoardRenderSource {
                bounds: board.imported.bounds,
                outline: board.problem.outline,
                part_count: board.imported.parts.len(),
                provenance: "live_kicad_fallback",
            },
            Err(live_err) => {
                return Ok(json!({
                    "error": format!(
                        "{file_err}; live KiCad geometry fallback also failed: {live_err}"
                    )
                }));
            }
        },
    };
    let tmp = tempfile::tempdir().context("temp dir for PCB SVG export")?;
    let svg_path = tmp.path().join("board.svg");
    let cli = ctx.env();
    let svg_path = match cli.export_pcb_svg(&pcb_path, &svg_path, BOARD_RENDER_LAYERS, false) {
        Ok(path) => path,
        Err(err) => {
            return Ok(json!({
                "error": format!("kicad-cli pcb export svg failed: {err}"),
            }));
        }
    };
    let svg = std::fs::read_to_string(&svg_path)
        .with_context(|| format!("reading PCB SVG {}", svg_path.display()))?;
    let svg = add_visual_overlays(&svg, source.outline.as_ref(), &source.bounds);

    let plan = gordian_runtime::render::render_plan(
        source.part_count,
        render_bounds(&source.bounds),
        ctx.config().tools.render_max_px,
    );
    let png = gordian_runtime::render::svg_to_png(&svg, plan.overview_px)?;
    let path = ctx.workspace().write_render(&png)?;

    let mut detail_paths = serde_json::Map::new();
    let mut detail_errors = Vec::new();
    if let Some(detail_px) = plan.detail_px {
        for (side, layers, mirror) in [
            ("front", BOARD_FRONT_DETAIL_LAYERS, false),
            ("back", BOARD_BACK_DETAIL_LAYERS, true),
        ] {
            match render_side_detail(
                cli,
                &pcb_path,
                tmp.path(),
                side,
                layers,
                mirror,
                detail_px,
                ctx,
            ) {
                Ok(detail_path) => {
                    detail_paths.insert(
                        side.to_owned(),
                        Value::String(detail_path.display().to_string()),
                    );
                }
                Err(err) => detail_errors.push(format!("{side}: {err}")),
            }
        }
    }

    let mut obj = json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "overview_px": plan.overview_px,
        "detail_paths": detail_paths,
        "source": source.provenance,
        "note": format!(
            "Board rendered directly from the saved .kicad_pcb using KiCad's PCB SVG export and attached. \
             Layers: {BOARD_RENDER_LAYERS}. The PNG has an explicit dark background, \
             the board outline is overlaid in cyan, and coordinate axes/ticks \
             are drawn in board millimetres for vision readability. \
             PNG also saved to png_path for the user to open. Dense/large boards also \
             return uncluttered front and mirrored-back detail_paths."
        ),
    });
    if !detail_errors.is_empty() {
        obj["detail_errors"] = json!(detail_errors);
    }
    obj[gordian_runtime::tool::IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
}

#[derive(Debug)]
struct BoardRenderSource {
    bounds: Rect,
    outline: Option<Polygon>,
    part_count: usize,
    provenance: &'static str,
}

/// Read only the geometry render needs from the authoritative board file. This
/// intentionally does not construct a routing snapshot: footprint count and the
/// outer Edge.Cuts rectangle/polygon are enough to retain overview sizing, axes,
/// outline overlays, and the dense-board front/back detail decision.
fn board_render_source(path: &Path) -> std::result::Result<BoardRenderSource, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read saved board {}: {err}", path.display()))?;
    parse_board_render_source(&text).map_err(|err| {
        format!(
            "could not read render geometry from {}: {err}",
            path.display()
        )
    })
}

#[derive(Clone, Copy)]
struct BoardNode {
    start: usize,
    end: usize,
}

fn parse_board_render_source(text: &str) -> std::result::Result<BoardRenderSource, String> {
    let root_start = text.find("(kicad_pcb").ok_or("not a kicad_pcb document")?;
    let root = balanced_node(text, root_start).ok_or("unbalanced kicad_pcb document")?;
    let nodes = child_board_nodes(text, root.start + 1, root.end - 1);
    let part_count = nodes
        .iter()
        .filter(|node| board_node_head(text, node) == "footprint")
        .count();
    let edge_nodes: Vec<_> = nodes
        .iter()
        .filter(|node| {
            let head = board_node_head(text, node);
            matches!(head, "gr_rect" | "gr_line" | "gr_poly")
                && text[node.start..node.end].contains("(layer \"Edge.Cuts\")")
        })
        .copied()
        .collect();
    if edge_nodes.is_empty() {
        return Err("board has no supported outer Edge.Cuts geometry".to_owned());
    }

    if edge_nodes.len() == 1 {
        let node = edge_nodes[0];
        let block = &text[node.start..node.end];
        match board_node_head(text, &node) {
            "gr_rect" => {
                let start = kicad_board::sexpr_point(block, "start")
                    .ok_or("Edge.Cuts rectangle has no start")?;
                let end = kicad_board::sexpr_point(block, "end")
                    .ok_or("Edge.Cuts rectangle has no end")?;
                let bounds = Rect::new(
                    start.x.min(end.x),
                    start.y.min(end.y),
                    start.x.max(end.x),
                    start.y.max(end.y),
                );
                if bounds.max_x <= bounds.min_x || bounds.max_y <= bounds.min_y {
                    return Err("Edge.Cuts rectangle has empty bounds".to_owned());
                }
                return Ok(BoardRenderSource {
                    bounds,
                    outline: None,
                    part_count,
                    provenance: "saved_board_file",
                });
            }
            "gr_poly" => {
                let points = board_poly_points(block)?;
                let outline = Polygon::new(points)
                    .map_err(|err| format!("invalid Edge.Cuts polygon: {err}"))?;
                return Ok(BoardRenderSource {
                    bounds: outline.bbox(),
                    outline: Some(outline),
                    part_count,
                    provenance: "saved_board_file",
                });
            }
            _ => {}
        }
    }

    let mut segments = Vec::with_capacity(edge_nodes.len());
    for node in edge_nodes {
        let block = &text[node.start..node.end];
        if board_node_head(text, &node) != "gr_line" {
            return Err(
                "mixed or curved Edge.Cuts need a saved rectangular/line/polygon outline"
                    .to_owned(),
            );
        }
        let start =
            kicad_board::sexpr_point(block, "start").ok_or("Edge.Cuts line has no start")?;
        let end = kicad_board::sexpr_point(block, "end").ok_or("Edge.Cuts line has no end")?;
        segments.push((start, end));
    }
    let outline = Polygon::new(stitch_board_outline(segments)?)
        .map_err(|err| format!("invalid Edge.Cuts line polygon: {err}"))?;
    Ok(BoardRenderSource {
        bounds: outline.bbox(),
        outline: Some(outline),
        part_count,
        provenance: "saved_board_file",
    })
}

fn balanced_node(text: &str, start: usize) -> Option<BoardNode> {
    kicad_board::sexpr_end(text, start).map(|end| BoardNode { start, end })
}

fn child_board_nodes(text: &str, start: usize, end: usize) -> Vec<BoardNode> {
    let mut nodes = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let Some(relative) = text[cursor..end].find('(') else {
            break;
        };
        let node_start = cursor + relative;
        let Some(node) = balanced_node(text, node_start) else {
            break;
        };
        cursor = node.end;
        nodes.push(node);
    }
    nodes
}

fn board_node_head<'a>(text: &'a str, node: &BoardNode) -> &'a str {
    text[node.start + 1..node.end]
        .split(|ch: char| ch.is_ascii_whitespace() || ch == '(' || ch == ')')
        .find(|token| !token.is_empty())
        .unwrap_or("")
}

fn board_poly_points(block: &str) -> std::result::Result<Vec<Point2>, String> {
    let pts_start = block
        .find("(pts")
        .ok_or("Edge.Cuts polygon has no points")?;
    let pts = balanced_node(block, pts_start).ok_or("unbalanced Edge.Cuts polygon points")?;
    let mut points = Vec::new();
    for node in child_board_nodes(block, pts.start + 1, pts.end - 1) {
        if board_node_head(block, &node) != "xy" {
            continue;
        }
        let body = &block[node.start + 1..node.end - 1];
        let mut values = body.split_ascii_whitespace().skip(1);
        let x = values
            .next()
            .and_then(|value| value.parse().ok())
            .ok_or("invalid Edge.Cuts polygon x")?;
        let y = values
            .next()
            .and_then(|value| value.parse().ok())
            .ok_or("invalid Edge.Cuts polygon y")?;
        points.push(Point2::new(x, y));
    }
    if points.len() < 3 {
        return Err("Edge.Cuts polygon needs at least three points".to_owned());
    }
    Ok(points)
}

fn stitch_board_outline(
    mut segments: Vec<(Point2, Point2)>,
) -> std::result::Result<Vec<Point2>, String> {
    if segments.len() < 3 {
        return Err("Edge.Cuts line outline needs at least three segments".to_owned());
    }
    let (first, mut current) = segments.remove(0);
    let mut points = vec![first];
    while !segments.is_empty() {
        points.push(current);
        let Some((index, reverse)) = segments.iter().enumerate().find_map(|(index, &(a, b))| {
            if board_points_match(a, current) {
                Some((index, false))
            } else if board_points_match(b, current) {
                Some((index, true))
            } else {
                None
            }
        }) else {
            return Err("Edge.Cuts line segments do not form one closed outline".to_owned());
        };
        let (a, b) = segments.remove(index);
        current = if reverse { a } else { b };
    }
    if !board_points_match(current, first) {
        return Err("Edge.Cuts line outline is not closed".to_owned());
    }
    Ok(points)
}

fn board_points_match(a: Point2, b: Point2) -> bool {
    (a.x - b.x).abs() <= 1e-6 && (a.y - b.y).abs() <= 1e-6
}

#[allow(clippy::too_many_arguments)]
fn render_side_detail(
    cli: &KicadInstallation,
    pcb_path: &Path,
    tmp_dir: &Path,
    side: &str,
    layers: &str,
    mirror: bool,
    max_px: u32,
    ctx: &AgentRuntime,
) -> Result<PathBuf> {
    let svg_path = tmp_dir.join(format!("board-{side}.svg"));
    let svg_path = cli
        .export_pcb_svg(pcb_path, &svg_path, layers, mirror)
        .with_context(|| format!("exporting {side} PCB detail SVG"))?;
    let svg = std::fs::read_to_string(&svg_path)
        .with_context(|| format!("reading {side} PCB detail SVG {}", svg_path.display()))?;
    let svg = add_dark_background(&svg).unwrap_or(svg);
    let png = gordian_runtime::render::svg_to_png(&svg, max_px)
        .with_context(|| format!("rasterizing {side} PCB detail"))?;
    ctx.workspace()
        .write_render(&png)
        .with_context(|| format!("writing {side} PCB detail"))
}

fn add_dark_background(svg: &str) -> Option<String> {
    let (_, _, viewbox) = find_viewbox(svg)?;
    let background = format!(
        "\n  <rect id=\"gordian-render-background\" x=\"{:.4}\" y=\"{:.4}\" width=\"{:.4}\" height=\"{:.4}\" fill=\"{}\"/>\n",
        viewbox.x, viewbox.y, viewbox.w, viewbox.h, BOARD_RENDER_BG
    );
    let svg_tag_start = svg.find("<svg")?;
    let svg_tag_end = svg[svg_tag_start..].find('>')? + svg_tag_start + 1;
    let mut out = String::with_capacity(svg.len() + background.len());
    out.push_str(&svg[..svg_tag_end]);
    out.push_str(&background);
    out.push_str(&svg[svg_tag_end..]);
    Some(out)
}

fn add_visual_overlays(svg: &str, outline: Option<&Polygon>, bounds: &Rect) -> String {
    let mut svg = gordian_runtime::render::add_coordinate_overlay(
        svg,
        render_bounds(bounds),
        "mm",
        CoordinateOverlayStyle {
            background: BOARD_RENDER_BG,
            axis: BOARD_AXIS,
            grid: BOARD_AXIS_GRID,
            x_axis: BOARD_AXIS_X,
            y_axis: BOARD_AXIS_Y,
        },
    );

    let board_w = bounds.max_x - bounds.min_x;
    let board_h = bounds.max_y - bounds.min_y;
    let outer = (board_w.max(board_h) * 0.006).clamp(0.25, 0.8);
    let mut overlay = String::new();
    overlay.push_str("\n<g id=\"gordian-accessible-edge-cuts\" fill=\"none\" stroke-linejoin=\"round\" stroke-linecap=\"round\">\n");

    match outline {
        Some(poly) => {
            let points = poly
                .points()
                .iter()
                .map(|p| format!("{:.4},{:.4}", p.x - bounds.min_x, p.y - bounds.min_y))
                .collect::<Vec<_>>()
                .join(" ");
            push_outline_polygon(&mut overlay, &points, BOARD_OUTLINE_INNER, outer, 1.0);
        }
        None => {
            push_outline_rect(
                &mut overlay,
                board_w,
                board_h,
                BOARD_OUTLINE_INNER,
                outer,
                1.0,
            );
        }
    }

    overlay.push_str("</g>\n");

    if let Some(insert) = svg.rfind("</svg>") {
        svg.insert_str(insert, &overlay);
    }
    svg
}

fn render_bounds(bounds: &Rect) -> RenderBounds {
    RenderBounds::new(bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y)
}

#[derive(Clone, Copy, Debug)]
struct ViewBox {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn find_viewbox(svg: &str) -> Option<(usize, usize, ViewBox)> {
    let attr_start = svg.find("viewBox=\"")?;
    let value_start = attr_start + "viewBox=\"".len();
    let value_end = svg[value_start..].find('"')? + value_start;
    let mut vals = svg[value_start..value_end]
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(str::parse::<f64>);
    let viewbox = ViewBox {
        x: vals.next()?.ok()?,
        y: vals.next()?.ok()?,
        w: vals.next()?.ok()?,
        h: vals.next()?.ok()?,
    };
    Some((attr_start, value_end + 1, viewbox))
}

fn push_outline_polygon(out: &mut String, points: &str, color: &str, width: f64, opacity: f64) {
    writeln!(
        out,
        "  <polygon points=\"{points}\" stroke=\"{color}\" stroke-width=\"{width:.4}\" stroke-opacity=\"{opacity:.2}\"/>"
    )
    .unwrap();
}

fn push_outline_rect(
    out: &mut String,
    width: f64,
    height: f64,
    color: &str,
    stroke: f64,
    opacity: f64,
) {
    writeln!(
        out,
        "  <rect x=\"0\" y=\"0\" width=\"{width:.4}\" height=\"{height:.4}\" stroke=\"{color}\" stroke-width=\"{stroke:.4}\" stroke-opacity=\"{opacity:.2}\"/>"
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gordian_runtime::render::{RenderPlan, render_plan};

    const SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 10">
<rect x="0" y="0" width="20" height="10"/>
</svg>"#;
    const SVG_80X70: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 80 70">
<rect x="0" y="0" width="80" height="70"/>
</svg>"#;
    const SVG_80X70_SIZED: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="80mm" height="70mm" viewBox="0 0 80 70">
<rect x="0" y="0" width="80" height="70"/>
</svg>"#;

    #[test]
    fn saved_rect_board_source_preserves_bounds_and_dense_detail_count() {
        let footprints = (0..40)
            .map(|index| {
                format!("(footprint \"Part:{index}\" (property \"Reference\" \"R{index}\"))")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let board = format!(
            "(kicad_pcb\n{footprints}\n\
             (gr_rect (start 10 20) (end 85 75) (layer \"Edge.Cuts\"))\n)"
        );

        let source = parse_board_render_source(&board).expect("saved board geometry");

        assert_eq!(source.bounds, Rect::new(10.0, 20.0, 85.0, 75.0));
        assert!(source.outline.is_none());
        assert_eq!(source.part_count, 40);
        assert!(
            render_plan(source.part_count, render_bounds(&source.bounds), 1600)
                .detail_px
                .is_some()
        );
    }

    #[test]
    fn saved_line_board_source_stitches_shuffled_polygon_outline() {
        let board = r#"(kicad_pcb
            (footprint "A" (fp_line (start 0 0) (end 1 1)))
            (gr_line (start 50 55) (end 10 48) (layer "Edge.Cuts"))
            (gr_line (start 10 20) (end 50 20) (layer "Edge.Cuts"))
            (gr_line (start 10 48) (end 10 20) (layer "Edge.Cuts"))
            (gr_line (start 50 20) (end 50 55) (layer "Edge.Cuts"))
        )"#;

        let source = parse_board_render_source(board).expect("line outline");

        assert_eq!(
            source.part_count, 1,
            "nested footprint graphics are not parts"
        );
        assert_eq!(source.bounds, Rect::new(10.0, 20.0, 50.0, 55.0));
        let outline = source.outline.expect("non-rectangular polygon retained");
        assert_eq!(outline.points().len(), 4);
        let overlaid = add_visual_overlays(SVG, Some(&outline), &source.bounds);
        assert!(overlaid.contains("id=\"gordian-accessible-edge-cuts\""));
        assert!(overlaid.contains("40.0000,35.0000"));
    }

    #[test]
    fn sparse_small_board_keeps_the_configured_overview_only() {
        let plan = render_plan(12, render_bounds(&Rect::new(0.0, 0.0, 80.0, 70.0)), 1600);

        assert_eq!(
            plan,
            RenderPlan {
                overview_px: 1600,
                detail_px: None,
            }
        );
    }

    #[test]
    fn dense_or_physically_large_boards_get_bounded_readable_details() {
        let dense = render_plan(40, render_bounds(&Rect::new(0.0, 0.0, 80.0, 70.0)), 1600);
        assert_eq!(dense.detail_px, Some(1600));

        let large = render_plan(12, render_bounds(&Rect::new(0.0, 0.0, 200.0, 120.0)), 1600);
        let detail_px = large.detail_px.expect("large board detail render");
        assert!(detail_px > 1600 && detail_px <= 3200, "{large:?}");
        assert_eq!(large.overview_px, detail_px);
    }

    #[test]
    fn side_detail_background_preserves_the_original_viewbox() {
        let svg = add_dark_background(SVG).expect("valid SVG");

        assert!(svg.contains("viewBox=\"0 0 20 10\""));
        assert!(svg.contains("id=\"gordian-render-background\""));
        assert!(svg.contains("x=\"0.0000\" y=\"0.0000\" width=\"20.0000\" height=\"10.0000\""));
        assert!(
            svg.find("gordian-render-background").unwrap() < svg.find("<rect x=\"0\"").unwrap()
        );
    }

    #[test]
    fn board_render_overlay_adds_coordinate_rulers() {
        let bounds = Rect {
            min_x: 10.0,
            max_x: 30.0,
            min_y: 20.0,
            max_y: 30.0,
        };

        let svg = add_visual_overlays(SVG, None, &bounds);

        assert!(svg.contains("id=\"gordian-coordinate-rulers\""));
        assert!(svg.contains(">X mm</text>"));
        assert!(svg.contains(">Y mm</text>"));
        assert!(svg.contains(">10</text>"));
        assert!(svg.contains(">20</text>"));
    }

    #[test]
    fn board_render_overlay_handles_polygon_outline_without_white_halo() {
        let bounds = Rect {
            min_x: 10.0,
            max_x: 50.0,
            min_y: 20.0,
            max_y: 55.0,
        };
        let outline = Polygon::new(vec![
            pcb_model::Point2 { x: 10.0, y: 20.0 },
            pcb_model::Point2 { x: 50.0, y: 20.0 },
            pcb_model::Point2 { x: 44.0, y: 55.0 },
            pcb_model::Point2 { x: 10.0, y: 48.0 },
        ])
        .unwrap();

        let svg = add_visual_overlays(SVG, Some(&outline), &bounds);

        assert!(svg.contains(
            "<polygon points=\"0.0000,0.0000 40.0000,0.0000 34.0000,35.0000 0.0000,28.0000\""
        ));
        assert!(svg.contains("stroke=\"#00e5ff\""));
        assert!(!svg.contains("stroke=\"#ffffff\""));
        assert!(svg.contains("id=\"gordian-coordinate-rulers\""));
    }

    #[test]
    fn board_render_overlay_uses_asymmetric_viewbox_padding() {
        let bounds = Rect {
            min_x: 0.0,
            max_x: 80.0,
            min_y: 0.0,
            max_y: 70.0,
        };

        let svg = add_visual_overlays(SVG_80X70, None, &bounds);

        assert!(svg.contains("viewBox=\"-9.6000 -1.4400 101.6000 81.0400\""));
        assert!(svg.contains("width=\"101.6000mm\""));
        assert!(svg.contains("height=\"81.0400mm\""));
        assert!(
            !svg.contains("width=\"80.0000\" height=\"70.0000\" fill=\"none\" stroke=\"#94a3b8\"")
        );
        assert!(
            !svg.contains("x1=\"0.0000\" y1=\"0\" x2=\"0.0000\" y2=\"70.0000\" stroke=\"#94a3b8\"")
        );
        assert!(
            !svg.contains("x1=\"0\" y1=\"0.0000\" x2=\"80.0000\" y2=\"0.0000\" stroke=\"#94a3b8\"")
        );
    }

    #[test]
    fn board_render_overlay_rewrites_root_size_to_prevent_letterbox() {
        let bounds = Rect {
            min_x: 0.0,
            max_x: 80.0,
            min_y: 0.0,
            max_y: 70.0,
        };

        let svg = add_visual_overlays(SVG_80X70_SIZED, None, &bounds);

        assert!(svg.contains("width=\"101.6000mm\""));
        assert!(svg.contains("height=\"81.0400mm\""));
        assert!(!svg.contains("width=\"80mm\""));
        assert!(!svg.contains("height=\"70mm\""));
    }
}
