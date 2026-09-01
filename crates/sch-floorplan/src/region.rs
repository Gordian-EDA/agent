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
//! - **Fixed neighbours do not move.** An [`Item`] that arrives `frozen` keeps its live
//!   pose through seeding, search, and the overlap relaxers. The engines still `normalize`
//!   the sheet — a rigid translation — so the adapter measures that offset off the fixed
//!   set and takes it back out, returning poses in the caller's own frame.
//! - **Nothing lands on an obstacle.** Engines have no obstacle vocabulary (their only
//!   geometry is the items they place), so the adapter legalises afterwards: any movable
//!   part overlapping an obstacle, a fixed neighbour, or another movable part is walked
//!   out to the nearest clear grid position. With no obstacles this is a no-op.

use geom::{EPS, Point2, Rect};

use sch_check::Design;
use kicad::KicadInstallation;
use sch_place::ir::LayoutIr;
use sch_place::item::{Incidence, Item};
use sch_place::place::{PlaceOptions, PlaceResult};

use crate::contract::{PlacementEngine, RoutedEvaluator, RoutedSheetRealizer};
use crate::floorplan::place::{SchematicPlaceProblem, incidence, item_rect};

/// Step of the legalisation walk (100 mil — two schematic grid steps).
const WALK: f64 = 2.0 * geom::GRID_50_MIL.pitch();
/// How far the legalisation walk may push a part before giving up.
const WALK_RINGS: i32 = 120;

/// A region placement request: which parts to move, which to respect, and what else is
/// in the way.
pub struct RegionProblem<'a> {
    pub env: &'a KicadInstallation,
    pub design: &'a Design,
    /// The movable set — the parts to place.
    pub items: Vec<Item>,
    /// Neighbours at their LIVE positions. Forced `frozen`; they are never moved.
    pub fixed: Vec<Item>,
    /// Everything else on the sheet the placement must avoid: label boxes, wires'
    /// keepouts, other sheets' furniture — anything with no [`Item`] to speak for it.
    pub obstacles: Vec<Rect>,
    /// Net → pins over `items` followed by `fixed`. [`RegionProblem::new`] builds it.
    pub incidence: Incidence,
    pub ir: LayoutIr,
    pub engine: &'a dyn PlacementEngine,
    pub options: PlaceOptions,
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
        }
    }
}

/// Whether `r` clears every obstacle and every rect in `others`.
fn clear_of(r: &Rect, obstacles: &[Rect], others: &[Rect]) -> bool {
    !obstacles.iter().any(|o| r.overlaps(o)) && !others.iter().any(|o| r.overlaps(o))
}

/// Offsets on the ring `max(|dx|, |dy|) == ring`, nearest-first and deterministic.
fn ring_offsets(ring: i32) -> Vec<(i32, i32)> {
    let mut out: Vec<(i32, i32)> = (-ring..=ring)
        .flat_map(|dx| (-ring..=ring).map(move |dy| (dx, dy)))
        .filter(|(dx, dy)| dx.abs().max(dy.abs()) == ring)
        .collect();
    out.sort_by_key(|(dx, dy)| (dx.abs() + dy.abs(), *dx, *dy));
    out
}

/// Walk each movable part off any obstacle, fixed neighbour, or already-legalised
/// neighbour, to the nearest clear grid position. Deterministic; parts that are already
/// clear never move. A part with nowhere to go within [`WALK_RINGS`] keeps its position
/// (the caller sees the overlap rather than a part flung across the sheet).
fn legalize(movable: &mut [Item], fixed: &[Item], obstacles: &[Rect]) {
    let mut taken: Vec<Rect> = fixed.iter().map(|it| item_rect(it, it.at)).collect();
    let mut rest = movable;
    while let Some((item, tail)) = rest.split_first_mut() {
        let others: Vec<Rect> = taken
            .iter()
            .copied()
            .chain(tail.iter().map(|it| item_rect(it, it.at)))
            .collect();
        if !clear_of(&item_rect(item, item.at), obstacles, &others) {
            let from: [f64; 2] = item.at.into();
            let landed = (1..=WALK_RINGS).find_map(|ring| {
                ring_offsets(ring).into_iter().find_map(|(dx, dy)| {
                    let at = Point2::new(
                        geom::GRID_50_MIL.snap(from[0] + dx as f64 * WALK),
                        geom::GRID_50_MIL.snap(from[1] + dy as f64 * WALK),
                    );
                    clear_of(&item_rect(item, at), obstacles, &others).then_some(at)
                })
            });
            if let Some(at) = landed {
                item.at = at;
            }
        }
        taken.push(item_rect(item, item.at));
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
    } = problem;

    let movable = items.len();
    let mut all = items;
    all.extend(fixed.iter().cloned());
    for it in all.iter_mut().skip(movable) {
        it.frozen = true;
    }

    let mut place = SchematicPlaceProblem {
        items: all,
        inc: incidence,
        seed: crate::floorplan::place::SEARCH_SEED,
        options,
    };
    let out = engine.place(env, design, &mut place, Some(ir));

    // Undo the engines' whole-sheet `normalize` translation so the caller gets poses in
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

    let (moved, held) = place.items.split_at_mut(movable);
    legalize(moved, held, &obstacles);

    let realizer = RoutedSheetRealizer::new(env, &place.inc, &out.ir, options);
    let eval = RoutedEvaluator::new(&realizer);
    let result = PlaceResult {
        engine: out.result.engine,
        truthfulness_breaks: eval.truthfulness_breaks(&place.items),
        warnings: eval.warnings(&place.items),
        crossings: eval.crossings(&place.items),
        cost: out.result.cost,
    };
    RegionOutput {
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
