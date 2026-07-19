//! `place::measure` — the routed-sheet MEASUREMENT library. The heavy, NON-algorithmic
//! realization an engine pays for when IT chooses a measurement-based method: build +
//! route (+ text-solve) the schematic for a candidate placement exactly as the shipped
//! emit does, then read off the raw counts. This is INFRASTRUCTURE — *how to draw and
//! measure a sheet* — not a cost, an objective, or a search. It bakes in no weights and
//! no `premium` policy: a measuring engine calls it and applies ITS OWN objective.
//!
//! Three surfaces:
//! - [`RoutedSheetRealizer`] — build + route a candidate into a [`SchematicWriter`].
//! - [`RoutedEvaluator`] — read metrics from routed realizations.
//! - [`RawMetrics`] — the 16 raw count/length terms, weight-free. An engine's objective
//!   multiplies these by its own weights and sums them.

use std::collections::BTreeMap;

use circuit_lang::model::Design;
use geom::{EPS, Point2, Rect};
use kicad_env::KicadEnv;

use crate::write::SchematicWriter;
use sch_place::ir::LayoutIr;
use sch_place::item::{Incidence, Item};
use circuit_graph::netclass::is_ground;
use sch_place::place::Crossings;

use sch_place::place::PlaceResult;

use super::emit::{build_writer, compute_needs_flag};
use super::problem::SchematicPlaceProblem;
use super::score::{
    count_body_crossings, count_close_wires, count_collinear_body_crossings, count_congestion,
    count_corners, count_crossings, count_foreign_taps, count_ic_body_crossings, count_merges,
    count_parallel_body_crossings, count_shorts, count_stray, grid_order_viol, item_rect,
};
use super::*;

/// A schematic placement+routing ENGINE: searches over a neutral
/// [`SchematicPlaceProblem`] plus optional layout intent, and writes the final
/// placement into `problem.items`.
///
/// ## Contract
/// - **Deterministic given the problem.** No clock; a fixed `seed` reproduces.
/// - **Never panics.** A unit it cannot place reports through the result's counts, never
///   by unwinding.
/// - The returned [`PlaceResult`] describes the FINAL `problem.items` — the placement
///   the caller will ship.
pub trait PlacementEngine {
    /// Open provenance: the engine's stable name (e.g. `"anneal"`, `"constraint"`).
    fn name(&self) -> &'static str;

    /// Search placement+routing and write the final geometry into `problem.items`.
    ///
    /// Engines may use `ir` as hints/constraints, or ignore it entirely.
    fn place(
        &self,
        env: &KicadEnv,
        design: &Design,
        problem: &mut SchematicPlaceProblem,
        ir: Option<LayoutIr>,
    ) -> PlacementOutput;
}

/// Result of engine-owned placement orchestration: final item geometry lives in
/// `problem.items`; `ir` is the sheet intent the routed realizer should use.
pub struct PlacementOutput {
    pub result: PlaceResult,
    pub ir: LayoutIr,
}

/// Which routed realization to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRealization {
    /// Fast objective build used while scoring candidate moves.
    CandidateScore,
    /// Final shipped-sheet build, including riser fanning repairs.
    ShippedSheet,
}

impl RouteRealization {
    fn fan_risers(self) -> bool {
        matches!(self, Self::ShippedSheet)
    }
}

/// Builds/routes a candidate placement into a [`SchematicWriter`].
pub struct RoutedSheetRealizer<'a> {
    env: &'a KicadEnv,
    inc: &'a Incidence,
    ir: &'a LayoutIr,
}

impl<'a> RoutedSheetRealizer<'a> {
    pub fn new(env: &'a KicadEnv, inc: &'a Incidence, ir: &'a LayoutIr) -> Self {
        Self { env, inc, ir }
    }

