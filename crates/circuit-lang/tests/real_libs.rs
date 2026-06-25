//! DSL integration against the installed KiCAD symbol libraries: drive
//! `circuit_lang::compile` through the real [`RealSymbolProvider`] (skipped when
//! no KiCAD install is detected).

use kicad_cli::env::KicadEnv;
use kicad_symbol::provider::RealSymbolProvider;

#[test]
fn compiles_validated_bedrock_design_against_real_libs() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP");
        return;
    };
    let provider = RealSymbolProvider::new(env);
    let src = include_str!("../../../docs/validation/bedrock-oneshot-bluepill.circuit.yaml");
    let result = circuit_lang::compile(src, &provider);
    assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
    assert!(result.design.is_some());
}
