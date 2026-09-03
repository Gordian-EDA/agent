//! `region` — place a SUBSET of a sheet among neighbours that are already there.
//!
//! The whole-sheet pipeline lays out every part from nothing. Live editing needs the
//! other shape: "arrange these three parts, leave everything else exactly where it is".
//! [`arrange`] is that adapter — it drives an ordinary [`PlacementEngine`] over the union
//! of the movable set and the fixed neighbours, then hands back poses for the movable set
//! only. It is what an `arrange(selection)` tool calls, and what a bulk `place_parts`
//! calls with an empty fixed set.
//!
//! Two invariants the adapter owns, because the engine contract does not:
//! - **Fixed neighbours do not move.** The adapter marks them `frozen` (the search may not
//!   move them) AND `preseeded` (they already hold the pose the caller owns), so they keep
//!   that pose through seeding, search, and the overlap relaxers. The engines still `normalize`
//!   the sheet — a rigid translation — so the adapter measures that offset off the fixed
//!   set and takes it back out, returning poses in the caller's own frame.
//! - **Nothing lands on an obstacle.** Engines have no obstacle vocabulary (their only
//!   geometry is the items they place), so the adapter legalises afterwards: any movable
//!   part overlapping an obstacle, a fixed neighbour, or another movable part is walked
//!   out to the nearest clear grid position. With no obstacles this is a no-op.

use geom::{EPS, Point2, Rect};

use kicad::KicadInstallation;
use sch_check::Design;
use sch_model::engine::CandidateEvaluator;
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};
use sch_model::place::{Deadline, PlaceOptions, PlaceResult};

use sch_model::engine::{PlacementEngine, SchematicPlaceProblem};

use crate::floorplan::place::{RoutedEvaluator, RoutedSheetRealizer, incidence, resolve_pin_flow};
use sch_model::geometry::body_rect;

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
    pub design: &'a Design,
    /// The movable set — the parts to place.
    pub items: Vec<Item>,
    /// Neighbours at their LIVE positions. Forced `frozen` + `preseeded`; they are never
    /// moved nor re-seeded.
    pub fixed: Vec<Item>,
    /// Everything else on the sheet the placement must avoid: label boxes, wires'
    /// keepouts, other sheets' furniture — anything with no [`Item`] to speak for it.
    pub obstacles: Vec<Rect>,
    /// Net → pins over `items` followed by `fixed`. [`RegionProblem::new`] builds it.
    pub incidence: Incidence,
    pub ir: LayoutIr,
    pub engine: &'a dyn PlacementEngine,
    pub options: PlaceOptions,
    /// When the search must stop; `None` searches to its full iteration budget.
    pub deadline: Option<Deadline>,
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

/// New poses for the movable set, in input order, plus what the engine measured.
pub struct RegionOutput {
    pub poses: Vec<Pose>,
    /// The IR the engine finished with — its recognized idioms and rail decisions, which
    /// the realiser needs to draw the same sheet the engine scored.
    pub ir: LayoutIr,
    pub result: PlaceResult,
}

impl<'a> RegionProblem<'a> {
    /// Build a region problem, deriving the incidence from the parts themselves.
    pub fn new(
        env: &'a KicadInstallation,
        design: &'a Design,
        items: Vec<Item>,
        fixed: Vec<Item>,
        obstacles: Vec<Rect>,
        ir: LayoutIr,
        engine: &'a dyn PlacementEngine,
    ) -> Self {
        let mut all = items.clone();
        all.extend(fixed.iter().cloned());
        let incidence = incidence(&all);
        Self {
            env,
            design,
            items,
            fixed,
            obstacles,
            incidence,
            ir,
            engine,
            options: PlaceOptions::default(),
            deadline: None,
        }
    }

