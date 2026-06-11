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

    let out = emit_design(&env, &design).unwrap();
    let text = &out.sch;
    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .unwrap();
    std::fs::write(tmp.path(), text).unwrap();

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

    // Phase 2 readability oracle: the deterministic layout lint.
    //
    // The overlap lint is now BANK-AWARE (Task 12): same-bank adjacencies (the
    // parallel decouple caps packed tight at BANK_PITCH, sharing a bus with no
    // per-cap labels) are recognized in the engine as intentional and never
    // warned, while every other collision — bank-vs-anchor, cap-vs-non-cap,
    // label-vs-anything — still trips the lint. So the COLLISION oracle is
    // strict again: the validated bluepill must emit with ZERO `… overlaps …`
    // warnings. (Task 9 had relaxed this to tolerate cap-on-cap overlaps as a
    // test-level hack; that hack is gone now that the engine filters same-bank
    // pairs structurally.)
    //
    // `layout_warnings` also carries the Task-12 advisory notes (`grammar: …`
    // degradations and `sparse: …` whitespace hints) — those are readability
    // advisories, NOT collisions, so the strict collision oracle filters to the
    // overlap warnings. The bluepill's compact `power` block does trip a `sparse`
    // hint, which is expected and not a layout error.
    let collisions: Vec<&String> = out
        .layout_warnings
        .iter()
        .filter(|w| w.contains(" overlaps "))
        .collect();
    assert!(
        collisions.is_empty(),
        "layout collisions on bluepill: {:?}",
        collisions
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
        emit_design(&env, &design).unwrap().sch,
        emit_design(&env, &design).unwrap().sch,
        "re-emitting the same Design must be byte-identical"
    );
}
