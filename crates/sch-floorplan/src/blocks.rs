//! Blocks as the author draws them: a named set of parts with one outline and one
//! title, made and arranged in steps of their own after the parts are placed.
//!
//! [`create_block`] takes the parts, checks they stand alone — no drawn wire reaches a
//! part outside the set, and every member shares a net with another — tags them, and
//! draws the outline with the title inside its bottom edge. [`arrange_blocks`] takes
//! rows of block names and lays the blocks out as a grid: rigid moves of everything a
//! block draws, every outline re-fitted to its cell, contents centred, tops and lefts
//! shared. Nothing inside a block changes: the tree it was typeset from is gone, and
//! what is drawn is what moves.

use std::collections::{BTreeMap, BTreeSet};

use geom::{GRID_50_MIL, Point2, Rect};
use sch_doc::{Item, SchDoc, connect};
use sch_flex::pack::BLOCK_GAP;

use crate::frames::block_frame;
use crate::reseat::{pieces, wired};

/// The title's text size and the width one of its characters takes.
const TITLE_SIZE: f64 = 3.81;
const TITLE_EM: f64 = 2.8;
/// The note's text size and character width.
const NOTE_SIZE: f64 = 1.27;
const NOTE_EM: f64 = 1.0;
/// The band under the contents that the title, and a note above it, are written in.
fn caption_band(note: bool) -> f64 {
    TITLE_SIZE * 1.6 + if note { TITLE_SIZE } else { 0.0 }
}

/// What [`create_block`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockReport {
    pub name: String,
    pub parts: Vec<String>,
    pub frame: Rect,
}

/// Why a block could not be made.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BlockError {
    #[error("no part `{0}` on the sheet")]
    UnknownPart(String),
    #[error("a wire joins {inside} to {outside}, which is not in the block; blocks meet only through net labels")]
    WiredAcross { inside: String, outside: String },
    #[error("the parts do not connect to each other: {0}")]
    Split(String),
    #[error("no block named `{0}`")]
    UnknownBlock(String),
    #[error("blocks {0} are wired together and cannot be arranged apart")]
    Joined(String),
    #[error("the sheet holds a hierarchical sheet, which this cannot move")]
    Unsupported,
    #[error("{0}")]
    Doc(String),
}

/// Make `name` the block of `refs`: tag the parts, outline them, title the outline.
/// Calling it again for a name the sheet already has redefines that block.
pub fn create_block(
    doc: &mut SchDoc,
    name: &str,
    refs: &[String],
    title: Option<&str>,
    note: Option<&str>,
) -> Result<BlockReport, BlockError> {
    let members: BTreeSet<String> = refs.iter().cloned().collect();
    let symbols: Vec<usize> = doc
        .items()
        .iter()
        .enumerate()
        .filter_map(|(i, item)| match item {
            Item::Symbol(s) if members.contains(s.refdes()) => Some(i),
            _ => None,
        })
        .collect();
    for refdes in &members {
        if !symbols.iter().any(|i| matches!(&doc.items()[*i], Item::Symbol(s) if s.refdes() == refdes)) {
            return Err(BlockError::UnknownPart(refdes.clone()));
        }
    }
    standalone(doc, &members)?;
    connected(doc, &members)?;

    // The name is the tag every member carries; a member of an older block leaves it.
    for refdes in &members {
        doc.set_field(refdes, sch_model::result::AP_BLOCK, name)
            .map_err(|e| BlockError::Doc(e.to_string()))?;
    }
    let leavers: Vec<String> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Symbol(s)
                if !s.refdes().starts_with('#')
                    && !members.contains(s.refdes())
                    && s.fields.get(sch_model::result::AP_BLOCK).is_some_and(|f| f.value == name) =>
            {
                Some(s.refdes().to_string())
            }
            _ => None,
        })
        .collect();
    for refdes in &leavers {
        doc.set_field(refdes, sch_model::result::AP_BLOCK, sch_model::result::DEFAULT_BLOCK)
            .map_err(|e| BlockError::Doc(e.to_string()))?;
    }

    // The outline: parts, their labels and stubs, their rail glyphs, padded; the
    // title takes a band inside the bottom edge.
    let glyphs: Vec<usize> = doc
        .items()
        .iter()
        .enumerate()
        .filter_map(|(i, item)| match item {
            Item::Symbol(s) if s.refdes().starts_with('#') => Some(i),
            _ => None,
        })
        .collect();
    let Some(inner) = block_frame(doc, &symbols, &glyphs) else {
        return Err(BlockError::Doc("the parts draw nothing".into()));
    };
    let band = caption_band(note.is_some_and(|n| !n.is_empty()));
    let frame = Rect::new(
        GRID_50_MIL.snap(inner.min_x),
        GRID_50_MIL.snap(inner.min_y),
        GRID_50_MIL.snap(inner.max_x),
        GRID_50_MIL.snap(inner.max_y + band),
    );
    drop_outline_of(doc, &symbols, name);
    doc.add_rectangle(
        Point2::new(frame.min_x, frame.min_y),
        Point2::new(frame.max_x, frame.max_y),
    );
    write_caption(doc, frame, title.unwrap_or(name), note);
    Ok(BlockReport {
        name: name.to_string(),
        parts: members.into_iter().collect(),
        frame,
    })
}

