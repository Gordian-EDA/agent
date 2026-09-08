//! Put the reference designators somewhere a human can read them.
//!
//! KiCad drops every footprint's reference at the library's own offset, which on a placed board
//! lands on pads, on traces, on the board edge and on the neighbouring part's label. This walks a
//! ring of candidate spots around each part and takes the first that collides with nothing,
//! biggest part first so the crowded small passives get what is left rather than the other way
//! round. Text is always laid horizontal: a board whose labels point four ways reads as noise.

use crate::geom::{rotate, BBox, Point};
use crate::model::Board;
use crate::sexp::Node;

/// Width of one character relative to the font size, for KiCad's stroke font.
const CHAR_ASPECT: f64 = 0.75;
/// Breathing room around a label, so two of them never abut.
const TEXT_MARGIN: f64 = 0.15;
/// How far out from the courtyard the ring of candidates starts, and how far it steps.
const RING_START: f64 = 0.35;
const RING_STEP: f64 = 0.6;
const RING_TRIES: usize = 6;

/// The size KiCad gives reference text when the footprint states none.
const DEFAULT_TEXT_MM: f64 = 1.0;

fn text_size(node: &crate::sexp::SList) -> f64 {
    node.find("effects")
        .and_then(|e| e.find("font"))
        .and_then(|f| f.find("size"))
        .and_then(|s| s.arg_f64(1))
        .unwrap_or(DEFAULT_TEXT_MM)
}

fn label_box(centre: Point, text: &str, size: f64) -> BBox {
    let w = CHAR_ASPECT * size * text.chars().count().max(1) as f64;
    BBox::new(
        centre.0 - w / 2.0 - TEXT_MARGIN,
        centre.1 - size / 2.0 - TEXT_MARGIN,
        centre.0 + w / 2.0 + TEXT_MARGIN,
        centre.1 + size / 2.0 + TEXT_MARGIN,
    )
}

/// Board-frame spots to try for a label, nearest the part first: the four sides of its courtyard
/// at a widening offset, then the diagonals.
fn candidates(courtyard: &BBox, half_w: f64, half_h: f64) -> Vec<Point> {
    let (cx, cy) = courtyard.center();
    let mut out = Vec::new();
    for k in 0..RING_TRIES {
        let d = RING_START + k as f64 * RING_STEP;
        let above = courtyard.y0 - d - half_h;
        let below = courtyard.y1 + d + half_h;
        let left = courtyard.x0 - d - half_w;
        let right = courtyard.x1 + d + half_w;
        out.extend([
            (cx, above),
            (cx, below),
            (left, cy),
            (right, cy),
            (left, above),
            (right, above),
            (left, below),
            (right, below),
        ]);
    }
    out
}

/// Move every reference designator off the copper, off the board edge and off its neighbours.
///
/// Returns how many labels were moved. Values and other fields are left alone — they are hidden
/// on a stock footprint, and a board that shows them is stating something deliberate.
pub fn tidy_refs(board: &mut Board) -> usize {
    let fps = board.footprints();
    let Some(outline) = board.outline_bbox() else {
        return 0;
    };
    // everything a label must not sit on: pad copper, and every courtyard but the part's own
    let pads: Vec<BBox> = fps
        .iter()
        .flat_map(|f| f.pads.iter().map(|p| p.bbox()))
        .collect();
    let courtyards: Vec<(String, BBox)> = fps
        .iter()
        .map(|f| (f.ref_.clone(), f.courtyard_bbox()))
        .collect();

    // biggest part first: a big part has room on every side, a 0805 has one gap and needs it
    let mut order: Vec<usize> = (0..fps.len()).collect();
    order.sort_by(|&a, &b| {
        let area = |i: usize| {
            let c = fps[i].courtyard_bbox();
            if c.valid() { c.w() * c.h() } else { 0.0 }
        };
        area(b)
            .partial_cmp(&area(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(fps[a].ref_.cmp(&fps[b].ref_))
    });

    let mut placed: Vec<BBox> = Vec::new();
    let mut moves: Vec<(usize, Point, f64)> = Vec::new();
    for i in order {
        let f = &fps[i];
        if f.ref_.is_empty() || f.is_dnp() {
            continue;
        }
        let node = board.tree.items[f.index].as_list().expect("footprint node");
        let Some(prop) = node
            .lists(Some("property"))
            .into_iter()
            .find(|p| p.arg_text(0) == Some("Reference"))
        else {
            continue;
        };
        if prop.find("hide").is_some() {
            continue;
        }
        let size = text_size(prop);
        let court = f.courtyard_bbox();
        if !court.valid() {
            continue;
        }
        let probe = label_box((0.0, 0.0), &f.ref_, size);
        let (half_w, half_h) = (probe.w() / 2.0, probe.h() / 2.0);
        let ring = candidates(&court, half_w, half_h);
        let fits = |c: Point, strict: bool| {
            let bb = label_box(c, &f.ref_, size);
            bb.x0 >= outline.x0
                && bb.y0 >= outline.y0
                && bb.x1 <= outline.x1
                && bb.y1 <= outline.y1
                // never on copper: that is the one thing a fab check will not forgive
                && !pads.iter().any(|p| bb.overlaps(p))
                && !placed.iter().any(|p| bb.overlaps(p))
                && (!strict
                    || !courtyards
                        .iter()
                        .any(|(r, c)| *r != f.ref_ && c.valid() && bb.overlaps(c)))
        };
        // A crowded board runs out of clear pockets. Standing over a neighbour's courtyard is
        // untidy; standing over its pads is a DRC warning, so the fallback gives up the first.
        let spot = ring
            .iter()
            .copied()
            .find(|&c| fits(c, true))
            .or_else(|| ring.iter().copied().find(|&c| fits(c, false)));
        let Some(spot) = spot else { continue };
        placed.push(label_box(spot, &f.ref_, size));
        // the property's `(at ..)` is local to the footprint, and its angle is absolute
        let local = rotate((spot.0 - f.pos.0, spot.1 - f.pos.1), -f.rot);
        moves.push((f.index, local, 0.0));
    }

    let mut moved = 0usize;
    for (index, local, angle) in moves {
        let node = match &mut board.tree.items[index] {
            Node::List(l) => l,
            _ => continue,
        };
        for child in node.items.iter_mut() {
            let Node::List(p) = child else { continue };
            if p.is("property") && p.arg_text(0) == Some("Reference") {
                p.set(
                    "at",
                    vec![Node::num(local.0), Node::num(local.1), Node::num(angle)],
                );
                // a label buried under the part it names is no better than one on a pad
                if let Some(layer) = p.find_mut("layer") {
                    let name = layer.arg_text(0).unwrap_or("").to_string();
                    if name.ends_with(".Fab") {
                        let side = if name.starts_with("B.") { "B" } else { "F" };
                        layer.set_args(vec![Node::str(format!("{side}.SilkS"))]);
                    }
                }
                moved += 1;
            }
        }
    }
    moved
}
