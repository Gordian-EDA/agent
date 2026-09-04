//! What "cleanest" means, as a number.
//!
//! The terms are the ones a reviewer's eye lands on. A **fault** makes the
//! drawing wrong to look at — a wire ruled through a part, a wire end hanging
//! in space, a dot that connects nothing, two bodies on top of each other.
//! [`Metrics::faults`] sums them; wire length and crossings are read
//! separately, off the same [`Metrics`].
//!
//! Everything is read off a [`Sheet`], so measuring a redraw costs one scene
//! pass and no KiCAD.

use geom::{Point2, Rect, Segment};

use crate::sheet::Sheet;

/// A sheet measured.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Metrics {
    pub wire_length: f64,
    pub crossings: usize,
    pub through_bodies: usize,
    pub body_overlaps: usize,
    pub dangling_ends: usize,
    /// Labels naming a point with nothing drawn on it.
    pub stranded_labels: usize,
    pub junction_faults: usize,
}

impl Metrics {
    /// How many outright faults the sheet has — what a reviewer's score is
    /// capped by, however tidy the rest of it is.
    pub fn faults(&self) -> usize {
        self.through_bodies
            + self.body_overlaps
            + self.dangling_ends
            + self.stranded_labels
            + self.junction_faults
    }
}

fn segment_rect(a: Point2, b: Point2) -> Rect {
    Rect::from_points(a, b).inflate(0.01)
}

/// Perpendicular wire crossings, which are not connections.
fn crossings(sheet: &Sheet) -> usize {
    let boxes: Vec<Rect> = sheet.wires.iter().map(|w| segment_rect(w.a, w.b)).collect();
    geom::candidate_pairs(&boxes)
        .into_iter()
        .filter(|(i, j)| {
            let (u, v) = (&sheet.wires[*i], &sheet.wires[*j]);
            Segment::new(u.a, u.b).axis_aligned_crosses_interior(Segment::new(v.a, v.b))
        })
        .count()
}

/// Wire segments that run through a part's body without terminating on it.
fn through_bodies(sheet: &Sheet) -> usize {
    let mut hits = 0;
    for body in sheet.part_bodies() {
        let rect = body.rect.inflate(-0.05);
        for wire in &sheet.wires {
            if !Segment::new(wire.a, wire.b).axis_aligned_hits_rect_interior(&rect) {
                continue;
            }
            let own = |p: Point2| {
                sheet
                    .pins_at
                    .get(&crate::sheet::key(p))
                    .is_some_and(|list| list.iter().any(|i| sheet.pins[*i].owner == body.uuid))
            };
            if !own(wire.a) && !own(wire.b) {
                hits += 1;
            }
        }
    }
    hits
}

/// Part bodies that overlap.
fn spacing(sheet: &Sheet) -> usize {
    let boxes: Vec<Rect> = sheet.part_bodies().map(|b| b.rect).collect();
    geom::candidate_pairs(&boxes)
        .into_iter()
        .filter(|(i, j)| boxes[*i].overlaps(&boxes[*j]))
        .count()
}

/// Measure a sheet.
pub fn measure(sheet: &Sheet) -> Metrics {
    Metrics {
        wire_length: sheet.wires.iter().map(|w| w.length()).sum(),
        crossings: crossings(sheet),
        through_bodies: through_bodies(sheet),
        body_overlaps: spacing(sheet),
        dangling_ends: sheet.dangling_ends(),
        stranded_labels: sheet.stranded_labels(),
        junction_faults: sheet.junction_faults(),
    }
}