/// A drawn wire from a member's pin must end on a member's pin: a wire that reaches
/// out of the set makes the set no block.
fn standalone(doc: &SchDoc, members: &BTreeSet<String>) -> Result<(), BlockError> {
    let Some((joinable, mut sets)) = wired(doc) else {
        return Err(BlockError::Unsupported);
    };
    let items = doc.items();
    let mut root_of_member: BTreeMap<usize, String> = BTreeMap::new();
    for &i in &joinable {
        let Item::Symbol(s) = &items[i] else { continue };
        if members.contains(s.refdes()) {
            root_of_member.insert(sets.find(i), s.refdes().to_string());
        }
    }
    for &i in &joinable {
        let Item::Symbol(s) = &items[i] else { continue };
        if s.refdes().starts_with('#') || members.contains(s.refdes()) {
            continue;
        }
        if let Some(inside) = root_of_member.get(&sets.find(i)) {
            return Err(BlockError::WiredAcross {
                inside: inside.clone(),
                outside: s.refdes().to_string(),
            });
        }
    }
    Ok(())
}

/// Every member shares a net — any net, rails included — with another member, so the
/// set reads as one circuit and not two.
fn connected(doc: &SchDoc, members: &BTreeSet<String>) -> Result<(), BlockError> {
    if members.len() < 2 {
        return Ok(());
    }
    let netlist = connect::extract(doc);
    let order: Vec<&String> = members.iter().collect();
    let index: BTreeMap<&str, usize> = order.iter().enumerate().map(|(i, r)| (r.as_str(), i)).collect();
    let mut parent: Vec<usize> = (0..order.len()).collect();
    fn find(p: &mut Vec<usize>, i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            return find(p, p[i]);
        }
        i
    }
    for net in &netlist.nets {
        let mut on: Vec<usize> = net
            .pins
            .iter()
            .filter_map(|pin| index.get(pin.refdes.as_str()).copied())
            .collect();
        on.dedup();
        for w in on.windows(2) {
            let (a, b) = (find(&mut parent, w[0]), find(&mut parent, w[1]));
            if a != b {
                parent[b] = a;
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    for i in 0..order.len() {
        groups.entry(find(&mut parent, i)).or_default().push(order[i]);
    }
    if groups.len() > 1 {
        let shown: Vec<String> = groups.values().map(|g| g.join("+")).collect();
        return Err(BlockError::Split(shown.join(" | ")));
    }
    Ok(())
}

/// Drop the outline and caption this block had, by whatever name: a rectangle that
/// holds a member and no non-member, and the texts within a line of it.
fn drop_outline_of(doc: &mut SchDoc, symbols: &[usize], name: &str) {
    let inside: Vec<Point2> = symbols
        .iter()
        .filter_map(|i| match &doc.items()[*i] {
            Item::Symbol(s) => Some(s.at.point()),
            _ => None,
        })
        .collect();
    let others: Vec<Point2> = doc
        .items()
        .iter()
        .enumerate()
        .filter_map(|(i, item)| match item {
            Item::Symbol(s) if !s.refdes().starts_with('#') && !symbols.contains(&i) => {
                Some(s.at.point())
            }
            _ => None,
        })
        .collect();
    let stale: Vec<Rect> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Rectangle(r) => Some(Rect::from_points(r.start, r.end)),
            _ => None,
        })
        .filter(|r| inside.iter().any(|p| r.contains(*p)) && !others.iter().any(|p| r.contains(*p)))
        .collect();
    // A caption of the outline being replaced sits inside it, or a line above its top
    // edge where the placer wrote bare captions. A caption inside an outline that
    // stays is that block's, however the two outlines overlap before the blocks are
    // arranged.
    let live: Vec<Rect> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Rectangle(r) => Some(Rect::from_points(r.start, r.end)),
            _ => None,
        })
        .filter(|r| !stale.contains(r))
        .collect();
    let drop_text = |t: &sch_doc::Text| {
        let at = t.at.point();
        if live.iter().any(|r| r.contains(at)) {
            return t.text == name;
        }
        t.text == name
            || stale.iter().any(|r| {
                let reach = Rect::new(r.min_x, r.min_y - 3.0, r.max_x, r.max_y);
                reach.contains(at)
            })
    };
    doc.retain_drawing(|item| match item {
        Item::Rectangle(r) => !stale.contains(&Rect::from_points(r.start, r.end)),
        Item::Text(t) => !drop_text(t),
        _ => true,
    });
}

