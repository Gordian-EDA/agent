//! What "cleanest" means, as a number.
//!
//! The terms are the ones a reviewer's eye lands on: how much wire there is,
//! how often it turns, how often it crosses, whether it runs through a part,
//! whether text collides, whether a connection was made with a label instead of
//! a wire, and how far apart the parts of one net were left. Everything is read
//! off a [`Sheet`], so scoring a candidate costs one scene pass and no KiCAD.

use geom::{Point2, Rect, Segment};

use crate::sheet::Sheet;

/// How much each defect counts against a sheet.
///
/// Millimetres are the unit of account: a weight is "how many millimetres of
/// extra wire this defect is worth", which is what keeps the terms comparable
/// when the search trades one for another.
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    pub bend: f64,
    pub crossing: f64,
    pub through_body: f64,
    pub text_collision: f64,
    pub label: f64,
    pub net_spread: f64,
    pub body_overlap: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Weights {
            bend: 2.0,
            crossing: 14.0,
            through_body: 60.0,
            text_collision: 8.0,
            label: 20.0,
            net_spread: 0.30,
            body_overlap: 500.0,
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
    pub text_collisions: usize,
    pub labels: usize,
    /// Sum over nets of the half-perimeter of the box their pins span — the
    /// term that pulls a decoupling cap onto its IC's pin.
    pub net_spread: f64,
    pub body_overlaps: usize,
}

impl Metrics {
    /// The single number the search minimises.
    pub fn score(&self, w: &Weights) -> f64 {
        self.wire_length
            + w.bend * self.bends as f64
            + w.crossing * self.crossings as f64
            + w.through_body * self.through_bodies as f64
            + w.text_collision * self.text_collisions as f64
            + w.label * self.labels as f64
            + w.net_spread * self.net_spread
            + w.body_overlap * self.body_overlaps as f64
    }
}

fn segment_rect(a: Point2, b: Point2) -> Rect {
    Rect::from_points(a, b).inflate(0.01)
}

/// Perpendicular wire crossings that are not connections.
pub fn crossings(sheet: &Sheet) -> usize {
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
pub fn through_bodies(sheet: &Sheet) -> usize {
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
pub fn bends(sheet: &Sheet) -> usize {
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
pub fn text_collisions(sheet: &Sheet) -> usize {
    let mut boxes: Vec<Rect> = sheet.texts.iter().map(|t| t.rect).collect();
    let text_count = boxes.len();
    boxes.extend(sheet.part_bodies().map(|b| b.rect));
    let mut count = 0;
    for (i, j) in geom::candidate_pairs(&boxes) {
        if i < text_count && boxes[i].overlaps(&boxes[j]) {
            count += 1;
        }
    }
    for text in &sheet.texts {
        if sheet
            .wires
            .iter()
            .any(|w| Segment::new(w.a, w.b).axis_aligned_hits_rect_interior(&text.rect))
        {
            count += 1;
        }
    }
    count
}

/// Part bodies that overlap each other.
pub fn body_overlaps(sheet: &Sheet) -> usize {
    let boxes: Vec<Rect> = sheet.part_bodies().map(|b| b.rect).collect();
    geom::candidate_pairs(&boxes)
        .into_iter()
        .filter(|(i, j)| boxes[*i].overlaps(&boxes[*j]))
        .count()
}

/// Sum over nets of the half-perimeter of the box their pins span.
pub fn net_spread(sheet: &Sheet) -> f64 {
    let mut by_net: std::collections::HashMap<&str, Vec<Point2>> = std::collections::HashMap::new();
    for pin in &sheet.pins {
        if let Some(net) = sheet.net_at(pin.at)
            && !net.starts_with('#')
        {
            by_net.entry(net).or_default().push(pin.at);
        }
    }
    by_net
        .values()
        .filter(|pins| pins.len() > 1)
        // A rail with dozens of pins is never drawn compactly; charging its full
        // span would drown every local decision out.
        .filter(|pins| pins.len() <= 8)
        .filter_map(|pins| Rect::bounding(pins))
        .map(|r| r.half_perimeter())
        .sum()
}

/// Measure a sheet.
pub fn measure(sheet: &Sheet) -> Metrics {
    Metrics {
        wire_length: sheet.wires.iter().map(|w| w.length()).sum(),
        bends: bends(sheet),
        crossings: crossings(sheet),
        through_bodies: through_bodies(sheet),
        text_collisions: text_collisions(sheet),
        labels: sheet.label_names.len(),
        net_spread: net_spread(sheet),
        body_overlaps: body_overlaps(sheet),
    }
}
