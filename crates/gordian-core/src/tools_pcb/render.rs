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
    let svg = add_accessible_outline(&svg, board.problem.outline.as_ref(), &board.imported.bounds);

    let png = crate::render::svg_to_png(&svg, ctx.config().tools.render_max_px)?;
    let path = ctx.workspace().next_render_path()?;
    std::fs::write(&path, &png).with_context(|| format!("writing render to {}", path.display()))?;

    let mut obj = json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "note": format!(
            "Board rendered from KiCad's PCB SVG export and attached. \
             Layers: {BOARD_RENDER_LAYERS}. The PNG has an explicit dark background, \
             and Edge.Cuts is overlaid with a high-contrast outline for vision readability. \
             PNG also saved to png_path for the user to open."
        ),
    });
    obj[crate::tools::IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
}

fn add_accessible_outline(svg: &str, outline: Option<&Polygon>, bounds: &Rect) -> String {
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

    overlay.push_str("</g>\n");

    if let Some(insert) = svg.rfind("</svg>") {
        svg.insert_str(insert, &overlay);
    }
    svg
}

fn expand_viewbox_and_add_background(svg: &str, bounds: &Rect) -> Option<String> {
    let (viewbox_start, viewbox_end, viewbox) = find_viewbox(svg)?;
    let pad =
        ((bounds.max_x - bounds.min_x).max(bounds.max_y - bounds.min_y) * 0.025).clamp(1.0, 4.0);
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
