//! The bulk-create input: JSON round trip, pin resolution, decouple expansion.

use sch_check::model::{Origin, PinTarget};
use sch_check::place_parts::{
    DEFAULT_BLOCK, ExistingSheet, PlacePartsInput, into_design, place_parts_input_schema,
};
use sch_check::{PinType, Severity, SymbolTable};

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
    p.mock_add(
        "Sensor:RailNamed",
        vec![
            ("1", "VIN", PowerInput, 1),
            ("2", "GND", PowerInput, 1),
            ("3", "SDA", Other, 1),
        ],
    );
    p.mock_add(
        "Audio:Codec",
        vec![
            ("1", "AVDD", PowerInput, 1),
            ("2", "AGND", PowerInput, 1),
            ("3", "DVDD", PowerInput, 1),
        ],
    );
    p.mock_add(
        "Fixture:NoPowerPins",
        vec![("1", "VIN", Other, 1), ("2", "GND", Passive, 1)],
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

const CAMPAIGN_BMS_POWER_PROTECTION: &str = r#"{
  "block": "power_protection",
  "intent": {
    "flow": "lr",
    "ports": {
      "+3V3": "top", "CHG": "top", "DSG": "top", "GND": "bottom",
      "LOAD+": "right", "LOAD-": "right", "PACK+": "left", "PACK-": "left"
    },
    "relations": [
      {"kind": "group", "members": ["J1", "F1", "J2", "Q1", "Q2", "RS1", "D1"], "name": "power_path", "side": "right"},
      {"anchor": "J1", "kind": "group", "members": ["U2", "C1", "C2"], "name": "ldo", "side": "top"}
    ]
  },
  "name": "10S Li-ion BMS",
  "parts": [
    {"footprint": "TerminalBlock_Phoenix:TerminalBlock_Phoenix_MKDS-1,5-2_1x02_P5.00mm_Horizontal", "part": "TerminalBlock_Phoenix:TerminalBlock_Phoenix_MKDS-1,5-2_1x02_P5.00mm_Horizontal", "pins": {"1": "PACK+", "2": "PACK-"}, "ref": "J1", "value": "PACK"},
    {"footprint": "TerminalBlock_Phoenix:TerminalBlock_Phoenix_MKDS-1,5-2_1x02_P5.00mm_Horizontal", "part": "TerminalBlock_Phoenix:TerminalBlock_Phoenix_MKDS-1,5-2_1x02_P5.00mm_Horizontal", "pins": {"1": "LOAD+", "2": "LOAD-"}, "ref": "J2", "value": "PROTECTED LOAD"},
    {"footprint": "Fuse:Fuse_1206_3216Metric", "part": "Fuse:Fuse", "pins": {"1": "PACK+", "2": "FUSED+"}, "ref": "F1", "value": "5A"},
    {"footprint": "Diode_SMD:D_SMB", "part": "Device:D_TVS", "pins": {"1": "LOAD+", "2": "LOAD-"}, "ref": "D1", "value": "SMBJ43CA 43V BIDIR"},
    {"footprint": "Resistor_SMD:R_2512_6332Metric", "part": "Device:R_Shunt", "pins": {"1": "PACK-", "2": "SHUNT-"}, "ref": "RS1", "value": "2mR"},
    {"footprint": "Package_SO:PowerPAK_SO-8_Single", "part": "Transistor_FET:Q_NMOS_GSD", "pins": {"D": "SHUNT-", "G": "CHG_GATE", "S": "FET_MID"}, "ref": "Q1", "value": "CHG NMOS"},
    {"footprint": "Package_SO:PowerPAK_SO-8_Single", "part": "Transistor_FET:Q_NMOS_GSD", "pins": {"D": "LOAD-", "G": "DSG_GATE", "S": "FET_MID"}, "ref": "Q2", "value": "DSG NMOS"},
    {"footprint": "Resistor_SMD:R_0603_1608Metric", "part": "Device:R", "pins": {"1": "CHG", "2": "CHG_GATE"}, "ref": "RCHG", "value": "100R GATE"},
    {"footprint": "Resistor_SMD:R_0603_1608Metric", "part": "Device:R", "pins": {"1": "DSG", "2": "DSG_GATE"}, "ref": "RDSG", "value": "100R GATE"},
    {"footprint": "Resistor_SMD:R_0603_1608Metric", "part": "Device:R", "pins": {"1": "CHG_GATE", "2": "FET_MID"}, "ref": "RPG", "value": "1M CHG PULLDOWN"},
    {"footprint": "Resistor_SMD:R_0603_1608Metric", "part": "Device:R", "pins": {"1": "DSG_GATE", "2": "FET_MID"}, "ref": "RPD", "value": "1M DSG PULLDOWN"},
    {"decouple": {"1uF": 2}, "footprint": "Package_TO_SOT_SMD:SOT-23", "part": "Regulator_Linear:MCP1799x-330xxTT", "pins": {"GND": "GND", "IN": "PACK+", "OUT": "+3V3"}, "ref": "U2", "value": "3V3 LDO"},
    {"footprint": "Capacitor_SMD:C_0603_1608Metric", "part": "Device:C", "pins": {"1": "PACK+", "2": "GND"}, "ref": "C1", "value": "1uF LDO IN"},
    {"footprint": "Capacitor_SMD:C_0603_1608Metric", "part": "Device:C", "pins": {"1": "+3V3", "2": "GND"}, "ref": "C2", "value": "1uF LDO OUT"}
  ]
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
    let (design, diags, _) = into_design(&parse(), &provider(), &Default::default());
    assert!(!diags.has_errors(), "{:?}", diags.0);
    let comps = &design.blocks[DEFAULT_BLOCK].components;
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
    let (design, _, _) = into_design(&parse(), &provider(), &Default::default());
    let comps = &design.blocks[DEFAULT_BLOCK].components;
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
fn only_attributed_nets_get_an_entry() {
    let (design, _, _) = into_design(&parse(), &provider(), &Default::default());
    // No power symbol in the fixture, so no net earns an attribute — the same
    // design the kernel produces for the same circuit.
    assert!(design.nets.is_empty(), "{:?}", design.nets);

    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "PWR1", "part": "power:+3V3", "pins": {"1": "+3V3"}},
             {"ref": "R1", "part": "Device:R", "pins": {"1": "+3V3", "2": "SIG"}}]}"#,
    )
    .unwrap();
    let (design, _, _) = into_design(&input, &provider(), &Default::default());
    assert!(design.nets["+3V3"].power);
    assert!(!design.nets.contains_key("SIG"));
}

