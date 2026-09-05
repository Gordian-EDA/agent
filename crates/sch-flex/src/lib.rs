//! `sch-flex` — the schematic TYPESETTER: a block is drawn from the arrangement its
//! author composed, not from a search.
//!
//! The model states each block as a [`sch_model::tree::Tree`] — nested rows and columns
//! of parts, the way a person describes a schematic out loud ("the divider on the left of
//! the op-amp, the feedback cap above it"). This crate does the part the model cannot: it
//! measures the symbols, applies the drawing conventions (which way a passive lies, which
//! pin faces its neighbour, where a column beside an IC sits), and computes every
//! coordinate.
//!
//! ```text
//! Tree + Item geometry ──► measure ──► align ──► place ──► pack the blocks
//! ```
//!
//! [`typeset`] writes poses into the items and returns nothing else: it never routes,
//! labels or emits. The composition root does that from the poses, as it always has.
//!
//! Two things it deliberately does not know. It measures each pin's label and power-symbol
//! room from the netlist ONCE, where the reference implementation this follows re-measures
//! after routing — our routing happens downstream, so the reservation is an estimate.
//! And a part's body is the box its pins bound (the convention the whole placement stack
//! shares), not the symbol's drawn outline.

mod measure;
mod orient;
pub mod pack;
mod part;

use std::collections::{BTreeMap, BTreeSet};

use geom::{Point2, Rect};
use sch_model::item::Item;
use sch_model::tree::{Align, Axis, Container, Tree, Trees};

use measure::typeset_block;
use pack::{BLOCK_GAP, FRAME_PAD, corner_pack};
use part::Part;
/// The width-to-height ratio a graft aims for when no page is named — a landscape page's
/// usable area.
pub(crate) const SHEET_ASPECT: f64 = 1.5;
/// Usable width (mm) of the page a graft starts on; a pack wider than this grows the
/// paper, which reads worse than a taller sheet.
const PAGE_WIDTH: f64 = 260.0;
/// Room (mm) held back on each axis of a page's usable box for the drawing this crate does
/// not measure: the power rails that run in bands above and below the blocks, and the
/// cross-block labels that sit outside their frames. Packing to the last millimetre buys
/// the next page up as soon as the router draws. One rail band and its riser on each side.
const ROUTING_ROOM: f64 = 12.7;

/// What the typesetter had to decide for itself, because its author did not.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Blocks whose author composed no tree; each was drawn as one bare row.
    pub untreed: Vec<String>,
    /// Block → the parts its tree left out, drawn as a bare row underneath it.
    pub uncomposed: BTreeMap<String, Vec<String>>,
}

impl Report {
    /// What the author needs to hear: a bare row is not a composition, and the
    /// typesetter drawing one is not the same as the author having chosen it.
    pub fn warnings(&self) -> Vec<String> {
        let untreed = self.untreed.iter().map(|block| {
            format!(
                "`layout` has no tree for block `{block}`, so it was drawn as one bare row: \
                 compose it as rows and cols to get a readable block"
            )
        });
        let uncomposed = self.uncomposed.iter().map(|(block, parts)| {
            format!(
                "`layout.{block}` leaves out {}, drawn as a bare row under the rest: \
                 give every part of a block a place in its tree",
                parts.join(", ")
            )
        });
        untreed.chain(uncomposed).collect()
    }
}

