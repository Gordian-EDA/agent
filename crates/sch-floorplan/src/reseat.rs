//! `reseat` — re-pack the blocks a sheet already carries.
//!
//! A sheet built one `place_parts` at a time is seated block by block: each call packs
//! its own block and then lands it beside whatever is already down
//! ([`crate::region::arrange`]). No call ever moves what came before, so the arrangement
//! is only ever as good as the order the blocks arrived in — four good blocks strewn
//! across the top of an A2 with 60% of the paper blank is what that looks like.
//!
//! This is the pass that reclaims it. Once a block is DRAWN its frame is known exactly,
//! where a seat could only work from an upper-bound claim, so the whole sheet is packed
//! again from the frames as drawn ([`sch_flex::pack_blocks`], the same packer a
//! whole-sheet typeset uses) and each block is moved rigidly onto its new seat.
//!
//! ## Why a rigid move is safe
//!
//! Blocks meet through net labels, so moving one whole block changes no connectivity —
//! but only if "one whole block" is the truth. What actually moves together is a
//! CONNECTED PIECE of the drawing: every item reachable from a block's symbols through a
//! shared point or a wire. Two blocks a seam stitch wired together are one such piece and
//! travel as one. Nothing is ever moved away from something it touches.
//!
//! The proof is still checked rather than argued: the extracted partition before and
//! after must be identical, and no symbol may land on another. A re-seat that fails
//! either — or that does not make the sheet smaller — is rolled back and the sheet keeps
//! the arrangement it had.

use std::collections::{BTreeMap, BTreeSet};

use geom::{GRID_50_MIL, Point2, Rect};
use sch_doc::{Item, SchDoc, connect};
use sch_model::text::TextKind;

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

/// What [`reseat`] did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Reseat {
    /// Pieces moved. Zero means the sheet was left exactly as it was.
    pub moved: usize,
    /// The hull the drawing occupied before and after, in mm².
    pub hull: [f64; 2],
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
struct Sets(Vec<usize>);

impl Sets {
    fn new(n: usize) -> Self {
        Sets((0..n).collect())
    }

