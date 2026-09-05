//! A net the payload names on ONE pin is a port to another sheet, and is drawn as
//! the label that says so; a one-pin net under a tool-derived name is a pin left
//! unconnected on purpose, and gets the marker.
//!
//! SKIPs cleanly without a KiCAD installation.

use kicad::KicadInstallation;
use sch_check::place_parts::PlacePartsInput;
use sch_floorplan::live;

fn sheet(env: &KicadInstallation) -> sch_doc::SchDoc {
    let input: PlacePartsInput = serde_json::from_value(serde_json::json!({
        "parts": [
            {"ref": "R42", "block": "peak", "part": "Device:R",
             "pins": {"1": "ISENSE-", "2": "N$3"}},
            {"ref": "U11", "block": "peak", "part": "Amplifier_Current:INA241A2xDDF",
             "pins": {"1": "N$4", "2": "GNDA", "3": "GNDA", "4": "GNDA", "5": "N$1",
                      "6": "VANA", "7": "VREFH", "8": "N$3"}},
            {"ref": "C44", "block": "peak", "part": "Device:C",
             "pins": {"1": "N$3", "2": "N$4"}},
        ]
    }))
    .unwrap();
    let mut doc = live::blank_sheet().unwrap();
    let report = live::place_parts(env, &mut doc, &input).unwrap();
    assert!(report.committed, "{:?}", report.mismatch);
    doc
}

#[test]
fn a_named_one_pin_net_is_drawn_as_a_port_label() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let doc = sheet(&env);
    let netlist = sch_doc::connect::extract(&doc);
    let pins_of = |name: &str| -> Vec<String> {
        netlist
            .nets
            .iter()
            .filter(|net| net.name == name)
            .flat_map(|net| net.pins.iter().map(|pin| format!("{}.{}", pin.refdes, pin.pin)))
            .collect()
    };
    assert_eq!(pins_of("ISENSE-"), vec!["R42.1".to_string()]);
    assert_eq!(pins_of("VREFH"), vec!["U11.7".to_string()]);
}

#[test]
fn a_derived_one_pin_net_is_marked_no_connect() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let doc = sheet(&env);
    let labels: Vec<String> = doc.labels().map(|l| sch_doc::unescape(&l.text)).collect();
    assert!(!labels.iter().any(|l| l == "N$1"), "{labels:?}");
    let markers = doc
        .items()
        .iter()
        .filter(|item| matches!(item, sch_doc::Item::NoConnect(_)))
        .count();
    assert!(markers >= 1, "U11.5 carries no marker");
}