/// Lay out every non-preseeded item from its block's tree, writing `at`, `angle` and
/// `mirror` back into `items`. Items a caller already posed are left exactly where they
/// are, and nothing is drawn on top of them — the caller's region adapter owns that.
///
/// `pages` are the usable boxes of the pages this drawing could be printed on, smallest
/// first, with margins and any title-block band already taken off. Empty means the caller
/// owns the page — a graft beside existing content — and the blocks are then packed for a
/// page's proportions without aiming at one.
pub fn typeset(items: &mut [Item], trees: &Trees, pages: &[[f64; 2]]) -> Report {
    let movable: Vec<usize> = (0..items.len()).filter(|i| !items[*i].preseeded).collect();
    let mut report = Report::default();
    let blocks = compose(items, &movable, trees, &mut report);
    let drawings: Vec<(Vec<measure::Placed>, Rect)> = blocks
        .iter()
        .map(|(block, tree, members)| {
            let labelled = label_pins(items, block, members);
            let mine: Vec<Part> = members
                .iter()
                .map(|i| Part::new(*i, &items[*i], &labelled))
                .collect();
            let index = |refdes: &str, unit: u8| {
                mine.iter()
                    .position(|p| p.item.refdes == refdes && p.item.unit == unit)
            };
            let placed = typeset_block(tree, &mine, &index);
            let bbox = drawing_bbox(&placed, &mine);
            let global = placed
                .into_iter()
                .map(|p| measure::Placed {
                    part: mine[p.part].index,
                    ..p
                })
                .collect();
            (global, bbox)
        })
        .collect();
    let sizes: Vec<(f64, f64)> = drawings
        .iter()
        .map(|(_, b)| (b.width() + 2.0 * FRAME_PAD, b.height() + 2.0 * FRAME_PAD))
        .collect();
    for (origin, (placed, bbox)) in pack_blocks(&sizes, pages).into_iter().zip(&drawings) {
        let shift = block_shift(origin, *bbox);
        for p in placed {
            let item = &mut items[p.part];
            item.at = Point2::new(shift.x + p.at.x, shift.y + p.at.y);
            item.angle = p.pose.angle;
            item.mirror = p.pose.mirror;
        }
    }
    report
}

/// The pins that will carry a net label: one per net of `block` that continues elsewhere
/// (or has no second pin at all). The writer draws one label per net per block, so
/// reserving room on every pin of the net is what makes a block sprawl.
fn label_pins(
    items: &[Item],
    block: &str,
    members: &[usize],
) -> BTreeSet<(usize, String)> {
    let leaving = leaving(items, block);
    let mut taken: BTreeSet<&str> = BTreeSet::new();
    let mut out = BTreeSet::new();
    for i in members {
        for (number, _, net) in &items[*i].pins {
            let Some(net) = net.as_deref() else { continue };
            if leaving.contains(net) && taken.insert(net) {
                out.insert((*i, number.clone()));
            }
        }
    }
    out
}

/// Nets of `block` that carry a pin somewhere else — they will need a label here.
fn leaving(items: &[Item], block: &str) -> BTreeSet<String> {
    let mut here: BTreeSet<&str> = BTreeSet::new();
    let mut elsewhere: BTreeSet<&str> = BTreeSet::new();
    let mut pins: BTreeMap<&str, usize> = BTreeMap::new();
    for item in items {
        for net in item.pins.iter().filter_map(|(_, _, net)| net.as_deref()) {
            *pins.entry(net).or_default() += 1;
            if item.block == block {
                here.insert(net);
            } else {
                elsewhere.insert(net);
            }
        }
    }
    here.into_iter()
        .filter(|net| elsewhere.contains(net) || pins.get(net).copied().unwrap_or(0) < 2)
        .map(str::to_owned)
        .collect()
}

/// The blocks to draw, each with the tree it is drawn from and the items it owns.
///
/// A block whose author left it out of `trees` gets one row in payload order, and says so:
/// composing the arrangement is the model's job, and a bare row reads like one.
fn compose(
    items: &[Item],
    movable: &[usize],
    trees: &Trees,
    report: &mut Report,
) -> Vec<(String, Tree, Vec<usize>)> {
    let mut order: Vec<String> = Vec::new();
    let mut members: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for i in movable {
        let block = items[*i].block.clone();
        if !members.contains_key(&block) {
            order.push(block.clone());
        }
        members.entry(block).or_default().push(*i);
    }
    order
        .into_iter()
        .map(|block| {
            let mine = members.remove(&block).unwrap_or_default();
            let units = || {
                mine.iter()
                    .map(|i| (items[*i].refdes.clone(), items[*i].unit))
                    .collect::<Vec<_>>()
            };
            let tree = match trees.get(&block) {
                Some(tree) => complete(tree, items, &mine, &block, report),
                None => {
                    report.untreed.push(block.clone());
                    Tree::row_of(units())
                }
            };
            (block, tree, mine)
        })
        .collect()
}

