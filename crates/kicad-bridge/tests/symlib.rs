//! Acceptance tests for symbol-library parsing, encoding empirically
//! validated ground truth from the installed KiCAD libraries.

use circuit_lang::PinType;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::symlib::SymbolLib;

fn lib(name: &str) -> Option<SymbolLib> {
    let env = KicadEnv::detect()?;
    SymbolLib::load(&env.symbol_dir.join(format!("{name}.kicad_sym"))).ok()
}

#[test]
fn stm32h743vitx_has_100_pins_with_correct_types() {
    let Some(l) = lib("MCU_ST_STM32H7") else {
        eprintln!("SKIP");
        return;
    };
    let s = l.symbol("STM32H743VITx").unwrap();
    assert_eq!(s.pins.len(), 100);
    // VCAP: stacked name, power-out, at numbers 48 and 73
    let vcaps: Vec<_> = s.pins.iter().filter(|p| p.name == "VCAP").collect();
    assert_eq!(vcaps.len(), 2);
    assert!(vcaps.iter().all(|p| p.etype == PinType::PowerOutput));
    let mut nums: Vec<_> = vcaps.iter().map(|p| p.number.as_str()).collect();
    nums.sort();
    assert_eq!(nums, ["48", "73"]);
    // VDD stacked power-in
    assert!(
        s.pins
            .iter()
            .filter(|p| p.name == "VDD")
            .all(|p| p.etype == PinType::PowerInput)
    );
}

#[test]
fn extends_chain_resolves() {
    let Some(l) = lib("Regulator_Linear") else {
        eprintln!("SKIP");
        return;
    };
    let s = l.symbol("AMS1117-3.3").unwrap(); // extends AP1117-15
    assert_eq!(s.pins.len(), 3);
    let names: std::collections::BTreeSet<_> = s.pins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["GND", "VI", "VO"].into_iter().collect());
}

#[test]
fn multi_unit_symbol_reports_units() {
    let Some(l) = lib("Amplifier_Operational") else {
        eprintln!("SKIP");
        return;
    };
    let s = l.symbol("LM358").unwrap();
    let max_unit = s.pins.iter().map(|p| p.unit).max().unwrap();
    assert!(
        max_unit >= 2,
        "LM358 has at least 2 symbol units, got {max_unit}"
    );
}

#[test]
fn sub_symbol_blocks_are_not_listed_as_symbols() {
    let Some(l) = lib("Device") else {
        eprintln!("SKIP");
        return;
    };
    assert!(l.symbol("R").is_some());
    assert!(
        l.symbol("R_0_1").is_none(),
        "unit blocks must be merged, not exposed"
    );
}
