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

/// Space between two blocks on the sheet.
const BLOCK_GAP: f64 = 10.0 * UNIT_MM;
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
            let outside = leaving(items, block);
            let mine: Vec<Part> = members
                .iter()
                .map(|i| Part::new(*i, &items[*i], &outside))
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
    for (origin, (placed, bbox)) in pack(&drawings).into_iter().zip(&drawings) {
        for p in placed {
            let item = &mut items[p.part];
            item.at = Point2::new(
                origin.x + p.at.x - bbox.min_x,
                origin.y + p.at.y - bbox.min_y,
            );
            item.angle = p.pose.angle;
            item.mirror = p.pose.mirror;
        }
    }
    report
}

/// Nets of `block` that carry a pin somewhere else — they will need a label here, and a
/// label needs room the measure has to know about.
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
            let refdes = || mine.iter().map(|i| items[*i].refdes.clone()).collect::<Vec<_>>();
            let tree = match trees.get(&block) {
                Some(tree) => complete(tree, items, &mine),
                None => {
                    report.untreed.push(block.clone());
                    Tree::row_of(refdes())
                }
            };
            (block, tree, mine)
        })
        .collect()
}

/// The authored tree plus a trailing row of whatever it forgot, so every part is drawn.
fn complete(tree: &Tree, items: &[Item], members: &[usize]) -> Tree {
    let named = tree.keys();
    let missing: Vec<String> = members
        .iter()
        .filter(|i| {
            !named
                .iter()
                .any(|(r, u)| *r == items[**i].refdes && *u == items[**i].unit)
        })
        .map(|i| items[*i].refdes.clone())
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

/// Shelf-pack the blocks, choosing the shelf width whose finished sheet is closest to a
/// page's proportions — a sheet in one long row and a sheet in one long column are the two
/// ways a multi-block drawing reads badly.
fn pack(drawings: &[(Vec<measure::Placed>, Rect)]) -> Vec<Point2> {
    let sizes: Vec<(f64, f64)> = drawings
        .iter()
        .map(|(_, b)| (b.width(), b.height()))
        .collect();
    let widest = sizes.iter().map(|s| s.0).fold(0.0, f64::max);
    let total: f64 = sizes.iter().map(|s| s.0 + BLOCK_GAP).sum();
    let shelve = |limit: f64| {
        let (mut origins, mut x, mut y, mut shelf, mut used) =
            (Vec::new(), MARGIN, MARGIN, 0.0f64, MARGIN);
        for (w, h) in &sizes {
            if x > MARGIN && x + w > MARGIN + limit {
                x = MARGIN;
                y += shelf + BLOCK_GAP;
                shelf = 0.0;
            }
            origins.push(Point2::new(x, y));
            x += w + BLOCK_GAP;
            shelf = shelf.max(*h);
            used = used.max(x - BLOCK_GAP);
        }
        (origins, used - MARGIN, y + shelf - MARGIN)
    };
    let mut best: Option<(f64, Vec<Point2>)> = None;
    let mut limit = widest;
    while limit <= total + 1.0 {
        let (origins, w, h) = shelve(limit);
        let score = (w / h.max(1.0) - SHEET_ASPECT).abs();
        if best.as_ref().is_none_or(|(b, _)| score < *b) {
            best = Some((score, origins));
        }
        limit += widest.max(BLOCK_GAP);
    }
    best.map(|(_, o)| o).unwrap_or_default()
}