    /// Stop the search by `deadline`; the engine ships its best-so-far.
    pub fn by(mut self, deadline: Option<Deadline>) -> Self {
        self.deadline = deadline;
        self
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
fn legalize(movable: &mut [Item], fixed: &[Item], obstacles: &[Rect]) {
    let blockers: Vec<Rect> = fixed
        .iter()
        .map(|it| body_rect(it, it.at))
        .chain(obstacles.iter().copied())
        .collect();
    slide_block(movable, &blockers);
    nudge_parts(movable, fixed, obstacles);
}

/// Slide the whole movable set to the nearest offset where its bounding box clears every
/// blocker. A clear bbox means every member is clear, so this is one rect test per
/// candidate offset; a block already in free sheet does not move.
fn slide_block(movable: &mut [Item], blockers: &[Rect]) {
    let Some(bbox) = Rect::bounding(
        &movable
            .iter()
            .flat_map(|it| {
                let r = body_rect(it, it.at);
                [Point2::new(r.min_x, r.min_y), Point2::new(r.max_x, r.max_y)]
            })
            .collect::<Vec<_>>(),
    ) else {
        return;
    };
    let shifted = |dx: f64, dy: f64| Rect::new(bbox.min_x + dx, bbox.min_y + dy, bbox.max_x + dx, bbox.max_y + dy);
    let clear = |r: &Rect| !blockers.iter().any(|o| r.overlaps(o));
    if clear(&bbox) {
        return;
    }
    let landed = (1..=BLOCK_RINGS).find_map(|ring| {
        ring_offsets(ring).into_iter().find_map(|(dx, dy)| {
            let (dx, dy) = (dx as f64 * BLOCK_WALK, dy as f64 * BLOCK_WALK);
            clear(&shifted(dx, dy)).then_some((dx, dy))
        })
    });
    let Some((dx, dy)) = landed else { return };
    for it in movable.iter_mut() {
        it.at = Point2::new(
            geom::GRID_50_MIL.snap(it.at[0] + dx),
            geom::GRID_50_MIL.snap(it.at[1] + dy),
        );
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

/// The rigid translation the engine applied to the whole sheet, read off the fixed set.
fn engine_offset(placed_fixed: &[Item], live: &[Item]) -> Point2 {
    placed_fixed
        .iter()
        .zip(live)
        .next()
        .map(|(p, l)| Point2::new(p.at[0] - l.at[0], p.at[1] - l.at[1]))
        .unwrap_or(Point2::new(0.0, 0.0))
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
        design,
        items,
        fixed,
        obstacles,
        incidence,
        ir,
        engine,
        options,
        deadline,
    } = problem;

    let movable = items.len();
    let mut all = items;
    all.extend(fixed.iter().cloned());
    for it in all.iter_mut().take(movable) {
        it.frozen = false;
        it.preseeded = false;
    }
    for it in all.iter_mut().skip(movable) {
        it.frozen = true;
        it.preseeded = true;
    }

    let pin_flow = resolve_pin_flow(env, &all);
    let mut place = SchematicPlaceProblem {
        items: all,
        inc: incidence,
        ir,
        pin_flow,
        seed: sch_model::refine::SEARCH_SEED,
        options,
        deadline,
    };
    let out = {
        let (inc, intent) = (place.inc.clone(), place.ir.clone());
        let realizer = RoutedSheetRealizer::new(env, &inc, &intent);
        engine.place(&mut place, &RoutedEvaluator::new(realizer, design))
    };

    // Reverse the engines' whole-sheet `normalize` translation so the caller gets poses in
    // its own frame, then restore the neighbours bit-for-bit.
    let d = engine_offset(&place.items[movable..], &fixed);
    if d[0].abs() > EPS || d[1].abs() > EPS {
        for it in &mut place.items {
            it.at = Point2::new(it.at[0] - d[0], it.at[1] - d[1]);
        }
    }
    for (it, live) in place.items.iter_mut().skip(movable).zip(&fixed) {
        it.at = live.at;
        it.angle = live.angle;
    }

    // With nothing to avoid, the engine's own overlap handling is authoritative — walking
    // parts apart here would only reverse the placement it spent its whole search tuning.
    if !obstacles.is_empty() || !fixed.is_empty() {
        let (moved, held) = place.items.split_at_mut(movable);
        legalize(moved, held, &obstacles);
    }

    let result = {
        let realizer = RoutedSheetRealizer::new(env, &place.inc, &out.ir);
        let eval = RoutedEvaluator::new(realizer, design);
        PlaceResult {
            engine: out.result.engine,
            truthfulness_breaks: eval.truthfulness_breaks(&place.items),
            warnings: eval.warnings(&place.items),
            crossings: eval.crossings(&place.items),
            cost: out.result.cost,
        }
    };
    RegionOutput {
        ir: out.ir,
        poses: place.items[..movable]
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
