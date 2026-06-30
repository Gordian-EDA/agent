//! DSL integration against the installed KiCAD symbol libraries: drive
//! `circuit_lang::compile` through a real [`SymbolTable`] (skipped when no KiCAD
//! install is detected).

use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;

#[test]
fn compiles_minimal_design_against_real_libs() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP");
        return;
    };
    let provider = SymbolTable::from_env(&env);
    // Self-contained smoke circuit — standard KiCAD symbols only (a regulator + bypass caps), so
    // the test has no external-fixture dependency. Verifies the DSL drives `compile` through a
    // real [`SymbolTable`] and produces a design with zero diagnostics.
    let src = r#"
version: 1
name: real-libs-smoke
blocks:
  supply:
    components:
      PWR2: {part: power:GND, pins: {1: GND}}
      PWR3: {part: power:VCC, pins: {1: VBUS}}
  power:
    components:
      U2: {part: Regulator_Linear:AMS1117-3.3, pins: {VI: VBUS, VO: 3V3, GND: GND}}
      C9: {part: C, value: 10uF, between: [VBUS, GND]}
      C10: {part: C, value: 22uF, between: [3V3, GND]}
"#;
    let result = circuit_lang::compile(src, &provider);
    assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
    assert!(result.design.is_some());
}
