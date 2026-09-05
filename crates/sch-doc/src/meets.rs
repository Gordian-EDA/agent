//! Where a sheet's conductors meet, and whether KiCAD needs a dot there.

use std::collections::BTreeMap;

use geom::{Point2, Segment};

use crate::{Item, SchDoc};

const EPS: f64 = 1e-6;

/// A sheet point as an exact key (µm), so coincident geometry compares equal.
pub type MeetKey = (i64, i64);

fn key(p: Point2) -> MeetKey {
    (
        (p[0] * 1000.0).round() as i64,
        (p[1] * 1000.0).round() as i64,
    )
}

/// The conductors that meet at one sheet point.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Meet {
    /// Wire segments that END here.
    pub ends: usize,
    /// Wire segments this point splits — each is two conductors leaving the point.
    pub passes: usize,
    /// Symbol pin tips here.
    pub pins: usize,
}

impl Meet {
    /// KiCAD's degree: one per wire end and pin tip, two per wire split. A label
    /// names the net without branching it, so it does not count.
    pub fn degree(&self) -> usize {
        self.ends + self.pins + 2 * self.passes
    }

    /// Whether KiCAD needs a junction dot here: three conductors meet, which
    /// includes a wire ending on another wire's interior. Two segments at a bend
    /// or butted end to end are degree two and take none — a dot there reads as a
    /// branch that is not on the sheet.
    pub fn needs_dot(&self) -> bool {
        self.degree() >= 3
    }
}

/// Every point of `doc` where a wire ends or a pin lands, with what meets there.
///
/// Two wires merely crossing mid-span is not such a point: KiCAD leaves that
/// crossing unconnected, and neither end of it is a place anything terminates.
pub fn meets(doc: &SchDoc) -> BTreeMap<MeetKey, Meet> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut out: BTreeMap<MeetKey, Meet> = BTreeMap::new();
    for item in doc.items() {
        if let Item::Wire(w) = item {
            segments.extend(w.points.windows(2).map(|p| Segment::new(p[0], p[1])));
        }
    }
    for seg in &segments {
        for end in [seg.a, seg.b] {
            out.entry(key(end)).or_default().ends += 1;
        }
    }
    for pin in crate::placed_pins(doc) {
        out.entry(key(pin.at)).or_default().pins += 1;
    }
    let points: Vec<(MeetKey, Point2)> = out
        .keys()
        .map(|k| (*k, Point2::from([k.0 as f64 / 1000.0, k.1 as f64 / 1000.0])))
        .collect();
    for (k, p) in points {
        out.get_mut(&k).unwrap().passes = segments
            .iter()
            .filter(|seg| {
                seg.contains_point(p)
                    && (p[0] - seg.a[0]).abs() + (p[1] - seg.a[1]).abs() > EPS
                    && (p[0] - seg.b[0]).abs() + (p[1] - seg.b[1]).abs() > EPS
            })
            .count();
    }
    out
}

/// The junction dots `doc` draws, keyed like [`meets`].
pub fn drawn_dots(doc: &SchDoc) -> BTreeMap<MeetKey, Point2> {
    doc.items()
        .iter()
        .filter_map(|item| match item {
            Item::Junction(j) => Some((key(j.at), j.at)),
            _ => None,
        })
        .collect()
}