/// A pin key the symbol does not have leaves that ONE part out, with the repair —
/// the rest of the payload is still a circuit and is still placed.
#[test]
fn an_unknown_pin_leaves_its_part_out_with_a_suggestion() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "U2", "part": "MCU:STM32F103C8T", "pins": {"PA99": "SIG"}},
             {"ref": "R1", "part": "Device:R", "pins": {"1": "SIG", "2": "GND"}}]}"#,
    )
    .unwrap();
    let (design, diags, audit) = into_design(&input, &provider(), &Default::default());

    assert!(audit.is_valid(), "{audit:?}");
    assert!(!diags.has_errors(), "{diags:?}");
    assert_eq!(audit.unplaced.len(), 1, "{:?}", audit.unplaced);
    assert_eq!(audit.unplaced[0].refdes, "U2");
    assert!(audit.unplaced[0].reason.contains("PA99"), "{audit:?}");
    assert_eq!(audit.unplaced[0].did_you_mean.first().map(String::as_str), Some("PA9"));
    let placed: Vec<&String> = design
        .blocks
        .values()
        .flat_map(|block| block.components.keys())
        .collect();
    assert_eq!(placed, ["R1"], "every other part is still placed");
}

fn live_power_nets() -> ExistingSheet {
    ExistingSheet {
        net_pins: [("+3V3".to_string(), 1), ("GND".to_string(), 1)]
            .into_iter()
            .collect(),
        ..Default::default()
    }
}