    fn find(&mut self, i: usize) -> usize {
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

    // Frames and captions carry no connection, so they follow the drawing they name: a
    // rectangle the piece whose parts it encloses, a caption the piece of the nearest
    // frame — the pairing the realiser drew them with.
    let mut owner: BTreeMap<usize, usize> = BTreeMap::new();
    for &i in &joinable {
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
            .or_else(|| nearest(&frame, &parts, &mut sets))
        else {
            continue;
        };
        owner.insert(i, root);
        rects.push((i, frame, root));
    }
    for (i, item) in items.iter().enumerate() {
        let Item::Text(t) = item else { continue };
        let at = t.at.point();
        let near = rects
            .iter()
            .map(|(_, frame, root)| (gap(frame, at), *root))
            .filter(|(d, _)| *d <= CAPTION_REACH)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, root)| root)
            .or_else(|| nearest(&Rect::new(at.x, at.y, at.x, at.y), &parts, &mut sets));
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

/// Wire segments running through a net label that is not their own.
///
/// Packing blocks closer leaves the router less air, and the ink it then has least room
/// for is the label column between two blocks. This is that cost, counted so a re-seat
/// that buys its page with unreadable labels is refused.
fn label_hits(doc: &SchDoc) -> usize {
    let labels: Vec<sch_model::text::DrawnText> = sch_doc::drawn_texts(doc)
        .into_iter()
        .filter(|t| matches!(t.kind, TextKind::Label | TextKind::PortLabel))
        .collect();
    sch_doc::connect::scene(doc)
        .segments
        .into_iter()
        .map(|(a, b, net)| {
            let seg = geom::Segment::new(a, b);
            labels
                .iter()
                .filter(|t| t.text != net && seg.axis_aligned_hits_rect_interior(&t.bbox))
                .count()
        })
        .sum()
}

/// How big the sheet is, ranked the way a reader sees it: the paper first, and how much
/// of the drawing's own hull is air only within one paper size. A wide ribbon has the
/// smaller hull and buys the bigger sheet, so hull alone is the wrong objective.
fn sheet_size(doc: &SchDoc) -> (f64, f64) {
    let page = doc.page().map_or(f64::INFINITY, |p| p[0] * p[1]);
    let hull = doc
        .content_bbox()
        .map_or(f64::INFINITY, |r| r.width() * r.height());
    (page, hull)
}

/// Pack the sheet's pieces again from the frames they DREW, move each rigidly onto its
/// new seat, and size the paper to what is left.
///
/// Kept only if the sheet can prove it is better: a smaller page — or the same page with
/// less air — the same extracted partition, and no new symbol-on-symbol. Otherwise the
/// document is restored exactly as it was and this reports `moved: 0`. A re-seat is an
/// optimisation, never a risk to a drawing that is already correct.
pub fn reseat(doc: &mut SchDoc) -> Reseat {
    let Some(pieces) = pieces(doc) else {
        return Reseat::default();
    };
    let was = sheet_size(doc);
    let held = Reseat {
        moved: 0,
        hull: [was.1; 2],
    };
    if pieces.len() < 2 {
        return held;
    }
    let sizes: Vec<(f64, f64)> = pieces
        .iter()
        .map(|p| (p.frame.width(), p.frame.height()))
        .collect();
    let origins = sch_flex::pack_blocks(&sizes, &crate::write::usable_pages());
    let seats: Vec<Point2> = origins
        .iter()
        .zip(&pieces)
        .map(|(at, piece)| {
            Point2::new(
                GRID_50_MIL.snap(at.x - piece.frame.min_x),
                GRID_50_MIL.snap(at.y - piece.frame.min_y),
            )
        })
        .collect();
    if seats.iter().all(|d| d.x == 0.0 && d.y == 0.0) {
        return held;
    }

    let partition = connect::extract(doc).partition();
    let overlaps = crate::visual::body_overlaps(doc).len();
    let over_labels = label_hits(doc);
    let snapshot = doc.snapshot();
    let moved = pieces
        .iter()
        .zip(&seats)
        .filter(|(piece, delta)| {
            let move_it = delta.x != 0.0 || delta.y != 0.0;
            if move_it {
                doc.translate_items(&piece.uuids, delta.x, delta.y);
            }
            move_it
        })
        .count();
    doc.refit_page(&BTreeSet::new());
    let now = sheet_size(doc);
    let kept = now < was
        && connect::extract(doc).partition() == partition
        && crate::visual::body_overlaps(doc).len() <= overlaps
        && label_hits(doc) <= over_labels;
    if !kept {
        tracing::debug!(?was, ?now, "the re-seated sheet was no better; kept the seats");
        let _ = doc.restore(snapshot);
        return held;
    }
    Reseat {
        moved,
        hull: [was.1, now.1],
    }
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

    /// Re-seating shrinks the hull and leaves the netlist alone.
    #[test]
    fn a_reseat_shrinks_the_sheet_without_changing_the_netlist() {
        let mut doc = sheet();
        let before = connect::extract(&doc).partition();
        let out = reseat(&mut doc);
        assert!(out.moved > 0, "nothing moved: {out:?}");
        assert!(out.hull[1] < out.hull[0], "{out:?}");
        assert_eq!(connect::extract(&doc).partition(), before);
    }

    /// It settles: a sheet already packed is left exactly as it is.
    #[test]
    fn a_reseat_is_idempotent() {
        let mut doc = sheet();
        reseat(&mut doc);
        let once: Vec<geom::Point2> = doc.wires().flat_map(|w| w.points.clone()).collect();
        let again = reseat(&mut doc);
        assert_eq!(again.moved, 0, "{again:?}");
        let twice: Vec<geom::Point2> = doc.wires().flat_map(|w| w.points.clone()).collect();
        assert_eq!(once, twice);
    }
}
