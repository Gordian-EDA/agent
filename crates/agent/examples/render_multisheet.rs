//! Render a multi-BLOCK circuit as ONE COMPOSED sheet: each functional block laid out
//! independently, then tiled onto a single page as a labeled bounding-box region, plus the
//! committable `.kicad_sch`.
//!
//! The single-sheet sprawl ceiling caps complete boards ~5-7; laying out each block
//! independently (the professional practice) lets each region read like the clean fixtures
//! (8-9), and global labels join cross-block nets across the one sheet with no border-crossing
//! wires. Compiles the design with the REAL parser (handles `between:`, `power:`, multi-line,
//! units), then composes via `agent::multisheet::compose_single_sheet` (the production path —
//! the exact function `apply_design` calls) and renders the result. Critic the PNG with
//! tools/schematic_critic.py.
//!
//! Usage: cargo run --release -p agent --example render_multisheet -- <draft.yaml> <out_dir>

use circuit_lang::SymbolProvider;
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let yaml = args.next().expect("usage: render_multisheet <draft.yaml> <out_dir>");
    let out_dir = args.next().expect("usage: render_multisheet <draft.yaml> <out_dir>");
    std::fs::create_dir_all(&out_dir)?;

    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(&yaml)?;
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.ok_or_else(|| {
        let errs: Vec<String> = result.diagnostics.0.iter().map(|d| d.message.clone()).collect();
        anyhow::anyhow!("compile produced no design: {}", errs.join("; "))
    })?;

    if design.blocks.len() < 2 {
        eprintln!("only {} block(s) — composing needs a multi-block design", design.blocks.len());
    }

    // COMPOSE the single committable sheet (refine into groups, per-group anneal, tile each as
    // a labeled bounding box, join cross-block nets via global labels) — the production path.
    let root =
        agent::multisheet::compose_single_sheet(&env, &design, std::path::Path::new(&out_dir))?;
    println!("wrote composed sheet -> {}", root.display());

    // Render it to a PNG for the critic.
    let svg_dir = tempfile::tempdir()?;
    let svg_path = KicadCli::new(&env).export_svg_opts(&root, svg_dir.path(), true)?;
    let svg = std::fs::read_to_string(&svg_path)?;
    let png = agent::render::svg_to_png(&svg, 2400)?;
    let stem =
        std::path::Path::new(&yaml).file_stem().and_then(|s| s.to_str()).unwrap_or("composed");
    let out_png = format!("{out_dir}/{stem}.png");
    std::fs::write(&out_png, png)?;
    println!("rendered -> {out_png}");

    match KicadCli::new(&env).erc(&root) {
        Ok(r) => println!("ERC: {} errors, {} warnings", r.error_count(), r.warning_count()),
        Err(e) => println!("ERC failed: {e}"),
    }
    Ok(())
}
