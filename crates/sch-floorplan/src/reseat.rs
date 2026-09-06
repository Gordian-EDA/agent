//! The pieces of a sheet: what moves together when a block is moved.
//!
//! A piece is everything reachable from a block's symbols through a shared point or a
//! wire, joined with everything carrying the same block tag, plus the frame and caption
//! that name it — the unit [`crate::blocks::arrange_blocks`] moves rigidly.

use std::collections::{BTreeMap, BTreeSet};

use geom::{Point2, Rect};
use sch_doc::{Item, SchDoc};

/// How far a caption may sit from the frame it names, in mm — the same reach
/// [`crate::realize`] pairs one by.
const CAPTION_REACH: f64 = 12.7;

/// One piece of the drawing that moves as a unit: a block, or several blocks a wire
/// joins.
#[derive(Debug, Clone)]
pub struct Piece {
    /// The `ap_block` names its symbols carry, in sorted order. Empty for a piece of
    /// furniture — power flags and their wiring with no part among them.
    pub blocks: BTreeSet<String>,
    /// Every drawing item that moves with it.
    pub uuids: BTreeSet<String>,
    /// The rectangle it draws, text and frame included.
    pub frame: Rect,
}

/// Quantise to 1 µm, as the connectivity extractor does, so float dust never splits a
/// join.
fn key(p: Point2) -> (i64, i64) {
    ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64)
}

/// Whether `p` lies on the segment `a`-`b`, endpoints included.
fn on_segment(p: Point2, a: Point2, b: Point2) -> bool {
    const EPS: f64 = 0.001;
    let (ab, ap) = ((b.x - a.x, b.y - a.y), (p.x - a.x, p.y - a.y));
    let len = ab.0.hypot(ab.1);
    if len < EPS {
        return p.near_eq(a, EPS);
    }
    let along = (ap.0 * ab.0 + ap.1 * ab.1) / len;
    let across = (ap.0 * ab.1 - ap.1 * ab.0) / len;
    across.abs() <= EPS && along >= -EPS && along <= len + EPS
}

/// Disjoint sets over item indices.
pub(crate) struct Sets(Vec<usize>);

impl Sets {
    fn new(n: usize) -> Self {
        Sets((0..n).collect())
    }

    pub(crate) fn find(&mut self, i: usize) -> usize {
        let mut root = i;
        while self.0[root] != root {
            root = self.0[root];
        }
        let mut walk = i;
        while self.0[walk] != root {
            let next = self.0[walk];
            self.0[walk] = root;
            walk = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[b] = a;
        }
    }
}

/// The points at which item `i` can join another: a symbol's pins, a wire's corners, a
/// label's or a marker's anchor.
fn anchors(item: &Item, pins: &BTreeMap<String, Vec<Point2>>) -> Vec<Point2> {
    match item {
        Item::Symbol(s) => {
            let mut out = vec![s.at.point()];
            out.extend(pins.get(&s.uuid).into_iter().flatten().copied());
            out
        }
        Item::Wire(w) => w.points.clone(),
        Item::Junction(j) => vec![j.at],
        Item::NoConnect(n) => vec![n.at],
        Item::Label(l) => vec![l.at.point()],
        _ => Vec::new(),
    }
}

/// The pieces of `doc`, in the order their first item was drawn.
///
/// `None` when the sheet carries something this pass cannot account for — a hierarchical
/// sheet, an undecoded node with geometry — because a piece it cannot see is a piece it
/// would leave behind.
pub fn pieces(doc: &SchDoc) -> Option<Vec<Piece>> {
    let items = doc.items();
    let (joinable, mut sets) = wired(doc)?;
    // A block is rigid whether or not its parts are wired to each other: two halves of
    // one block joined only by a net label still have to travel together. A part the
    // payload put in no block of its own joins the sheet's default region, so an
    // undivided sheet is ONE piece and is never taken apart here. A rail glyph carries
    // no region at all and travels with whatever it is wired to.
    let mut first: BTreeMap<&str, usize> = BTreeMap::new();
    for &i in &joinable {
        let Item::Symbol(s) = &items[i] else { continue };
        if s.refdes().starts_with('#') {
            continue;
        }
        let block = s
            .fields
            .get(sch_model::result::AP_BLOCK)
            .map(|field| field.value.as_str())
            .unwrap_or(sch_model::result::DEFAULT_BLOCK);
        if let Some(j) = first.insert(block, i) {
            sets.union(i, j);
        }
    }
    pieces_of(doc, &joinable, &mut sets, &first)
}

