//! The bench: symbols that are on the sheet and on their nets, but not laid out.
//!
//! A design is built incrementally, so "this part exists and is wired, but nobody
//! has decided where it goes" is a legal state and has to be drawable. A benched
//! symbol sits in a reserved rectangle clear of the drawing, carries
//! [`AP_BENCH`](sch_model::result::AP_BENCH), and names each of its pins with a
//! label instead of a wire. Connectivity is therefore COMPLETE the moment a part
//! is benched — the extractor and `kicad-cli` both put a benched pin on the same
//! net as the pins it was declared with — which is what lets every other tool
//! treat the bench as progress rather than as damage.
//!
//! Two things put a symbol here: [`add_parts`](crate::live::add_parts), which is a
//! payload with no layout at all, and a placement whose every engine drew a sheet
//! that did not mean what the payload said — those symbols are benched rather than
//! the whole netlist discarded. [`crate::live::arrange`] is how they leave.
//!
//! Only an AUTHORED net name may be benched. KiCAD derives an unnamed net's name
//! from its own pins (`Net-(R1-Pad1)`), so writing that name as a label forks the
//! net the moment the partition shifts; [`bench`] refuses such a pin rather than
//! draw a lie.

use std::collections::{BTreeMap, BTreeSet};

use geom::{Point2, Rect};
use sch_doc::{LabelKind, Pose, SchDoc, SymbolSource};
use sch_model::result::{AP_BENCH, AP_BLOCK};
use serde::{Deserialize, Serialize};

/// Gap between the drawing and the bench, and between bench cells.
const GUTTER: f64 = 25.4;

/// One symbol to bench: what to draw and what its pins are on.
#[derive(Debug, Clone)]
pub struct BenchPart {
    pub refdes: String,
    pub lib_id: String,
    pub value: String,
    pub footprint: Option<String>,
    /// Physical pin number → net name. A pin left out is drawn no-connect.
    pub pins: BTreeMap<String, String>,
    /// Why this part is on the bench, in one clause.
    pub why: String,
}

/// One benched symbol, as every tool reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Benched {
    #[serde(rename = "ref")]
    pub refdes: String,
    pub why: String,
}

/// Refdes → why, for every symbol currently on `doc`'s bench.
pub fn benched(doc: &SchDoc) -> Vec<String> {
    doc.symbols()
        .filter(|symbol| is_benched(symbol))
        .map(|symbol| symbol.refdes().to_string())
        .collect()
}

/// Whether one symbol is on the bench.
pub fn is_benched(symbol: &sch_doc::SymbolInst) -> bool {
    symbol
        .fields
        .get(AP_BENCH)
        .is_some_and(|field| field.value == "1")
}

/// Take `refs` off the bench, so a later layout treats them as ordinary symbols.
///
/// Their pin labels are left alone: whoever lays them out redraws the wiring from
/// the netlist, and a label that survives is a debit that call reports, not a
/// connectivity change.
pub fn unbench(doc: &mut SchDoc, refs: &BTreeSet<String>) -> Vec<String> {
    let taken: Vec<(String, String)> = doc
        .symbols()
        .filter(|symbol| is_benched(symbol) && refs.contains(symbol.refdes()))
        .map(|symbol| (symbol.uuid.clone(), symbol.refdes().to_string()))
        .collect();
    for (uuid, _) in &taken {
        let _ = doc.set_field(uuid, AP_BENCH, "");
    }
    taken.into_iter().map(|(_, refdes)| refdes).collect()
}

/// What [`bench`] did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BenchReport {
    /// Symbols now on the bench, in refdes order.
    pub benched: Vec<Benched>,
    /// Pins left unnamed because their net has no authored name — the one thing
    /// the bench cannot express.
    pub unnamed_pins: Vec<String>,
}

/// Put `parts` on `block`'s bench, wiring each pin by name.
///
/// `doc` is edited in place. The caller gates the result as it gates any other
/// edit; benching draws no wire and touches nothing already on the sheet, so the
/// only net change it can make is the one it was asked for.
pub fn bench(
    doc: &mut SchDoc,
    source: &SymbolSource,
    block: &str,
    parts: &[BenchPart],
) -> sch_doc::Result<BenchReport> {
    let mut report = BenchReport::default();
    let global = global_nets(doc);
    let mut cursor = Cursor::new(doc);
    for part in parts {
        let at = cursor.next_slot();
        let uuids = doc.add_symbol(&part.lib_id, &part.refdes, &part.value, at, source)?;
        for uuid in &uuids {
            doc.set_field(uuid, AP_BENCH, "1")?;
            doc.set_field(uuid, AP_BLOCK, block)?;
            if let Some(footprint) = &part.footprint {
                doc.set_field(uuid, "Footprint", footprint)?;
            }
        }
        report.benched.push(Benched {
            refdes: part.refdes.clone(),
            why: part.why.clone(),
        });
        name_pins(doc, part, &global, &mut report);
    }
    report.benched.sort_by(|a, b| a.refdes.cmp(&b.refdes));
    Ok(report)
}