#[test]
fn a_dangling_led_cathode_is_reported_but_not_fatal() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "D1", "part": "Device:LED",
             "pins": {"A": "+3V3", "K": "LED_K"}}]}"#,
    )
    .unwrap();
    let (_, _, audit) = into_design(&input, &provider(), &live_power_nets());

    assert!(audit.is_valid(), "a dangling pin must not refuse the payload");
    assert!(!audit.is_clean());
    assert_eq!(audit.dangling.len(), 1);
    assert_eq!(audit.dangling[0].refdes, "D1");
    assert_eq!(audit.dangling[0].pin, "K");
    assert_eq!(audit.dangling[0].net, "LED_K");
}

#[test]
fn a_divider_between_power_rails_is_accepted() {
    let input: PlacePartsInput = serde_json::from_str(include_str!(
        "../../../quality/cases/replace-pcb-component/input/seed.place-parts.json"
    ))
    .unwrap();
    let (_, _, audit) = into_design(&input, &provider(), &Default::default());

    assert!(audit.is_valid(), "{audit:?}");
}

#[test]
fn a_single_pin_signal_is_reported_but_not_fatal() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "R1", "part": "Device:R", "pins": {"1": "SIG_A"}}]}"#,
    )
    .unwrap();
    let (_, _, audit) = into_design(&input, &provider(), &Default::default());

    assert!(audit.is_valid(), "a dangling pin must not refuse the payload");
    assert_eq!(audit.dangling.len(), 1);
    assert_eq!(audit.dangling[0].net, "SIG_A");
}

#[test]
fn a_declared_port_may_have_one_pin() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "R1", "part": "Device:R", "pins": {"1": "SIG_A"}}],
            "intent": {"ports": {"SIG_A": "left"}}}"#,
    )
    .unwrap();
    let (_, _, audit) = into_design(&input, &provider(), &Default::default());

    assert!(audit.is_valid(), "{audit:?}");
}

#[test]
fn an_led_cathode_on_existing_ground_is_accepted() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "D1", "part": "Device:LED",
             "pins": {"A": "+3V3", "K": "GND"}}]}"#,
    )
    .unwrap();
    let (_, _, audit) = into_design(&input, &provider(), &live_power_nets());

    assert!(audit.is_valid(), "{audit:?}");
}

#[test]
fn a_single_pin_gnd_typo_suggests_the_existing_ground_net() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "D1", "part": "Device:LED",
             "pins": {"A": "+3V3", "K": "GRND"}}]}"#,
    )
    .unwrap();
    let (_, _, audit) = into_design(&input, &provider(), &live_power_nets());

    assert_eq!(audit.did_you_mean["GRND"], "GND");
}

#[test]
fn a_library_no_connect_pin_becomes_an_explicit_gap() {
    let mut symbols = provider();
    symbols.mock_add(
        "MCU:WithNC",
        vec![
            ("1", "NC", PinType::NoConnect, 1),
            ("2", "IO", PinType::Other, 1),
        ],
    );
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "U3", "part": "MCU:WithNC",
             "pins": {"NC": "GND", "IO": "+3V3"}}]}"#,
    )
    .unwrap();
    let (design, _, audit) = into_design(&input, &symbols, &live_power_nets());

    assert!(audit.is_valid(), "{audit:?}");
    assert_eq!(
        audit.nc_overridden,
        vec![sch_check::NcOverride {
            refdes: "U3".into(),
            pin: "1".into(),
            requested_net: "GND".into(),
        }]
    );
    assert_eq!(
        design.blocks[DEFAULT_BLOCK].components["U3"].pins["1"],
        sch_check::PinTarget::NoConnect
    );
}

