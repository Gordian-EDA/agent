//! `place::measure` — the routed-sheet MEASUREMENT library. The heavy, NON-algorithmic
//! realization an engine pays for when IT chooses a measurement-based method: build +
//! route (+ text-solve) the schematic for a candidate placement exactly as the shipped
//! emit does, then read off the raw counts. This is INFRASTRUCTURE — *how to draw and
//! measure a sheet* — not a cost, an objective, or a search. It bakes in no weights and
//! no `premium` policy: a measuring engine calls it and applies ITS OWN objective.
//!
//! Two surfaces:
//! - [`RoutedSheetRealizer`] — build + route a candidate into a [`SchematicWriter`].
//! - [`RoutedEvaluator`] — the [`CandidateEvaluator`] the engine crates ask what a
//!   candidate placement would cost. It answers in weight-free [`RawMetrics`]; the
//!   weights and the search are engine-owned.

use std::collections::BTreeMap;

use geom::{EPS, Point2, Rect};
use kicad::KicadInstallation;
use sch_check::model::Design;

use crate::write::SchematicWriter;
use circuit_graph::netclass::is_ground;
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};
use sch_model::engine::{CandidateEvaluator, CohesionPlan, RawMetrics};
use sch_model::place::Crossings;


use super::emit::{build_writer, compute_needs_flag};
use sch_model::geometry::body_overlap_count;
use sch_model::relation::{relation_group_spread, relation_viol};
use super::score::{signal_anchor_centroid, supply_pin_target, count_body_crossings, count_close_wires, count_collinear_body_crossings, count_congestion, count_corners, count_crossings, count_foreign_taps, count_ic_body_crossings, count_merges, count_parallel_body_crossings, count_shorts, count_stray};
use sch_model::geometry::{grid_order_viol, item_rect};

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
#[derive(Clone, Copy)]
pub struct RoutedSheetRealizer<'a> {
    env: &'a KicadInstallation,
    inc: &'a Incidence,
    ir: &'a LayoutIr,
    driven: &'a [String],
}

impl<'a> RoutedSheetRealizer<'a> {
    pub fn new(env: &'a KicadInstallation, inc: &'a Incidence, ir: &'a LayoutIr) -> Self {
        Self {
            env,
            inc,
            ir,
            driven: &[],
        }
    }

    /// Nets a power-output pin already drives elsewhere in the document these items
    /// are drawn into. Two power outputs on one net is an ERC error, so the realiser
    /// draws no `PWR_FLAG` for them.
    pub fn already_driven(mut self, driven: &'a [String]) -> Self {
        self.driven = driven;
        self
    }

