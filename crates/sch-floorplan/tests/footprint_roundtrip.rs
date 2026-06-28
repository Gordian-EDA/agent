//! End-to-end guard for the footprint round-trip: a footprint authored in the
//! circuit model must survive emit -> .kicad_sch -> lift back into the model.
//! This is the path the harnesses miss (they build PCB drafts from standalone
//! JSON, never through the schematic). See docs/specs/unified-kicad-pcb-state.md §1.

use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;
use sch_io::read::lift;

#[test]
fn footprint_survives_emit_then_lift() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("no KiCAD environment — skipping footprint round-trip test");
        return;
    };
    let provider = SymbolTable::from_env(&env);

    // Minimal design: one capacitor carrying a footprint assignment.
    let yaml = r#"
version: 1
name: footprint-roundtrip
blocks:
  main:
    components:
      C1:
        part: Device:C
        value: 100nF
        footprint: Capacitor_SMD:C_0603_1608Metric
        pins: { "1": VCC, "2": GND }
      C2:
        part: Device:C
        value: 1uF
        footprint: Capacitor_SMD:C_0805_2012Metric
        pins: { "1": VCC, "2": GND }
"#;
    let result = circuit_lang::compile(yaml, &provider);
    assert!(
        !result.diagnostics.has_errors(),
        "compile: {:#?}",
        result.diagnostics
    );
    let design = result.design.expect("yaml compiles to a design");

    // Emit through the floorplan engine to a temp .kicad_sch.
    let ir = floorplan::baseline_ir(&design);
    let out = floorplan::emit_strategy(&env, &design, Box::new(anneal_place::Anneal), Some(ir))
        .expect("emit a .kicad_sch");
    let dir = tempfile::tempdir().unwrap();
    let sch_path = dir.path().join("rt.kicad_sch");
    std::fs::write(&sch_path, &out.sch).unwrap();

    // The emitted sheet must carry the real Footprint field (the emit fix).
    assert!(
        out.sch
            .contains("(property \"Footprint\" \"Capacitor_SMD:C_0603_1608Metric\""),
        "emitted .kicad_sch must record the footprint:\n{}",
        out.sch
    );

    // Lift it back and confirm the footprint survived (the lift fix).
    let lifted_yaml = lift(&env, &sch_path).expect("lift the schematic");
    let lifted = circuit_lang::compile(&lifted_yaml, &provider)
        .design
        .expect("lifted yaml compiles");
    let c1 = lifted
        .blocks
        .values()
        .flat_map(|b| b.components.iter())
        .find(|(r, _)| r.as_str() == "C1")
        .map(|(_, c)| c)
        .expect("C1 round-trips");
    assert_eq!(
        c1.footprint.as_deref(),
        Some("Capacitor_SMD:C_0603_1608Metric"),
        "footprint must survive emit -> lift\nlifted yaml:\n{lifted_yaml}"
    );
}
