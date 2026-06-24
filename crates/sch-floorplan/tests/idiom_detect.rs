//! Idiom-engine regression: `infer_ir` must recognize the crystal-network and
//! decoupling-bank idioms from connectivity alone (no new YAML syntax) and report
//! them on `LayoutIr.idioms`, pinning their members in `LayoutIr.frozen`.

use kicad_cli::env::KicadEnv;
use kicad_sexpr::provider::RealSymbolProvider;
use sch_floorplan::floorplan;
use std::path::Path;

fn compile_fixture(provider: &RealSymbolProvider, name: &str) -> circuit_lang::Design {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../docs/validation/{name}.circuit.yaml"));
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    let result = circuit_lang::compile(&src, provider);
    assert!(!result.diagnostics.has_errors(), "{name}: {:#?}", result.diagnostics);
    result.design.expect("design")
}

#[test]
fn infer_ir_recognizes_crystal_and_decoupling_idioms() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("no KiCAD environment; skipping idiom detection test");
        return;
    };
    let provider = RealSymbolProvider::new(env.clone());
    let design = compile_fixture(&provider, "idiom-stm32");
    let ir = floorplan::infer_ir(&env, &design);

    // A crystal cluster (Y1 + its two 22pF load caps) is recognized.
    let crystal = ir
        .idioms
        .iter()
        .find(|i| i.kind == "crystal")
        .expect("crystal idiom detected on idiom-stm32");
    assert_eq!(crystal.anchor, "U1");
    assert!(crystal.parts.contains(&"Y1".to_string()), "crystal includes Y1: {:?}", crystal.parts);
    assert_eq!(crystal.parts.len(), 3, "crystal = Y1 + 2 load caps: {:?}", crystal.parts);

    // A decoupling bank (>=3 rail-to-rail caps on the +3V3 rail) is recognized.
    let deco = ir
        .idioms
        .iter()
        .find(|i| i.kind == "decoupling")
        .expect("decoupling idiom detected on idiom-stm32");
    assert_eq!(deco.anchor, "U1");
    assert!(deco.parts.len() >= 3, "decoupling bank >=3 caps: {:?}", deco.parts);

    // FROZEN idioms (crystal/decoupling) pin their members so the search ships the
    // cluster intact; a REPORT-ONLY idiom (led_indicator) is recognized but flows
    // through normal placement (tidied by an mm post-pass), so it is NOT frozen.
    for idiom in &ir.idioms {
        let must_freeze = idiom.kind != "led_indicator";
        for p in &idiom.parts {
            assert_eq!(
                ir.frozen.contains(p),
                must_freeze,
                "idiom {} member {p} frozen?",
                idiom.kind
            );
        }
    }

    // A board with NO crystal/decoupling-bank fires no idiom (additive engine).
    let plain = compile_fixture(&provider, "divider-filter");
    assert!(
        floorplan::infer_ir(&env, &plain).idioms.is_empty(),
        "divider-filter has no idioms"
    );
}

/// Regression: the decoupling bank must still fire when the supply rail it sits on
/// reaches a SECOND IC — the universal case of a regulator (U2, an AMS1117 LDO)
/// feeding the MCU (U1) it decouples. A bypass cap's nets are BOTH rails (V+ and
/// GND), and that V+ rail necessarily reaches the LDO too; an earlier guard dropped
/// any cap whose net touched another IC, which collapsed the whole bank on every
/// real LDO+MCU board and scattered the caps. Only a shared SIGNAL net should
/// disqualify a cap, never a shared rail.
#[test]
fn decoupling_bank_survives_a_shared_rail_to_a_second_ic() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("no KiCAD environment; skipping idiom detection test");
        return;
    };
    let provider = RealSymbolProvider::new(env.clone());
    let design = compile_fixture(&provider, "idiom-stm32-ldo");
    let ir = floorplan::infer_ir(&env, &design);

    let deco = ir
        .idioms
        .iter()
        .find(|i| i.kind == "decoupling")
        .expect("decoupling bank still detected with an LDO on the same +3V3 rail");
    assert_eq!(deco.anchor, "U1", "bank decouples the MCU, not the regulator");
    assert!(
        deco.parts.len() >= 3,
        "the full bank survives the shared rail (>=3 caps): {:?}",
        deco.parts
    );
}
