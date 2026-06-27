//! `render_board` - render the live KiCad board to a PNG for the model and user.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use kicad_cli::KicadCli;
use pcb_model::{Polygon, Rect};
use serde_json::{Value, json};

use crate::AgentRuntime;

const BOARD_RENDER_LAYERS: &str = "F.Cu,B.Cu,F.SilkS,B.SilkS,Edge.Cuts";
const BOARD_RENDER_BG: &str = "#050b12";
const BOARD_OUTLINE_HALO: &str = "#ffffff";
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
    let path = ctx.workspace().next_render_path()?;
    std::fs::write(&path, &png).with_context(|| format!("writing render to {}", path.display()))?;

    let mut obj = json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "note": format!(
            "Board rendered from KiCad's PCB SVG export and attached. \
             Layers: {BOARD_RENDER_LAYERS}. The PNG has an explicit dark background, \
             Edge.Cuts is overlaid with a high-contrast outline, and coordinate axes/ticks \
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
    let inner = (outer * 0.45).clamp(0.12, 0.36);
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
            push_outline_polygon(&mut overlay, &points, BOARD_OUTLINE_HALO, outer, 0.95);
            push_outline_polygon(&mut overlay, &points, BOARD_OUTLINE_INNER, inner, 1.0);
        }
        None => {
            push_outline_rect(
                &mut overlay,
                board_w,
                board_h,
                BOARD_OUTLINE_HALO,
                outer,
                0.95,
            );
            push_outline_rect(
                &mut overlay,
                board_w,
                board_h,
                BOARD_OUTLINE_INNER,
                inner,
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
    let pad =
        ((bounds.max_x - bounds.min_x).max(bounds.max_y - bounds.min_y) * 0.08).clamp(5.0, 14.0);
    let expanded = ViewBox {
        x: viewbox.x - pad,
        y: viewbox.y - pad,
        w: viewbox.w + 2.0 * pad,
        h: viewbox.h + 2.0 * pad,
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
    out.insert_str(svg_tag_end, &background);
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
    let pad = (long * 0.08).clamp(5.0, 14.0);
    let axis_gap = (pad * 0.42).clamp(2.2, 5.8);
    let tick = (long * 0.012).clamp(0.45, 1.5);
    let font = (long * 0.018).clamp(1.0, 2.2);
    let step = nice_tick_step(long);
    let x_axis_y = board_h + axis_gap;
    let y_axis_x = -axis_gap;

    out.push_str("  <g id=\"gordian-coordinate-rulers\" fill=\"none\" stroke-linecap=\"round\" font-family=\"ui-monospace, SFMono-Regular, Menlo, Consolas, monospace\">\n");
    writeln!(
        out,
        "    <rect x=\"0\" y=\"0\" width=\"{board_w:.4}\" height=\"{board_h:.4}\" fill=\"none\" stroke=\"{BOARD_AXIS_GRID}\" stroke-width=\"0.1200\" stroke-opacity=\"0.28\"/>"
    )
    .unwrap();

    let mut x = first_tick(bounds.min_x, step);
    while x <= bounds.max_x + 1e-6 {
        let lx = x - bounds.min_x;
        writeln!(
            out,
            "    <line x1=\"{lx:.4}\" y1=\"0\" x2=\"{lx:.4}\" y2=\"{board_h:.4}\" stroke=\"{BOARD_AXIS_GRID}\" stroke-width=\"0.0800\" stroke-opacity=\"0.22\"/>"
        )
        .unwrap();
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
        writeln!(
            out,
            "    <line x1=\"0\" y1=\"{ly:.4}\" x2=\"{board_w:.4}\" y2=\"{ly:.4}\" stroke=\"{BOARD_AXIS_GRID}\" stroke-width=\"0.0800\" stroke-opacity=\"0.22\"/>"
        )
        .unwrap();
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
        "    <line x1=\"0\" y1=\"{x_axis_y:.4}\" x2=\"{board_w:.4}\" y2=\"{x_axis_y:.4}\" stroke=\"{BOARD_AXIS_X}\" stroke-width=\"0.2800\" stroke-opacity=\"0.98\"/>"
    )
    .unwrap();
    writeln!(
        out,
        "    <line x1=\"{y_axis_x:.4}\" y1=\"0\" x2=\"{y_axis_x:.4}\" y2=\"{board_h:.4}\" stroke=\"{BOARD_AXIS_Y}\" stroke-width=\"0.2800\" stroke-opacity=\"0.98\"/>"
    )
    .unwrap();
    writeln!(
        out,
        "    <text x=\"{:.4}\" y=\"{:.4}\" fill=\"{BOARD_AXIS_X}\" stroke=\"none\" font-size=\"{:.4}\" font-weight=\"700\" text-anchor=\"end\">X mm</text>",
        board_w,
        x_axis_y - tick * 1.8,
        font * 1.08,
    )
    .unwrap();
    writeln!(
        out,
        "    <text x=\"{:.4}\" y=\"{:.4}\" fill=\"{BOARD_AXIS_Y}\" stroke=\"none\" font-size=\"{:.4}\" font-weight=\"700\" text-anchor=\"start\">Y mm down</text>",
        y_axis_x + tick * 1.6,
        font,
        font * 1.08,
    )
    .unwrap();
    out.push_str("  </g>\n");
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

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 10">
<rect x="0" y="0" width="20" height="10"/>
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
        assert!(svg.contains(">Y mm down</text>"));
        assert!(svg.contains(">10</text>"));
        assert!(svg.contains(">20</text>"));
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
