//! Rendering the live schematic to PNG.
//!
//! One KiCAD SVG export feeds two consumers: the `render_schematic` tool result
//! the model looks at (a millimetre-annotated overview, plus quadrant details on
//! a dense sheet), and [`sheet_pngs`], the clean/annotated pair the visual
//! [`crate::review`] critic grades.

use std::path::PathBuf;

use anyhow::{Context, Result};
use gordian_runtime::AgentRuntime;
use gordian_runtime::render::{RenderBounds, RenderPlan};
use gordian_runtime::tool::IMAGE_PATH_KEY;
use serde_json::{Value, json};

/// The same sheet twice: as KiCAD draws it, and with the millimetre coordinate
/// overlay that lets a critic name a defect's position.
pub struct SheetPngs {
    pub clean: Vec<u8>,
    pub annotated: Vec<u8>,
    /// The engine's exact geometric + netlist analysis of the same sheet — the
    /// ground truth that settles the critic's false-positive-prone classes.
    pub visual: sch_floorplan::visual::VisualFacts,
    /// Where `annotated` was saved, for the agent loop to attach as an image.
    pub annotated_path: PathBuf,
    /// `refdes=value` for every real part, as the review's circuit context.
    pub parts: String,
}

/// One SVG export plus the geometry every render of it shares.
struct Sheet {
    doc: sch_doc::SchDoc,
    visual: sch_floorplan::visual::VisualFacts,
    svg: String,
    content: RenderBounds,
    overview: RenderBounds,
    plan: RenderPlan,
}

fn read_sheet(ctx: &AgentRuntime) -> Result<Sheet> {
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).context("reading schematic visual facts")?;
    let visual = sch_floorplan::visual::measure(&doc);
    let content = render_bounds(visual.sheet_extent);
    let plan = gordian_runtime::render::render_plan(
        part_count(&doc),
        content,
        ctx.config().tools.render_max_px,
    );
    Ok(Sheet {
        svg: gordian_runtime::render::schematic_svg(ctx.env(), ctx.sch_path())?,
        doc,
        visual,
        content,
        overview: padded_bounds(content, 2.54),
        plan,
    })
}

fn part_count(doc: &sch_doc::SchDoc) -> usize {
    real_parts(doc).count()
}

fn real_parts(doc: &sch_doc::SchDoc) -> impl Iterator<Item = &sch_doc::SymbolInst> {
    doc.symbols()
        .filter(|symbol| !sch_floorplan::bench::is_benched(symbol))
        .filter(|symbol| !symbol.refdes().is_empty() && !symbol.refdes().starts_with('#'))
}

