//! Generality regression (code-review finding): a power-INPUT pin may land on a
//! net the author never declared as a power rail. The kernel/lint permits this —
//! lint only requires power-input pins to be on *some* net, not a power-declared
//! one. Such a net has no power-output and (before the fix) no `PWR_FLAG`, so
//! KiCAD ERC raises `power_pin_not_driven` at ERROR severity: an LLM that
//! under-declares `rails:` would emit an ERC-failing schematic.
//!
//! `emit_design` must drive every net carrying a power-input pin by *etype*, not
//! just the nets the author declared in `rails:`. Here the AMS1117-3.3 regulator
//! has a power-input `VI` pin mapped to the UNDECLARED net `DRIVEME` (rails lists
//! only GND), `GND` (also power-input) to the declared GND, and the power-output
//! `VO` to `OUT`. The fix adds one flag to `DRIVEME` so ERC reports zero errors.
//!
//! SKIP-graceful when no KiCAD is detected; runs against KiCAD 10 here.

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;
use sch_engine::emit_design;

/// AMS1117-3.3: GND (pin 1, power-input), VO (pin 2, power-output), VI (pin 3,
/// power-input). VI lives on `DRIVEME`, which is NOT in `rails:`. Pre-fix this
/// net gets no flag and ERC errors with `power_pin_not_driven`.
const SRC: &str = r#"
version: 1
name: undeclared_power_input
rails: [GND]
blocks:
  main:
    components:
      U1:
        part: Regulator_Linear:AMS1117-3.3
        pins:
          VI: DRIVEME
          VO: OUT
          GND: GND
"#;

#[test]
fn power_input_on_undeclared_net_ercs_clean() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = RealSymbolProvider::new(env.clone());
    let design = circuit_lang::compile(SRC, &provider)
        .design
        .expect("design compiles (lint permits a power-input pin on an undeclared net)");

    // Sanity: DRIVEME is genuinely undeclared as a power rail in the kernel
    // design, so the *only* thing that can clear power_pin_not_driven on it is
    // the by-etype flag logic under test.
    assert!(
        !design.nets.get("DRIVEME").map(|a| a.power).unwrap_or(false),
        "DRIVEME must NOT be a declared power net for this regression to be meaningful"
    );

    let text = emit_design(&env, &design).unwrap();
    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .unwrap();
    std::fs::write(tmp.path(), &text).unwrap();

    let report = KicadCli::new(&env).erc(tmp.path()).unwrap();
    assert_eq!(
        report.error_count(),
        0,
        "ERC errors must be zero (the undeclared power-input net DRIVEME must be \
         driven by a PWR_FLAG), got {}: {:#?}",
        report.error_count(),
        report
            .violations
            .iter()
            .filter(|v| v.severity == "error")
            .collect::<Vec<_>>()
    );
}
