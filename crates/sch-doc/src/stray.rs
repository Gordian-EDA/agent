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
    let (segments, touched) = conductors(doc, |_| true);
    doc.labels()
        .filter(|label| {
            let at = label.at.point();
            !touched.contains(&key(at)) && !segments.iter().any(|seg| seg.contains_point(at))
        })
        .map(|label| label.uuid.clone())
        .collect()
}

/// The power glyphs — `#PWR` rails and `#FLG` flags — whose pin touches no
/// conductor: no wire, no pin of another symbol, no label, no junction.
///
/// A rail glyph is the engine's own furniture. One left with nothing under it
/// names a node that does not exist, and KiCAD reports the pin as unconnected;
/// the model then spends its turn removing rails one by one. A `#PWR` glyph is
/// only taken when its net still has a member elsewhere, so the sole mention of
/// a rail on the sheet stays for the author to wire.
///
/// A sheet that draws a bus keeps its glyphs, as [`stray_labels`] keeps its labels.
pub fn loose_power_glyphs(doc: &SchDoc) -> Vec<String> {
    if draws_a_bus(doc) {
        return Vec::new();
    }
    let glyph = |inst: &crate::SymbolInst| inst.refdes().starts_with('#');
    let (segments, mut touched) = conductors(doc, |pin| !pin.refdes.starts_with('#'));
    touched.extend(doc.labels().map(|label| key(label.at.point())));
    let pins = crate::placed_pins(doc);
    let netlist = crate::connect::extract(doc);
    let has_other_member = |name: &str| {
        netlist
            .nets
            .iter()
            .any(|net| net.name == name && net.pins.iter().any(|pin| !pin.refdes.starts_with('#')))
    };
    doc.symbols()
        .filter(|inst| glyph(inst))
        .filter(|inst| {
            pins.iter().filter(|pin| pin.owner == inst.uuid).all(|pin| {
                !touched.contains(&key(pin.at)) && !segments.iter().any(|seg| seg.contains_point(pin.at))
            })
        })
        .filter(|inst| inst.refdes().starts_with("#FLG") || has_other_member(inst.value()))
        .map(|inst| inst.uuid.clone())
        .collect()
}

/// The wire segments of the sheet, and every point something conducts at:
/// wire ends, junctions, no-connect markers, sheet pins, and the tips of the
/// placed pins `keep` accepts.
fn conductors(doc: &SchDoc, keep: impl Fn(&crate::PlacedPin) -> bool) -> (Vec<Segment>, BTreeSet<(i64, i64)>) {
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
    touched.extend(crate::placed_pins(doc).iter().filter(|pin| keep(pin)).map(|pin| key(pin.at)));
    (segments, touched)
}
