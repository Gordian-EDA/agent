//! [`RoutedCost`] — the one concrete [`PlacementCost`] implementation, the routed
//! scorer the placement engines minimise. It captures the fixed-per-problem scoring
//! state (`KicadEnv`, connectivity, intent IR, the ERC `needs_flag`) and delegates
//! each method to the routed `score` primitives in [`super::score`], so the engines
//! (`greedy-place`, `anneal-place`) score candidates through the [`PlacementCost`]
//! trait boundary instead of importing this crate's scorers.
//!
//! The agent constructs it and injects `&dyn PlacementCost` into the
//! [`PlaceProblem`], exactly as it injects the `Box<dyn PlacementEngine>`.

use std::collections::BTreeSet;

use kicad_cli_rs::env::KicadEnv;
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};
use sch_model::place::{Crossings, PlacementCost};

use super::score::{
    crossing_counts, premium_score_items, premium_score_with_w, score_items, truthfulness_breaks,
    warning_count,
};

/// The routed [`PlacementCost`]: scores a candidate by building + routing (+ text-
/// solving) the schematic and pricing the result, exactly as the shipped emit does.
/// Borrows the problem's scoring state for the lifetime of one placement search.
pub struct RoutedCost<'a> {
    env: &'a KicadEnv,
    inc: &'a Incidence,
    ir: &'a LayoutIr,
    needs_flag: &'a BTreeSet<String>,
}

impl<'a> RoutedCost<'a> {
    /// Capture the per-problem scoring state. `needs_flag` is the ERC PWR_FLAG set
    /// computed up front by the emit pipeline (`compute_needs_flag`).
    pub fn new(
        env: &'a KicadEnv,
        inc: &'a Incidence,
        ir: &'a LayoutIr,
        needs_flag: &'a BTreeSet<String>,
    ) -> Self {
        Self { env, inc, ir, needs_flag }
    }
}

impl PlacementCost for RoutedCost<'_> {
    fn cost(&self, items: &[Item]) -> f64 {
        score_items(self.env, items, self.inc, self.ir, self.needs_flag)
    }

    fn premium_cost_with_warnings(&self, items: &[Item], warnings: usize) -> f64 {
        premium_score_with_w(self.env, items, self.inc, self.ir, self.needs_flag, warnings)
    }

    fn premium_cost(&self, items: &[Item]) -> f64 {
        premium_score_items(self.env, items, self.inc, self.ir, self.needs_flag)
    }

    fn warnings(&self, items: &[Item]) -> usize {
        warning_count(self.env, items, self.inc, self.ir, self.needs_flag)
    }

    fn crossings(&self, items: &[Item]) -> Crossings {
        let (body, ic, wire) = crossing_counts(self.env, items, self.inc, self.ir, self.needs_flag);
        Crossings { body, ic, wire }
    }

    fn truthfulness_breaks(&self, items: &[Item]) -> usize {
        truthfulness_breaks(self.env, items, self.inc, self.ir, self.needs_flag)
    }
}