/// The authored tree plus everything it left out, so every part is drawn.
///
/// A part left out falls into two kinds, and they are not the same omission. A SYNTHESIZED
/// part — a `decouple` cap the sugar expanded — was never the author's to name: it is
/// seated beside the part it supports, which is where a human draws it, and nobody is told
/// off for it. Anything else is the author's own gap: it goes in a trailing row with a note
/// saying so, because a bare row is not a composition.
///
/// This only ever INSERTS. An authored leaf never changes place or order, so a tree that
/// already reads as a signal path still does.
fn complete(
    tree: &Tree,
    items: &[Item],
    members: &[usize],
    block: &str,
    report: &mut Report,
) -> Tree {
    let named = tree.keys();
    let key = |i: usize| (items[i].refdes.clone(), items[i].unit);
    let missing: Vec<usize> = members
        .iter()
        .copied()
        .filter(|i| !named.iter().any(|k| *k == key(*i)))
        .collect();
    if missing.is_empty() {
        return tree.clone();
    }
    // A part supports something only if that something is in this tree: a cap whose parent
    // the author ALSO left out has no slot to be seated beside.
    let mut beside: BTreeMap<String, Vec<(String, u8)>> = BTreeMap::new();
    let mut orphans: Vec<(String, u8)> = Vec::new();
    for i in missing {
        match items[i]
            .supports
            .clone()
            .filter(|parent| named.iter().any(|(r, _)| r == parent))
        {
            Some(parent) => beside.entry(parent).or_default().push(key(i)),
            None => orphans.push(key(i)),
        }
    }
    let seatable: Vec<(String, u8)> = beside.values().flatten().cloned().collect();
    let tree = beside
        .into_iter()
        .fold(tree.clone(), |tree, (parent, parts)| {
            seat_beside(&tree, &parent, parts)
        });
    // A tree that is one bare leaf has no slot beside anything. Whatever the seating could
    // not place still has to be DRAWN — a part left out of the tree is never placed at all,
    // and a stack of symbols at the origin renders as one part and extracts as none.
    let placed = tree.keys();
    orphans.extend(seatable.into_iter().filter(|k| !placed.contains(k)));
    if orphans.is_empty() {
        return tree;
    }
    report.uncomposed.insert(
        block.to_owned(),
        orphans.iter().map(|(refdes, _)| refdes.clone()).collect(),
    );
    Tree::Container(Container {
        axis: Axis::Col,
        children: vec![tree, Tree::row_of(orphans)],
        gap: None,
        align: Align::Start,
        wrap: None,
    })
}

/// Put `parts` in the slot right after the leaf drawing `parent`, wherever in the tree that
/// leaf sits — the row of decoupling caps a human draws beside the device they serve.
///
/// They go in as ONE row, not as loose siblings. A bank spliced flat is measured child by
/// child, and the wrap then folds the container between two of them: the caps come out in
/// two bands with the far one stranded, which is the defect this is here to fix.
fn seat_beside(tree: &Tree, parent: &str, parts: Vec<(String, u8)>) -> Tree {
    let Tree::Container(c) = tree else {
        return tree.clone();
    };
    let mut children: Vec<Tree> = c
        .children
        .iter()
        .map(|child| seat_beside(child, parent, parts.clone()))
        .collect();
    if let Some(at) = children
        .iter()
        .position(|k| matches!(k, Tree::Leaf(l) if l.part == parent))
    {
        children.insert(at + 1, Tree::row_of(parts));
    }
    Tree::Container(Container {
        children,
        ..c.clone()
    })
}

