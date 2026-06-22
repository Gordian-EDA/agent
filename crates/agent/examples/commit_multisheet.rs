//! Validate the production multi-sheet commit (`agent::multisheet::emit_multisheet` — the
//! exact function `apply_design` calls for dense multi-block designs): compile a draft,
//! emit a hierarchical KiCAD project, run ERC.
//!
//! Usage: cargo run --release -p agent --example commit_multisheet -- <draft.yaml> <out_dir>

use circuit_lang::SymbolProvider;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn main() -> anyhow::Result<()> {
    let yaml = std::env::args().nth(1).expect("usage: commit_multisheet <draft.yaml> <out_dir>");
    let out = std::env::args().nth(2).expect("usage: commit_multisheet <draft.yaml> <out_dir>");
    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(&yaml)?;
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.ok_or_else(|| anyhow::anyhow!("compile produced no design"))?;
    let n_blocks = design.blocks.values().filter(|b| !b.components.is_empty()).count();
    let n_parts: usize = design.blocks.values().map(|b| b.components.len()).sum();
    println!("design: {n_blocks} blocks, {n_parts} parts");
    let (root, errors, warnings) =
        agent::multisheet::emit_and_check(&env, &design, std::path::Path::new(&out))?;
    println!("root -> {}", root.display());
    println!("ERC: {errors} errors, {warnings} warnings");
    Ok(())
}
