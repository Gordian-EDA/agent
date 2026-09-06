//! Block frames drawn from what the sheet holds, not from what one call placed.
//!
//! A block is built over several calls — the regulator first, a cap on the next
//! turn, a diode after the check — and the realiser only ever saw the parts of the
//! call in hand: a frame drawn around three parts was thrown away the moment a
//! fourth arrived alone, and an arrange redrew the parts without it. On the live
//! suite half the parts of a sheet sat outside any outline for that reason. This
//! pass reads the sheet as it stands: every block with parts enough for an outline
//! gets one rectangle around all of them, and its caption is seated on that frame.

use std::collections::{BTreeMap, BTreeSet};

use geom::{GRID_50_MIL, Point2, Rect};
use sch_doc::{Item, SchDoc};

/// How many parts a block needs before its outline is worth drawing; the realiser's
/// own threshold, kept in step with `write::caption`.
const FRAMED_MIN_PARTS: usize = 3;
/// Air between the parts' ink and the outline.
const FRAME_PAD: f64 = 3.81;
/// How far a label's near edge may sit from a part's body and still be its own — a
/// stub's length and a little; anything further off belongs to another block.
const REACH: f64 = 5.08;
/// A caption within this of a frame belongs to it.
const CAPTION_REACH: f64 = 12.7;
/// The line above the outline the title is written on.
const TITLE_BAND: f64 = 2.54;
/// The line a rail glyph's name takes beyond its arrow.
const RAIL_NAME_LINE: f64 = 2.54;
/// The caption's text size, and the width one of its characters takes.
const TITLE_SIZE: f64 = 2.54;
const TITLE_EM: f64 = 1.9;

/// Redraw every block's frame around the parts it has on the sheet. Returns the blocks
/// reframed.
pub fn reframe(doc: &mut SchDoc) -> Vec<String> {
    let mut members: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, item) in doc.items().iter().enumerate() {
        let Item::Symbol(s) = item else { continue };
        if s.refdes().starts_with('#') {
            continue;
        }
        let Some(block) = s.fields.get(sch_model::result::AP_BLOCK) else { continue };
        if sch_model::result::synthesized_block(&block.value) {
            continue;
        }
        members.entry(block.value.clone()).or_default().push(i);
    }
    let mut done = Vec::new();
    for (block, indices) in members {
        if indices.len() < FRAMED_MIN_PARTS {
            continue;
        }
        let Some(frame) = block_frame(doc, &indices) else { continue };
        replace_frame(doc, &block, frame);
        done.push(block);
    }
    done
}

/// The outline around a block's parts: their bodies and fields, the labels and stubs
/// within reach of them, padded and snapped to the grid.
fn block_frame(doc: &SchDoc, indices: &[usize]) -> Option<Rect> {
    let items = doc.items();
    // A rail glyph's name is written a line beyond its arrow, outside the symbol's
    // own box; the outline has to clear it or the caption lands on it.
    let mut hull = union(indices.iter().filter_map(|i| {
        let bbox = doc.item_bbox(&items[*i])?;
        Some(match &items[*i] {
            Item::Symbol(s) if s.lib_id.starts_with("power:") => grow(bbox, RAIL_NAME_LINE),
            _ => bbox,
        })
    }))?;
    let near = items
        .iter()
        .filter(|item| matches!(item, Item::Label(_) | Item::NoConnect(_) | Item::Junction(_)))
        .filter_map(|item| doc.item_bbox(item))
        .filter(|b| rect_gap(&hull, b) <= REACH);
    hull = union(std::iter::once(hull).chain(near))?;
    let padded = grow(hull, FRAME_PAD);
    Some(Rect::new(
        GRID_50_MIL.snap(padded.min_x),
        GRID_50_MIL.snap(padded.min_y - TITLE_BAND),
        GRID_50_MIL.snap(padded.max_x),
        GRID_50_MIL.snap(padded.max_y),
    ))
}

/// Drop the rectangles this block's parts sit in, draw `frame`, and seat the block's
/// title on its top-left corner.
fn replace_frame(doc: &mut SchDoc, block: &str, frame: Rect) {
    let core = grow(frame, -FRAME_PAD);
    let stale: Vec<String> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Rectangle(r) => {
                let rect = Rect::from_points(r.start, r.end);
                (rect.intersection(&core).is_some() || contains(&rect, &core)).then(|| r.uuid.clone())
            }
            _ => None,
        })
        .collect();
    let titles: Vec<(String, Point2)> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Text(t) => Some((t.uuid.clone(), t.at.point())),
            _ => None,
        })
        .collect();
    let stale_rects: Vec<Rect> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Rectangle(r) if stale.contains(&r.uuid) => Some(Rect::from_points(r.start, r.end)),
            _ => None,
        })
        .collect();
    // The caption nearest the frame being replaced — or, for a block that never had
    // one drawn, nearest the parts themselves — is this block's title.
    let anchors: Vec<Rect> = if stale_rects.is_empty() { vec![core] } else { stale_rects };
    let title = titles
        .iter()
        .filter_map(|(uuid, at)| {
            let gap = anchors.iter().map(|r| gap(r, *at)).fold(f64::MAX, f64::min);
            (gap <= CAPTION_REACH).then_some((gap, uuid.clone(), *at))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0));
    doc.retain_drawing(|item| match item {
        Item::Rectangle(r) => !stale.contains(&r.uuid),
        _ => true,
    });
    doc.add_rectangle(
        Point2::new(frame.min_x, frame.min_y),
        Point2::new(frame.max_x, frame.max_y),
    );
    if let Some((_, uuid, at)) = title {
        let text_len = doc
            .items()
            .iter()
            .find_map(|item| match item {
                Item::Text(t) if t.uuid == uuid => Some(t.text.chars().count()),
                _ => None,
            })
            .unwrap_or(8);
        let target = caption_seat(doc, &uuid, frame, text_len);
        let moved: BTreeSet<String> = std::iter::once(uuid).collect();
        doc.translate_items(&moved, target.x - at.x, target.y - at.y);
    }
    let _ = block;
}

/// Where the title goes: a line above the frame at its left corner, else the right,
/// else below — the first corner whose text box lands on nothing already drawn.
fn caption_seat(doc: &SchDoc, own: &str, frame: Rect, chars: usize) -> Point2 {
    let width = chars as f64 * TITLE_EM;
    let corners = [
        Point2::new(frame.min_x, frame.min_y - 1.27),
        Point2::new(frame.max_x - width, frame.min_y - 1.27),
        Point2::new(frame.min_x, frame.max_y + TITLE_SIZE + 1.27),
        Point2::new(frame.max_x - width, frame.max_y + TITLE_SIZE + 1.27),
    ];
    let ink: Vec<Rect> = doc
        .items()
        .iter()
        .filter(|item| match item {
            Item::Text(t) => t.uuid != own,
            Item::Rectangle(_) => false,
            _ => true,
        })
        .filter_map(|item| doc.item_bbox(item))
        .collect();
    corners
        .into_iter()
        .find(|at| {
            let text = Rect::new(at.x, at.y - TITLE_SIZE, at.x + width, at.y);
            !ink.iter().any(|b| b.intersection(&text).is_some())
        })
        .unwrap_or(corners[0])
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

fn gap(r: &Rect, p: Point2) -> f64 {
    let dx = (r.min_x - p.x).max(p.x - r.max_x).max(0.0);
    let dy = (r.min_y - p.y).max(p.y - r.max_y).max(0.0);
    dx.max(dy)
}