#[test]
fn a_shared_pin_name_only_overrides_its_library_nc_pin() {
    let mut symbols = provider();
    symbols.mock_add(
        "MCU:MixedName",
        vec![
            ("1", "MIX", PinType::NoConnect, 1),
            ("2", "MIX", PinType::Other, 1),
        ],
    );
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "U3", "part": "MCU:MixedName",
             "pins": {"MIX": "SIGNAL"}}]}"#,
    )
    .unwrap();
    let (design, _, audit) = into_design(&input, &symbols, &Default::default());

    assert!(audit.is_valid(), "{audit:?}");
    let pins = &design.blocks[DEFAULT_BLOCK].components["U3"].pins;
    assert_eq!(pins["1"], sch_check::PinTarget::NoConnect);
    assert_eq!(pins["2"], sch_check::PinTarget::Net("SIGNAL".into()));
    assert_eq!(audit.nc_overridden.len(), 1);
    assert_eq!(audit.nc_overridden[0].pin, "1");
}

#[test]
fn decouple_uses_power_pin_types_for_a_3v3_sensor() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "U3", "part": "Sensor:RailNamed",
             "pins": {"VIN": "3V3", "GND": "GND", "SDA": "SDA"},
             "decouple": {"100nF": 1}}]}"#,
    )
    .unwrap();
    let (design, diags, _) = into_design(&input, &provider(), &Default::default());
    assert!(!diags.0.iter().any(|d| d.code == "decouple-ambiguous"));
    let caps: Vec<_> = design.blocks[DEFAULT_BLOCK]
        .components
        .values()
        .filter(|component| matches!(component.origin, Origin::Synthesized { .. }))
        .collect();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].pins["1"], PinTarget::Net("3V3".into()));
    assert_eq!(caps[0].pins["2"], PinTarget::Net("GND".into()));
}

#[test]
fn decouple_covers_each_codec_supply_net() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "U4", "part": "Audio:Codec",
             "pins": {"AVDD": "AVDD", "AGND": "GND", "DVDD": "DVDD"},
             "decouple": {"100nF": 1}}]}"#,
    )
    .unwrap();
    let (design, diags, _) = into_design(&input, &provider(), &Default::default());
    assert!(!diags.0.iter().any(|d| d.code == "decouple-ambiguous"));
    let caps: Vec<_> = design.blocks[DEFAULT_BLOCK]
        .components
        .values()
        .filter(|component| matches!(component.origin, Origin::Synthesized { .. }))
        .collect();
    assert_eq!(caps.len(), 2);
    let mut supplies: Vec<&str> = caps
        .iter()
        .filter_map(|cap| match &cap.pins["1"] {
            PinTarget::Net(net) => Some(net.as_str()),
            PinTarget::NoConnect => None,
        })
        .collect();
    supplies.sort();
    assert_eq!(supplies, ["AVDD", "DVDD"]);
    assert!(
        caps.iter()
            .all(|cap| cap.pins["2"] == PinTarget::Net("GND".into()))
    );
}

#[test]
fn decouple_without_power_pins_names_the_fallback_candidates() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "U5", "part": "Fixture:NoPowerPins",
             "pins": {"VIN": "3V3", "GND": "GND"},
             "decouple": {"100nF": 1}}]}"#,
    )
    .unwrap();
    let (design, diags, audit) = into_design(&input, &provider(), &Default::default());
    let diag = diags
        .0
        .iter()
        .find(|diag| diag.code == "decouple-ambiguous")
        .expect("missing decouple diagnostic");
    assert!(diag.message.contains("no power_in pins"), "{diag}");
    assert!(diag.message.contains("VDD*/VCC*"), "{diag}");
    assert!(diag.message.contains("VSS*/GND*"), "{diag}");
    assert!(diag.message.contains("[] / [\"GND\"]"), "{diag}");
    assert_eq!(diag.severity, Severity::Warning);
    assert_eq!(audit.decouple_unresolved.len(), 1);
    assert_eq!(audit.decouple_unresolved[0].refdes, "U5");
    assert!(
        audit.decouple_unresolved[0]
            .why
            .contains("needs supply and ground candidates")
    );
    assert!(audit.decouple_unresolved[0].how.contains("explicitly"));
    assert_eq!(design.blocks[DEFAULT_BLOCK].components.len(), 1);
}

