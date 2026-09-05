//! `region` — place a SUBSET of a sheet among neighbours that are already there.
//!
//! The whole-sheet pipeline typesets every block from nothing. Live editing needs the
//! other shape: "arrange these three parts, leave everything else exactly where it is".
//! [`arrange`] is that adapter — it runs the typesetter over the movable set among the
//! fixed neighbours, then hands back poses for the movable set only. It is what an
//! `arrange(selection)` tool calls, and what a bulk `place_parts` calls with an empty
//! fixed set.
//!
//! Two invariants the adapter owns, because the typesetter does not:
//! - **Fixed neighbours do not move.** The adapter marks them `preseeded` (they already
//!   hold the pose the caller owns), and the typesetter leaves those alone.
//! - **Nothing lands on an obstacle.** The typesetter's only geometry is the parts it
//!   draws, so the adapter legalises afterwards: any movable part overlapping an obstacle,
//!   a fixed neighbour, or another movable part is walked out to the nearest clear grid
//!   position. With no obstacles this is a no-op.

use std::collections::BTreeMap;

use geom::{Point2, Rect};

use kicad::KicadInstallation;
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};
use sch_model::place::PlaceResult;

use crate::floorplan::place::{RoutedEvaluator, RoutedSheetRealizer, incidence};
use sch_flex::pack::{BLOCK_GAP, landings};
use sch_model::geometry::body_rect;

/// The width-to-height ratio a sheet aims for: a landscape page's usable area.
const SHEET_ASPECT: f64 = 1.5;
/// Step of the legalisation walk (100 mil — two schematic grid steps).
const WALK: f64 = 2.0 * geom::GRID_50_MIL.pitch();
/// How far a single part may be nudged once its block has landed. A local repair: a part
/// walked further than this is no longer part of the block the engine arranged, and the
/// islands that produced were the sheet's worst defect.
const WALK_RINGS: i32 = 12;

/// A region placement request: which parts to move, which to respect, and what else is
/// in the way.
pub struct RegionProblem<'a> {
    pub env: &'a KicadInstallation,
    /// The movable set — the parts to place.
    pub items: Vec<Item>,
    /// Neighbours at their LIVE positions. Forced `preseeded`; they are never
    /// moved nor re-seeded.
    pub fixed: Vec<Item>,
    /// Everything else on the sheet the placement must avoid: label boxes, wires'
    /// keepouts, other sheets' furniture — anything with no [`Item`] to speak for it.
    pub obstacles: Vec<Rect>,
    /// Net → pins over `items` followed by `fixed`. [`RegionProblem::new`] builds it.
    pub incidence: Incidence,
    pub ir: LayoutIr,
}

/// The pose of one placed part, in the CALLER's coordinate frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Pose {
    pub refdes: String,
    pub unit: u8,
    pub at: Point2,
    pub angle: f64,
    pub mirror: bool,
}

/// New poses for the movable set, in input order, plus what the finished sheet measures.
pub struct RegionOutput {
    pub poses: Vec<Pose>,
    /// What the typesetter had to decide because the author's trees did not.
    pub warnings: Vec<String>,
    /// The IR the sheet ships with — its rail and port decisions.
    pub ir: LayoutIr,
    pub result: PlaceResult,
}

impl<'a> RegionProblem<'a> {
    /// Build a region problem, deriving the incidence from the parts themselves.
    pub fn new(
        env: &'a KicadInstallation,
        items: Vec<Item>,
        fixed: Vec<Item>,
        obstacles: Vec<Rect>,
        ir: LayoutIr,
    ) -> Self {
        let mut all = items.clone();
        all.extend(fixed.iter().cloned());
        let incidence = incidence(&all);
        Self {
            env,
            items,
            fixed,
            obstacles,
            incidence,
            ir,
        }
    }
}

/// One rect per block among `which`: the frame the realiser will draw around it.
///
/// A frame is a BLOCK's, not a part's. Measuring it per part and letting the merge below
/// recover the block reads the same on a tight block and quite differently on a loose
/// one, and neither is the rectangle the drawing ends up carrying — which is the only one
/// a seat may be trusted to keep clear.
fn block_frames(all: &[Item], which: impl Iterator<Item = usize>) -> Vec<Rect> {
    let mut blocks: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for i in which {
        blocks.entry(all[i].block.as_str()).or_default().push(i);
    }
    blocks
        .into_values()
        .filter_map(|members| sch_flex::pack::block_frame(all, &members))
        .collect()
}

fn union(a: &Rect, b: &Rect) -> Rect {
    Rect::new(
        a.min_x.min(b.min_x),
        a.min_y.min(b.min_y),
        a.max_x.max(b.max_x),
        a.max_y.max(b.max_y),
    )
}

