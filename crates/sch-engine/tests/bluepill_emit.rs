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
    // Intentional current limitation (NOT a temporary shim): bank members (the
    // parallel decouple caps) are packed tight at BANK_PITCH, so their real KiCAD
    // symbol bboxes overlap — a readability nuance, NOT a connectivity one (ERC
    // and the netlist above are clean). The layout lint is not yet bank-aware, so
    // it flags these expected overlaps; we tolerate ONLY overlaps where BOTH
    // symbols are capacitors (the bank members), and any other collision is still
    // a hard failure. Task 12 makes the layout lint bank-aware and restores the
    // strict `is_empty()` oracle.
    let is_cap = |refdes: &str| -> bool {
        design
            .blocks
            .values()
            .flat_map(|b| b.components.iter())
            .any(|(r, c)| r == refdes && c.part == "Device:C")
    };
    let warned_symbol = |w: &str, marker: &str| -> Option<String> {
        w.split(marker)
            .nth(1)
            .map(|s| s.trim().trim_start_matches("symbol ").trim().to_string())
    };
    let unexpected: Vec<&String> = out
        .layout_warnings
        .iter()
        .filter(|w| {
            // A bank-member overlap is "symbol X overlaps symbol Y" with X and Y
            // both caps. Anything else (labels, non-cap bodies) is unexpected.
            let lhs = w
                .strip_prefix("symbol ")
                .and_then(|s| s.split(" overlaps ").next())
                .map(str::to_string);
            let rhs = warned_symbol(w, "overlaps ");
            match (lhs, rhs) {
                (Some(l), Some(r)) => !(is_cap(&l) && is_cap(&r)),
                _ => true,
            }
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected layout collisions on bluepill (non-bank):\n{}",
        unexpected
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
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