/// What the drawing joins by touching: symbols, wires, junctions, markers and labels,
/// in disjoint sets over item indices, before any block is read. `None` when the
/// sheet carries something this cannot account for.
pub(crate) fn wired(doc: &SchDoc) -> Option<(Vec<usize>, Sets)> {
    let items = doc.items();
    if items.iter().any(|item| matches!(item, Item::Sheet(_))) {
        return None;
    }
    let mut pins: BTreeMap<String, Vec<Point2>> = BTreeMap::new();
    for pin in sch_doc::placed_pins(doc) {
        pins.entry(pin.owner).or_default().push(pin.at);
    }

    let joinable: Vec<usize> = (0..items.len())
        .filter(|i| {
            matches!(
                items[*i],
                Item::Symbol(_) | Item::Wire(_) | Item::Junction(_) | Item::NoConnect(_) | Item::Label(_)
            )
        })
        .collect();
    let mut sets = Sets::new(items.len());
    let mut at: BTreeMap<(i64, i64), usize> = BTreeMap::new();
    let mut wires: Vec<(usize, [Point2; 2])> = Vec::new();
    for &i in &joinable {
        if let Item::Wire(w) = &items[i] {
            for pair in w.points.windows(2) {
                wires.push((i, [pair[0], pair[1]]));
            }
        }
    }
    for &i in &joinable {
        for p in anchors(&items[i], &pins) {
            if let Some(j) = at.insert(key(p), i) {
                sets.union(i, j);
            }
            for (j, [a, b]) in &wires {
                if *j != i && on_segment(p, *a, *b) {
                    sets.union(i, *j);
                }
            }
        }
    }
    Some((joinable, sets))
}