    pub fn env(&self) -> &'a KicadEnv {
        self.env
    }

    pub fn realize_writer(
        &self,
        title: Option<&str>,
        items: &[Item],
        mode: RouteRealization,
    ) -> std::io::Result<SchematicWriter> {
        let needs_flag = compute_needs_flag(self.env, items, self.ir);
        build_writer(
            self.env,
            title,
            items,
            self.inc,
            self.ir,
            &needs_flag,
            mode.fan_risers(),
        )
    }
}

/// Routed metrics for candidate placements.
pub struct RoutedEvaluator<'a> {
    realizer: &'a RoutedSheetRealizer<'a>,
}

impl<'a> RoutedEvaluator<'a> {
    pub fn new(realizer: &'a RoutedSheetRealizer<'a>) -> Self {
        Self { realizer }
    }

    pub fn measure(&self, items: &[Item]) -> RawMetrics {
        match self
            .realizer
            .realize_writer(None, items, RouteRealization::CandidateScore)
        {
            Ok(w) => raw_metrics(
                self.realizer.env,
                &w,
                items,
                self.realizer.inc,
                self.realizer.ir,
            ),
            Err(_) => RawMetrics {
                fallbacks: usize::MAX,
                junctions: usize::MAX,
                length: f64::INFINITY,
                crossings: usize::MAX,
                corners: usize::MAX,
                merges: usize::MAX,
                overlaps: usize::MAX,
                congestion: usize::MAX,
                body_cross: usize::MAX,
                stray: f64::INFINITY,
                orient_viol: usize::MAX,
                leg_viol: usize::MAX,
                spine_viol: usize::MAX,
                spread: f64::INFINITY,
                grid_order: usize::MAX,
                sib_spread: f64::INFINITY,
            },
        }
    }

    pub fn warnings(&self, items: &[Item]) -> usize {
        match self
            .realizer
            .realize_writer(None, items, RouteRealization::ShippedSheet)
        {
            Ok(mut w) => {
                w.set_frame(true);
                w.prepare();
                w.layout_warnings().len()
            }
            Err(_) => usize::MAX,
        }
    }

    /// The `(warning count, content extent)` of `items` AS THE EMIT SHIPS IT — realized with
    /// the emit's orphan label-columns added (edge labels for cross-ref nets), then text-solved
    /// and reframed. The only faithful measure of a placement's final sprawl AND warnings: a
    /// gate reading the raw pre-emit geometry misses the orphan columns, which both balloon a
    /// dense board's bbox and collide into warnings. One realize pass serves both. `None` if
    /// the route can't be built or the sheet is empty.
    pub fn rendered(&self, design: &Design, items: &[Item]) -> Option<(usize, Rect)> {
        let mut w = self
            .realizer
            .realize_writer(
                design.name.as_deref(),
                items,
                RouteRealization::ShippedSheet,
            )
            .ok()?;
        super::emit::add_orphan_label_columns(&mut w, design, self.realizer.inc);
        w.set_frame(true);
        w.prepare();
        let warnings = w.layout_warnings().len();
        w.content_bbox().map(|r| (warnings, r))
    }

    /// `(crossings, warnings, content extent)` of the shipped sheet in ONE realize pass — the
    /// whole-placement gate (`lib.rs`) needs all three, and crossings are wire-based (invariant
    /// to the orphan label-columns + text-solve that `rendered` adds), so they share a writer.
    /// Halves the gate's realize cost vs calling `crossings` and `rendered` separately.
    pub fn shipped(&self, design: &Design, items: &[Item]) -> Option<(Crossings, usize, Rect)> {
        let mut w = self
            .realizer
            .realize_writer(
                design.name.as_deref(),
                items,
                RouteRealization::ShippedSheet,
            )
            .ok()?;
        let cr = shipped_crossings(self.realizer.env, &w, items);
        super::emit::add_orphan_label_columns(&mut w, design, self.realizer.inc);
        w.set_frame(true);
        w.prepare();
        let warnings = w.layout_warnings().len();
        w.content_bbox().map(|r| (cr, warnings, r))
    }