fn hull(rects: &[Rect]) -> Option<Rect> {
    rects.split_first().map(|(a, rest)| rest.iter().fold(*a, |h, r| union(&h, r)))
}

/// What the sheet is already using: the blocks' own frames and every obstacle, with
/// overlapping ones merged.
///
/// Two blocks that were drawn apart stay apart, and the hole between them is a hole a new
/// block may land in. Seating against a single hull of all of it — what this used to do —
/// is what cost a row or a column of sheet per block added, whatever the block's own size.
fn occupied(frames: &[Rect], obstacles: &[Rect]) -> Vec<Rect> {
    let mut rects: Vec<Rect> = frames.iter().chain(obstacles).copied().collect();
    let mut merged = true;
    while merged {
        merged = false;
        let mut out: Vec<Rect> = Vec::with_capacity(rects.len());
        for r in rects {
            match out.iter_mut().find(|o| o.overlaps(&r)) {
                Some(o) => {
                    *o = union(o, &r);
                    merged = true;
                }
                None => out.push(r),
            }
        }
        rects = out;
    }
    rects
}

/// The boxes the new blocks may be typeset for: on each standard page, the larger of the
/// two free strips `there` leaves — beside it and under it — and then the whole pages.
///
/// The strips come first so a GROUP of blocks arranges itself for the room actually left;
/// the whole pages follow so the list is never empty. An empty list is what dropped the
/// typesetter into its no-page fallback, which packs a fixed 260 mm column whatever the
/// sheet looks like — and a column is exactly what a nearly full sheet must not be handed.
///
/// A box never changes how a single block is DRAWN: a block's arrangement comes from its
/// author's tree and knows nothing of the page. So on the one-call-per-block path this
/// list decides nothing at all — [`seat_beside`] re-seats the group and the packed origin
/// cancels. It earns its keep only when one call carries several blocks.
fn beside_pages(there: Rect) -> Vec<[f64; 2]> {
    let right = there.max_x + BLOCK_GAP - geom::PAGE_MARGIN;
    let under = there.max_y + BLOCK_GAP - geom::PAGE_MARGIN;
    let strips = crate::write::usable_pages().into_iter().filter_map(|page| {
        let beside = [page[0] - right, page[1]];
        let below = [page[0], page[1] - under];
        let room = if beside[0] * beside[1] >= below[0] * below[1] {
            beside
        } else {
            below
        };
        (room[0] > 0.0 && room[1] > 0.0).then_some(room)
    });
    strips.chain(crate::write::usable_pages()).collect()
}

