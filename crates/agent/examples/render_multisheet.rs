//! Render a multi-BLOCK circuit as MULTI-SHEET: one clean .png per block + a committable
//! hierarchical KiCAD project.
//!
//! The single-sheet sprawl ceiling caps complete boards ~5-7; drawing each block on its own
//! sheet (the professional practice) lets each sheet score like the clean fixtures (9-10).
//! Compiles the design with the REAL parser (handles `between:`, `power:`, multi-line, units),
//! refines the blocks into uniform sheet GROUPS via `agent::multisheet::refine_blocks` (the
//! production split/merge — the single source of truth), then for each group emits a sub-design
//! (shared nets auto-become labeled ports) through the normal anneal path, renders it, and
//! commits the hierarchy via `agent::multisheet::write_project`. Critic each PNG with
//! tools/schematic_critic.py.
//!
//! Usage: cargo run --release -p agent --example render_multisheet -- <draft.yaml> <out_dir>

use agent::multisheet::{refine_blocks, sanitize, write_project};
use circuit_lang::SymbolProvider;
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let yaml = args.next().expect("usage: render_multisheet <draft.yaml> <out_dir>");
    let out_dir = args.next().expect("usage: render_multisheet <draft.yaml> <out_dir>");
    std::fs::create_dir_all(&out_dir)?;

    // These ARE multi-sheet sub-sheets, so opt them into the route-aware crossing refinement
    // on the small path (a peripheral/bus sub-sheet tangles its port fanout; the refinement
    // takes e.g. an I2C sheet 16→13 xings and a power sheet 8→9). Single-sheet emit paths
    // (bench_corpus, agent_design) don't set this, so references stay byte-identical.
    unsafe { std::env::set_var("MULTISHEET_REFINE", "1") };

    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(&yaml)?;
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.ok_or_else(|| {
        let errs: Vec<String> = result.diagnostics.0.iter().map(|d| d.message.clone()).collect();
        anyhow::anyhow!("compile produced no design: {}", errs.join("; "))
    })?;

    if design.blocks.len() < 2 {
        eprintln!("only {} block(s) — multi-sheet needs a multi-block design", design.blocks.len());
    }

    // Refine the agent's blocks into uniform sheet GROUPS (split over-crammed, merge tiny) —
    // the production logic, shared with the agent's commit path.
    let mut sheets: Vec<(String, String)> = Vec::new();
    for (name, members) in refine_blocks(&design.blocks) {
        // A sub-design holding this group's block(s): cross-group nets touch only these pins, so
        // the engine auto-labels the single-pin ones as ports and keeps multi-pin ones internal.
        let mut sub = design.clone();
        sub.blocks = members.into_iter().collect();
        let nparts: usize = sub.blocks.values().map(|b| b.components.len()).sum();

        // Shelf-pack seed + Anneal search (the premium tier).
        let ir = sch_layout::floorplan::infer_ir(&env, &sub);
        let emit = match sch_layout::floorplan::emit_anneal(&env, &sub, &ir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("{name}: emit failed: {e}");
                continue;
            }
        };
        let tmp = tempfile::tempdir()?;
        let sch = tmp.path().join("s.kicad_sch");
        std::fs::write(&sch, emit.sch.as_bytes())?;
        let svg_dir = tempfile::tempdir()?;
        let svg_path = KicadCli::new(&env).export_svg_opts(&sch, svg_dir.path(), true)?;
        let svg = std::fs::read_to_string(&svg_path)?;
        let png = agent::render::svg_to_png(&svg, 1600)?;
        let out_png = format!("{out_dir}/{name}.png");
        std::fs::write(&out_png, png)?;
        println!(
            "{name}: {nparts} parts, {} warnings, {} wire-xings -> {out_png}",
            emit.layout_warnings.len(),
            emit.wire_crossings
        );
        for w in &emit.layout_warnings {
            println!("    WARN[{name}]: {w}");
        }
        sheets.push((sanitize(&name), emit.sch.clone()));
    }

    // COMMIT a hierarchical KiCAD project (root + per-block sub-sheets) so the multi-sheet
    // design is openable + ERC-checkable, not just separate rendered PNGs.
    let root = write_project(&env, std::path::Path::new(&out_dir), &sheets)?;
    println!("wrote multi-sheet project -> {} ({} sheets)", root.display(), sheets.len());
    match KicadCli::new(&env).erc(&root) {
        Ok(r) => println!("PROJECT ERC: {} errors, {} warnings", r.error_count(), r.warning_count()),
        Err(e) => println!("PROJECT ERC failed: {e}"),
    }
    Ok(())
}
