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
