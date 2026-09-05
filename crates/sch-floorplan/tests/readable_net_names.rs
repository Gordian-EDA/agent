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

/// The sheet's connectivity alone, as sorted pin lists — comparable across a
/// re-arrange without net names getting in the way.
fn partition(doc: &SchDoc) -> std::collections::BTreeSet<Vec<String>> {
    sch_doc::connect::extract(doc)
        .nets
        .iter()
        .map(|net| {
            let mut pins: Vec<String> = net
                .pins
                .iter()
                .filter(|pin| !pin.refdes.starts_with('#'))
                .map(|pin| format!("{}.{}", pin.refdes, pin.pin))
                .collect();
            pins.sort();
            pins
        })
        .filter(|pins| pins.len() > 1)
        .collect()
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

/// Re-arranging PART of a sheet names every net it straddles that the sheet has
/// no name for. Whatever it draws, nothing machine-shaped may appear — and the
/// netlist the sheet had is the netlist it keeps.
#[test]
fn a_boundary_net_minted_by_arrange_reads_as_a_pin_name() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let input: PlacePartsInput = serde_json::from_value(serde_json::json!({
        "parts": [
            {"ref": "U1", "part": "MCU_ST_STM32F1:STM32F103C8Tx",
             "pins": {"PB6": "@R1.1", "VSS": "GND", "VDD": "+3V3"}},
            {"ref": "R1", "part": "Device:R", "pins": {"2": "+3V3"}},
        ]
    }))
    .unwrap();
    let mut doc = live::blank_sheet().unwrap();
    let placed = live::place_parts(&env, &mut doc, &input).unwrap();
    assert!(placed.committed, "{:?}", placed.mismatch);

    let before = partition(&doc);
    let selection = live::Selection::Refs(vec!["R1".to_string()]);
    let report = live::arrange(&env, &mut doc, &selection, None, None).unwrap();
    assert!(report.committed, "rolled back — {:?}", report.mismatch);
    assert_eq!(before, partition(&doc), "arranging changed a net");

    let drawn = labels(&doc);
    assert!(
        machine_shaped(&drawn).is_empty(),
        "machine-made names were drawn: {:?}",
        machine_shaped(&drawn)
    );
}
