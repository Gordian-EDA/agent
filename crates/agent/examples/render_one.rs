//! Render a draft as a SINGLE sheet (the whole design, no multi-sheet partitioning) via the
//! premium anneal — or the `cola` constraint engine when COLA_PLACE=1. For testing cola on
//! LARGE un-partitioned sheets, where its global-optimisation edge should finally matter vs
//! the SA's local search (the multi-sheet path keeps sheets small enough that the SA suffices).
//!
//! Usage: cargo run --release -p agent --example render_one -- <draft.yaml> <out.png>

use circuit_lang::SymbolProvider;
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let yaml = args.next().expect("usage: render_one <draft.yaml> <out.png>");
    let out = args.next().expect("usage: render_one <draft.yaml> <out.png>");

    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(&yaml)?;
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.ok_or_else(|| {
        let errs: Vec<String> = result.diagnostics.0.iter().map(|d| d.message.clone()).collect();
        anyhow::anyhow!("compile produced no design: {}", errs.join("; "))
    })?;

    let ir = sch_layout::floorplan::infer_ir(&env, &design);
    let emit = sch_layout::floorplan::emit_anneal(&env, &design, &ir)
        .map_err(|e| anyhow::anyhow!("emit failed: {e}"))?;

    let tmp = tempfile::tempdir()?;
    let sch = tmp.path().join("s.kicad_sch");
    std::fs::write(&sch, emit.sch.as_bytes())?;
    let svg_dir = tempfile::tempdir()?;
    let svg_path = KicadCli::new(&env).export_svg_opts(&sch, svg_dir.path(), true)?;
    let svg = std::fs::read_to_string(&svg_path)?;
    let png = agent::render::svg_to_png(&svg, 1600)?;
    std::fs::write(&out, png)?;
    println!(
        "{out}: {} parts, {} warnings, {} wire-xings",
        design.blocks.values().map(|b| b.components.len()).sum::<usize>(),
        emit.layout_warnings.len(),
        emit.wire_crossings
    );
    Ok(())
}
