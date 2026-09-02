//! What "cleanest" means, as a number.
//!
//! The terms are the ones a reviewer's eye lands on, in the three tiers a
//! reviewer reads them in. A **fault** makes the drawing wrong to look at — a
//! wire ruled through a part, a wire end hanging in space, a dot that connects
//! nothing, two bodies on top of each other. One of those caps a sheet's score
//! however tidy the rest is, so each is worth hundreds of millimetres here. A
//! **blemish** is a crossing or a needless corner. The rest is *tidiness*: total
//! wire, how far apart one net's pins were left, how badly identical parts fail
//! to line up.
//!
//! Everything is read off a [`Sheet`], so scoring a candidate costs one scene
//! pass and no KiCAD.

use std::collections::HashMap;

use geom::{Point2, Rect, Segment};

use crate::sheet::Sheet;

/// How much each defect counts against a sheet, in millimetres of equivalent
/// wire — the unit that keeps the terms comparable when the search trades one
/// for another.
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    pub through_body: f64,
    pub body_overlap: f64,
    pub dangling_end: f64,
    pub stranded_label: f64,
    pub junction_fault: f64,
    pub crossing: f64,
    pub bend: f64,
    pub text_collision: f64,
    pub crowding: f64,
    pub label: f64,
    pub net_spread: f64,
    pub misalignment: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Weights {
            through_body: 400.0,
            body_overlap: 600.0,
            dangling_end: 400.0,
            stranded_label: 400.0,
            junction_fault: 200.0,
            crossing: 25.0,
            bend: 10.0,
            text_collision: 12.0,
            crowding: 40.0,
            label: 60.0,
            net_spread: 1.0,
            misalignment: 1.0,
        }
    }
}

