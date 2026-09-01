//! The two front ends must describe the same circuit.
//!
//! The YAML language and `place_parts` lower independently, so the property that
//! matters is that the *netlist* they produce is identical: the same parts, and
//! for each the same physical-pin → net map, decoupling caps and no-connects
//! included. Pin-map keys differ by design (names vs numbers) and are resolved
//! away here.

use std::collections::BTreeSet;

use sch_check::model::{Design, PinTarget};
use sch_check::place_parts::{PlacePartsInput, into_design};
use sch_check::{PinType, SymbolTable, pins};

fn provider() -> SymbolTable {
    use PinType::*;
    let mut p = SymbolTable::with_basics();
    p.mock_add(
        "M:CPU",
        vec![
            ("1", "VDD", PowerInput, 1),
            ("2", "VDD", PowerInput, 1),
            ("3", "VSS", PowerInput, 1),
            ("4", "PB6", Other, 1),
            ("5", "PB7", Other, 1),
            ("6", "NRST", Other, 1),
        ],
    );
    p
}

/// `refdes.pin-number = net` (or `nc`) for every part, symbol-resolved.
fn netlist(d: &Design, provider: &SymbolTable) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for block in d.blocks.values() {
        for (refdes, comp) in &block.components {
            let meta = provider.symbol(&comp.part);
            for (key, target) in comp.pins.iter().chain(comp.units.values().flatten()) {
                let net = match target {
                    PinTarget::Net(n) => n.as_str(),
                    PinTarget::NoConnect => "nc",
                };
                let numbers: Vec<String> = match &meta {
                    Some(m) => pins::resolve(m, key)
                        .iter()
                        .map(|p| p.number.clone())
                        .collect(),
                    None => vec![key.clone()],
                };
                for number in numbers {
                    out.insert(format!("{refdes}.{number}={net}"));
                }
            }
        }
    }
    out
}

const YAML: &str = "
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, decouple: {100nF: 2}, pins: {VDD: +3V3, VSS: GND, PB6: SCL, PB7: SDA}}
      R1: {part: Device:R, value: 4.7k, pins: {1: +3V3, 2: SCL}}
      R2: {part: Device:R, value: 4.7k, pins: {1: +3V3, 2: SDA}}
      PWR1: {part: power:+3V3, pins: {1: +3V3}}
      PWR2: {part: power:GND, pins: {1: GND}}
";

const JSON: &str = r#"{
  "parts": [
    {"ref": "U1", "part": "M:CPU", "decouple": {"100nF": 2},
     "pins": {"VDD": "+3V3", "VSS": "GND", "PB6": "SCL", "PB7": "SDA"}},
    {"ref": "R1", "part": "Device:R", "value": "4.7k", "pins": {"1": "+3V3", "2": "SCL"}},
    {"ref": "R2", "part": "Device:R", "value": "4.7k", "pins": {"1": "+3V3", "2": "SDA"}},
    {"ref": "PWR1", "part": "power:+3V3", "pins": {"1": "+3V3"}},
    {"ref": "PWR2", "part": "power:GND", "pins": {"1": "GND"}}
  ]
}"#;

#[test]
fn yaml_and_place_parts_lower_to_the_same_netlist() {
    let provider = provider();
    let from_yaml = circuit_lang::compile(YAML, &provider)
        .design
        .expect("the YAML compiles");
    let input: PlacePartsInput = serde_json::from_str(JSON).unwrap();
    let (from_json, diags) = into_design(&input, &provider);
    assert!(!diags.has_errors(), "{:?}", diags.0);
    assert_eq!(
        netlist(&from_yaml, &provider),
        netlist(&from_json, &provider)
    );
}

#[test]
fn both_front_ends_synthesize_the_same_decoupling_caps() {
    let provider = provider();
    let from_yaml = circuit_lang::compile(YAML, &provider).design.unwrap();
    let input: PlacePartsInput = serde_json::from_str(JSON).unwrap();
    let (from_json, _) = into_design(&input, &provider);
    let caps = |d: &Design| -> Vec<(String, Option<String>)> {
        d.blocks
            .values()
            .flat_map(|b| b.components.iter())
            .filter(|(_, c)| matches!(c.origin, sch_check::model::Origin::Synthesized { .. }))
            .map(|(r, c)| (r.clone(), c.value.clone()))
            .collect()
    };
    assert_eq!(caps(&from_yaml), caps(&from_json));
    assert_eq!(caps(&from_yaml).len(), 2);
}

#[test]
fn both_front_ends_derive_the_same_net_attributes() {
    let provider = provider();
    let from_yaml = circuit_lang::compile(YAML, &provider).design.unwrap();
    let input: PlacePartsInput = serde_json::from_str(JSON).unwrap();
    let (from_json, _) = into_design(&input, &provider);
    for net in ["+3V3", "GND"] {
        assert!(
            from_yaml.nets[net].power && from_json.nets[net].power,
            "{net}"
        );
    }
    assert!(!from_json.nets["SCL"].power);
}
