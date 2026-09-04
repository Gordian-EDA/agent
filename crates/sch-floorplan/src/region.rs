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

use geom::{Point2, Rect};

use kicad::KicadInstallation;
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};
use sch_model::place::PlaceResult;

use crate::floorplan::place::{RoutedEvaluator, RoutedSheetRealizer, incidence};
use sch_model::geometry::body_rect;

/// Space left between the content already on a sheet and a block placed beside it.
const BLOCK_MARGIN: f64 = 10.0 * geom::GRID_50_MIL.pitch();
/// Step of the legalisation walk (100 mil — two schematic grid steps).
const WALK: f64 = 2.0 * geom::GRID_50_MIL.pitch();
/// How far a whole BLOCK may be slid to find free sheet. A block legitimately travels the
/// width of the sheet: it is being added beside content that is already there.
const BLOCK_RINGS: i32 = 60;
/// Step of the block slide. Coarser than [`WALK`] because a block is looking for a free
/// REGION, not for a grid cell, and 60 rings of it reach 762 mm — past any page.
const BLOCK_WALK: f64 = 10.0 * geom::GRID_50_MIL.pitch();
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

/// Slide a freshly typeset block clear of the content already on the sheet, so it starts
/// beside it rather than on top of it.
fn beside_the_fixed(movable: &mut [Item], fixed: &[Item]) {
    let corners = |items: &[Item]| {
        let pts: Vec<Point2> = items
            .iter()
            .flat_map(|it| {
                let r = body_rect(it, it.at);
                [Point2::new(r.min_x, r.min_y), Point2::new(r.max_x, r.max_y)]
            })
            .collect();
        Rect::bounding(&pts)
    };
    let (Some(here), Some(there)) = (corners(movable), corners(fixed)) else {
        return;
    };
    let delta = Point2::new(
        geom::GRID_50_MIL.snap(there.max_x + BLOCK_MARGIN - here.min_x),
        geom::GRID_50_MIL.snap(there.min_y - here.min_y),
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

/// Move the movable set off every obstacle and fixed neighbour — as a BLOCK first.
///
/// The engine arranged these parts together; walking each one out on its own is what
/// shreds a block into islands hundreds of millimetres apart, which is exactly how a
/// grafted sheet ends up 1168 mm wide with 90% of its area empty. So the block slides
/// rigidly, keeping its internal geometry bit-for-bit, until its bounding box clears
/// everything already on the sheet. Only what still overlaps AFTER that — movable parts
/// colliding with each other, the engine's own business — gets the local per-part nudge,
/// bounded to [`WALK_RINGS`].
///
/// Clearance is measured on [`body_rect`], the space the drawing occupies. The text pad
/// is a claim the field solver may abandon, and pricing it here made a 5 mm phantom touch
/// worth a sheet-width of travel.
///
/// Returns how many parts are still overlapping something when it is done. Both passes
/// give up rather than fling a part across the sheet, so this is how the caller learns
/// the sheet it is about to commit has a collision on it.
fn legalize(movable: &mut [Item], fixed: &[Item], obstacles: &[Rect]) -> usize {
    let blockers: Vec<Rect> = fixed
        .iter()
        .map(|it| body_rect(it, it.at))
        .chain(obstacles.iter().copied())
        .collect();
    slide_block(movable, &blockers);
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

/// Slide the whole movable set to the nearest offset where its bounding box clears every
/// blocker. A clear bbox means every member is clear, so this is one rect test per
/// candidate offset; a block already in free sheet does not move.
///
/// Among the offsets that clear, the one that leaves the SMALLEST sheet wins over the
/// merely nearest: a block flung 700 mm sideways clears everything and costs the sheet a
/// page size, which is the same defect measured from the other end. The search widens ring
/// by ring and keeps the best landing of the first ring that has one, so it still stops as
/// soon as the block has somewhere to go.
fn slide_block(movable: &mut [Item], blockers: &[Rect]) {
    let corners = |it: &Item| {
        let r = body_rect(it, it.at);
        [Point2::new(r.min_x, r.min_y), Point2::new(r.max_x, r.max_y)]
    };
    let Some(bbox) = Rect::bounding(&movable.iter().flat_map(corners).collect::<Vec<_>>()) else {
        return;
    };
    let shifted = |dx: f64, dy: f64| {
        Rect::new(
            bbox.min_x + dx,
            bbox.min_y + dy,
            bbox.max_x + dx,
            bbox.max_y + dy,
        )
    };
    let clear = |r: &Rect| !blockers.iter().any(|o| r.overlaps(o));
    if clear(&bbox) {
        return;
    }
    // How much sheet the landing costs: the extent of everything on it afterwards.
    let sheet = blockers.iter().fold(bbox, |acc, o| {
        Rect::new(
            acc.min_x.min(o.min_x),
            acc.min_y.min(o.min_y),
            acc.max_x.max(o.max_x),
            acc.max_y.max(o.max_y),
        )
    });
    let cost = |r: &Rect| {
        let w = sheet.max_x.max(r.max_x) - sheet.min_x.min(r.min_x);
        let h = sheet.max_y.max(r.max_y) - sheet.min_y.min(r.min_y);
        w + h
    };
    // There is no sheet to the left of the origin: a block slid to a negative coordinate
    // is drawn outside the frame, and the page fitter can only rescue it by sliding the
    // block back — which it refuses to do when the block touches what is already drawn.
    // So the landing must be on the page; only if nothing on the page is free does an
    // off-page landing beat leaving the block on top of something.
    let on_page = |r: &Rect| r.min_x >= sch_doc::PAGE_MARGIN && r.min_y >= sch_doc::PAGE_MARGIN;
    let search = |page_only: bool| {
        (1..=BLOCK_RINGS).find_map(|ring| {
            ring_offsets(ring)
                .into_iter()
                .map(|(dx, dy)| (dx as f64 * BLOCK_WALK, dy as f64 * BLOCK_WALK))
                .filter(|&(dx, dy)| {
                    let r = shifted(dx, dy);
                    clear(&r) && (!page_only || on_page(&r))
                })
                .min_by(|a, b| cost(&shifted(a.0, a.1)).total_cmp(&cost(&shifted(b.0, b.1))))
        })
    };
    let landed = search(true).or_else(|| search(false));
    let Some((dx, dy)) = landed else { return };
    // ONE snapped delta for the whole block: snapping each part independently would move
    // them by different amounts and break the arrangement the engine just searched for.
    let (dx, dy) = (geom::GRID_50_MIL.snap(dx), geom::GRID_50_MIL.snap(dy));
    for it in movable.iter_mut() {
        it.at = Point2::new(it.at[0] + dx, it.at[1] + dy);
    }
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

    // A sheet with nothing on it is the typesetter's to fill, so it packs the blocks for a
    // real page. A graft has no such freedom: its blocks land beside content this crate
    // cannot see, and packing them for a whole page would push the sheet onto a custom one.
    let pages = match fixed.is_empty() && obstacles.is_empty() {
        true => crate::write::usable_pages(),
        false => Vec::new(),
    };
    let typeset = sch_flex::typeset(&mut all, &ir.trees, &pages);
    // The typesetter lays a block out from the origin: it draws the block, not the sheet.
    // On a sheet that already has content that is on top of what is there, so the new
    // block starts BESIDE it — the slide below only has to fine-tune from there.
    beside_the_fixed(&mut all[..movable], &fixed);

    // With nothing to avoid, the typeset arrangement is authoritative — walking parts
    // apart here would only undo the alignment it just computed.
    let stuck = if obstacles.is_empty() && fixed.is_empty() {
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
            // and the realiser cannot see it: both passes give up rather than fling a part
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