/// Seat the freshly typeset blocks in the free sheet among what is already drawn.
/// `frames` is what the new blocks will DRAW — the rects the realiser outlines — so a
/// landing that clears `taken` is a frame that lands clear of its neighbours' rather than
/// through them.
///
/// The landing is chosen from the same corner lattice the typesetter packs an empty page
/// with ([`sch_flex::pack::landings`]) — beside and under every block already down, one
/// [`sch_flex::pack::BLOCK_GAP`] apart, the same air a whole-sheet pack leaves — and the
/// one that leaves the SMALLEST sheet on the smallest page it fits wins. So a new block
/// fills the hole a short neighbour leaves instead of starting a column beside everything,
/// and a sheet built one call at a time lands where the same blocks would have landed had
/// they been packed together.
///
/// Seating against the single bounding box of all the content, which is what this did,
/// cost a whole row or column of sheet per block: a 28x20 mm block grew the sheet by
/// 12,815 mm². Nine blocks came out 2.9x the area the same nine pack into, and the
/// sparsest agent sheets were exactly the ones built from the most calls.
///
/// What is left to win here is about one point. Measured over the nine multi-frame
/// fixtures of the block replay: the drawn frames leave 32.4% of their hull empty, an
/// OFFLINE optimal pack of the very same claims — free to reorder the blocks — leaves
/// about 31%, and an offline optimal pack of the frames as DRAWN leaves 15%. The 16 points
/// between the last two are not packing at all: a claim is an upper bound and comes out
/// 18.4% larger in area than the frame the realiser then draws inside it. Only a pass that
/// re-seats blocks once they are drawn can reclaim that; no landing rule can.
fn seat_beside(movable: &mut [Item], frames: &[Rect], taken: &[Rect], drawn: &[Rect]) {
    let (Some(here), false) = (hull(frames), taken.is_empty()) else {
        return;
    };
    let size = (here.width(), here.height());
    // The sheet is what is DRAWN on it. An obstacle is something to keep off, not sheet
    // the drawing claims: pricing the label columns and wire keepouts into the extent
    // made a strip beside them look free and stretched the sheet into a ribbon.
    let sheet = |at: &Point2| {
        let r = Rect::new(at.x, at.y, at.x + size.0, at.y + size.1);
        drawn.iter().fold(r, |h, o| union(&h, o))
    };
    // Area with the sheet's proportions as a tie-break: two landings that grow the sheet
    // by the same amount are not equally good, and the one that leaves a ribbon reads
    // worse. A landing that fills a hole changes neither term, so it still wins outright.
    let cost = |r: &Rect| {
        let aspect = ((r.width() / r.height().max(1.0)) / SHEET_ASPECT).ln().abs();
        r.width() * r.height() * (1.0 + aspect)
    };
    let best = |limit: f64, page: Option<[f64; 2]>| {
        landings(taken, size, limit, BLOCK_GAP)
            .into_iter()
            .filter(|at| {
                page.is_none_or(|p| {
                    let s = sheet(at);
                    s.max_x <= geom::PAGE_MARGIN + p[0] + geom::EPS
                        && s.max_y <= geom::PAGE_MARGIN + p[1] + geom::EPS
                })
            })
            .min_by(|a, b| cost(&sheet(a)).total_cmp(&cost(&sheet(b))))
    };
    let landed = crate::write::usable_pages()
        .into_iter()
        .find_map(|page| best(page[0], Some(page)))
        .or_else(|| best(f64::INFINITY, None));
    // No landing at all — the corner lattice had nothing clear on any page. Leaving the
    // group where the typesetter drew it, which is what this did, drops the whole block
    // on top of the sheet: the block draws from its own origin, so "unmoved" means "on
    // top of whatever is already at the origin". Below everything already down is always
    // free, so that is the fallback.
    let at = landed.unwrap_or_else(|| {
        let there = hull(taken).expect("taken is not empty");
        Point2::new(geom::PAGE_MARGIN, there.max_y + BLOCK_GAP)
    });
    // ONE snapped delta for the whole group: snapping each part independently would move
    // them by different amounts and break the arrangement the typesetter just computed.
    let delta = Point2::new(
        geom::GRID_50_MIL.snap(at.x - here.min_x),
        geom::GRID_50_MIL.snap(at.y - here.min_y),
    );
    for it in movable {
        it.at = Point2::new(it.at.x + delta.x, it.at.y + delta.y);
    }
}

/// Whether `r` clears every obstacle and every rect in `others`.
fn clear_of(r: &Rect, obstacles: &[Rect], others: &[Rect]) -> bool {
    !obstacles.iter().any(|o| r.overlaps(o)) && !others.iter().any(|o| r.overlaps(o))
}

/// Offsets on the ring `max(|dx|, |dy|) == ring`, nearest-first and deterministic.
fn ring_offsets(ring: i32) -> Vec<(i32, i32)> {
    let edges = (-ring..=ring).flat_map(move |d| [(d, -ring), (d, ring)]);
    let sides = (-ring + 1..ring).flat_map(move |d| [(-ring, d), (ring, d)]);
    let mut out: Vec<(i32, i32)> = edges.chain(sides).collect();
    out.sort_by_key(|(dx, dy)| (dx.abs() + dy.abs(), *dx, *dy));
    out
}

/// Repair what is still overlapping once the group has been seated.
///
/// [`seat_beside`] lands the group in free sheet, so this is only ever the engine's own
/// business: movable parts colliding with each other, or with an obstacle whose frame the
/// seat could not price. Each is nudged locally, bounded to [`WALK_RINGS`] — a part walked
/// further than that is no longer part of the block the engine arranged, and the islands
/// that produced were the sheet's worst defect.
///
/// Clearance is measured on [`body_rect`], the space the drawing occupies. The text pad is
/// a claim the field solver may abandon, and pricing it here made a 5 mm phantom touch
/// worth a sheet-width of travel.
///
/// Returns how many parts are still overlapping something when it is done. The walk gives
/// up rather than fling a part across the sheet, so this is how the caller learns the
/// sheet it is about to commit has a collision on it.
fn legalize(movable: &mut [Item], fixed: &[Item], obstacles: &[Rect]) -> usize {
    let blockers: Vec<Rect> = fixed
        .iter()
        .map(|it| body_rect(it, it.at))
        .chain(obstacles.iter().copied())
        .collect();
    nudge_parts(movable, fixed, obstacles);
    movable
        .iter()
        .enumerate()
        .filter(|(i, it)| {
            let r = body_rect(it, it.at);
            blockers.iter().any(|o| r.overlaps(o))
                || movable
                    .iter()
                    .enumerate()
                    .any(|(j, other)| j != *i && r.overlaps(&body_rect(other, other.at)))
        })
        .count()
}

