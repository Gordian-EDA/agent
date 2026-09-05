//! A netlist's numbered net (`N$4`) names nothing but the net, and a later call
//! spelling the same number means the same net: the two pins join across calls.
//!
//! SKIPs cleanly without a KiCAD installation.

use kicad::KicadInstallation;
use sch_check::place_parts::PlacePartsInput;
use sch_floorplan::live;

#[test]
fn a_numbered_net_joins_across_two_calls() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let first: PlacePartsInput = serde_json::from_value(serde_json::json!({"parts": [
        {"ref": "U3", "block": "reg", "part": "Regulator_Linear:AMS1117-3.3",
         "pins": {"1": "GND", "2": "+3V3", "3": "+5V"}},
        {"ref": "C8", "block": "reg", "part": "Device:C", "pins": {"1": "+3V3", "2": "N$4"}}
    ]}))
    .unwrap();
    let second: PlacePartsInput = serde_json::from_value(serde_json::json!({"parts": [
        {"ref": "C7", "block": "acc", "part": "Device:C", "pins": {"1": "N$4", "2": "GND"}}
    ]}))
    .unwrap();
    let mut doc = live::blank_sheet().unwrap();
    for input in [&first, &second] {
        let report = live::place_parts(&env, &mut doc, input).unwrap();
        assert!(report.committed, "{:?}", report.mismatch);
    }
    let netlist = sch_doc::connect::extract(&doc);
    let mut pins: Vec<String> = netlist
        .nets
        .iter()
        .filter(|net| net.name == "N$4")
        .flat_map(|net| net.pins.iter().map(|pin| format!("{}.{}", pin.refdes, pin.pin)))
        .collect();
    pins.sort();
    assert_eq!(pins, vec!["C7.1".to_string(), "C8.2".to_string()]);
}
