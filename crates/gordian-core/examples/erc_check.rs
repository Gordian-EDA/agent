//! Run the deterministic quantitative ERC (`sch_check::erc::erc_checks`) on a circuit-YAML and
//! print the defect lines — the exact-math layer (feedback-divider ratios, LED current) that runs
//! under the LLM review ensemble. Useful for debugging the checks on real generated designs.
//!
//! Usage: cargo run --release -p gordian-core --example erc_check -- <design.yaml>

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: erc_check <design.yaml>");
    let env = KicadInstallation::detect().expect("no KiCAD environment detected");
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let src = std::fs::read_to_string(&path)?;
    let design = circuit_lang::compile(&src, &provider)
        .design
        .ok_or_else(|| anyhow::anyhow!("compile produced no design"))?;
    let defects = sch_check::erc::erc_checks(&design);
    if defects.is_empty() {
        println!("erc: clean (no deterministic quantitative defect)");
    }
    for d in defects {
        println!("{d}");
    }
    Ok(())
}
