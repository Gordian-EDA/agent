//! Blocks as the author draws them: a named set of parts with one outline and one
//! title, made and arranged in steps of their own after the parts are placed.
//!
//! [`create_block`] takes the parts, checks they stand alone — no drawn wire reaches a
//! part outside the set, and every member shares a net with another — tags them, and
//! draws the outline with the title inside its bottom edge. [`arrange_blocks`] takes
//! rows of block names and lays the blocks out as a grid from the page corner: rigid
//! moves of everything a block draws, every outline re-fitted to its cell, contents
//! centred, tops shared along a row. Whatever the rows do not name is set aside in a
//! row under the grid. Nothing inside a block changes: the tree it was typeset from is
//! gone, and what is drawn is what moves.

use std::collections::{BTreeMap, BTreeSet};

use geom::{GRID_50_MIL, PAGE_MARGIN, Point2, Rect};
use sch_doc::{Item, SchDoc, connect};
use sch_flex::pack::BLOCK_GAP;

use crate::frames::block_frame;
use crate::reseat::{Piece, pieces, wired};

/// The title's text size and the width one of its characters takes.
const TITLE_SIZE: f64 = 3.81;
const TITLE_EM: f64 = 3.4;
/// The band under the contents that the title is written in.
const CAPTION_BAND: f64 = TITLE_SIZE * 1.6;

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
    #[error("a block needs at least one part; power flags and rails are furniture, not parts")]
    NoParts,
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
/// Power furniture named among `refs` is ignored — a block's rails come with its parts.
/// Calling it again for a name the sheet already has redefines that block.
pub fn create_block(
    doc: &mut SchDoc,
    name: &str,
    refs: &[String],
    title: Option<&str>,
) -> Result<BlockReport, BlockError> {
    let members: BTreeSet<String> = refs.iter().filter(|r| !r.starts_with('#')).cloned().collect();
    if members.is_empty() {
        return Err(BlockError::NoParts);
    }
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

    // The outline: parts, their labels and stubs, their rail glyphs, padded, and no
    // narrower than the title, which takes a band inside the bottom edge.
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
    let title = title.unwrap_or(name);
    let slack = ((title.chars().count() as f64 * TITLE_EM + 2.54 - inner.width()) / 2.0).max(0.0);
    let frame = Rect::new(
        GRID_50_MIL.snap(inner.min_x - slack),
        GRID_50_MIL.snap(inner.min_y),
        GRID_50_MIL.snap(inner.max_x + slack),
        GRID_50_MIL.snap(inner.max_y + CAPTION_BAND),
    );
    drop_outline_of(doc, &symbols, name);
    doc.add_rectangle(
        Point2::new(frame.min_x, frame.min_y),
        Point2::new(frame.max_x, frame.max_y),
    );
    write_caption(doc, frame, title);
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

/// Every member shares a net with another member, through wires or labels: rails
/// count, so a block of two parts that meet only on GND is still one block.
fn connected(doc: &SchDoc, members: &BTreeSet<String>) -> Result<(), BlockError> {
    if members.len() < 2 {
        return Ok(());
    }
    let netlist = connect::extract(doc);
    let order: Vec<&String> = members.iter().collect();
    let index: BTreeMap<&str, usize> = order.iter().enumerate().map(|(i, r)| (r.as_str(), i)).collect();
    let mut parent: Vec<usize> = (0..order.len()).collect();
    fn find(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
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

/// The title centred on the outline's bottom edge, inside it.
fn write_caption(doc: &mut SchDoc, frame: Rect, title: &str) {
    let width = title.chars().count() as f64 * TITLE_EM;
    let x = GRID_50_MIL.snap((frame.min_x + frame.max_x) / 2.0 - width / 2.0).max(frame.min_x + 1.27);
    let y = GRID_50_MIL.snap(frame.max_y - 1.27);
    doc.add_text(title, Point2::new(x, y), TITLE_SIZE, true);
}

/// What [`arrange_blocks`] did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ArrangeBlocksReport {
    pub moved: Vec<String>,
    /// The parts the rows did not name, now in a row under the grid.
    pub set_aside: Vec<String>,
    pub page: Option<[f64; 2]>,
}

/// One named block as [`arrange_blocks`] moves it: the piece that is exactly it, its
/// outline, what it draws inside, and its title.
struct Cell {
    name: String,
    uuids: BTreeSet<String>,
    outline: (String, Rect),
    content: Rect,
    caption: Option<(String, Point2)>,
}

/// Lay the named blocks out as a grid from the page corner: `rows` top to bottom, each
/// row's blocks left to right at their own widths, a row as tall as its tallest block;
/// every outline is re-fitted to its cell with the contents centred. Whatever the rows
/// do not name — parts of no block, blocks left out — goes in a row under the grid.
pub fn arrange_blocks(doc: &mut SchDoc, rows: &[Vec<String>]) -> Result<ArrangeBlocksReport, BlockError> {
    let Some(all) = pieces(doc) else {
        return Err(BlockError::Unsupported);
    };
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
        if let Some(cell) = cell_of(doc, name, piece)? {
            cells.insert(name.clone(), cell);
        }
    }
    let heights: Vec<f64> = rows
        .iter()
        .map(|row| row.iter().filter_map(|n| cells.get(n)).map(|c| c.outline.1.height()).fold(0.0, f64::max))
        .collect();

    let partition = connect::extract(doc).partition();
    let overlaps = crate::visual::body_overlaps(doc).len();
    let snapshot = doc.snapshot();
    let mut moved = Vec::new();
    let mut y = PAGE_MARGIN;
    for (r, row) in rows.iter().enumerate() {
        let mut x = PAGE_MARGIN;
        for name in row {
            let Some(cell) = cells.get(name) else { continue };
            let target = Rect::new(x, y, x + cell.outline.1.width(), y + heights[r]);
            seat_cell(doc, cell, target)?;
            moved.push(cell.name.clone());
            x += target.width() + BLOCK_GAP;
        }
        if heights[r] > 0.0 {
            y += heights[r] + BLOCK_GAP;
        }
    }
    let set_aside = set_aside(doc, &all, &cells, y);
    let fit = doc.refit_page(&BTreeSet::new());
    let kept = connect::extract(doc).partition() == partition
        && crate::visual::body_overlaps(doc).len() <= overlaps;
    if !kept {
        let _ = doc.restore(snapshot);
        return Err(BlockError::Doc("moving the blocks would change the netlist or land a part on another; nothing moved".into()));
    }
    Ok(ArrangeBlocksReport {
        moved,
        set_aside,
        page: fit.map(|f| f.page),
    })
}

/// The block's outline, contents and title within its piece; `None` when it draws no
/// contents, which nothing can centre.
fn cell_of(doc: &SchDoc, name: &str, piece: &Piece) -> Result<Option<Cell>, BlockError> {
    let mut outline = None;
    let mut content: Option<Rect> = None;
    let mut caption = None;
    for item in doc.items() {
        let Some(uuid) = item.uuid() else { continue };
        if !piece.uuids.contains(uuid) {
            continue;
        }
        match item {
            Item::Rectangle(r) => outline = Some((r.uuid.clone(), Rect::from_points(r.start, r.end))),
            Item::Text(t) if t.size() >= TITLE_SIZE - 0.01 => caption = Some((t.uuid.clone(), t.at.point())),
            Item::Text(_) => {}
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
    Ok(content.map(|content| Cell {
        name: name.to_string(),
        uuids: piece.uuids.clone(),
        outline,
        content,
        caption,
    }))
}

/// Move the block into `target`: a rigid move of everything it draws so the contents
/// sit centred above the title band, the outline re-fitted to the cell, the title
/// re-centred on the cell's bottom edge.
fn seat_cell(doc: &mut SchDoc, cell: &Cell, target: Rect) -> Result<(), BlockError> {
    let body = Rect::new(target.min_x, target.min_y, target.max_x, target.max_y - CAPTION_BAND);
    let dx = GRID_50_MIL.snap((body.min_x + body.max_x) / 2.0 - (cell.content.min_x + cell.content.max_x) / 2.0);
    let dy = GRID_50_MIL.snap((body.min_y + body.max_y) / 2.0 - (cell.content.min_y + cell.content.max_y) / 2.0);
    let drawing: BTreeSet<String> = cell
        .uuids
        .iter()
        .filter(|u| **u != cell.outline.0 && cell.caption.as_ref().is_none_or(|(c, _)| c != *u))
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
    if let Some((uuid, at)) = &cell.caption {
        let text = doc
            .items()
            .iter()
            .find_map(|item| match item {
                Item::Text(t) if &t.uuid == uuid => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let width = text.chars().count() as f64 * TITLE_EM;
        let tx = GRID_50_MIL.snap((target.min_x + target.max_x) / 2.0 - width / 2.0).max(target.min_x + 1.27);
        let ty = GRID_50_MIL.snap(target.max_y - 1.27);
        let one: BTreeSet<String> = std::iter::once(uuid.clone()).collect();
        doc.translate_items(&one, tx - at.x, ty - at.y);
    }
    Ok(())
}

/// Every piece the grid did not seat, moved rigidly into a row under it, left to
/// right in the order it was drawn. Returns the parts moved.
fn set_aside(doc: &mut SchDoc, all: &[Piece], cells: &BTreeMap<String, Cell>, top: f64) -> Vec<String> {
    let mut parts = Vec::new();
    let mut x = PAGE_MARGIN;
    for piece in all.iter().filter(|p| !p.blocks.iter().any(|b| cells.contains_key(b))) {
        let dx = GRID_50_MIL.snap(x - piece.frame.min_x);
        let dy = GRID_50_MIL.snap(top - piece.frame.min_y);
        if dx != 0.0 || dy != 0.0 {
            doc.translate_items(&piece.uuids, dx, dy);
        }
        parts.extend(doc.items().iter().filter_map(|item| match item {
            Item::Symbol(s) if !s.refdes().starts_with('#') && item.uuid().is_some_and(|u| piece.uuids.contains(u)) => {
                Some(s.refdes().to_string())
            }
            _ => None,
        }));
        x += piece.frame.width() + BLOCK_GAP;
    }
    parts
}

fn union(a: &Rect, b: &Rect) -> Rect {
    Rect::new(
        a.min_x.min(b.min_x),
        a.min_y.min(b.min_y),
        a.max_x.max(b.max_x),
        a.max_y.max(b.max_y),
    )
}