    pub fn crossings(&self, items: &[Item]) -> Crossings {
        match self
            .realizer
            .realize_writer(None, items, RouteRealization::ShippedSheet)
        {
            Ok(w) => shipped_crossings(self.realizer.env, &w, items),
            Err(_) => Crossings::default(),
        }
    }

    pub fn truthfulness_breaks(&self, items: &[Item]) -> usize {
        match self
            .realizer
            .realize_writer(None, items, RouteRealization::ShippedSheet)
        {
            Ok(w) => {
                let wires = w.wires_with_nets();
                count_merges(&wires, &w.junction_positions())
                    + count_shorts(self.realizer.env, &w, items, self.realizer.inc, &wires)
                    + count_foreign_taps(&wires)
            }
            Err(_) => usize::MAX,
        }
    }
}

/// The raw, weight-FREE measurements of a routed candidate placement — the 16 terms an
/// engine's objective combines under its own weights. Built by [`SchematicPlaceProblem::measure`]
/// from the `fan_risers=false` routed build (the per-move objective build, which differs
/// from the shipped `fan_risers=true` sheet `warnings`/`crossings` measure). Splitting
/// the raw extraction (shared infrastructure) from the weighting (engine-owned method) is
/// what lets greedy and anneal own genuinely different objectives over one realization.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RawMetrics {
    /// Signal-label fallbacks (a wire degraded to a label).
    pub fallbacks: usize,
    /// Junction dots.
    pub junctions: usize,
    /// Total Manhattan wire length (mm).
    pub length: f64,
    /// Visual wire-wire crossings between different nets.
    pub crossings: usize,
    /// Wire corners (L-bends).
    pub corners: usize,
    /// Net merges + placement shorts + foreign taps (hard truthfulness failures).
    pub merges: usize,
    /// Body-overlap pairs + symbol/label-box collisions.
    pub overlaps: usize,
    /// Cramped junction/wire/body proximity.
    pub congestion: usize,
    /// Wires routed through a part body (2-pin transverse/collinear/parallel + IC).
    pub body_cross: usize,
    /// Stray distance: satellites far from the anchor pins they wire to.
    pub stray: f64,
    /// 2-pin parts on the unconventional axis (series/decoupling/leg orientation).
    pub orient_viol: usize,
    /// 1-rail pull/leg orientation+direction violations (the premium-boosted subset).
    pub leg_viol: usize,
    /// Divider/totem spine pairs not drawn in one column.
    pub spine_viol: usize,
    /// Bounding-box half-perimeter of all part bodies (compactness).
    pub spread: f64,
    /// Authored per-block `layout:` relative-order violations.
    pub grid_order: usize,
    /// Same-refdes (multi-unit) bounding-box spread (cohesion).
    pub sib_spread: f64,
}

/// Body / IC / wire crossing triple read from a shipped (`fan_risers=true`) writer.
pub(crate) fn shipped_crossings(env: &KicadEnv, w: &SchematicWriter, items: &[Item]) -> Crossings {
    let wires = w.wires_with_nets();
    let (bodies, ic_rects) = bodies_and_ic_rects(env, w, items);
    Crossings {
        body: count_body_crossings(&bodies, &wires)
            + count_collinear_body_crossings(&bodies, &wires)
            + count_parallel_body_crossings(&bodies, &wires),
        ic: count_ic_body_crossings(&ic_rects, &wires),
        wire: count_crossings(&wires),
    }
}

/// The 2-pin part body axes and the IC (3+ pin) body-interior rects of a placed item set
/// — the wire-through-body obstacles. Shared by the routed measurements so every
/// crossing count extracts identical geometry.
type BodyAxis = ([f64; 2], [f64; 2]);
type BodyObstacles = (Vec<BodyAxis>, Vec<Rect>);