/// A sheet measured.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Metrics {
    pub wire_length: f64,
    pub bends: usize,
    pub crossings: usize,
    pub through_bodies: usize,
    pub body_overlaps: usize,
    pub dangling_ends: usize,
    /// Labels naming a point with nothing drawn on it.
    pub stranded_labels: usize,
    pub junction_faults: usize,
    pub text_collisions: usize,
    /// Bodies closer than a wire can pass between.
    pub crowding: usize,
    /// Local labels standing in for a wire — the "wired it with a name" debit,
    /// and the only label count worth having: a power, global or hierarchical
    /// name is design, not a shortcut.
    pub labels: usize,
    /// Sum over small nets of the half-perimeter of the box their pins span —
    /// the term that pulls a decoupling cap onto its IC's pin.
    pub net_spread: f64,
    /// How badly identical parts fail to share a row or a column.
    pub misalignment: f64,
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

    /// The single number the search minimises.
    pub fn score(&self, w: &Weights) -> f64 {
        self.wire_length
            + w.through_body * self.through_bodies as f64
            + w.body_overlap * self.body_overlaps as f64
            + w.dangling_end * self.dangling_ends as f64
            + w.stranded_label * self.stranded_labels as f64
            + w.junction_fault * self.junction_faults as f64
            + w.crossing * self.crossings as f64
            + w.bend * self.bends as f64
            + w.text_collision * self.text_collisions as f64
            + w.crowding * self.crowding as f64
            + w.label * self.labels as f64
            + w.net_spread * self.net_spread
            + w.misalignment * self.misalignment
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

/// Corners: a point where exactly two wire ends meet at a right angle and
/// nothing else is going on.
fn bends(sheet: &Sheet) -> usize {
    let mut count = 0;
    for (node, wires) in &sheet.incident {
        if wires.len() != 2 || sheet.fixtures.contains(node) || sheet.pins_at.contains_key(node) {
            continue;
        }
        let (u, v) = (&sheet.wires[wires[0]], &sheet.wires[wires[1]]);
        if u.horizontal() != v.horizontal() {
            count += 1;
        }
    }
    count
}

/// Text that overlaps other text, a part body, or a wire.
fn text_collisions(sheet: &Sheet) -> usize {
    let mut boxes: Vec<Rect> = sheet.texts.clone();
    let text_count = boxes.len();
    boxes.extend(sheet.part_bodies().map(|b| b.rect));
    let mut count = 0;
    for (i, j) in geom::candidate_pairs(&boxes) {
        if i < text_count && boxes[i].overlaps(&boxes[j]) {
            count += 1;
        }
    }
    for text in &sheet.texts {
        count += usize::from(
            sheet
                .wires
                .iter()
                .any(|w| Segment::new(w.a, w.b).axis_aligned_hits_rect_interior(text)),
        );
    }
    count
}

/// Part bodies that overlap, and part bodies too close for a wire to pass.
fn spacing(sheet: &Sheet) -> (usize, usize) {
    let boxes: Vec<Rect> = sheet.part_bodies().map(|b| b.rect).collect();
    let near: Vec<Rect> = boxes.iter().map(|r| r.inflate(1.27)).collect();
    let (mut overlaps, mut crowded) = (0, 0);
    for (i, j) in geom::candidate_pairs(&near) {
        if boxes[i].overlaps(&boxes[j]) {
            overlaps += 1;
        } else if near[i].overlaps(&near[j]) {
            crowded += 1;
        }
    }
    (overlaps, crowded)
}

/// Local labels standing in for a wire: a name used exactly twice, which is
/// what a two-ended connection drawn as a name looks like.
fn substitute_labels(sheet: &Sheet) -> usize {
    let mut uses: HashMap<&str, usize> = HashMap::new();
    for (node, name) in &sheet.label_names {
        if sheet.local_labels.contains(node) {
            *uses.entry(name.as_str()).or_default() += 1;
        }
    }
    uses.values().filter(|n| **n == 2).count()
}

/// Sum over small nets of the half-perimeter of the box their pins span.
fn net_spread(sheet: &Sheet) -> f64 {
    let mut by_net: HashMap<&str, Vec<Point2>> = HashMap::new();
    for pin in &sheet.pins {
        if let Some(net) = sheet.net_at(pin.at)
            && !net.starts_with('#')
        {
            by_net.entry(net).or_default().push(pin.at);
        }
    }
    by_net
        .values()
        // A rail with dozens of pins is never drawn compactly; charging its full
        // span would drown every local decision out.
        .filter(|pins| (2..=8).contains(&pins.len()))
        .filter_map(|pins| Rect::bounding(pins))
        .map(|r| r.half_perimeter())
        .sum()
}

/// How far identical parts are from sharing a row or a column.
///
/// A bank of decoupling caps or pull-ups a human drew is flush; the same parts
/// scattered are not, and no wire-length term notices the difference.
fn misalignment(sheet: &Sheet) -> f64 {
    let mut banks: HashMap<&str, Vec<Point2>> = HashMap::new();
    for body in sheet.part_bodies() {
        banks
            .entry(body.lib_id.as_str())
            .or_default()
            .push(body.rect.center());
    }
    let mut debt = 0.0;
    for centres in banks.values().filter(|c| c.len() > 1) {
        for centre in centres {
            debt += centres
                .iter()
                .filter(|other| **other != *centre)
                .map(|other| (other.x - centre.x).abs().min((other.y - centre.y).abs()))
                .fold(f64::INFINITY, f64::min)
                .min(5.08);
        }
    }
    debt
}

/// Measure a sheet.
pub fn measure(sheet: &Sheet) -> Metrics {
    let (body_overlaps, crowding) = spacing(sheet);
    Metrics {
        wire_length: sheet.wires.iter().map(|w| w.length()).sum(),
        bends: bends(sheet),
        crossings: crossings(sheet),
        through_bodies: through_bodies(sheet),
        body_overlaps,
        dangling_ends: sheet.dangling_ends(),
        stranded_labels: sheet.stranded_labels(),
        junction_faults: sheet.junction_faults(),
        text_collisions: text_collisions(sheet),
        crowding,
        labels: substitute_labels(sheet),
        net_spread: net_spread(sheet),
        misalignment: misalignment(sheet),
    }
}