    pub fn realize_writer(
        &self,
        title: Option<&str>,
        items: &[Item],
        mode: RouteRealization,
    ) -> std::io::Result<SchematicWriter> {
        let mut needs_flag = compute_needs_flag(self.env, items, self.ir);
        needs_flag.retain(|net| !self.driven.iter().any(|d| d == net));
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

/// The routed [`CandidateEvaluator`]: every engine question answered off a real routed
/// realization of the candidate. Bound to the design whose orphan label columns and title
/// the shipped measures include, so the trait itself stays design-free.
pub struct RoutedEvaluator<'a> {
    realizer: RoutedSheetRealizer<'a>,
    design: &'a Design,
}

impl<'a> RoutedEvaluator<'a> {
    pub fn new(realizer: RoutedSheetRealizer<'a>, design: &'a Design) -> Self {
        Self { realizer, design }
    }
}

impl CandidateEvaluator for RoutedEvaluator<'_> {
    fn measure(&self, items: &[Item]) -> RawMetrics {
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
            Err(_) => RawMetrics::unbuildable(),
        }
    }

    fn warnings(&self, items: &[Item]) -> usize {
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
    fn rendered(&self, items: &[Item]) -> Option<(usize, Rect)> {
        let mut w = self
            .realizer
            .realize_writer(
                self.design.name.as_deref(),
                items,
                RouteRealization::ShippedSheet,
            )
            .ok()?;
        super::emit::add_orphan_label_columns(&mut w, self.design, self.realizer.inc);
        w.set_frame(true);
        w.prepare();
        let warnings = w.layout_warnings().len();
        w.content_bbox().map(|r| (warnings, r))
    }

    /// `(crossings, warnings, content extent)` of the shipped sheet in ONE realize pass — the
    /// whole-placement gate (`lib.rs`) needs all three, and crossings are wire-based (invariant
    /// to the orphan label-columns + text-solve that `rendered` adds), so they share a writer.
    /// Halves the gate's realize cost vs calling `crossings` and `rendered` separately.
    fn shipped(&self, items: &[Item]) -> Option<(Crossings, usize, Rect)> {
        let mut w = self
            .realizer
            .realize_writer(
                self.design.name.as_deref(),
                items,
                RouteRealization::ShippedSheet,
            )
            .ok()?;
        let cr = shipped_crossings(self.realizer.env, &w, items);
        super::emit::add_orphan_label_columns(&mut w, self.design, self.realizer.inc);
        w.set_frame(true);
        w.prepare();
        let warnings = w.layout_warnings().len();
        w.content_bbox().map(|r| (cr, warnings, r))
    }

    fn crossings(&self, items: &[Item]) -> Crossings {
        match self
            .realizer
            .realize_writer(None, items, RouteRealization::ShippedSheet)
        {
            Ok(w) => shipped_crossings(self.realizer.env, &w, items),
            Err(_) => Crossings::default(),
        }
    }

    fn truthfulness_breaks(&self, items: &[Item]) -> usize {
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

    fn warning_messages(&self, items: &[Item]) -> Vec<String> {
        match self
            .realizer
            .realize_writer(None, items, RouteRealization::ShippedSheet)
        {
            Ok(mut w) => {
                w.set_frame(true);
                w.prepare();
                w.layout_warnings()
            }
            Err(_) => Vec::new(),
        }
    }

    fn cohesion_plans(&self, items: &[Item]) -> Vec<CohesionPlan> {
        let (env, inc, ir) = (self.realizer.env, self.realizer.inc, self.realizer.ir);
        let Ok(w) = self
            .realizer
            .realize_writer(None, items, RouteRealization::CandidateScore)
        else {
            return Vec::new();
        };
        let mut plans = Vec::new();
        for (item, s) in items.iter().enumerate() {
            if s.geom.pins.len() != 2 {
                continue;
            }
            let pos = |n: &str| {
                w.pin_dirs(env, &s.refdes, n)
                    .ok()
                    .and_then(|v| v.first().map(|x| x.0))
            };
            let (Some(p0), Some(p1)) = (pos(&s.geom.pins[0].number), pos(&s.geom.pins[1].number))
            else {
                continue;
            };
            let vertical = (p0[1] - p1[1]).abs() >= (p0[0] - p1[0]).abs();
            if let Some(target) = signal_anchor_centroid(env, &w, items, inc, ir, s, false)
                .or_else(|| supply_pin_target(env, &w, items, inc, ir, s))
            {
                plans.push(CohesionPlan {
                    item,
                    vertical,
                    target,
                });
            }
        }
        plans
    }

    fn with_ir<'a>(&'a self, ir: &'a LayoutIr) -> Box<dyn CandidateEvaluator + 'a> {
        Box::new(RoutedEvaluator {
            realizer: RoutedSheetRealizer { ir, ..self.realizer },
            design: self.design,
        })
    }
}

/// Body / IC / wire crossing triple read from a shipped (`fan_risers=true`) writer.
pub(crate) fn shipped_crossings(
    env: &KicadInstallation,
    w: &SchematicWriter,
    items: &[Item],
) -> Crossings {
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

fn bodies_and_ic_rects(
    env: &KicadInstallation,
    w: &SchematicWriter,
    items: &[Item],
) -> BodyObstacles {
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
    env: &KicadInstallation,
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
        relation: relation_viol(items, ir),
        group_spread: relation_group_spread(items, ir),
    }
}