fn bodies_and_ic_rects(env: &KicadEnv, w: &SchematicWriter, items: &[Item]) -> BodyObstacles {
    let bodies: Vec<BodyAxis> = items
        .iter()
        .filter(|i| i.geom.pins.len() == 2)
        .filter_map(|it| {
            let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
            match (
                w.pin_dirs(env, &it.refdes, n0),
                w.pin_dirs(env, &it.refdes, n1),
            ) {
                (Ok(d0), Ok(d1)) => match (d0.first(), d1.first()) {
                    (Some((a, _)), Some((b, _))) => Some((*a, *b)),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect();
    let ic_rects: Vec<Rect> = items
        .iter()
        .filter(|it| it.geom.pins.len() >= 3)
        .filter_map(|it| {
            let mut pts = Vec::new();
            // Only THIS unit's pins: a multi-unit part places one Item per unit,
            // and `pin_dirs` resolves a foreign unit's pin number to that OTHER
            // instance's position — bounding all units would fabricate a rect
            // spanning every placed unit (a sheet-wide phantom "body").
            for pg in &it.geom.pins {
                if pg.unit.max(1) != it.unit.max(1) {
                    continue;
                }
                if let Ok(d) = w.pin_dirs(env, &it.refdes, &pg.number)
                    && let Some((p, _)) = d.first()
                {
                    pts.push(Point2::from(*p));
                }
            }
            Rect::bounding(&pts)
                .map(|r| Rect::new(r.min_x + 2.0, r.min_y + 2.0, r.max_x - 2.0, r.max_y - 2.0))
        })
        .collect();
    (bodies, ic_rects)
}

/// Read the raw 16 measurement terms off a built (`fan_risers=false`) writer,
/// returned unweighted for an engine to weight.
pub fn raw_metrics(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
) -> RawMetrics {
    let fallbacks = w.signal_label_count();
    let junctions = w.junction_count();
    let wires = w.wires_with_nets();
    let length: f64 = wires
        .iter()
        .map(|wire| wire.segment.a.manhattan(wire.segment.b))
        .sum();
    let crossings = count_crossings(&wires);
    let corners = count_corners(&wires);
    let merges = count_merges(&wires, &w.junction_positions())
        + count_shorts(env, w, items, inc, &wires)
        + count_foreign_taps(&wires);
    let label_boxes = w.cluster_label_boxes();
    let overlaps = body_overlap_count(items)
        + items
            .iter()
            .filter(|it| {
                let r = item_rect(it, it.at);
                label_boxes.iter().any(|b| r.overlaps(b))
            })
            .count();
    let (bodies, ic_rects) = bodies_and_ic_rects(env, w, items);
    let congestion = count_congestion(&w.junction_positions()) + count_close_wires(&wires, &bodies);
    let body_cross = count_body_crossings(&bodies, &wires)
        + count_collinear_body_crossings(&bodies, &wires)
        + count_parallel_body_crossings(&bodies, &wires)
        + count_ic_body_crossings(&ic_rects, &wires);
    let stray = count_stray(env, w, items, inc, ir);
    let mut orient_viol = 0usize;
    let mut leg_viol = 0usize;
    for it in items.iter().filter(|i| i.geom.pins.len() == 2) {
        let rail_count = it
            .pins
            .iter()
            .filter(|(_, _, n)| n.as_deref().is_some_and(|n| ir.rails.contains_key(n)))
            .count();
        let prefer_vertical: Option<bool> = match rail_count {
            0 => Some(false),
            2 => Some(true),
            _ => None,
        };
        let Some(prefer_vertical) = prefer_vertical else {
            continue;
        };
        let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
        if let (Ok(d0), Ok(d1)) = (
            w.pin_dirs(env, &it.refdes, n0),
            w.pin_dirs(env, &it.refdes, n1),
        ) && let (Some((a, _)), Some((b, _))) = (d0.first(), d1.first())
        {
            let horizontal = (a[0] - b[0]).abs() > (a[1] - b[1]).abs();
            if prefer_vertical == horizontal {
                orient_viol += 1;
                if rail_count == 1 {
                    leg_viol += 1;
                }
            } else if rail_count == 1 && prefer_vertical {
                let net_of = |pn: &str| {
                    it.pins
                        .iter()
                        .find(|(p, _, _)| p == pn)
                        .and_then(|(_, _, n)| n.as_deref())
                };
                let n0_rail = net_of(n0).is_some_and(|n| ir.rails.contains_key(n));
                let rail = if n0_rail { net_of(n0) } else { net_of(n1) };
                if let Some(rn) = rail {
                    let (rail_pos, other_pos) = if n0_rail { (a, b) } else { (b, a) };
                    let rail_up = rail_pos[1] < other_pos[1] - EPS;
                    if is_ground(rn) == rail_up {
                        leg_viol += 1;
                    }
                }
            }
        }
    }
    let legs: Vec<(usize, Vec<&str>, bool)> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| it.geom.pins.len() == 2)
        .map(|(i, it)| {
            let nets: Vec<&str> = it
                .pins
                .iter()
                .filter_map(|(_, _, n)| n.as_deref())
                .collect();
            let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
            let vertical = match (
                w.pin_dirs(env, &it.refdes, n0),
                w.pin_dirs(env, &it.refdes, n1),
            ) {
                (Ok(d0), Ok(d1)) => match (d0.first(), d1.first()) {
                    (Some((a, _)), Some((b, _))) => (a[1] - b[1]).abs() > (a[0] - b[0]).abs(),
                    _ => false,
                },
                _ => false,
            };
            (i, nets, vertical)
        })
        .collect();
    let is_spine = |a: &[&str], b: &[&str]| -> bool {
        let is_rail = |n: &str| ir.rails.contains_key(n);
        let Some(node) = a.iter().copied().find(|n| b.contains(n) && !is_rail(n)) else {
            return false;
        };
        let ra = a.iter().copied().find(|n| *n != node && is_rail(n));
        let rb = b.iter().copied().find(|n| *n != node && is_rail(n));
        matches!((ra, rb), (Some(x), Some(y)) if x != y)
    };
    let mut spine_viol = 0usize;
    for a in 0..legs.len() {
        for b in (a + 1)..legs.len() {
            let (ia, na, va) = (legs[a].0, &legs[a].1, legs[a].2);
            let (ib, nb, vb) = (legs[b].0, &legs[b].1, legs[b].2);
            let cap = |i: usize| items[i].refdes.starts_with('C');
            if va
                && vb
                && !cap(ia)
                && !cap(ib)
                && is_spine(na, nb)
                && (items[ia].at[0] - items[ib].at[0]).abs() > EPS
            {
                spine_viol += 1;
            }
        }
    }
    let mut body_corners = Vec::new();
    for it in items {
        let r = item_rect(it, it.at);
        body_corners.push(Point2::new(r.min_x, r.min_y));
        body_corners.push(Point2::new(r.max_x, r.max_y));
    }
    let spread = Rect::bounding(&body_corners).map_or(0.0, |r| r.half_perimeter());
    let grid_order = grid_order_viol(items, ir);
    let mut by_refdes: BTreeMap<&str, Vec<Point2>> = BTreeMap::new();
    for it in items {
        by_refdes.entry(&it.refdes).or_default().push(it.at);
    }
    let sib_spread: f64 = by_refdes
        .values()
        .filter_map(|pts| Rect::bounding(pts))
        .map(|r| r.half_perimeter())
        .sum();
    RawMetrics {
        fallbacks,
        junctions,
        length,
        crossings,
        corners,
        merges,
        overlaps,
        congestion,
        body_cross,
        stray,
        orient_viol,
        leg_viol,
        spine_viol,
        spread,
        grid_order,
        sib_spread,
    }
}
