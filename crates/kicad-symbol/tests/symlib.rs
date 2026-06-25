//! Acceptance tests for symbol-library parsing, encoding empirically
//! validated ground truth from the installed KiCAD libraries. Everything is
//! exercised through the public [`SymbolTable`] oracle.

use kicad_cli::env::KicadEnv;
use kicad_symbol::{PinType, SymbolTable};

/// A table over the installed KiCAD symbol directory, or `None` to SKIP.
fn installed() -> Option<SymbolTable> {
    Some(SymbolTable::from_env(&KicadEnv::detect()?))
}

#[test]
fn stm32h743vitx_has_100_pins_with_correct_types() {
    let Some(t) = installed() else {
        eprintln!("SKIP");
        return;
    };
    let s = t.symbol("MCU_ST_STM32H7:STM32H743VITx").unwrap();
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
    let Some(t) = installed() else {
        eprintln!("SKIP");
        return;
    };
    let s = t.symbol("Regulator_Linear:AMS1117-3.3").unwrap(); // extends AP1117-15
    assert_eq!(s.pins.len(), 3);
    let names: std::collections::BTreeSet<_> = s.pins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["GND", "VI", "VO"].into_iter().collect());
}

#[test]
fn deep_extends_chain_resolves() {
    let Some(t) = installed() else {
        eprintln!("SKIP");
        return;
    };
    // Depth-4 chain in the real KiCAD library (the exact symbol set varies by KiCAD version):
    // MAX7409xUA -> MAX7408xUA -> MAX7408xPA -> MAX7400xPA -> MAX7400xSA
    let Some(s) = t.symbol("Filter:MAX7409xUA") else {
        eprintln!("SKIP: MAX7409xUA absent in this KiCAD version's Filter library");
        return;
    };
    assert_eq!(s.pins.len(), 8);
}

#[test]
fn deep_extends_chain_resolves_inline() {
    // Deterministic, version-independent companion to `deep_extends_chain_resolves`: a depth-4
    // extends chain A -> B -> C -> D -> E where only the base E declares pins. Resolving A must
    // walk the whole chain and surface E's pins.
    let lib_text = r#"(kicad_symbol_lib
	(version 20231120)
	(generator "test")
	(symbol "E"
		(symbol "E_1_1"
			(pin power_in line (at 0 0 0) (length 2.54)
				(name "VI" (effects (font (size 1.27 1.27))))
				(number "1" (effects (font (size 1.27 1.27)))))
			(pin power_out line (at 0 0 0) (length 2.54)
				(name "VO" (effects (font (size 1.27 1.27))))
				(number "2" (effects (font (size 1.27 1.27)))))
			(pin power_in line (at 0 0 0) (length 2.54)
				(name "GND" (effects (font (size 1.27 1.27))))
				(number "3" (effects (font (size 1.27 1.27)))))
		)
	)
	(symbol "D" (extends "E"))
	(symbol "C" (extends "D"))
	(symbol "B" (extends "C"))
	(symbol "A" (extends "B"))
)
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("deep.kicad_sym"), lib_text).unwrap();
    let t = SymbolTable::from_symbol_dir(dir.path().to_path_buf());
    let a = t.symbol("deep:A").unwrap();
    assert_eq!(
        a.pins.len(),
        3,
        "A inherits E's 3 pins through the depth-4 extends chain"
    );
    let names: std::collections::BTreeSet<_> = a.pins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["GND", "VI", "VO"].into_iter().collect());
}

#[test]
fn extends_cycle_terminates_with_zero_pins() {
    let lib_text = r#"(kicad_symbol_lib
	(version 20231120)
	(generator "test")
	(symbol "A"
		(extends "B")
	)
	(symbol "B"
		(extends "A")
	)
)
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cycle.kicad_sym"), lib_text).unwrap();
    let t = SymbolTable::from_symbol_dir(dir.path().to_path_buf());
    // A cycle must terminate (no hang, no recursion overflow) and yield the
    // pins the symbol actually has: none.
    assert!(t.symbol("cycle:A").unwrap().pins.is_empty());
    assert!(t.symbol("cycle:B").unwrap().pins.is_empty());
}

#[test]
fn missing_extends_parent_keeps_own_pins() {
    let lib_text = r#"(kicad_symbol_lib
	(version 20231120)
	(generator "test")
	(symbol "Orphan"
		(extends "DoesNotExist")
	)
)
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("orphan.kicad_sym"), lib_text).unwrap();
    let t = SymbolTable::from_symbol_dir(dir.path().to_path_buf());
    assert!(t.symbol("orphan:Orphan").unwrap().pins.is_empty());
}

#[test]
fn split_symbol_dir_resolves_extends_across_files() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Device.kicad_symdir");
    std::fs::create_dir(&lib).unwrap();
    std::fs::write(
        lib.join("Base.kicad_sym"),
        r#"(kicad_symbol_lib
	(version 20251024)
	(generator "test")
	(symbol "Base"
		(symbol "Base_1_1"
			(pin passive line (at 0 0 0) (length 2.54)
				(name "A" (effects (font (size 1.27 1.27))))
				(number "1" (effects (font (size 1.27 1.27)))))
		)
	)
)
"#,
    )
    .unwrap();
    std::fs::write(
        lib.join("Alias.kicad_sym"),
        r#"(kicad_symbol_lib
	(version 20251024)
	(generator "test")
	(symbol "Alias"
		(extends "Base")
	)
)
"#,
    )
    .unwrap();

    let t = SymbolTable::from_symbol_dir(dir.path().to_path_buf());
    let alias = t.symbol("Device:Alias").unwrap();
    assert_eq!(alias.pins.len(), 1);
    assert_eq!(alias.pins[0].number, "1");
    assert_eq!(alias.pins[0].name, "A");
}

#[test]
fn multi_unit_symbol_reports_units() {
    let Some(t) = installed() else {
        eprintln!("SKIP");
        return;
    };
    let s = t.symbol("Amplifier_Operational:LM358").unwrap();
    let max_unit = s.pins.iter().map(|p| p.unit).max().unwrap();
    assert!(
        max_unit >= 2,
        "LM358 has at least 2 symbol units, got {max_unit}"
    );
}

#[test]
fn sub_symbol_blocks_are_not_listed_as_symbols() {
    let Some(t) = installed() else {
        eprintln!("SKIP");
        return;
    };
    assert!(t.symbol("Device:R").is_some());
    assert!(
        t.symbol("Device:R_0_1").is_none(),
        "unit blocks must be merged, not exposed"
    );
}