/// The pieces, given the joined sets and each block's first symbol.
fn pieces_of(
    doc: &SchDoc,
    joinable: &[usize],
    sets: &mut Sets,
    first: &BTreeMap<&str, usize>,
) -> Option<Vec<Piece>> {
    let items = doc.items();
    // Frames and captions carry no connection, so they follow the drawing they name: a
    // rectangle the piece whose parts it encloses, a caption the piece of the nearest
    // frame — the pairing the realiser drew them with.
    let mut owner: BTreeMap<usize, usize> = BTreeMap::new();
    for &i in joinable {
        owner.insert(i, sets.find(i));
    }
    let parts: Vec<(usize, Point2)> = joinable
        .iter()
        .filter_map(|i| match &items[*i] {
            Item::Symbol(s) if !s.refdes().starts_with('#') => Some((*i, s.at.point())),
            _ => None,
        })
        .collect();
    let mut rects: Vec<(usize, Rect, usize)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Item::Rectangle(r) = item else { continue };
        let frame = Rect::from_points(r.start, r.end);
        let mut tally: BTreeMap<usize, usize> = BTreeMap::new();
        for (j, at) in &parts {
            if frame.contains(*at) {
                *tally.entry(sets.find(*j)).or_default() += 1;
            }
        }
        let Some(root) = tally
            .into_iter()
            .max_by_key(|(root, n)| (*n, std::cmp::Reverse(*root)))
            .map(|(root, _)| root)
            .or_else(|| nearest(&frame, &parts, sets))
        else {
            continue;
        };
        owner.insert(i, root);
        rects.push((i, frame, root));
    }
    for (i, item) in items.iter().enumerate() {
        let Item::Text(t) = item else { continue };
        let at = t.at.point();
        // A caption NAMES its block, so the name is what it travels by. Geometry is the
        // fallback for a title the author gave the block instead: taking the nearest
        // frame is what carried `mechanical` off under its neighbour's outline.
        let named = first.get(t.text.as_str()).map(|i| sets.find(*i));
        let near = named
            .or_else(|| {
                rects
                    .iter()
                    .map(|(_, frame, root)| (gap(frame, at), *root))
                    .filter(|(d, _)| *d <= CAPTION_REACH)
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .map(|(_, root)| root)
            })
            .or_else(|| nearest(&Rect::new(at.x, at.y, at.x, at.y), &parts, sets));
        if let Some(root) = near {
            owner.insert(i, root);
        }
    }

    let mut grouped: BTreeMap<usize, (usize, Piece)> = BTreeMap::new();
    for (i, root) in owner {
        let Some(uuid) = items[i].uuid() else { continue };
        let Some(bbox) = doc.item_bbox(&items[i]) else {
            continue;
        };
        let entry = grouped.entry(root).or_insert_with(|| {
            (
                i,
                Piece {
                    blocks: BTreeSet::new(),
                    uuids: BTreeSet::new(),
                    frame: bbox,
                },
            )
        });
        entry.0 = entry.0.min(i);
        entry.1.uuids.insert(uuid.to_string());
        entry.1.frame = union(&entry.1.frame, &bbox);
        if let Item::Symbol(s) = &items[i]
            && let Some(block) = s.fields.get(sch_model::result::AP_BLOCK)
        {
            entry.1.blocks.insert(block.value.clone());
        }
    }
    // In the order they were DRAWN, which is the order their blocks were called for,
    // which is the sheet's signal flow. The pack reads its input in order and keeps that
    // order whenever it fits, so this is what makes a re-seated sheet still read left to
    // right. Sorting the pieces by name instead makes the arrangement independent of the
    // call order and costs 15-30% of the hull on the block replay — the author's order
    // is information, not noise.
    let mut out: Vec<(usize, Piece)> = grouped.into_values().collect();
    out.sort_by_key(|(first, _)| *first);
    Some(out.into_iter().map(|(_, piece)| piece).collect())
}

/// The piece whose nearest part is closest to `r`.
fn nearest(r: &Rect, parts: &[(usize, Point2)], sets: &mut Sets) -> Option<usize> {
    let closest = parts
        .iter()
        .map(|(j, at)| (gap(r, *at), *j))
        .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))?;
    Some(sets.find(closest.1))
}

/// How far `at` lies outside `r`, in mm. Zero inside it.
fn gap(r: &Rect, at: Point2) -> f64 {
    let dx = (r.min_x - at.x).max(at.x - r.max_x).max(0.0);
    let dy = (r.min_y - at.y).max(at.y - r.max_y).max(0.0);
    dx.hypot(dy)
}

fn union(a: &Rect, b: &Rect) -> Rect {
    Rect::new(
        a.min_x.min(b.min_x),
        a.min_y.min(b.min_y),
        a.max_x.max(b.max_x),
        a.max_y.max(b.max_y),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::SchematicWriter;

    /// Two blocks drawn far apart, each a wire with a label — no symbols, so the pieces
    /// are furniture. The packer still pulls them together.
    fn sheet() -> SchDoc {
        let mut w = SchematicWriter::new();
        w.add_wire_on_net([25.4, 25.4], [50.8, 25.4], "A");
        w.add_wire_on_net([250.0, 200.0], [275.4, 200.0], "B");
        crate::realize::to_doc(w).unwrap()
    }

    /// The two runs are separate pieces: nothing joins them.
    #[test]
    fn a_sheet_splits_into_the_pieces_that_touch() {
        let doc = sheet();
        let pieces = pieces(&doc).expect("a flat sheet");
        assert_eq!(pieces.len(), 2, "{pieces:#?}");
    }
}
