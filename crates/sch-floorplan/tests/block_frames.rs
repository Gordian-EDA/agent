//! A block's frame is drawn from the parts it has on the sheet: it survives parts
//! added one call at a time, and an arrange of the block.
//!
//! SKIPs cleanly without a KiCAD installation.

use kicad::KicadInstallation;
use sch_check::place_parts::PlacePartsInput;
use sch_doc::{Item, SchDoc};
use sch_floorplan::live::{self, Selection};

fn frames(doc: &SchDoc) -> usize {
    doc.items()
        .iter()
        .filter(|item| matches!(item, Item::Rectangle(_)))
        .count()
}

fn captions(doc: &SchDoc) -> Vec<String> {
    doc.items()
        .iter()
        .filter_map(|item| match item {
            Item::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

fn call(parts: serde_json::Value) -> PlacePartsInput {
    serde_json::from_value(serde_json::json!({
        "block": "POWER", "blocks": {"POWER": {"title": "Power"}}, "parts": parts
    }))
    .unwrap()
}

#[test]
fn a_block_built_over_three_calls_keeps_one_frame_around_all_of_it() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let calls = [
        call(serde_json::json!([
            {"ref": "U1", "part": "Regulator_Linear:AMS1117-3.3", "pins": {"1": "GND", "2": "+3V3", "3": "VBUS"}},
            {"ref": "C1", "part": "Device:C", "pins": {"1": "VBUS", "2": "GND"}},
            {"ref": "C2", "part": "Device:C", "pins": {"1": "+3V3", "2": "GND"}}
        ])),
        call(serde_json::json!([{"ref": "C3", "part": "Device:C", "pins": {"1": "+3V3", "2": "GND"}}])),
        call(serde_json::json!([{"ref": "D1", "part": "Device:D", "pins": {"1": "GND", "2": "VBUS"}}])),
    ];
    let mut doc = live::blank_sheet().unwrap();
    for input in &calls {
        let report = live::place_parts(&env, &mut doc, input).unwrap();
        assert!(report.committed, "{:?}", report.mismatch);
        assert_eq!(frames(&doc), 1, "captions {:?}", captions(&doc));
        assert_eq!(captions(&doc), vec!["Power".to_string()]);
    }
    let report = live::arrange(&env, &mut doc, &Selection::Block("POWER".into()), None, None).unwrap();
    assert_eq!(report.moved.len(), 5);
    assert_eq!(frames(&doc), 1);
    assert_eq!(captions(&doc), vec!["Power".to_string()]);
}

/// A part that joins a block a call later lands beside that block, inside its
/// outline — not in whatever hole the sheet had elsewhere.
#[test]
fn a_part_added_to_a_block_later_lands_inside_its_frame() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let power = call(serde_json::json!([
        {"ref": "U1", "part": "Regulator_Linear:AMS1117-3.3", "pins": {"1": "GND", "2": "+3V3", "3": "VBUS"}},
        {"ref": "C1", "part": "Device:C", "pins": {"1": "VBUS", "2": "GND"}},
        {"ref": "C2", "part": "Device:C", "pins": {"1": "+3V3", "2": "GND"}}
    ]));
    let mcu: PlacePartsInput = serde_json::from_value(serde_json::json!({
        "block": "MCU", "blocks": {"MCU": {"title": "MCU"}}, "parts": [
            {"ref": "U2", "part": "MCU_ST_STM32F1:STM32F103C8Tx", "pins": {"VDD": "+3V3", "VSS": "GND", "NRST": "NRST"}},
            {"ref": "R1", "part": "Device:R", "pins": {"1": "+3V3", "2": "NRST"}},
            {"ref": "C4", "part": "Device:C", "pins": {"1": "NRST", "2": "GND"}}
        ]
    }))
    .unwrap();
    let later = call(serde_json::json!([{"ref": "C3", "part": "Device:C", "pins": {"1": "+3V3", "2": "GND"}}]));
    let mut doc = live::blank_sheet().unwrap();
    for input in [&power, &mcu, &later] {
        let report = live::place_parts(&env, &mut doc, input).unwrap();
        assert!(report.committed, "{:?}", report.mismatch);
    }
    let at = |refdes: &str| {
        doc.symbols()
            .find(|s| s.refdes() == refdes)
            .map(|s| s.at.point())
            .unwrap()
    };
    let frames: Vec<geom::Rect> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Rectangle(r) => Some(geom::Rect::from_points(r.start, r.end)),
            _ => None,
        })
        .collect();
    let c3 = at("C3");
    let power_frame = frames
        .iter()
        .find(|f| f.contains(at("U1")))
        .expect("the power block has a frame");
    assert!(
        power_frame.contains(c3),
        "C3 at {c3:?} is outside the power frame {power_frame:?}"
    );
}