/// Nudge each still-overlapping movable part to the nearest clear grid position.
/// Deterministic; parts that are already clear never move, and a part with nowhere to go
/// within [`WALK_RINGS`] keeps its position (the caller sees the overlap rather than a
/// part flung across the sheet).
fn nudge_parts(movable: &mut [Item], fixed: &[Item], obstacles: &[Rect]) {
    let mut taken: Vec<Rect> = fixed.iter().map(|it| body_rect(it, it.at)).collect();
    let mut rest = movable;
    while let Some((item, tail)) = rest.split_first_mut() {
        let others: Vec<Rect> = taken
            .iter()
            .copied()
            .chain(tail.iter().map(|it| body_rect(it, it.at)))
            .collect();
        if !clear_of(&body_rect(item, item.at), obstacles, &others) {
            let from: [f64; 2] = item.at.into();
            let landed = (1..=WALK_RINGS).find_map(|ring| {
                ring_offsets(ring).into_iter().find_map(|(dx, dy)| {
                    let at = Point2::new(
                        geom::GRID_50_MIL.snap(from[0] + dx as f64 * WALK),
                        geom::GRID_50_MIL.snap(from[1] + dy as f64 * WALK),
                    );
                    clear_of(&body_rect(item, at), obstacles, &others).then_some(at)
                })
            });
            if let Some(at) = landed {
                item.at = at;
            }
        }
        taken.push(body_rect(item, item.at));
        rest = tail;
    }
}

/// Place `problem.items` among `problem.fixed` and `problem.obstacles`, returning new
/// poses for the movable set only.
///
/// The fixed neighbours come back at exactly their input positions; the movable poses are
/// expressed in the same frame. The reported [`PlaceResult`] is measured on the FINAL,
/// legalised geometry, so a caller can gate on `truthfulness_breaks` before committing.
pub fn arrange(problem: RegionProblem) -> RegionOutput {
    let RegionProblem {
        env,
        items,
        fixed,
        obstacles,
        incidence,
        ir,
    } = problem;

    let movable = items.len();
    let mut all = items;
    all.extend(fixed.iter().cloned());
    for it in all.iter_mut().take(movable) {
        it.preseeded = false;
    }
    for it in all.iter_mut().skip(movable) {
        it.preseeded = true;
    }

    // What the sheet is already using, one rect per block — and, from it, what the new
    // blocks may be typeset for: the whole page ladder on an empty sheet, the free strips
    // of each page on a sheet that already carries a drawing.
    let neighbours = block_frames(&all, movable..all.len());
    let taken = occupied(&neighbours, &obstacles);
    let drawn = occupied(&neighbours, &[]);
    let pages = match hull(&taken) {
        None => crate::write::usable_pages(),
        Some(there) => beside_pages(there),
    };
    let typeset = sch_flex::typeset(&mut all, &ir.trees, &pages);
    // The typesetter lays a block out from the origin: it draws the block, not the sheet.
    // On a sheet that already has content, that is on top of what is there, so the group
    // is seated in the free sheet among the blocks already down.
    let ours = block_frames(&all, 0..movable);
    seat_beside(&mut all[..movable], &ours, &taken, &drawn);

    // With nothing to avoid, the typeset arrangement is authoritative — walking parts
    // apart here would only undo the alignment it just computed. A collision the
    // typesetter drew into a block is caught by the caller's body-overlap gate instead.
    let stuck = if taken.is_empty() {
        0
    } else {
        let (moved, held) = all.split_at_mut(movable);
        legalize(moved, held, &obstacles)
    };

    let result = {
        let realizer = RoutedSheetRealizer::new(env, &incidence, &ir);
        let eval = RoutedEvaluator::new(realizer);
        PlaceResult {
            truthfulness_breaks: eval.truthfulness_breaks(&all),
            // A part legalisation could not clear is a readability defect like any other,
            // and the realiser cannot see it: the walk gives up rather than fling a part
            // across the sheet, so this is the only place it is counted.
            warnings: eval.warnings(&all) + stuck,
            crossings: eval.crossings(&all),
        }
    };
    RegionOutput {
        ir,
        warnings: typeset.warnings(),
        poses: all[..movable]
            .iter()
            .map(|it| Pose {
                refdes: it.refdes.clone(),
                unit: it.unit,
                at: it.at,
                angle: it.angle,
                mirror: it.mirror,
            })
            .collect(),
        result,
    }
}
