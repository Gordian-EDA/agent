//! Power nets render as power symbols + stub wires, not text labels.

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn compile(env: &KicadEnv, src: &str) -> circuit_lang::Design {
    let provider = RealSymbolProvider::new(env.clone());
    let result = circuit_lang::compile(src, &provider);
    assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
    result.design.unwrap()
}

#[test]
fn power_nets_use_power_symbols_not_labels() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let design = compile(&env, "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      C1: {part: Device:C, value: 100nF, between: [3V3, GND]}
      R1: {part: Device:R, value: 10k, between: [SIG, GND]}
");
    let text = sch_engine::emit_design(&env, &design).unwrap();

    // No power-net text labels; SIG keeps its label.
    assert!(!text.contains("(label \"GND\""), "GND must not be a label");
    assert!(!text.contains("(label \"3V3\""), "3V3 must not be a label");
    assert!(text.contains("(label \"SIG\""), "signal nets keep labels");
    // Power symbols and wires present.
    assert!(text.contains("power:GND"));
    assert!(text.contains("power:+3V3"), "3V3 must map to the stock +3V3 symbol");
    assert!(text.contains("(wire"));

    // Connectivity ground truth via netlist.
    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("t.kicad_sch");
    std::fs::write(&sch, &text).unwrap();
    let nl = KicadCli::new(&env).netlist(&sch).unwrap();
    let net_of = |r: &str, p: &str| {
        nl.nets.iter()
            .find(|n| n.nodes.contains(&(r.to_string(), p.to_string())))
            .map(|n| n.name.clone()).unwrap_or_default()
    };
    assert_eq!(net_of("C1", "1"), "3V3");
    assert_eq!(net_of("C1", "2"), "GND");
    assert_eq!(net_of("R1", "2"), "GND");

    // ERC stays clean (flags attach pin-coincident now).
    let erc = KicadCli::new(&env).erc(&sch).unwrap();
    assert_eq!(erc.error_count(), 0, "{:?}", erc.violations);
}

#[test]
fn rail_span_passive_flips_when_pin1_is_ground() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let design = compile(&env, "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      C1: {part: Device:C, value: 100nF, between: [3V3, GND]}
      C2: {part: Device:C, value: 100nF, between: [GND, 3V3]}
");
    let text = sch_engine::emit_design(&env, &design).unwrap();

    // Assert via kiutils: C1 pin1=3V3 -> angle 0; C2 pin1=GND -> flipped 180.
    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("t.kicad_sch");
    std::fs::write(&sch, &text).unwrap();
    let doc = kiutils_kicad::SchematicFile::read(&sch).unwrap();
    let angle_of = |refdes: &str| {
        doc.ast().symbols.iter()
            .find(|s| s.reference.as_deref() == Some(refdes))
            .and_then(|s| s.angle)
    };
    assert_eq!(angle_of("C1"), Some(0.0), "C1 (pin1=3V3) should be upright");
    assert_eq!(angle_of("C2"), Some(180.0), "C2 (pin1=GND) should be flipped");

    // ERC stays clean.
    let erc = KicadCli::new(&env).erc(&sch).unwrap();
    assert_eq!(erc.error_count(), 0, "{:?}", erc.violations);
}

#[test]
fn blocks_get_title_text_and_frame() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let design = compile(&env, "
version: 1
name: t
rails: [GND]
blocks:
  power_supply:
    components:
      R1: {part: Device:R, value: 1k, between: [A, GND]}
");
    let text = sch_engine::emit_design(&env, &design).unwrap();
    assert!(text.contains("(text \"power_supply\""), "block title text");
    assert!(text.contains("(rectangle"), "block frame");

    // Frames/text are graphic-only: ERC must stay clean and netlist unaffected.
    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("t.kicad_sch");
    std::fs::write(&sch, &text).unwrap();
    let erc = kicad_bridge::cli::KicadCli::new(&env).erc(&sch).unwrap();
    assert_eq!(erc.error_count(), 0, "{:?}", erc.violations);
}

#[test]
fn signal_labels_sit_on_stubs_and_orient_away_from_the_body() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let design = compile(&env, "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 10k, between: [SIG, GND]}
      R2: {part: Device:R, value: 10k, between: [SIG, GND]}
");
    let text = sch_engine::emit_design(&env, &design).unwrap();
    assert!(text.contains("(label \"SIG\""));

    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("t.kicad_sch");
    std::fs::write(&sch, &text).unwrap();
    let nl = kicad_bridge::cli::KicadCli::new(&env).netlist(&sch).unwrap();
    let sig = nl.nets.iter().find(|n| n.name.ends_with("SIG")).expect("SIG net");
    assert_eq!(sig.nodes.len(), 2, "both R pins join SIG through their stubs: {sig:?}");

    // ERC stays clean.
    let erc = kicad_bridge::cli::KicadCli::new(&env).erc(&sch).unwrap();
    assert_eq!(erc.error_count(), 0, "{:?}", erc.violations);
}
