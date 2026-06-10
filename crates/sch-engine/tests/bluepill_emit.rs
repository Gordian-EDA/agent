//! THE milestone (MVP acceptance criterion #1): the validated bluepill design
//! emits a `.kicad_sch` that KiCAD 10 loads and ERCs with **zero errors**.
//!
//! The fixture is the real Bedrock one-shot bluepill (28 components, three power
//! rails, 70 intentional no-connects on the STM32H7 and the USB-C connector).
//! `emit_design` must turn the kernel `Design` into a schematic whose netlist
//! has every component and whose ERC report shows no error-severity violations.
//! Warnings are acceptable; errors are not. SKIP-graceful when no KiCAD is
//! detected; runs against KiCAD 10 here.

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;
use sch_engine::emit_design;

#[test]
fn bluepill_design_emits_and_ercs_clean() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = RealSymbolProvider::new(KicadEnv::detect().unwrap());
    let src = include_str!("../../../docs/validation/bedrock-oneshot-bluepill.circuit.yaml");
    let design = circuit_lang::compile(src, &provider)
        .design
        .expect("compiles");

    let text = emit_design(&env, &design).unwrap();
    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .unwrap();
    std::fs::write(tmp.path(), &text).unwrap();

    // KiCAD loads it; the netlist has every component.
    let nl = KicadCli::new(&env).netlist(tmp.path()).unwrap();
    let comp_count: usize = design.blocks.values().map(|b| b.components.len()).sum();
    assert_eq!(
        nl.components.len(),
        comp_count,
        "every component must appear in the netlist"
    );

    // ERC has ZERO errors — the milestone.
    let report = KicadCli::new(&env).erc(tmp.path()).unwrap();
    assert_eq!(
        report.error_count(),
        0,
        "ERC errors must be zero, got {}: {:#?}",
        report.error_count(),
        report
            .violations
            .iter()
            .filter(|v| v.severity == "error")
            .collect::<Vec<_>>()
    );
}

#[test]
fn emit_is_byte_deterministic() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = RealSymbolProvider::new(KicadEnv::detect().unwrap());
    let src = include_str!("../../../docs/validation/bedrock-oneshot-bluepill.circuit.yaml");
    let design = circuit_lang::compile(src, &provider).design.unwrap();
    assert_eq!(
        emit_design(&env, &design).unwrap(),
        emit_design(&env, &design).unwrap(),
        "re-emitting the same Design must be byte-identical"
    );
}
