//! `render_board` - render the live KiCad board to a PNG for the model and user.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kicad_cli::KicadCli;
use pcb_model::{Point2, Polygon, Rect};
use serde_json::{Value, json};

use gordian_runtime::AgentRuntime;

const BOARD_RENDER_LAYERS: &str = "F.Cu,B.Cu,F.SilkS,B.SilkS";
const BOARD_FRONT_DETAIL_LAYERS: &str = "F.Cu,F.SilkS";
const BOARD_BACK_DETAIL_LAYERS: &str = "B.Cu,B.SilkS";
const BOARD_RENDER_BG: &str = "#050b12";
const BOARD_OUTLINE_INNER: &str = "#00e5ff";
const BOARD_AXIS: &str = "#f8fafc";
const BOARD_AXIS_GRID: &str = "#94a3b8";
const BOARD_AXIS_X: &str = "#fb7185";
const BOARD_AXIS_Y: &str = "#60a5fa";
const DENSE_BOARD_PARTS: usize = 40;
const REFERENCE_TEXT_HEIGHT_MM: f64 = 0.8;
const TARGET_REFERENCE_HEIGHT_PX: f64 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderPlan {
    overview_px: u32,
    detail_px: Option<u32>,
}

/// Render the board to a PNG using KiCad's own PCB SVG exporter, save under
/// `.gordian/renders/`, and attach via `IMAGE_PATH_KEY`.
pub fn render_board(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let pcb_path = ctx.pcb_path();
    if !pcb_path.exists() {
        return Ok(json!({
            "error": "no board exists yet — run regenerate_board first"
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
        Err(file_err) => match super::active::board_problem(ctx) {
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
    let cli = KicadCli::new(ctx.env());
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

    let plan = render_plan(
        source.part_count,
        &source.bounds,
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
                &cli,
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
                let start =
                    super::sexpr::sexpr_point(block, "start").ok_or("Edge.Cuts rectangle has no start")?;
                let end =
                    super::sexpr::sexpr_point(block, "end").ok_or("Edge.Cuts rectangle has no end")?;
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
        let start = super::sexpr::sexpr_point(block, "start").ok_or("Edge.Cuts line has no start")?;
        let end = super::sexpr::sexpr_point(block, "end").ok_or("Edge.Cuts line has no end")?;
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
    super::sexpr::sexpr_end(text, start).map(|end| BoardNode { start, end })
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

fn render_plan(part_count: usize, bounds: &Rect, configured_max_px: u32) -> RenderPlan {
    let base = configured_max_px.max(1);
    let board_w = (bounds.max_x - bounds.min_x).max(0.0);
    let board_h = (bounds.max_y - bounds.min_y).max(0.0);
    let board_long = board_w.max(board_h);
    let needs_detail = part_count >= DENSE_BOARD_PARTS
        || estimated_reference_pixels(board_long, base) < TARGET_REFERENCE_HEIGHT_PX;
    if !needs_detail {
        return RenderPlan {
            overview_px: base,
            detail_px: None,
        };
    }

    // At most double the caller's normal render budget. This keeps memory bounded
    // while making the common 1600 px configuration produce a 3200 px inspection
    // artifact for a dense 200 mm board (roughly 10 px-high 0.8 mm references).
    let margins = overlay_margins(board_long);
    let overview_long =
        (board_w + margins.left + margins.right).max(board_h + margins.top + margins.bottom);
    let required = ((overview_long / REFERENCE_TEXT_HEIGHT_MM) * TARGET_REFERENCE_HEIGHT_PX)
        .ceil()
        .max(base as f64) as u32;
    let detail_px = required.min(base.saturating_mul(2)).max(base);
    RenderPlan {
        overview_px: detail_px,
        detail_px: Some(detail_px),
    }
}

fn estimated_reference_pixels(board_long_mm: f64, long_edge_px: u32) -> f64 {
    if board_long_mm <= 0.0 {
        return f64::INFINITY;
    }
    REFERENCE_TEXT_HEIGHT_MM * long_edge_px as f64 / board_long_mm
}

#[allow(clippy::too_many_arguments)]
fn render_side_detail(
    cli: &KicadCli,
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
    let Some(mut svg) = expand_viewbox_and_add_background(svg, bounds) else {
        return svg.to_owned();
    };

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

    push_coordinate_axes(&mut overlay, bounds);
    overlay.push_str("</g>\n");

    if let Some(insert) = svg.rfind("</svg>") {
        svg.insert_str(insert, &overlay);
    }
    svg
}

fn expand_viewbox_and_add_background(svg: &str, bounds: &Rect) -> Option<String> {
    let (viewbox_start, viewbox_end, viewbox) = find_viewbox(svg)?;
    let margins = overlay_margins((bounds.max_x - bounds.min_x).max(bounds.max_y - bounds.min_y));
    let expanded = ViewBox {
        x: viewbox.x - margins.left,
        y: viewbox.y - margins.top,
        w: viewbox.w + margins.left + margins.right,
        h: viewbox.h + margins.top + margins.bottom,
    };
    let background = format!(
        "\n  <rect id=\"gordian-render-background\" x=\"{:.4}\" y=\"{:.4}\" width=\"{:.4}\" height=\"{:.4}\" fill=\"{}\"/>\n",
        expanded.x, expanded.y, expanded.w, expanded.h, BOARD_RENDER_BG
    );

    let mut out = String::with_capacity(svg.len() + background.len() + 64);
    out.push_str(&svg[..viewbox_start]);
    write!(
        out,
        "viewBox=\"{:.4} {:.4} {:.4} {:.4}\"",
        expanded.x, expanded.y, expanded.w, expanded.h
    )
    .unwrap();
    out.push_str(&svg[viewbox_end..]);

    let svg_tag_start = out.find("<svg")?;
    let svg_tag_end = out[svg_tag_start..].find('>')? + svg_tag_start + 1;
    out = replace_svg_root_dimension(&out, svg_tag_start, svg_tag_end, "width", expanded.w)?;
    let svg_tag_start = out.find("<svg")?;
    let svg_tag_end = out[svg_tag_start..].find('>')? + svg_tag_start + 1;
    out = replace_svg_root_dimension(&out, svg_tag_start, svg_tag_end, "height", expanded.h)?;
    let svg_tag_start = out.find("<svg")?;
    let svg_tag_end = out[svg_tag_start..].find('>')? + svg_tag_start + 1;
    out.insert_str(svg_tag_end, &background);
    Some(out)
}

fn replace_svg_root_dimension(
    svg: &str,
    svg_tag_start: usize,
    svg_tag_end: usize,
    attr: &str,
    value_mm: f64,
) -> Option<String> {
    let tag = &svg[svg_tag_start..svg_tag_end];
    let needle = format!("{attr}=\"");
    let mut out = String::with_capacity(svg.len() + 16);
    if let Some(rel_attr) = tag.find(&needle) {
        let rel_start = rel_attr + needle.len();
        let value_start = svg_tag_start + rel_start;
        let value_end = svg[value_start..].find('"')? + value_start;
        out.push_str(&svg[..value_start]);
        write!(out, "{value_mm:.4}mm").unwrap();
        out.push_str(&svg[value_end..]);
    } else {
        let insert = svg_tag_end - 1;
        out.push_str(&svg[..insert]);
        write!(out, " {attr}=\"{value_mm:.4}mm\"").unwrap();
        out.push_str(&svg[insert..]);
    }
    Some(out)
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

fn push_coordinate_axes(out: &mut String, bounds: &Rect) {
    let board_w = bounds.max_x - bounds.min_x;
    let board_h = bounds.max_y - bounds.min_y;
    if board_w <= 0.0 || board_h <= 0.0 {
        return;
    }

    let long = board_w.max(board_h);
    let margins = overlay_margins(long);
    let axis_gap = (margins.left * 0.36).clamp(2.8, 6.2);
    let tick = (long * 0.012).clamp(0.45, 1.5);
    let font = (long * 0.018).clamp(1.0, 2.2);
    let arrow = (tick * 2.8).clamp(1.8, 3.8);
    let step = nice_tick_step(long);
    let x_axis_y = board_h + axis_gap;
    let y_axis_x = -axis_gap;
    let x_axis_end = board_w + arrow;
    let y_axis_end = board_h + arrow;

    out.push_str("  <g id=\"gordian-coordinate-rulers\" fill=\"none\" stroke-linecap=\"round\" font-family=\"ui-monospace, SFMono-Regular, Menlo, Consolas, monospace\">\n");

    let mut x = first_tick(bounds.min_x, step);
    while x <= bounds.max_x + 1e-6 {
        let lx = x - bounds.min_x;
        if lx > 1e-6 && lx < board_w - 1e-6 {
            writeln!(
                out,
                "    <line x1=\"{lx:.4}\" y1=\"0\" x2=\"{lx:.4}\" y2=\"{board_h:.4}\" stroke=\"{BOARD_AXIS_GRID}\" stroke-width=\"0.0800\" stroke-opacity=\"0.22\"/>"
            )
            .unwrap();
        }
        writeln!(
            out,
            "    <line x1=\"{lx:.4}\" y1=\"{:.4}\" x2=\"{lx:.4}\" y2=\"{:.4}\" stroke=\"{BOARD_AXIS}\" stroke-width=\"0.1800\" stroke-opacity=\"0.95\"/>",
            x_axis_y - tick,
            x_axis_y + tick,
        )
        .unwrap();
        writeln!(
            out,
            "    <text x=\"{lx:.4}\" y=\"{:.4}\" fill=\"{BOARD_AXIS}\" stroke=\"none\" font-size=\"{font:.4}\" text-anchor=\"middle\">{}</text>",
            x_axis_y + tick + font,
            fmt_axis_label(x),
        )
        .unwrap();
        x += step;
    }

    let mut y = first_tick(bounds.min_y, step);
    while y <= bounds.max_y + 1e-6 {
        let ly = y - bounds.min_y;
        if ly > 1e-6 && ly < board_h - 1e-6 {
            writeln!(
                out,
                "    <line x1=\"0\" y1=\"{ly:.4}\" x2=\"{board_w:.4}\" y2=\"{ly:.4}\" stroke=\"{BOARD_AXIS_GRID}\" stroke-width=\"0.0800\" stroke-opacity=\"0.22\"/>"
            )
            .unwrap();
        }
        writeln!(
            out,
            "    <line x1=\"{:.4}\" y1=\"{ly:.4}\" x2=\"{:.4}\" y2=\"{ly:.4}\" stroke=\"{BOARD_AXIS}\" stroke-width=\"0.1800\" stroke-opacity=\"0.95\"/>",
            y_axis_x - tick,
            y_axis_x + tick,
        )
        .unwrap();
        writeln!(
            out,
            "    <text x=\"{:.4}\" y=\"{:.4}\" fill=\"{BOARD_AXIS}\" stroke=\"none\" font-size=\"{font:.4}\" text-anchor=\"end\">{}</text>",
            y_axis_x - tick * 1.2,
            ly + font * 0.35,
            fmt_axis_label(y),
        )
        .unwrap();
        y += step;
    }

    writeln!(
        out,
        "    <line x1=\"0\" y1=\"{x_axis_y:.4}\" x2=\"{x_axis_end:.4}\" y2=\"{x_axis_y:.4}\" stroke=\"{BOARD_AXIS_X}\" stroke-width=\"0.2800\" stroke-opacity=\"0.98\"/>"
    )
    .unwrap();
    writeln!(
        out,
        "    <polygon points=\"{:.4},{:.4} {:.4},{:.4} {:.4},{:.4}\" fill=\"{BOARD_AXIS_X}\" stroke=\"none\" fill-opacity=\"0.98\"/>",
        x_axis_end,
        x_axis_y,
        x_axis_end - arrow,
        x_axis_y - arrow * 0.45,
        x_axis_end - arrow,
        x_axis_y + arrow * 0.45,
    )
    .unwrap();
    writeln!(
        out,
        "    <line x1=\"{y_axis_x:.4}\" y1=\"0\" x2=\"{y_axis_x:.4}\" y2=\"{y_axis_end:.4}\" stroke=\"{BOARD_AXIS_Y}\" stroke-width=\"0.2800\" stroke-opacity=\"0.98\"/>"
    )
    .unwrap();
    writeln!(
        out,
        "    <polygon points=\"{:.4},{:.4} {:.4},{:.4} {:.4},{:.4}\" fill=\"{BOARD_AXIS_Y}\" stroke=\"none\" fill-opacity=\"0.98\"/>",
        y_axis_x,
        y_axis_end,
        y_axis_x - arrow * 0.45,
        y_axis_end - arrow,
        y_axis_x + arrow * 0.45,
        y_axis_end - arrow,
    )
    .unwrap();
    writeln!(
        out,
        "    <text x=\"{:.4}\" y=\"{:.4}\" fill=\"{BOARD_AXIS_X}\" stroke=\"none\" font-size=\"{:.4}\" font-weight=\"700\" text-anchor=\"start\">X mm</text>",
        x_axis_end + font * 0.45,
        x_axis_y + font * 0.35,
        font * 1.08,
    )
    .unwrap();
    writeln!(
        out,
        "    <text x=\"{:.4}\" y=\"{:.4}\" fill=\"{BOARD_AXIS_Y}\" stroke=\"none\" font-size=\"{:.4}\" font-weight=\"700\" text-anchor=\"middle\">Y mm</text>",
        y_axis_x,
        y_axis_end + font * 1.2,
        font * 1.08,
    )
    .unwrap();
    out.push_str("  </g>\n");
}

#[derive(Clone, Copy, Debug)]
struct OverlayMargins {
    top: f64,
    right: f64,
    bottom: f64,
    left: f64,
}

fn overlay_margins(long: f64) -> OverlayMargins {
    let main = (long * 0.12).clamp(8.0, 18.0);
    OverlayMargins {
        top: (long * 0.018).clamp(1.2, 3.0),
        right: main * 1.25,
        bottom: main,
        left: main,
    }
}

fn nice_tick_step(span: f64) -> f64 {
    let raw = (span / 6.0).max(1.0);
    let exp = raw.log10().floor();
    let base = 10f64.powf(exp);
    for factor in [1.0, 2.0, 5.0, 10.0] {
        let step = factor * base;
        if step >= raw {
            return step;
        }
    }
    10.0 * base
}

fn first_tick(min: f64, step: f64) -> f64 {
    (min / step).ceil() * step
}

fn fmt_axis_label(v: f64) -> String {
    let v = if v.abs() < 0.0005 { 0.0 } else { v };
    if (v - v.round()).abs() < 0.0005 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
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
            render_plan(source.part_count, &source.bounds, 1600)
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
        let plan = render_plan(12, &Rect::new(0.0, 0.0, 80.0, 70.0), 1600);

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
        let dense = render_plan(40, &Rect::new(0.0, 0.0, 80.0, 70.0), 1600);
        assert_eq!(dense.detail_px, Some(1600));

        let large = render_plan(12, &Rect::new(0.0, 0.0, 200.0, 120.0), 1600);
        let detail_px = large.detail_px.expect("large board detail render");
        assert!(detail_px > 1600 && detail_px <= 3200, "{large:?}");
        assert_eq!(large.overview_px, detail_px);

        let margins = overlay_margins(200.0);
        let overview_long =
            (200.0 + margins.left + margins.right).max(120.0 + margins.top + margins.bottom);
        assert!(
            estimated_reference_pixels(overview_long, large.overview_px)
                >= TARGET_REFERENCE_HEIGHT_PX - 0.01
        );
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
