//! Round-trip checker: lift a `.kicad_sch` back to circuit YAML and re-compile it,
//! asserting the lifted YAML is VALID (the schematic → YAML → schematic loop holds).
//!
//! Usage: cargo run --release -p agent --example lift_roundtrip -- <file.kicad_sch> [...]
//! Exits nonzero if any lifted YAML fails to compile.

use circuit_lang::SymbolProvider;
use kicad_cli_rs::env::KicadEnv;
use kicad_sexpr::provider::RealSymbolProvider;

fn main() -> anyhow::Result<()> {
    let env = KicadEnv::detect().expect("no KiCAD environment");
    let provider = RealSymbolProvider::new(env.clone());
    let mut bad = 0;
    for path in std::env::args().skip(1) {
        let yaml = match sch_layout::lift::lift(&env, std::path::Path::new(&path)) {
            Ok(y) => y,
            Err(e) => {
                println!("LIFT-FAIL {path}: {e}");
                bad += 1;
                continue;
            }
        };
        let result = circuit_lang::compile(&yaml, &provider as &dyn SymbolProvider);
        let errs: Vec<String> = result
            .diagnostics
            .0
            .iter()
            .filter(|d| matches!(d.severity, circuit_lang::diag::Severity::Error))
            .map(|d| d.message.clone())
            .collect();
        if result.design.is_some() && errs.is_empty() {
            println!("VALID   {path}");
        } else {
            println!("INVALID {path}: {}", errs.join("; "));
            bad += 1;
        }
    }
    if bad > 0 {
        anyhow::bail!("{bad} schematic(s) failed lift round-trip");
    }
    Ok(())
}
