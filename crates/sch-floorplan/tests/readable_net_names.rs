//! A net the engine has to name is named after the pin a reader would name it
//! after — `PB6`, not `N_U1_42`.
//!
//! SKIPs cleanly without a KiCAD installation.

use kicad::KicadInstallation;
use sch_check::place_parts::PlacePartsInput;
use sch_doc::SchDoc;
use sch_floorplan::live;

fn labels(doc: &SchDoc) -> Vec<String> {
    doc.labels().map(|l| sch_doc::unescape(&l.text)).collect()
}

fn machine_shaped(labels: &[String]) -> Vec<&String> {
    labels
        .iter()
        .filter(|text| sch_doc::netname::machine_parts(text).is_some())
        .collect()
}

/// `PB6` runs from the MCU to a header. KiCAD's own derivation of that net is no
/// name a person wrote, so the drawn label is the MCU's pin name.
#[test]
fn a_boundary_net_off_an_mcu_pin_is_labelled_with_that_pin() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let input: PlacePartsInput = serde_json::from_value(serde_json::json!({
        "parts": [
            {"ref": "U1", "block": "mcu", "part": "MCU_ST_STM32F1:STM32F103C8Tx",
             "pins": {"PB6": "Net-(U1-PB6)", "VSS": "GND", "VDD": "+3V3"}},
        ]
    }))
    .unwrap();
    let header: PlacePartsInput = serde_json::from_value(serde_json::json!({
        "parts": [
            {"ref": "J1", "block": "io", "part": "Connector_Generic:Conn_01x02",
             "pins": {"1": "Net-(U1-PB6)", "2": "GND"}},
        ]
    }))
    .unwrap();

    let mut doc = live::blank_sheet().unwrap();
    let first = live::place_parts(&env, &mut doc, &input).unwrap();
    assert!(first.committed, "{:?}", first.mismatch);
    let second = live::place_parts(&env, &mut doc, &header).unwrap();
    assert!(second.committed, "{:?}", second.mismatch);

    let drawn = labels(&doc);
    assert!(drawn.contains(&"PB6".to_string()), "labels: {drawn:?}");
    assert!(
        machine_shaped(&drawn).is_empty(),
        "machine-made names were drawn: {:?}",
        machine_shaped(&drawn)
    );

    // The rename is a rename, not a re-wire: both ends are still one net.
    let netlist = sch_doc::connect::extract(&doc);
    let pb6 = netlist
        .nets
        .iter()
        .find(|net| net.name == "PB6")
        .unwrap_or_else(|| panic!("no net PB6 in {:?}", netlist.nets));
    let mut pins: Vec<String> = pb6
        .pins
        .iter()
        .map(|pin| format!("{}.{}", pin.refdes, pin.pin))
        .collect();
    pins.sort();
    assert_eq!(pins, vec!["J1.1".to_string(), "U1.42".to_string()]);
}

/// A net only passives touch has no pin name worth reading, so it keeps the
/// `N_REF_PAD` form — which is meaningless but always available. Wired inside one
/// call it needs no label at all, so nothing machine-shaped is ever drawn.
#[test]
fn a_passive_only_net_is_wired_rather_than_labelled() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let input: PlacePartsInput = serde_json::from_value(serde_json::json!({
        "parts": [
            {"ref": "R1", "block": "a", "part": "Device:R",
             "pins": {"1": "VIN", "2": "Net-(R1-Pad2)"}},
            {"ref": "R2", "block": "b", "part": "Device:R",
             "pins": {"1": "Net-(R1-Pad2)", "2": "GND"}},
        ]
    }))
    .unwrap();
    let mut doc = live::blank_sheet().unwrap();
    let report = live::place_parts(&env, &mut doc, &input).unwrap();
    assert!(report.committed, "{:?}", report.mismatch);
    let drawn = labels(&doc);
    assert!(
        machine_shaped(&drawn).is_empty(),
        "machine-made names were drawn: {:?}",
        machine_shaped(&drawn)
    );
}