#[test]
fn campaign_bms_unresolvable_decoupled_part_is_reported_without_panicking() {
    let input: PlacePartsInput = serde_json::from_str(CAMPAIGN_BMS_POWER_PROTECTION).unwrap();
    let (design, diags, audit) = into_design(&input, &provider(), &Default::default());

    assert!(
        design
            .blocks
            .values()
            .flat_map(|block| block.components.keys())
            .any(|reference| reference == "C1")
    );
    assert!(audit.unplaced.iter().any(|part| part.refdes == "U2"), "{audit:?}");
    assert_eq!(audit.decouple_unresolved[0].refdes, "U2", "{audit:?}");
    assert!(
        diags
            .0
            .iter()
            .any(|diagnostic| diagnostic.code == "decouple-unplaced"),
        "{diags:?}"
    );

    let available = design
        .blocks
        .values()
        .flat_map(|block| block.components.keys().cloned())
        .collect();
    let (ir, warnings) = input.intent.unwrap().into_layout_ir_for(&available);
    assert!(ir.relations.is_empty(), "{ir:?}");
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("J1") && warning.contains("J2")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|warning| warning.contains("U2")),
        "{warnings:?}"
    );
}

#[test]
fn schema_describes_the_accepted_shape() {
    let schema = place_parts_input_schema();
    let item = &schema["properties"]["parts"]["items"];
    assert_eq!(item["required"], serde_json::json!(["part"]));
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
            assert!(item["properties"][key].is_object(), "missing schema for {key}");
        }
    }
}

#[test]
fn unmentioned_signal_pins_become_no_connects() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "U2", "part": "MCU:STM32F103C8T",
             "pins": {"VDD": "+3V3", "VSS": "GND", "PA9": "TX"}}]}"#,
    )
    .unwrap();
    let (design, _, _) = into_design(&input, &provider(), &Default::default());
    let mcu = &design.blocks[DEFAULT_BLOCK].components["U2"];
    // NRST (4) and PA10 (6) were left out: explicit no-connects, not silence.
    assert_eq!(mcu.pins["4"], PinTarget::NoConnect);
    assert_eq!(mcu.pins["6"], PinTarget::NoConnect);
    // A power input is never auto-NC'd — an unconnected one is a lint error.
    assert_eq!(mcu.pins["1"], PinTarget::Net("+3V3".into()));
}

#[test]
fn a_duplicate_refdes_is_an_error() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "R1", "part": "Device:R"}, {"ref": "R1", "part": "Device:C"}]}"#,
    )
    .unwrap();
    let (design, diags, audit) = into_design(&input, &provider(), &Default::default());
    assert!(diags.0.iter().any(|d| d.code == "duplicate-ref"));
    assert_eq!(
        audit.duplicate_refs,
        [sch_check::DuplicateRef {
            refdes: "R1".into(),
            next_free: "R2".into(),
        }]
    );
    // The last declaration wins; the diagnostic says the other one is lost.
    assert_eq!(
        design.blocks[DEFAULT_BLOCK].components["R1"].part,
        "Device:C"
    );
}

#[test]
fn an_existing_reference_is_refused_with_the_next_free_designator() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "C2", "part": "Device:C", "pins": {"1": "VIN", "2": "GND"}}]}"#,
    )
    .unwrap();
    let existing = ExistingSheet {
        refs: ["C1".to_string(), "C2".to_string()].into_iter().collect(),
        ..Default::default()
    };
    let (_, _, audit) = into_design(&input, &provider(), &existing);

    assert!(!audit.is_valid());
    assert_eq!(
        audit.duplicate_refs,
        [sch_check::DuplicateRef {
            refdes: "C2".into(),
            next_free: "C3".into(),
        }]
    );
}

