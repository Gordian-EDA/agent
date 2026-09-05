//! Ink that names nothing.

use std::collections::BTreeSet;

use geom::{Point2, Segment};

use crate::{Item, SchDoc};

fn key(p: Point2) -> (i64, i64) {
    ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64)
}

/// Whether the sheet draws a bus, whose geometry this crate only retains.
fn draws_a_bus(doc: &SchDoc) -> bool {
    doc.items()
        .iter()
        .any(|item| matches!(item.head(), "bus" | "bus_entry"))
}

/// The labels whose anchor touches no conductor at all — no wire along its
/// length, no pin tip, no sheet pin, no junction, no no-connect marker.
///
/// KiCAD calls one of these `label_dangling`, and it is the one kind of label
/// that may be taken away without asking what it meant: a name printed where
/// nothing conducts joins nothing to anything, so the net partition is exactly
/// the same with it and without it. Every caller still proves that — the commit
/// path diffs the extraction — but the property is structural, not a hope.
///
/// A sheet that draws a bus keeps all of its labels: bus geometry is retained
/// verbatim here, so a label sitting on one is invisible to this test.
pub fn stray_labels(doc: &SchDoc) -> Vec<String> {
    if draws_a_bus(doc) {
        return Vec::new();
    }
    let mut segments: Vec<Segment> = Vec::new();
    let mut touched: BTreeSet<(i64, i64)> = BTreeSet::new();
    for item in doc.items() {
        match item {
            Item::Wire(wire) => {
                for pair in wire.points.windows(2) {
                    touched.insert(key(pair[0]));
                    touched.insert(key(pair[1]));
                    segments.push(Segment::new(pair[0], pair[1]));
                }
            }
            Item::Junction(junction) => {
                touched.insert(key(junction.at));
            }
            Item::NoConnect(no_connect) => {
                touched.insert(key(no_connect.at));
            }
            Item::Sheet(sheet) => {
                touched.extend(sheet.pins.iter().map(|pin| key(pin.at.point())));
            }
            _ => {}
        }
    }
    touched.extend(crate::placed_pins(doc).iter().map(|pin| key(pin.at)));
    doc.labels()
        .filter(|label| {
            let at = label.at.point();
            !touched.contains(&key(at)) && !segments.iter().any(|seg| seg.contains_point(at))
        })
        .map(|label| label.uuid.clone())
        .collect()
}
