//! A router-free [`CandidateEvaluator`] so a placement leaf can be developed, tested and
//! benchmarked with no KiCAD installation and no realiser.
//!
//! It is a TEST DOUBLE, not a second cost model: it answers every question from item
//! geometry alone (half-perimeter net length, body overlaps, extent), reports zero
//! truthfulness breaks, and never routes. A search driven by it converges on something
//! plausible and, crucially, deterministically — which is what a contract test and a
//! `cargo run --example bench` need. Ship quality is measured only against the real
//! routed evaluator in `sch-floorplan`.

use std::collections::BTreeMap;

use geom::{Point2, Rect};

use crate::engine::{CandidateEvaluator, CohesionPlan, RawMetrics};
use crate::geometry::{body_overlap_count, grid_order_viol, item_rect};
use crate::ir::LayoutIr;
use crate::item::{Incidence, Item};
use crate::place::Crossings;
use crate::relation::{relation_group_spread, relation_viol};

/// Geometry-only stand-in for the routed evaluator.
pub struct StubEvaluator<'a> {
    inc: &'a Incidence,
    ir: &'a LayoutIr,
}

impl<'a> StubEvaluator<'a> {
    pub fn new(inc: &'a Incidence, ir: &'a LayoutIr) -> Self {
        Self { inc, ir }
    }

    /// Total half-perimeter of every net's terminal bounding box — the classic
    /// placement proxy for wire length, standing in for the routed measure.
    fn hpwl(&self, items: &[Item]) -> f64 {
        self.inc
            .values()
            .filter_map(|pins| {
                let pts: Vec<Point2> = pins.iter().map(|(i, _)| items[*i].at).collect();
                Rect::bounding(&pts).map(|r| r.half_perimeter())
            })
            .sum()
    }

    fn extent(&self, items: &[Item]) -> Option<Rect> {
        let mut corners = Vec::with_capacity(items.len() * 2);
        for it in items {
            let r = item_rect(it, it.at);
            corners.push(Point2::new(r.min_x, r.min_y));
            corners.push(Point2::new(r.max_x, r.max_y));
        }
        Rect::bounding(&corners)
    }
}

impl CandidateEvaluator for StubEvaluator<'_> {
    fn measure(&self, items: &[Item]) -> RawMetrics {
        let overlaps = body_overlap_count(items);
        let spread = self.extent(items).map_or(0.0, |r| r.half_perimeter());
        let mut by_refdes: BTreeMap<&str, Vec<Point2>> = BTreeMap::new();
        for it in items {
            by_refdes.entry(&it.refdes).or_default().push(it.at);
        }
        RawMetrics {
            fallbacks: 0,
            junctions: 0,
            length: self.hpwl(items),
            crossings: 0,
            corners: 0,
            merges: 0,
            overlaps,
            congestion: 0,
            body_cross: 0,
            stray: 0.0,
            orient_viol: 0,
            leg_viol: 0,
            spine_viol: 0,
            spread,
            grid_order: grid_order_viol(items, self.ir),
            sib_spread: by_refdes
                .values()
                .filter_map(|pts| Rect::bounding(pts))
                .map(|r| r.half_perimeter())
                .sum(),
            relation: relation_viol(items, self.ir),
            group_spread: relation_group_spread(items, self.ir),
        }
    }

    fn warnings(&self, items: &[Item]) -> usize {
        body_overlap_count(items)
    }

    fn warning_messages(&self, items: &[Item]) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                if item_rect(&items[i], items[i].at).overlaps(&item_rect(&items[j], items[j].at)) {
                    out.push(format!("{} overlaps {}", items[i].refdes, items[j].refdes));
                }
            }
        }
        out
    }

    fn crossings(&self, _items: &[Item]) -> Crossings {
        Crossings::default()
    }

    fn truthfulness_breaks(&self, _items: &[Item]) -> usize {
        0
    }

    fn rendered(&self, items: &[Item]) -> Option<(usize, Rect)> {
        self.extent(items).map(|r| (self.warnings(items), r))
    }

    fn shipped(&self, items: &[Item]) -> Option<(Crossings, usize, Rect)> {
        self.extent(items)
            .map(|r| (Crossings::default(), self.warnings(items), r))
    }

    fn cohesion_plans(&self, items: &[Item]) -> Vec<CohesionPlan> {
        let mut plans = Vec::new();
        for (item, s) in items.iter().enumerate() {
            if s.geom.pins.len() != 2 {
                continue;
            }
            let anchors: Vec<Point2> = s
                .pins
                .iter()
                .filter_map(|(_, _, net)| net.as_deref())
                .filter(|net| !self.ir.rails.contains_key(*net))
                .flat_map(|net| self.inc.get(net).into_iter().flatten())
                .filter(|(j, _)| items[*j].geom.pins.len() >= 3)
                .map(|(j, _)| items[*j].at)
                .collect();
            if anchors.is_empty() {
                continue;
            }
            let n = anchors.len() as f64;
            let target = [
                anchors.iter().map(|p| p.x).sum::<f64>() / n,
                anchors.iter().map(|p| p.y).sum::<f64>() / n,
            ];
            plans.push(CohesionPlan {
                item,
                vertical: ((s.angle / 90.0).round() as i64).rem_euclid(2) == 1,
                target,
            });
        }
        plans
    }

    fn with_ir<'a>(&'a self, ir: &'a LayoutIr) -> Box<dyn CandidateEvaluator + 'a> {
        Box::new(StubEvaluator { inc: self.inc, ir })
    }
}