/// Where a block's own frame lands on the sheet: ONE snapped delta for every part in it.
///
/// A block's extent is measured over text and label room, which is not a grid multiple, so
/// the shift has to be snapped — unsnapped, every pin in the block sits a fraction of a
/// millimetre off the wire drawn to it, and the sheet renders perfectly while its netlist
/// is empty.
fn block_shift(origin: Point2, bbox: Rect) -> Point2 {
    geom::GRID_50_MIL.snap_point(Point2::new(
        origin.x + FRAME_PAD - bbox.min_x,
        origin.y + FRAME_PAD - bbox.min_y,
    ))
}

fn drawing_bbox(placed: &[measure::Placed], parts: &[Part]) -> Rect {
    let corners: Vec<Point2> = placed
        .iter()
        .flat_map(|p| {
            let r = parts[p.part].extent(p.pose);
            [
                Point2::new(p.at.x + r.min_x, p.at.y + r.min_y),
                Point2::new(p.at.x + r.max_x, p.at.y + r.max_y),
            ]
        })
        .collect();
    Rect::bounding(&corners).unwrap_or_else(|| Rect::new(0.0, 0.0, 0.0, 0.0))
}

/// Pack block frames of `sizes` onto the smallest of `pages` they fill, returning one
/// origin per block in input order.
///
/// A pack that does not know which page it is filling optimises for a sheet nobody will
/// print: it shelves a wide drawing into a narrow column, and the writer then buys the
/// next page up to hold the column's height — the drawing ends in one corner of a sheet
/// twice the size it needed. So the pages come in, smallest first, and the first one an
/// arrangement fits wins.
///
/// The widths worth trying are exactly the ones a shelf boundary can fall on: the total
/// width of each contiguous run of blocks. Anything between two of those packs identically.
pub fn pack_blocks(sizes: &[(f64, f64)], pages: &[[f64; 2]]) -> Vec<Point2> {
    pages
        .iter()
        .find_map(|page| fills(sizes, *page, true))
        .or_else(|| pages.last().and_then(|page| fills(sizes, *page, false)))
        .unwrap_or_else(|| loose(sizes))
}

/// Pack `sizes` into `page` (a usable box), returning the arrangement whose proportions
/// best match the page's.
///
/// `strict` demands the arrangement fit inside the page, and gives `None` when none does.
/// The largest page is then packed again without it, so a drawing no page holds still
/// comes out with a page's proportions instead of as a ribbon several sheets wide.
///
/// The author's block order is the sheet's signal flow, so it is tried first and kept
/// whenever it fits; a reordering is only ever allowed to rescue a page the given order
/// could not fill, never to shave an aspect error.
fn fills(sizes: &[(f64, f64)], page: [f64; 2], strict: bool) -> Option<Vec<Point2>> {
    let target = page[0] / page[1].max(1.0);
    let room = [page[0] - ROUTING_ROOM, page[1] - ROUTING_ROOM];
    orders(sizes).into_iter().find_map(|order| {
        limits(sizes, room[0])
            .into_iter()
            .map(|limit| corner_pack(sizes, &order, limit))
            .filter(|(_, w, h)| !strict || (*w <= room[0] + geom::EPS && *h <= room[1] + geom::EPS))
            .min_by(|a, b| {
                aspect_error(a.1, a.2, target).total_cmp(&aspect_error(b.1, b.2, target))
            })
            .map(|(origins, ..)| origins)
    })
}

/// Pack `sizes` with no page to fill — an incremental graft, whose blocks land beside
/// content this crate never sees. Proportions only, in the author's order, with the width
/// held near a page's so a grafted group does not widen the sheet onto a custom page.
fn loose(sizes: &[(f64, f64)]) -> Vec<Point2> {
    let order: Vec<usize> = (0..sizes.len()).collect();
    limits(sizes, PAGE_WIDTH)
        .into_iter()
        .map(|limit| corner_pack(sizes, &order, limit))
        .min_by(|a, b| {
            let error =
                |w: f64, h: f64| aspect_error(w, h, SHEET_ASPECT) + (w - PAGE_WIDTH).max(0.0);
            error(a.1, a.2).total_cmp(&error(b.1, b.2))
        })
        .map(|(origins, ..)| origins)
        .unwrap_or_default()
}

