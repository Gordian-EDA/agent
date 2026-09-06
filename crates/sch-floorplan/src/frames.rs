//! The outline a set of parts draws: their bodies and fields, the labels and stubs
//! within reach, their own rail glyphs — what [`crate::blocks`] frames a block by.


use geom::{GRID_50_MIL, Rect};
use sch_doc::{Item, SchDoc};

/// Air between the parts' ink and the outline.
const FRAME_PAD: f64 = 3.81;
/// How far a label's near edge may sit from a part's body and still be its own — a
/// stub's length and a little; anything further off belongs to another block.
const REACH: f64 = 5.08;
/// The line a rail glyph's name takes beyond its arrow.
const RAIL_NAME_LINE: f64 = 2.54;
/// How far beyond its parts a block's own rail glyph may stand — a bank's rail is a
/// lane out, its glyph a line beyond that.
const FURNITURE_REACH: f64 = 12.7;

/// The outline around a block's parts: their bodies and fields, the labels and stubs
/// within reach of them, padded and snapped to the grid.
pub(crate) fn block_frame(doc: &SchDoc, indices: &[usize], glyphs: &[usize]) -> Option<Rect> {
    let items = doc.items();
    let parts = union(indices.iter().filter_map(|i| doc.item_bbox(&items[*i])))?;
    // The block's own rail glyphs and flags sit a lane or two beyond its parts, and a
    // glyph's name is written a line beyond its arrow, outside the symbol's own box:
    // the outline has to hold them, or the rail runs under the frame line.
    let mut hull = union(std::iter::once(parts).chain(glyphs.iter().filter_map(|i| {
        let bbox = doc.item_bbox(&items[*i])?;
        (rect_gap(&parts, &bbox) <= FURNITURE_REACH).then(|| grow(bbox, RAIL_NAME_LINE))
    })))?;
    // Labels on stubs, markers, junctions — and the wires that reach from the parts to
    // the glyphs, a bank's rail among them. A wire is taken only when it starts within
    // reach of the parts: one that merely passes by belongs to a neighbour.
    let near: Vec<Rect> = items
        .iter()
        .filter(|item| {
            matches!(
                item,
                Item::Label(_) | Item::NoConnect(_) | Item::Junction(_) | Item::Wire(_)
            )
        })
        .filter_map(|item| {
            let b = doc.item_bbox(item)?;
            let reach = match item {
                Item::Wire(_) => 0.0,
                _ => REACH,
            };
            (rect_gap(&hull, &b) <= reach + geom::EPS && contains(&grow(hull, FURNITURE_REACH), &b))
                .then_some(b)
        })
        .collect();
    hull = union(std::iter::once(hull).chain(near))?;
    let padded = grow(hull, FRAME_PAD);
    Some(Rect::new(
        GRID_50_MIL.snap(padded.min_x),
        GRID_50_MIL.snap(padded.min_y),
        GRID_50_MIL.snap(padded.max_x),
        GRID_50_MIL.snap(padded.max_y),
    ))
}

fn union(rects: impl Iterator<Item = Rect>) -> Option<Rect> {
    rects.fold(None, |acc: Option<Rect>, r| {
        Some(match acc {
            None => r,
            Some(a) => Rect::new(
                a.min_x.min(r.min_x),
                a.min_y.min(r.min_y),
                a.max_x.max(r.max_x),
                a.max_y.max(r.max_y),
            ),
        })
    })
}

fn grow(r: Rect, by: f64) -> Rect {
    Rect::new(r.min_x - by, r.min_y - by, r.max_x + by, r.max_y + by)
}

fn contains(outer: &Rect, inner: &Rect) -> bool {
    inner.min_x >= outer.min_x
        && inner.min_y >= outer.min_y
        && inner.max_x <= outer.max_x
        && inner.max_y <= outer.max_y
}

fn rect_gap(a: &Rect, b: &Rect) -> f64 {
    let dx = (a.min_x - b.max_x).max(b.min_x - a.max_x).max(0.0);
    let dy = (a.min_y - b.max_y).max(b.min_y - a.max_y).max(0.0);
    dx.max(dy)
}
