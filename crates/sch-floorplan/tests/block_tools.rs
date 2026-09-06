//! Blocks made after placement: `create_block` outlines a standing set of parts and
//! refuses one a wire leaves; `arrange_blocks` tiles the outlines as a grid.
//!
//! SKIPs cleanly without a KiCAD installation.

use kicad::KicadInstallation;
use sch_check::place_parts::PlacePartsInput;
use sch_doc::{Item, SchDoc};
use sch_floorplan::{blocks, live};

fn call(parts: serde_json::Value) -> PlacePartsInput {
    serde_json::from_value(serde_json::json!({ "parts": parts })).unwrap()
}

fn rects(doc: &SchDoc) -> Vec<geom::Rect> {
    doc.items()
        .iter()
        .filter_map(|item| match item {
            Item::Rectangle(r) => Some(geom::Rect::from_points(r.start, r.end)),
            _ => None,
        })
        .collect()
}

fn at(doc: &SchDoc, refdes: &str) -> geom::Point2 {
    doc.symbols().find(|s| s.refdes() == refdes).map(|s| s.at.point()).unwrap()
}

fn sheet(env: &KicadInstallation) -> SchDoc {
    let mut doc = live::blank_sheet().unwrap();
    for input in [
        call(serde_json::json!([
            {"ref": "U1", "part": "Regulator_Linear:AMS1117-3.3", "pins": {"1": "GND", "2": "+3V3", "3": "VBUS"}},
            {"ref": "C1", "part": "Device:C", "pins": {"1": "VBUS", "2": "GND"}},
            {"ref": "C2", "part": "Device:C", "pins": {"1": "+3V3", "2": "GND"}}
        ])),
        call(serde_json::json!([
            {"ref": "R1", "part": "Device:R", "pins": {"1": "+3V3", "2": "LED_A"}},
            {"ref": "D1", "part": "Device:LED", "pins": {"2": "LED_A", "1": "GND"}}
        ])),
        call(serde_json::json!([
            {"ref": "U2", "part": "Timer:NE555P", "pins": {"VCC": "+3V3", "GND": "GND", "OUT": "OUT", "TRIG": "TRIG", "THRES": "TRIG", "DISCH": "DIS", "CONT": "CV", "~{RST}": "+3V3"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "+3V3", "2": "DIS"}},
            {"ref": "R3", "part": "Device:R", "pins": {"1": "DIS", "2": "TRIG"}},
            {"ref": "C3", "part": "Device:C", "pins": {"1": "TRIG", "2": "GND"}}
        ])),
    ] {
        let report = live::place_parts(env, &mut doc, &input).unwrap();
        assert!(report.committed, "{:?}", report.mismatch);
    }
    doc
}

#[test]
fn blocks_are_outlined_after_placement_and_tiled_as_a_grid() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let mut doc = sheet(&env);
    let refs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    blocks::create_block(&mut doc, "POWER", &refs(&["U1", "C1", "C2"]), Some("Power")).unwrap();
    blocks::create_block(&mut doc, "LED", &refs(&["R1", "D1"]), Some("Status LED")).unwrap();
    blocks::create_block(&mut doc, "TIMER", &refs(&["U2", "R2", "R3", "C3"]), Some("Timer")).unwrap();
    assert_eq!(rects(&doc).len(), 3);

    let before = sch_doc::connect::extract(&doc).partition();
    let rows = vec![vec!["POWER".to_string(), "LED".to_string()], vec!["TIMER".to_string()]];
    let report = blocks::arrange_blocks(&mut doc, &rows).unwrap();
    assert_eq!(report.moved.len(), 3);
    assert_eq!(sch_doc::connect::extract(&doc).partition(), before);

    let frames = rects(&doc);
    let holding = |refdes: &str| frames.iter().find(|f| f.contains(at(&doc, refdes))).copied().unwrap();
    let (power, led, timer) = (holding("U1"), holding("D1"), holding("U2"));
    // One row shares a top and a height; the next row starts below it; columns share a left.
    assert!((power.min_y - led.min_y).abs() < 0.01, "{power:?} {led:?}");
    assert!((power.height() - led.height()).abs() < 0.01);
    assert!(timer.min_y > power.max_y);
    assert!((timer.min_x - power.min_x).abs() < 0.01);
    for f in &frames {
        for g in &frames {
            assert!(std::ptr::eq(f, g) || !f.overlaps(g), "{f:?} overlaps {g:?}");
        }
    }
}

#[test]
fn a_set_a_wire_leaves_is_no_block() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let mut doc = sheet(&env);
    // R2 and R3 are wired to U2 (DIS, TRIG) inside the timer's drawing.
    let err = blocks::create_block(&mut doc, "HALF", &["R2".to_string(), "R3".to_string()], None).unwrap_err();
    assert!(matches!(err, blocks::BlockError::WiredAcross { .. }), "{err}");
}