/// The title centred on the outline's bottom edge, inside it; the note, if any, a
/// small line above the title.
fn write_caption(doc: &mut SchDoc, frame: Rect, title: &str, note: Option<&str>) {
    let width = title.chars().count() as f64 * TITLE_EM;
    let x = GRID_50_MIL.snap((frame.min_x + frame.max_x) / 2.0 - width / 2.0).max(frame.min_x + 1.27);
    let y = GRID_50_MIL.snap(frame.max_y - 1.27);
    doc.add_text(title, Point2::new(x, y), TITLE_SIZE, true);
    if let Some(note) = note.filter(|n| !n.is_empty()) {
        let nw = note.chars().count() as f64 * NOTE_EM;
        let nx = GRID_50_MIL.snap((frame.min_x + frame.max_x) / 2.0 - nw / 2.0).max(frame.min_x + 1.27);
        let ny = GRID_50_MIL.snap(y - TITLE_SIZE * 2.0);
        doc.add_text(note, Point2::new(nx, ny), NOTE_SIZE, false);
    }
}

/// What [`arrange_blocks`] did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ArrangeBlocksReport {
    pub moved: Vec<String>,
    pub page: Option<[f64; 2]>,
}

/// Lay the named blocks out as a grid: `rows` top to bottom, each row's blocks left to
/// right. A column is as wide as its widest block, a row as tall as its tallest; every
/// outline is re-fitted to its cell with the contents centred. Blocks the rows do not
/// name stay where they are.
pub fn arrange_blocks(doc: &mut SchDoc, rows: &[Vec<String>]) -> Result<ArrangeBlocksReport, BlockError> {
    let Some(all) = pieces(doc) else {
        return Err(BlockError::Unsupported);
    };
    // Each named block: the piece that is exactly it, its outline, and its caption.
    struct Cell {
        name: String,
        uuids: BTreeSet<String>,
        outline: (String, Rect),
        content: Rect,
        texts: Vec<(String, Point2, bool)>,
    }
    let mut cells: BTreeMap<String, Cell> = BTreeMap::new();
    for name in rows.iter().flatten() {
        if cells.contains_key(name) {
            continue;
        }
        let piece = all
            .iter()
            .find(|p| p.blocks.contains(name))
            .ok_or_else(|| BlockError::UnknownBlock(name.clone()))?;
        if piece.blocks.len() > 1 {
            return Err(BlockError::Joined(piece.blocks.iter().cloned().collect::<Vec<_>>().join(", ")));
        }
        let mut outline = None;
        let mut content: Option<Rect> = None;
        let mut texts = Vec::new();
        for item in doc.items() {
            let Some(uuid) = item.uuid() else { continue };
            if !piece.uuids.contains(uuid) {
                continue;
            }
            match item {
                Item::Rectangle(r) => outline = Some((r.uuid.clone(), Rect::from_points(r.start, r.end))),
                Item::Text(t) => texts.push((t.uuid.clone(), t.at.point(), t.size() >= TITLE_SIZE - 0.01)),
                _ => {
                    if let Some(b) = doc.item_bbox(item) {
                        content = Some(content.map_or(b, |c| union(&c, &b)));
                    }
                }
            }
        }
        let Some(outline) = outline else {
            return Err(BlockError::UnknownBlock(format!("{name} (it has no outline; create it first)")));
        };
        let Some(content) = content else { continue };
        cells.insert(name.clone(), Cell { name: name.clone(), uuids: piece.uuids.clone(), outline, content, texts });
    }

    // Cell sizes: a column's width is its widest outline, a row's height its tallest.
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0.0f64; columns];
    let mut heights = vec![0.0f64; rows.len()];
    for (r, row) in rows.iter().enumerate() {
        for (c, name) in row.iter().enumerate() {
            if let Some(cell) = cells.get(name) {
                widths[c] = widths[c].max(cell.outline.1.width());
                heights[r] = heights[r].max(cell.outline.1.height());
            }
        }
    }
    let partition = connect::extract(doc).partition();
    let overlaps = crate::visual::body_overlaps(doc).len();
    let snapshot = doc.snapshot();
    let mut moved = Vec::new();
    let mut y = geom::PAGE_MARGIN;
    for (r, row) in rows.iter().enumerate() {
        let mut x = geom::PAGE_MARGIN;
        for (c, name) in row.iter().enumerate() {
            let Some(cell) = cells.get(name) else { continue };
            let target = Rect::new(x, y, x + widths[c], y + heights[r]);
            // Rigid move of everything the block draws, so its contents sit centred in
            // the cell; then the outline takes the whole cell and the caption its edge.
            let band = caption_band(cell.texts.len() > 1);
            let body = Rect::new(target.min_x, target.min_y, target.max_x, target.max_y - band);
            let dx = GRID_50_MIL.snap((body.min_x + body.max_x) / 2.0 - (cell.content.min_x + cell.content.max_x) / 2.0);
            let dy = GRID_50_MIL.snap((body.min_y + body.max_y) / 2.0 - (cell.content.min_y + cell.content.max_y) / 2.0);
            let drawing: BTreeSet<String> = cell
                .uuids
                .iter()
                .filter(|u| **u != cell.outline.0 && !cell.texts.iter().any(|(t, _, _)| t == *u))
                .cloned()
                .collect();
            if dx != 0.0 || dy != 0.0 {
                doc.translate_items(&drawing, dx, dy);
            }
            doc.set_rectangle(
                &cell.outline.0,
                Point2::new(target.min_x, target.min_y),
                Point2::new(target.max_x, target.max_y),
            )
            .map_err(|e| BlockError::Doc(e.to_string()))?;
            for (uuid, at, is_title) in &cell.texts {
                let text = doc
                    .items()
                    .iter()
                    .find_map(|item| match item {
                        Item::Text(t) if &t.uuid == uuid => Some(t.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let (size, em) = if *is_title { (TITLE_SIZE, TITLE_EM) } else { (NOTE_SIZE, NOTE_EM) };
                let width = text.chars().count() as f64 * em;
                let tx = GRID_50_MIL.snap((target.min_x + target.max_x) / 2.0 - width / 2.0).max(target.min_x + 1.27);
                let ty = match is_title {
                    true => GRID_50_MIL.snap(target.max_y - 1.27),
                    false => GRID_50_MIL.snap(target.max_y - 1.27 - TITLE_SIZE * 2.0),
                };
                let one: BTreeSet<String> = std::iter::once(uuid.clone()).collect();
                doc.translate_items(&one, tx - at.x, ty - at.y);
                let _ = size;
            }
            moved.push(cell.name.clone());
            x += widths[c] + BLOCK_GAP;
        }
        y += heights[r] + BLOCK_GAP;
    }
    let fit = doc.refit_page(&BTreeSet::new());
    let kept = connect::extract(doc).partition() == partition
        && crate::visual::body_overlaps(doc).len() <= overlaps;
    if !kept {
        let _ = doc.restore(snapshot);
        return Err(BlockError::Doc("moving the blocks would change the netlist or land a part on another; nothing moved".into()));
    }
    Ok(ArrangeBlocksReport {
        moved,
        page: fit.map(|f| f.page),
    })
}

fn union(a: &Rect, b: &Rect) -> Rect {
    Rect::new(
        a.min_x.min(b.min_x),
        a.min_y.min(b.min_y),
        a.max_x.max(b.max_x),
        a.max_y.max(b.max_y),
    )
}