/// Render the sheet for the visual critic: the KiCAD export as drawn, and the
/// same crop carrying millimetre axes.
pub fn sheet_pngs(ctx: &AgentRuntime) -> Result<SheetPngs> {
    let sheet = read_sheet(ctx)?;
    let cropped = gordian_runtime::render::crop_svg(&sheet.svg, sheet.overview);
    let clean = gordian_runtime::render::svg_to_png(&cropped, sheet.plan.overview_px)?;
    let annotated = gordian_runtime::render::svg_to_png(
        &schematic_overlay(&sheet.svg, sheet.overview),
        sheet.plan.overview_px,
    )?;
    let annotated_path = ctx.workspace().write_render(&annotated)?;
    let parts = real_parts(&sheet.doc)
        .map(|symbol| match symbol.value() {
            "" => symbol.refdes().to_string(),
            value => format!("{}={value}", symbol.refdes()),
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(SheetPngs {
        clean,
        annotated,
        visual: sheet.visual,
        annotated_path,
        parts,
    })
}

/// The `render_schematic` tool: an annotated overview PNG plus the deterministic
/// visual facts, and quadrant detail crops when the sheet is too dense to read
/// at overview resolution.
pub fn render_schematic(ctx: &AgentRuntime) -> Result<Value> {
    let sheet = read_sheet(ctx)?;
    let visual_json = serde_json::to_value(&sheet.visual)?;
    let bench = sch_floorplan::bench::benched(&sheet.doc);
    let png = gordian_runtime::render::svg_to_png(
        &schematic_overlay(&sheet.svg, sheet.overview),
        sheet.plan.overview_px,
    )?;
    let path = ctx.workspace().write_render(&png)?;
    let mut detail_paths = Vec::new();
    if let Some(detail_px) = sheet.plan.detail_px {
        for region in detail_regions(sheet.content) {
            let detail_svg = schematic_overlay(&sheet.svg, region);
            let detail_png = gordian_runtime::render::svg_to_png(&detail_svg, detail_px)?;
            let detail_path = ctx.workspace().write_render(&detail_png)?;
            detail_paths.push(json!({
                "region": [region.min_x, region.min_y, region.max_x, region.max_y],
                "png_path": detail_path.display().to_string(),
            }));
        }
    }
    let mut obj = json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "bench": bench.len(),
        "bench_refs": bench,
        "overview_px": sheet.plan.overview_px,
        "detail_paths": detail_paths,
        "visual": visual_json,
        "note": format!(
            "Schematic rendered from the saved .kicad_sch using KiCad's schematic SVG export and attached. \
             Symbols, fields, labels, and wires are drawn on a light background; X/Y axes and ticks \
             are sheet millimetres, matching read_schematic @x,y positions. PNG saved to {}. \
             visual lists the deterministic measured problems; dense/large sheets also return \
             detail_paths whose region boxes can be passed to read_schematic.",
            path.display(),
        ),
    });
    obj[IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
}

fn render_bounds(extent: [f64; 4]) -> RenderBounds {
    let [mut min_x, mut min_y, mut max_x, mut max_y] = extent;
    if max_x - min_x < 1.0 {
        min_x -= 10.0;
        max_x += 10.0;
    }
    if max_y - min_y < 1.0 {
        min_y -= 10.0;
        max_y += 10.0;
    }
    RenderBounds::new(min_x, min_y, max_x, max_y)
}

fn padded_bounds(bounds: RenderBounds, padding: f64) -> RenderBounds {
    RenderBounds::new(
        bounds.min_x - padding,
        bounds.min_y - padding,
        bounds.max_x + padding,
        bounds.max_y + padding,
    )
}

fn schematic_overlay(svg: &str, bounds: RenderBounds) -> String {
    let cropped = gordian_runtime::render::crop_svg(svg, bounds);
    gordian_runtime::render::add_coordinate_overlay(
        &cropped,
        bounds,
        "mm",
        gordian_runtime::render::CoordinateOverlayStyle {
            background: "#fffdf7",
            axis: "#1f2937",
            grid: "#94a3b8",
            x_axis: "#be123c",
            y_axis: "#1d4ed8",
        },
    )
}

fn detail_regions(bounds: RenderBounds) -> [RenderBounds; 4] {
    let mid_x = (bounds.min_x + bounds.max_x) / 2.0;
    let mid_y = (bounds.min_y + bounds.max_y) / 2.0;
    let overlap = 1.27;
    [
        RenderBounds::new(
            bounds.min_x,
            bounds.min_y,
            (mid_x + overlap).min(bounds.max_x),
            (mid_y + overlap).min(bounds.max_y),
        ),
        RenderBounds::new(
            (mid_x - overlap).max(bounds.min_x),
            bounds.min_y,
            bounds.max_x,
            (mid_y + overlap).min(bounds.max_y),
        ),
        RenderBounds::new(
            bounds.min_x,
            (mid_y - overlap).max(bounds.min_y),
            (mid_x + overlap).min(bounds.max_x),
            bounds.max_y,
        ),
        RenderBounds::new(
            (mid_x - overlap).max(bounds.min_x),
            (mid_y - overlap).max(bounds.min_y),
            bounds.max_x,
            bounds.max_y,
        ),
    ]
}
