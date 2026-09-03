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
mod part;

use std::collections::{BTreeMap, BTreeSet};

use geom::{Point2, Rect};
use sch_model::geometry::MARGIN;
use sch_model::item::Item;
use sch_model::tree::{Align, Axis, Container, Tree, Trees, UNIT_MM};

use measure::typeset_block;
use part::Part;

/// Space between two blocks on the sheet, over and above [`FRAME_PAD`].
const BLOCK_GAP: f64 = 8.0 * UNIT_MM;
/// Room each block keeps outside its parts for the dashed frame the realiser draws around
/// it and the field text the solver seats along its edge.
const FRAME_PAD: f64 = 5.0 * UNIT_MM;
/// The width-to-height ratio a packed sheet aims for — a landscape page's usable area.
const SHEET_ASPECT: f64 = 1.5;

/// What the typesetter had to decide for itself.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Blocks whose author composed no tree; each was drawn as one bare row.
    pub untreed: Vec<String>,
}

impl Report {
    /// The warnings a caller shows the author.
    pub fn warnings(&self) -> Vec<String> {
        self.untreed
            .iter()
            .map(|block| {
                format!(
                    "block `{block}` has no `layout` tree, so it was drawn as one row: \
                     compose it as rows/cols of parts to get a readable block"
                )
            })
            .collect()
    }
}

/// Lay out every non-preseeded item from its block's tree, writing `at`, `angle` and
/// `mirror` back into `items`. Items a caller already posed are left exactly where they
/// are, and nothing is drawn on top of them — the caller's region adapter owns that.
pub fn typeset(items: &mut [Item], trees: &Trees) -> Report {
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
    for (origin, (placed, bbox)) in pack(&sizes).into_iter().zip(&drawings) {
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
                Some(tree) => complete(tree, items, &mine),
                None => {
                    report.untreed.push(block.clone());
                    Tree::row_of(units())
                }
            };
            (block, tree, mine)
        })
        .collect()
}

/// The authored tree plus a trailing row of whatever it forgot, so every part is drawn.
fn complete(tree: &Tree, items: &[Item], members: &[usize]) -> Tree {
    let named = tree.keys();
    let missing: Vec<(String, u8)> = members
        .iter()
        .filter(|i| {
            !named
                .iter()
                .any(|(r, u)| *r == items[**i].refdes && *u == items[**i].unit)
        })
        .map(|i| (items[*i].refdes.clone(), items[*i].unit))
        .collect();
    if missing.is_empty() {
        return tree.clone();
    }
    Tree::Container(Container {
        axis: Axis::Col,
        children: vec![tree.clone(), Tree::row_of(missing)],
        gap: None,
        align: Align::Start,
        wrap: None,
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

/// Lay the blocks out in COLUMNS, reading down each column and then across — the way a
/// person fills a sheet. The blocks keep their order; only the column boundaries move.
///
/// The number of columns is chosen on the proportions of the finished sheet: one long
/// column and one long row are the two ways a multi-block drawing wastes a page.
fn pack(sizes: &[(f64, f64)]) -> Vec<Point2> {
    (1..=sizes.len().max(1))
        .map(|columns| lay_columns(sizes, columns))
        .min_by(|a, b| aspect_error(a.1, a.2).total_cmp(&aspect_error(b.1, b.2)))
        .map(|(origins, ..)| origins)
        .unwrap_or_default()
}

/// The blocks in `columns` contiguous groups, each group stacked downward, the groups laid
/// left to right. Returns the origins and the finished sheet's extent.
fn lay_columns(sizes: &[(f64, f64)], columns: usize) -> (Vec<Point2>, f64, f64) {
    let total: f64 = sizes.iter().map(|s| s.1 + BLOCK_GAP).sum();
    let target = total / columns as f64;
    let (mut origins, mut x, mut y, mut wide, mut tall, mut used) =
        (Vec::new(), MARGIN, MARGIN, 0.0f64, 0.0f64, 0.0f64);
    let mut left = columns;
    for (w, h) in sizes {
        // Break to the next column once this one has its share — never on the last one,
        // which takes whatever is left.
        if left > 1 && used > 0.0 && used + h / 2.0 > target {
            x += wide + BLOCK_GAP;
            y = MARGIN;
            wide = 0.0;
            used = 0.0;
            left -= 1;
        }
        origins.push(Point2::new(x, y));
        y += h + BLOCK_GAP;
        used += h + BLOCK_GAP;
        wide = wide.max(*w);
        tall = tall.max(y - BLOCK_GAP - MARGIN);
    }
    (origins, x + wide - MARGIN, tall)
}

/// How far a packed sheet of `w` x `h` is from a page's proportions. The page itself grows
/// to whatever the content needs, so what matters is the SHAPE: one long column and one
/// long row are the two ways a multi-block drawing wastes its page.
fn aspect_error(w: f64, h: f64) -> f64 {
    (w / h.max(1.0) - SHEET_ASPECT).abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocks that would make one long column are laid in two, reading down and then
    /// across: the pack is chosen on the proportions of the finished sheet.
    #[test]
    fn blocks_are_laid_in_page_shaped_columns() {
        let origins = pack(&[(90.0, 60.0), (90.0, 60.0), (90.0, 60.0), (90.0, 60.0)]);
        assert_eq!(origins[0], Point2::new(MARGIN, MARGIN));
        assert!(origins[1].y > origins[0].y, "the column reads downward first");
        assert!(origins[2].x > origins[0].x, "then it breaks across");
        assert_eq!(origins[2].y, MARGIN, "a new column starts at the top");
    }

    /// Two wide blocks stack rather than sit side by side: a page twice as wide as it is
    /// tall reads worse than one that is roughly page-shaped.
    #[test]
    fn wide_blocks_stack_rather_than_widen_the_sheet() {
        let origins = pack(&[(200.0, 40.0), (200.0, 40.0)]);
        assert!(origins[1].y > origins[0].y);
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
