//! The bulk-create input: JSON round trip, pin resolution, decouple expansion.

use sch_check::model::{Origin, PinTarget};
use sch_check::place_parts::{BLOCK, PlacePartsInput, into_design, place_parts_input_schema};
use sch_check::{PinType, SymbolTable};

fn provider() -> SymbolTable {
    use PinType::*;
    let mut p = SymbolTable::with_basics();
    p.mock_add(
        "MCU:STM32F103C8T",
        vec![
            ("1", "VDD", PowerInput, 1),
            ("2", "VDD", PowerInput, 1),
            ("3", "VSS", PowerInput, 1),
            ("4", "NRST", Other, 1),
            ("5", "PA9", Other, 1),
            ("6", "PA10", Other, 1),
        ],
    );
    p.mock_add(
        "Regulator:AMS1117-3.3",
        vec![
            ("1", "GND", PowerInput, 1),
            ("2", "VO", PowerOutput, 1),
            ("3", "VI", PowerInput, 1),
        ],
    );
    p.mock_add(
        "Connector:USB_B_Micro",
        vec![("1", "VBUS", Passive, 1), ("2", "GND", Passive, 1)],
    );
    p
}

const TEN_PARTS: &str = r#"{
  "parts": [
    {"ref": "J1", "part": "Connector:USB_B_Micro", "pins": {"VBUS": "+5V", "GND": "GND"}},
    {"ref": "U1", "part": "Regulator:AMS1117-3.3", "value": "AMS1117-3.3",
     "pins": {"VI": "+5V", "VO": "+3V3", "GND": "GND"}},
    {"ref": "C1", "part": "Device:C", "value": "10uF", "pins": {"1": "+5V", "2": "GND"}},
    {"ref": "C2", "part": "Device:C", "value": "22uF", "pins": {"1": "+3V3", "2": "GND"}},
    {"ref": "U2", "part": "MCU:STM32F103C8T",
     "footprint": "Package_QFP:LQFP-48_7x7mm_P0.5mm",
     "props": {"MPN": "STM32F103C8T6"},
     "pins": {"VDD": "+3V3", "VSS": "GND", "NRST": "NRST", "PA9": "TX", "PA10": "RX"},
     "decouple": {"100nF": 2, "4.7uF": 1}},
    {"ref": "R1", "part": "Device:R", "value": "10k", "pins": {"1": "+3V3", "2": "NRST"}},
    {"ref": "C3", "part": "Device:C", "value": "100nF", "pins": {"1": "NRST", "2": "GND"}},
    {"ref": "D1", "part": "Device:LED", "pins": {"A": "+3V3", "K": "LED_K"}},
    {"ref": "R2", "part": "Device:R", "value": "1k", "pins": {"1": "LED_K", "2": "GND"}},
    {"ref": "TP1", "part": "Device:R", "value": "0R", "dnp": true,
     "pins": {"1": "TX", "2": "nc"}}
  ],
  "intent": {"flow": "lr", "rails": {"+3V3": "top", "GND": "bottom"},
             "ports": {"TX": "right", "RX": "right"}}
}"#;

fn parse() -> PlacePartsInput {
    serde_json::from_str(TEN_PARTS).expect("input parses")
}

#[test]
fn input_round_trips_through_json() {
    let input = parse();
    let again: PlacePartsInput =
        serde_json::from_value(serde_json::to_value(&input).unwrap()).unwrap();
    assert_eq!(input.parts, again.parts);
    let intent = again.intent.expect("intent survives");
    assert_eq!(intent.rails.len(), 2);
    assert_eq!(intent.ports.len(), 2);
}

