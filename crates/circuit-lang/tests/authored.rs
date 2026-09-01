//! The authored-input checks of `sch_check::authored`, through the YAML front
//! end: an unknown lib_id, an unknown pin key, and two keys claiming one pin.

use circuit_lang::desugar::desugar;
use circuit_lang::parse::parse_str;
use sch_check::{Diagnostics, PinType, SymbolTable};

fn provider() -> SymbolTable {
    use PinType::*;
    let mut p = SymbolTable::with_basics();
    p.mock_add(
        "M:CPU",
        vec![
            ("1", "VDD", PowerInput, 1),
            ("2", "VDD", PowerInput, 1), // stacked
            ("3", "VSS", PowerInput, 1),
            ("4", "PB6", Other, 1),
            ("5", "PB7", Other, 1),
            ("6", "NRST", Other, 1),
        ],
    );
    p
}

fn run(src: &str) -> Diagnostics {
    let p = provider();
    let (s, mut diags) = parse_str(src);
    let (d, ds) = desugar(&s.unwrap(), &p);
    diags.extend(ds);
    diags.extend(sch_check::authored::lint(&d, &p));
    diags
}

#[test]
fn unknown_part_and_pin_get_suggestions() {
    let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: V, VSS: G, PB66: X, PB7: X}}
      U2: {part: M:CPX, pins: {}}
");
    let pin = diags.0.iter().find(|d| d.code == "unknown-pin").unwrap();
    assert_eq!(pin.suggestion.as_deref(), Some("PB6"));
    let part = diags.0.iter().find(|d| d.code == "unknown-part").unwrap();
    assert_eq!(part.suggestion.as_deref(), Some("M:CPU"));
}

#[test]
fn two_keys_claiming_one_physical_pin_conflict() {
    // `3` and `VSS` are the same physical pin of M:CPU.
    let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, 3: GND, VSS: AGND}}
");
    assert!(diags.0.iter().any(|d| d.code == "pin-conflict"));
}
