//! Validate the production composed-sheet commit (`gordian_core::multisheet::compose_single_sheet`
//! — the exact function `apply_design` calls for multi-block designs): compile a draft,
//! compose ONE labeled-block-region `.kicad_sch`, run ERC.
//!
//! Usage: cargo run --release -p agent --example commit_multisheet -- <draft.yaml> <out_dir>

use circuit_lang::SymbolProvider;
use kicad_cli::env::KicadEnv;
use kicad_sexpr::provider::RealSymbolProvider;

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
        gordian_core::multisheet::emit_and_check(&env, &design, std::path::Path::new(&out))?;
    println!("root -> {}", root.display());
    println!("ERC: {errors} errors, {warnings} warnings");
    Ok(())
}
