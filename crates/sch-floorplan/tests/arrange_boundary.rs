//! Arranging PART of a sheet must leave every net it straddles alone.
//!
//! The selection's own drawing is erased and redrawn from the selection's
//! terminals, so a net whose other pin belongs to a symbol that did not move is
//! the case that has to be reached by NAME. A net KiCAD only auto-named has no
//! name to reach by, so `arrange` mints one on both halves.
//!
//! SKIPs cleanly without a KiCAD installation.

use std::collections::BTreeSet;

use kicad::KicadInstallation;
use sch_check::place_parts::PlacePartsInput;
use sch_doc::{SchDoc, connect};
use sch_floorplan::live::{self, Selection};

fn partition(doc: &SchDoc) -> BTreeSet<Vec<String>> {
    connect::extract(doc)
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

fn chain() -> PlacePartsInput {
    serde_json::from_value(serde_json::json!({
        "parts": [
            {"ref": "R1", "part": "Device:R", "pins": {"1": "VIN", "2": "A"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "A", "2": "B"}},
            {"ref": "R3", "part": "Device:R", "pins": {"1": "B", "2": "GND"}},
            {"ref": "C1", "part": "Device:C", "pins": {"1": "B", "2": "GND"}}
        ]
    }))
    .unwrap()
}

#[test]
fn arranging_one_symbol_keeps_its_neighbours_on_their_nets() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let mut doc = live::blank_sheet().unwrap();
    let placed = live::place_parts(
        &env,
        &mut doc,
        &chain(),
        Box::new(spine_place::SpinePlace),
        None,
    )
    .unwrap();
    assert!(placed.committed, "{:?}", placed.mismatch);
    let before = partition(&doc);

    let selection = Selection::Refs(vec!["R2".to_string()]);
    let report = live::arrange(
        &env,
        &mut doc,
        &selection,
        None,
        Box::new(spine_place::SpinePlace),
        None,
    )
    .unwrap();

    assert!(report.committed, "rolled back — {:?}", report.mismatch);
    assert_eq!(before, partition(&doc), "arranging changed a net");
}