#[test]
fn an_omitted_reference_uses_the_library_prefix_and_first_gap() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"part": "Device:R", "pins": {"1": "VIN", "2": "GND"}}]}"#,
    )
    .unwrap();
    let existing = ExistingSheet {
        refs: ["R1".to_string(), "R3".to_string()].into_iter().collect(),
        ..Default::default()
    };
    let (design, _, audit) = into_design(&input, &provider(), &existing);

    assert!(audit.is_valid(), "{audit:?}");
    assert!(design.blocks[DEFAULT_BLOCK].components.contains_key("R2"));
}

#[test]
fn two_keys_on_one_physical_pin_conflict() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "U2", "part": "MCU:STM32F103C8T",
             "pins": {"3": "GND", "VSS": "AGND"}}]}"#,
    )
    .unwrap();
    let (_, diags, _) = into_design(&input, &provider(), &Default::default());
    assert!(
        diags.0.iter().any(|d| d.code == "pin-conflict"),
        "{:?}",
        diags.0
    );
}

#[test]
fn parts_land_on_the_named_sheet() {
    let input: PlacePartsInput =
        serde_json::from_str(r#"{"block": "power", "parts": [{"ref": "R1", "part": "Device:R"}]}"#)
            .unwrap();
    let (design, _, _) = into_design(&input, &provider(), &Default::default());
    assert!(design.blocks.contains_key("power"));
}

#[test]
fn intent_becomes_a_layout_ir() {
    let ir = parse().intent.expect("intent").into_layout_ir();
    assert_eq!(ir.rails.len(), 2);
    assert_eq!(ir.ports.len(), 2);
    // The engine's own derived fields are not input.
    assert!(ir.idioms.is_empty() && ir.frozen.is_empty() && ir.zone.is_empty());
}

#[test]
fn engine_internal_intent_fields_are_rejected() {
    let err =
        serde_json::from_str::<PlacePartsInput>(r#"{"parts": [], "intent": {"frozen": ["U1"]}}"#)
            .unwrap_err();
    assert!(err.to_string().contains("frozen"), "{err}");
}

#[test]
fn one_refusal_names_every_fault_in_the_payload() {
    // A bad lib_id, a duplicate refdes and a dangling pin used to cost three
    // separate round trips because each validation layer masked the next.
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [
             {"ref": "R1", "part": "Device:R", "pins": {"1": "+3V3", "2": "GND"}},
             {"ref": "R1", "part": "Device:R", "pins": {"1": "+3V3", "2": "GND"}},
             {"ref": "U9", "part": "Nope:NotAThing", "pins": {"1": "GND"}},
             {"ref": "R7", "part": "Device:R", "pins": {"1": "SIG_A", "2": "GND"}}]}"#,
    )
    .unwrap();
    let (_, _, audit) = into_design(&input, &provider(), &live_power_nets());

    assert!(!audit.is_valid(), "a duplicate refdes is still fatal");
    assert_eq!(audit.duplicate_refs.len(), 1);
    assert_eq!(audit.duplicate_refs[0].refdes, "R1");
    assert_eq!(audit.dangling.len(), 1, "{:?}", audit.dangling);
    assert_eq!(audit.dangling[0].net, "SIG_A");
    assert_eq!(audit.unplaced.len(), 1, "{:?}", audit.unplaced);
    assert_eq!(audit.unplaced[0].refdes, "U9");
}

#[test]
fn a_footprint_written_as_a_lib_id_says_so() {
    let input: PlacePartsInput = serde_json::from_str(
        r#"{"parts": [{"ref": "J1",
             "part": "Connector_PinHeader_2.54mm:PinHeader_1x11_P2.54mm_Vertical",
             "pins": {"1": "GND", "2": "+3V3"}}]}"#,
    )
    .unwrap();
    let (_, _, audit) = into_design(&input, &provider(), &live_power_nets());

    let unplaced = audit.unplaced.first().expect("J1 left out");
    assert!(unplaced.reason.contains("FOOTPRINT"), "{unplaced:?}");
    assert_eq!(
        unplaced.did_you_mean,
        ["Connector_Generic:Conn_01x11"],
        "the footprint's pad geometry should identify its generic symbol: {unplaced:?}"
    );
}