#[test]
fn unknown_keys_are_rejected() {
    let err = serde_json::from_str::<PlacePartsInput>(
        r#"{"parts": [{"ref": "R1", "part": "Device:R", "pinz": {}}]}"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("pinz"), "{err}");
}

#[test]
fn pins_lower_to_physical_numbers() {
    let (design, diags) = into_design(&parse(), &provider());
    assert!(!diags.has_errors(), "{:?}", diags.0);
    let comps = &design.blocks[BLOCK].components;
    let mcu = &comps["U2"];
    // The name `VDD` covers both physical VDD pins.
    assert_eq!(mcu.pins["1"], PinTarget::Net("+3V3".into()));
    assert_eq!(mcu.pins["2"], PinTarget::Net("+3V3".into()));
    assert_eq!(mcu.pins["5"], PinTarget::Net("TX".into()));
    assert_eq!(comps["TP1"].pins["2"], PinTarget::NoConnect);
    assert!(comps["TP1"].dnp);
    assert_eq!(comps["U2"].props["MPN"], "STM32F103C8T6");
}

#[test]
fn decouple_expands_into_synthesized_caps() {
    let (design, _) = into_design(&parse(), &provider());
    let comps = &design.blocks[BLOCK].components;
    let synth: Vec<(&String, &sch_check::model::Component)> = comps
        .iter()
        .filter(|(_, c)| matches!(c.origin, Origin::Synthesized { .. }))
        .collect();
    assert_eq!(synth.len(), 3);
    for (refdes, cap) in &synth {
        assert!(refdes.starts_with('C'), "renumbered: {refdes}");
        assert_eq!(cap.part, "Device:C");
        assert_eq!(cap.pins["1"], PinTarget::Net("+3V3".into()));
        assert_eq!(cap.pins["2"], PinTarget::Net("GND".into()));
        let Origin::Synthesized { parent, role, .. } = &cap.origin else {
            unreachable!()
        };
        assert_eq!((parent.as_str(), role.as_str()), ("U2", "decouple"));
    }
    let mut values: Vec<&str> = synth
        .iter()
        .map(|(_, c)| c.value.as_deref().unwrap())
        .collect();
    values.sort();
    assert_eq!(values, ["100nF", "100nF", "4.7uF"]);
}

#[test]
fn power_symbol_free_rails_are_still_named_nets() {
    let (design, _) = into_design(&parse(), &provider());
    assert!(design.nets.contains_key("+3V3"));
    assert!(design.nets.contains_key("GND"));
}

#[test]
fn an_unknown_pin_is_reported_with_a_suggestion() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "U2", "part": "MCU:STM32F103C8T", "pins": {"PA99": "SIG"}}]}"#,
    )
    .unwrap();
    let (_, diags) = into_design(&input, &provider());
    let d = diags.0.iter().find(|d| d.code == "unknown-pin").unwrap();
    assert_eq!(d.suggestion.as_deref(), Some("PA9"));
}

#[test]
fn ambiguous_decouple_rails_are_reported() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "U2", "part": "MCU:STM32F103C8T",
             "pins": {"1": "+3V3", "2": "+1V8", "VSS": "GND"},
             "decouple": {"100nF": 1}}]}"#,
    )
    .unwrap();
    let (design, diags) = into_design(&input, &provider());
    assert!(diags.0.iter().any(|d| d.code == "decouple-ambiguous"));
    assert_eq!(design.blocks[BLOCK].components.len(), 1);
}

#[test]
fn schema_describes_the_accepted_shape() {
    let schema = place_parts_input_schema();
    let item = &schema["properties"]["parts"]["items"];
    assert_eq!(item["required"], serde_json::json!(["ref", "part"]));
    for key in [
        "ref",
        "part",
        "value",
        "footprint",
        "dnp",
        "props",
        "pins",
        "decouple",
    ] {
        assert!(item["properties"][key].is_object(), "missing {key}");
    }
    assert!(schema["properties"]["intent"].is_object());
    // The schema must describe exactly what the type accepts.
    let example: serde_json::Value = serde_json::from_str(TEN_PARTS).unwrap();
    for part in example["parts"].as_array().unwrap() {
        for key in part.as_object().unwrap().keys() {
            assert!(item["properties"][key].is_object(), "undocumented {key}");
        }
    }
}