/// Hang a label on each of the symbol's pins, so the netlist is complete without
/// a single wire being drawn.
fn name_pins(
    doc: &mut SchDoc,
    part: &BenchPart,
    global: &BTreeSet<String>,
    report: &mut BenchReport,
) {
    let placed: Vec<(String, Point2)> = sch_doc::placed_pins(doc)
        .into_iter()
        .filter(|pin| pin.refdes == part.refdes)
        .map(|pin| (pin.number, pin.at))
        .collect();
    for (number, at) in placed {
        let Some(net) = part.pins.get(&number) else {
            doc.add_no_connect(at);
            continue;
        };
        if is_derived_name(net) {
            report.unnamed_pins.push(format!("{}.{number}", part.refdes));
            continue;
        }
        let kind = match global.contains(net) {
            true => LabelKind::Global,
            false => LabelKind::Local,
        };
        doc.add_label(kind, net, Pose::new(at.x, at.y, 0.0));
    }
}

/// A name KiCAD generates from a net's own pins, which is therefore not a name at
/// all: writing it down forks the net as soon as the partition moves.
fn is_derived_name(net: &str) -> bool {
    net.starts_with("Net-(") || net.starts_with("unconnected-(")
}

/// The nets the sheet already names globally, so a bench label joins them in the
/// scope they are already drawn in rather than clashing with it.
fn global_nets(doc: &SchDoc) -> BTreeSet<String> {
    doc.labels()
        .filter(|label| label.kind == LabelKind::Global)
        .map(|label| sch_doc::unescape(&label.text))
        .collect()
}

/// Where the next benched symbol goes.
///
/// The bench is a rectangle of cells to the RIGHT of everything drawn, so it never
/// collides with the sheet and never has to move when the drawing grows. It is
/// bounded in height by the drawing's own, wrapping into further columns, so a long
/// bench stays on one page rather than running off the bottom. Each call measures
/// the sheet again, bench included, so one block's rectangle sits beside the last
/// rather than on top of it.
struct Cursor {
    origin: Point2,
    rows: usize,
    next: usize,
}

impl Cursor {
    fn new(doc: &SchDoc) -> Cursor {
        let occupied = occupied(doc);
        let origin = Point2::new(occupied.max_x + GUTTER, occupied.min_y.max(GUTTER));
        let rows = (((occupied.max_y - occupied.min_y) / GUTTER).floor() as usize).clamp(4, 12);
        Cursor {
            origin,
            rows,
            next: 0,
        }
    }

    fn next_slot(&mut self) -> Pose {
        let (row, column) = (self.next % self.rows, self.next / self.rows);
        self.next += 1;
        Pose::new(
            self.origin.x + column as f64 * GUTTER,
            self.origin.y + row as f64 * GUTTER,
            0.0,
        )
    }
}

/// Everything the sheet already draws — bodies and drawing alike — as one box.
fn occupied(doc: &SchDoc) -> Rect {
    let mut points: Vec<Point2> = sch_doc::body_rects(doc)
        .into_iter()
        .flat_map(|(_, body)| {
            [
                Point2::new(body.min_x, body.min_y),
                Point2::new(body.max_x, body.max_y),
            ]
        })
        .collect();
    for item in doc.items() {
        match item {
            sch_doc::Item::Wire(wire) => points.extend(wire.points.iter().copied()),
            sch_doc::Item::Label(label) => points.push(label.at.point()),
            _ => {}
        }
    }
    Rect::bounding(&points).unwrap_or(Rect::new(GUTTER, GUTTER, GUTTER, GUTTER))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_name_is_never_written_as_a_label() {
        assert!(is_derived_name("Net-(R1-Pad1)"));
        assert!(is_derived_name("unconnected-(U1-PA0-Pad14)"));
        assert!(!is_derived_name("VBUS"));
    }

    #[test]
    fn the_bench_column_starts_clear_of_the_drawing() {
        let mut cursor = Cursor {
            origin: Point2::new(100.0, 20.0),
            rows: 2,
            next: 0,
        };
        let slots: Vec<(f64, f64)> = (0..3)
            .map(|_| {
                let at = cursor.next_slot();
                (at.x, at.y)
            })
            .collect();
        assert_eq!(
            slots,
            [(100.0, 20.0), (100.0, 45.4), (100.0 + GUTTER, 20.0)],
            "cells fill a column before starting the next"
        );
    }
}
