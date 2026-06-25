//! DSL integration against the installed KiCAD symbol libraries: drive
//! `circuit_lang::compile` through a real [`SymbolTable`] (skipped when no KiCAD
//! install is detected).

use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;

#[test]
fn compiles_validated_bedrock_design_against_real_libs() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP");
        return;
    };
    let provider = SymbolTable::from_env(&env);
    let src = include_str!("../../../docs/validation/bedrock-oneshot-bluepill.circuit.yaml");
    let result = circuit_lang::compile(src, &provider);
    assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
    assert!(result.design.is_some());
}