/// Block orders worth trying, the author's first: then tallest, largest and widest first,
/// the classic bin-packing heuristics for getting an awkward set onto one page.
fn orders(sizes: &[(f64, f64)]) -> Vec<Vec<usize>> {
    let by = |key: fn(&(f64, f64)) -> f64| {
        let mut order: Vec<usize> = (0..sizes.len()).collect();
        order.sort_by(|a, b| key(&sizes[*b]).total_cmp(&key(&sizes[*a])).then(a.cmp(b)));
        order
    };
    vec![
        (0..sizes.len()).collect(),
        by(|s| s.1),
        by(|s| s.0 * s.1),
        by(|s| s.0),
    ]
}

/// Width limits that pack differently: the total width of each contiguous run of blocks,
/// plus the page's own width. Anything between two of those packs identically.
fn limits(sizes: &[(f64, f64)], page_width: f64) -> Vec<f64> {
    let mut widths = vec![page_width];
    for i in 0..sizes.len() {
        let mut run = 0.0;
        for (w, _) in &sizes[i..] {
            run += w + BLOCK_GAP;
            widths.push(run - BLOCK_GAP);
        }
    }
    widths.sort_by(f64::total_cmp);
    widths.dedup_by(|a, b| (*a - *b).abs() < geom::EPS);
    widths
}

/// How far a packed sheet of `w` x `h` is from proportions of `target`. Logarithmic, so
/// half as wide and twice as wide are the same defect — the linear form punished a tall
/// sheet far less than a wide one, which is how a drawing ends up in a column.
fn aspect_error(w: f64, h: f64, target: f64) -> f64 {
    ((w / h.max(1.0)) / target).ln().abs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::PAGE_MARGIN as MARGIN;

    /// With no page to fill, a pack wider than a page grows the paper, so a run that
    /// overflows loses to a taller sheet even when its proportions are better.
    #[test]
    fn a_graft_never_overflows_the_page_to_look_squarer() {
        let origins = pack_blocks(&[(200.0, 40.0), (200.0, 40.0)], &[]);
        assert!(origins[1].y > origins[0].y);
    }

    /// Given the pages it may be printed on, the pack fills one instead of stacking into a
    /// column that then buys the next page up. Three blocks this size fit nothing on A4;
    /// on A3 they go two to a shelf, inside its usable box.
    #[test]
    fn the_pack_aims_at_the_page_it_will_be_printed_on() {
        let a4 = [271.6, 151.6];
        let a3 = [394.6, 238.6];
        let blocks = [(180.0, 100.0), (180.0, 100.0), (150.0, 100.0)];
        assert!(
            fills(&blocks, a4, true).is_none(),
            "nothing this size fits A4"
        );
        let origins = pack_blocks(&blocks, &[a4, a3]);
        let shelves: BTreeSet<i64> = origins.iter().map(|o| (o.y * 100.0) as i64).collect();
        assert_eq!(
            shelves.len(),
            2,
            "three blocks on two shelves, not a column"
        );
        let far = origins
            .iter()
            .zip(blocks)
            .fold((0.0f64, 0.0f64), |m, (o, b)| {
                (m.0.max(o.x + b.0 - MARGIN), m.1.max(o.y + b.1 - MARGIN))
            });
        assert!(far.0 <= a3[0] && far.1 <= a3[1], "{far:?} outside A3");
    }

    /// A block lands on the lattice a pin connects on, however ragged the extent that
    /// positioned it. Unsnapped, every pin in the block sits a fraction of a millimetre off
    /// its wire — a sheet that renders perfectly and whose netlist is empty.
    #[test]
    fn a_block_lands_on_the_pin_lattice() {
        let ragged = Rect::new(-7.31, -4.09, 63.77, 51.13);
        let shift = block_shift(Point2::new(MARGIN, MARGIN), ragged);
        assert_eq!(geom::GRID_50_MIL.snap_point(shift), shift, "{shift:?}");
    }
}
