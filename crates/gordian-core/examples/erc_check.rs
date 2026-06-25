//! Run the deterministic quantitative ERC (`circuit_lang::erc::erc_checks`) on a circuit-YAML and
//! print the defect lines — the exact-math layer (feedback-divider ratios, LED current) that runs
//! under the LLM review ensemble. Useful for debugging the checks on real generated designs.
//!
//! Usage: cargo run --release -p agent --example erc_check -- <design.yaml>

use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: erc_check <design.yaml>");
    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let provider = SymbolTable::from_env(&env);
    let src = std::fs::read_to_string(&path)?;
    let design = circuit_lang::compile(&src, &provider)
        .design
        .ok_or_else(|| anyhow::anyhow!("compile produced no design"))?;
    let defects = circuit_lang::erc::erc_checks(&design);
    if defects.is_empty() {
        println!("erc: clean (no deterministic quantitative defect)");
    }
    for d in defects {
        println!("{d}");
    }
    Ok(())
}
