//! A sheet is the same sheet however many calls built it.
//!
//! The model states a design as several `place_parts` calls, one per functional section.
//! Every call after the first goes through the graft path, which seats its block beside
//! what is already drawn — so without a re-seat the arrangement is only ever as good as
//! the order the blocks arrived in, and the same design built in one call and in four
//! comes out on different paper.
//!
//! SKIPs cleanly without a KiCAD installation.

use kicad::KicadInstallation;
use sch_check::place_parts::PlacePartsInput;
use sch_doc::SchDoc;

/// The design: three blocks the model would place in three calls.
fn payload() -> serde_json::Value {
    serde_json::json!({
        "name": "reseat",
        "parts": [
            {"ref": "U1", "part": "Amplifier_Operational:LM358", "block": "gain",
             "pins": {"1": "OUT_A", "2": "INV_A", "3": "IN_A"}},
            {"ref": "R1", "part": "Device:R", "value": "10k", "block": "gain",
             "pins": {"1": "INV_A", "2": "GND"}},
            {"ref": "R2", "part": "Device:R", "value": "100k", "block": "gain",
             "pins": {"1": "INV_A", "2": "OUT_A"}},
            {"ref": "C1", "part": "Device:C", "value": "100n", "block": "filter",
             "pins": {"1": "OUT_A", "2": "FILT"}},
            {"ref": "R3", "part": "Device:R", "value": "1k", "block": "filter",
             "pins": {"1": "FILT", "2": "OUT"}},
            {"ref": "C2", "part": "Device:C", "value": "1u", "block": "filter",
             "pins": {"1": "OUT", "2": "GND"}},
            {"ref": "J1", "part": "Connector:Conn_01x03_Pin", "block": "io",
             "pins": {"1": "IN_A", "2": "OUT", "3": "GND"}},
            {"ref": "R4", "part": "Device:R", "value": "22k", "block": "io",
             "pins": {"1": "IN_A", "2": "GND"}},
            {"ref": "C3", "part": "Device:C", "value": "10n", "block": "io",
             "pins": {"1": "IN_A", "2": "GND"}}
        ]
    })
}

/// `payload` narrowed to one block — one call's worth.
fn only(block: &str) -> PlacePartsInput {
    let mut value = payload();
    let parts: Vec<serde_json::Value> = value["parts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|part| part["block"] == block)
        .cloned()
        .collect();
    value["parts"] = serde_json::Value::Array(parts);
    serde_json::from_value(value).unwrap()
}

fn build(env: &KicadInstallation, calls: &[PlacePartsInput]) -> SchDoc {
    let mut doc = sch_floorplan::live::blank_sheet().expect("blank sheet");
    for (i, input) in calls.iter().enumerate() {
        let report = sch_floorplan::live::place_parts(env, &mut doc, input)
            .unwrap_or_else(|e| panic!("call {i}: {e}"));
        assert!(report.committed, "call {i} was not committed: {report:?}");
    }
    doc
}

/// How far two builds of the same design may span apart before they are different
/// drawings. Not zero: a block routed beside foreign content is not drawn identically to
/// one routed on an empty sheet, so the blocks themselves differ by a few millimetres
/// whatever the packer then does with them. The page is the exact check.
const SPAN_TOLERANCE: f64 = 0.25;

fn hull(doc: &SchDoc) -> f64 {
    doc.content_bbox().map_or(0.0, |r| r.width() * r.height())
}

/// The same three blocks, placed in one call and in three, land on the same page and
/// span the same area — the whole point of re-seating what is already drawn.
#[test]
fn one_call_and_three_calls_draw_the_same_sheet() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD installation");
        return;
    };
    let whole = build(&env, &[serde_json::from_value(payload()).unwrap()]);
    let split = build(&env, &[only("gain"), only("filter"), only("io")]);

    assert_eq!(whole.page(), split.page(), "different paper");
    let (a, b) = (hull(&whole), hull(&split));
    assert!(
        (a - b).abs() <= SPAN_TOLERANCE * a.max(b),
        "one call spans {a:.0} mm² and three span {b:.0} mm²"
    );
}

/// Re-seating is deterministic: the block order a sheet was built in does not change
/// where the blocks end up.
#[test]
fn the_call_order_does_not_change_the_arrangement() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD installation");
        return;
    };
    let forward = build(&env, &[only("gain"), only("filter"), only("io")]);
    let backward = build(&env, &[only("io"), only("filter"), only("gain")]);
    assert_eq!(forward.page(), backward.page(), "different paper");
    let (a, b) = (hull(&forward), hull(&backward));
    assert!(
        (a - b).abs() <= SPAN_TOLERANCE * a.max(b),
        "forward spans {a:.0} mm² and backward {b:.0} mm²"
    );
}
