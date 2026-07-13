//! `render_board` - render the live KiCad board to a PNG for the model and user.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use kicad_cli::KicadCli;
use pcb_model::{Polygon, Rect};
use serde_json::{Value, json};

use crate::AgentRuntime;

const BOARD_RENDER_LAYERS: &str = "F.Cu,B.Cu,F.SilkS,B.SilkS";
const BOARD_RENDER_BG: &str = "#050b12";
const BOARD_OUTLINE_INNER: &str = "#00e5ff";
const BOARD_AXIS: &str = "#f8fafc";
const BOARD_AXIS_GRID: &str = "#94a3b8";
const BOARD_AXIS_X: &str = "#fb7185";
const BOARD_AXIS_Y: &str = "#60a5fa";

/// Render the board to a PNG using KiCad's own PCB SVG exporter, save under
/// `.gordian/renders/`, and attach via `IMAGE_PATH_KEY`.
pub fn render_board(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let board = match super::active::board_problem(ctx) {
        Ok(board) => board,
        Err(err) => return Ok(json!({ "error": err })),
    };

    let pcb_path = match super::active::save_live_board(ctx) {
        Ok(path) => path,
        Err(err) => return Ok(json!({ "error": err })),
    };
    let tmp = tempfile::tempdir().context("temp dir for PCB SVG export")?;
    let svg_path = tmp.path().join("board.svg");
    let svg_path = match KicadCli::new(ctx.env()).export_pcb_svg(
        &pcb_path,
        &svg_path,
        BOARD_RENDER_LAYERS,
        false,
    ) {
        Ok(path) => path,
        Err(err) => {
            return Ok(json!({
                "error": format!("kicad-cli pcb export svg failed: {err}"),
            }));
        }
    };
    let svg = std::fs::read_to_string(&svg_path)
        .with_context(|| format!("reading PCB SVG {}", svg_path.display()))?;
    let svg = add_visual_overlays(&svg, board.problem.outline.as_ref(), &board.imported.bounds);

    let png = crate::render::svg_to_png(&svg, ctx.config().tools.render_max_px)?;
    let path = ctx.workspace().write_render(&png)?;

    let mut obj = json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "note": format!(
            "Board rendered from KiCad's PCB SVG export and attached. \
             Layers: {BOARD_RENDER_LAYERS}. The PNG has an explicit dark background, \
             the board outline is overlaid in cyan, and coordinate axes/ticks \
             are drawn in board millimetres for vision readability. \
             PNG also saved to png_path for the user to open."
        ),
    });
    obj[crate::tools::IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
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
