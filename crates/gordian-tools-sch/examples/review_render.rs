//! Write the exact pair of PNGs the visual critic grades, for one `.kicad_sch`.
//!
//! `cargo run -p gordian-tools-sch --example review_render -- <sch> <out-stem>`
//! produces `<stem>-clean.png` (what the grader scores) and `<stem>-annotated.png`,
//! through the same crop, resolution and renderer as the live `review_schematic`.

use std::path::PathBuf;

use gordian_runtime::render::{RenderBounds, render_plan, schematic_sheet};
use kicad::KicadInstallation;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let sch = PathBuf::from(args.next().expect("usage: review_render <sch> <out-stem>"));
    let stem = PathBuf::from(args.next().expect("usage: review_render <sch> <out-stem>"));
    let env = KicadInstallation::detect().expect("no KiCAD environment");

    let doc = sch_doc::SchDoc::read(&sch)?;
    let visual = sch_floorplan::visual::measure(&doc);
    let parts = doc
        .symbols()
        .filter(|s| !sch_floorplan::bench::is_benched(s))
        .filter(|s| !s.refdes().is_empty() && !s.refdes().starts_with('#'))
        .count();
    let [min_x, min_y, max_x, max_y] = visual.sheet_extent;
    let content = RenderBounds::new(min_x, min_y, max_x, max_y);
    let plan = render_plan(parts, content, gordian_runtime::config::DEFAULT_RENDER_MAX_PX);
    let overview = RenderBounds::new(
        content.min_x - 2.54,
        content.min_y - 2.54,
        content.max_x + 2.54,
        content.max_y + 2.54,
    );

    let started = std::time::Instant::now();
    let raster = schematic_sheet(&env, &sch)?.raster(overview, plan.overview_px)?;
    let name = stem.file_name().unwrap().to_string_lossy().to_string();
    std::fs::write(stem.with_file_name(format!("{name}-clean.png")), &raster.png)?;
    let annotated = gordian_tools_sch::render::annotate(&raster, plan.overview_px)?;
    std::fs::write(stem.with_file_name(format!("{name}-annotated.png")), &annotated)?;
    println!(
        "{}x{} px over {:?} in {:?}",
        raster.width_px,
        raster.height_px,
        raster.bounds,
        started.elapsed()
    );
    Ok(())
}
