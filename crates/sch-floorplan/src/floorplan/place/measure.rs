//! `place::measure` — the routed-sheet MEASUREMENT library. The heavy, NON-algorithmic
//! realization an engine pays for when IT chooses a measurement-based method: build +
//! route (+ text-solve) the schematic for a candidate placement exactly as the shipped
//! emit does, then read off the raw counts. This is INFRASTRUCTURE — *how to draw and
//! measure a sheet* — not a cost, an objective, or a search. It bakes in no weights and
//! no `premium` policy: a measuring engine calls it and applies ITS OWN objective.
//!
//! Two surfaces:
//! - [`RoutedSheetRealizer`] — build + route a candidate into a [`SchematicWriter`].
//! - [`RoutedEvaluator`] — what a placement MEASURES once it is drawn: its truthfulness
//!   breaks, its readability warnings and its crossings.


use geom::{Point2, Rect};
use kicad::KicadInstallation;
use sch_check::model::Design;

use crate::write::SchematicWriter;
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};
use sch_model::place::Crossings;

use super::emit::{build_writer, compute_needs_flag};
use super::score::{
    count_body_crossings, count_collinear_body_crossings, count_crossings, count_foreign_taps, count_ic_body_crossings, count_merges,
    count_parallel_body_crossings, count_shorts,
};

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
    beside: Option<&'a sch_model::route::RouteScene>,
}

impl<'a> RoutedSheetRealizer<'a> {
    pub fn new(env: &'a KicadInstallation, inc: &'a Incidence, ir: &'a LayoutIr) -> Self {
        Self {
            env,
            inc,
            ir,
            driven: &[],
            beside: None,
        }
    }

    /// The drawing these items are being added BESIDE — the existing sheet's pins,
    /// wires and label anchors with the nets they carry. Without it the router draws
    /// this block's wires across the sheet's own geometry and welds nets it cannot see.
    pub fn beside(mut self, scene: &'a sch_model::route::RouteScene) -> Self {
        self.beside = Some(scene);
        self
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
            self.beside.cloned().unwrap_or_default(),
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

impl RoutedEvaluator<'_> {
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
    pub fn rendered(&self, items: &[Item]) -> Option<(usize, Rect)> {
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
    pub fn shipped(&self, items: &[Item]) -> Option<(Crossings, usize, Rect)> {
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

    pub fn warning_messages(&self, items: &[Item]) -> Vec<String> {
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
