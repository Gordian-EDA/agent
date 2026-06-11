//! Layout-revision reconciliation: hint changes re-place exactly the affected
//! block; user drags survive everywhere else; relayout=all starts fresh.

use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;
use sch_engine::reconcile::{Relayout, emit_design_reconciled};

fn compile(env: &KicadEnv, src: &str) -> circuit_lang::Design {
    let provider = RealSymbolProvider::new(env.clone());
    let r = circuit_lang::compile(src, &provider);
    assert!(!r.diagnostics.has_errors(), "{:?}", r.diagnostics);
    r.design.unwrap()
}

/// Read a symbol's position out of a rendered schematic via kiutils.
fn pos_of(sch_text: &str, refdes: &str) -> [f64; 2] {
    let tmp = tempfile::Builder::new().suffix(".kicad_sch").tempfile().unwrap();
    std::fs::write(tmp.path(), sch_text).unwrap();
    let doc = kiutils_kicad::SchematicFile::read(tmp.path()).unwrap();
    doc.ast().symbols.iter()
        .find(|s| s.reference.as_deref() == Some(refdes))
        .and_then(|s| s.at)
        .expect("symbol with position")
}

const SRC_A: &str = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
  b:
    components:
      R2: {part: Device:R, value: 1k, between: [N2, GND]}
";
const SRC_B: &str = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    layout: {edge: right}
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
  b:
    components:
      R2: {part: Device:R, value: 1k, between: [N2, GND]}
";

#[test]
fn hint_change_replaces_only_that_block_and_drags_survive_elsewhere() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let d_a = compile(&env, SRC_A);
    let first = emit_design_reconciled(&env, &d_a, None, &Relayout::None).unwrap().sch;

    // Simulate user drags: move BOTH R1 and R2 to recognizable spots.
    let p1 = pos_of(&first, "R1");
    let p2 = pos_of(&first, "R2");
    let dragged = first
        .replace(&format!("(at {} {} 0)", p1[0], p1[1]), "(at 254 127 0)")
        .replace(&format!("(at {} {} 0)", p2[0], p2[1]), "(at 254 152.4 0)");

    // Re-emit SAME design: both drags preserved.
    let same = emit_design_reconciled(&env, &d_a, Some(&dragged), &Relayout::None).unwrap().sch;
    assert_eq!(pos_of(&same, "R1"), [254.0, 127.0]);
    assert_eq!(pos_of(&same, "R2"), [254.0, 152.4]);

    // Re-emit with block `a` re-hinted: R1 re-placed, R2's drag survives.
    let d_b = compile(&env, SRC_B);
    let rehinted = emit_design_reconciled(&env, &d_b, Some(&dragged), &Relayout::None).unwrap().sch;
    assert_ne!(pos_of(&rehinted, "R1"), [254.0, 127.0], "hint change must re-place R1");
    assert_eq!(pos_of(&rehinted, "R2"), [254.0, 152.4], "untouched block keeps the drag");

    // relayout=all discards every prior position.
    let fresh = emit_design_reconciled(&env, &d_a, Some(&dragged), &Relayout::All).unwrap().sch;
    assert_ne!(pos_of(&fresh, "R1"), [254.0, 127.0]);
    assert_ne!(pos_of(&fresh, "R2"), [254.0, 152.4]);
}
