//! `cluster-place` — a schematic placement engine that adds the one lever the
//! per-component simulated annealer structurally lacks: **hub pose**.
//!
//! The SA's move set only TRANSLATES an anchor (carrying its block) and re-orients 2-pin
//! satellites; an IC's `angle`/`mirror` are seeded once from the heuristic IR and never
//! searched. Yet which way an IC faces — its rotation and its left↔right mirror — decides
//! whether its pins meet their neighbours head-on or force the wires to wrap around the
//! body and cross. This engine takes the SA's placement and then searches each hub's 8
//! poses ([`pose`]), moving the hub AND its satellite cluster RIGIDLY (an exact D4-group
//! transform) so the decoupling caps / pull-ups follow the rotated pins. A pose is kept
//! ONLY when it strictly cuts the shipped (truthfulness, warnings, crossings), so the
//! result is strictly additive — never worse than the SA, better where an IC was facing
//! the wrong way.
//!
//! What this engine deliberately does NOT do is part-level COMPACTION. Pulling stranded
//! satellites tight to their hub congests the labels every time (the additive gate then
//! reverts it) — the clean-vs-compact Pareto wall the SA already sits against. That
//! scaffold lives in [`compact`] behind `CLUSTER_COMPACT` for the standalone-floorplanner
//! work (where modules would carry reserved label-inclusive footprints), off by default.
//!
//! It owns its objective and search; it measures candidates through `sch-floorplan`'s
//! [`RoutedEvaluator`] and implements the published [`PlacementEngine`] trait.

mod compact;
mod eval;
mod pose;

use circuit_lang::model::Design;
use sch_place::ir::LayoutIr;
use sch_place::item::Item;
use sch_place::place::{Crossings, PlaceResult};

use sch_floorplan::contract::{
    KicadEnv, PlacementEngine, PlacementOutput, RoutedEvaluator, RoutedSheetRealizer,
    SchematicPlaceProblem,
};

/// Cluster-pose placement: the SA's leaf seating + a strictly-additive rigid hub-pose search.
pub struct ClusterPlace;

impl PlacementEngine for ClusterPlace {
    fn name(&self) -> &'static str {
        "cluster"
    }

    fn place(
        &self,
        env: &KicadEnv,
        design: &Design,
        problem: &mut SchematicPlaceProblem,
        ir: Option<LayoutIr>,
    ) -> PlacementOutput {
        // 1. Baseline placement: the SA's own best (its strong leaf search + the
        //    route-aware refinement). The pose lever is layered ON TOP so it is isolated —
        //    where pose finds nothing the result is byte-identical to the SA.
        let mut out = anneal_place::Anneal.place(env, design, problem, ir);
        if problem.items.is_empty() {
            return out;
        }
        let realizer = RoutedSheetRealizer::new(env, &problem.inc, &out.ir);
        let eval = RoutedEvaluator::new(&realizer);
        // The SA's RENDERED sprawl (post text-solve + orphan label-columns), captured BEFORE
        // pose, is the baseline the de-sprawl floorplanner must beat outright — measured the
        // same way as the candidate so the comparison is apples-to-apples (a pose move that
        // spreads an IC can't lower the bar either).
        let baseline_rendered = eval
            .rendered_extent(design, &problem.items)
            .map(|r| compact::rendered_sprawl(&r, problem.items.len()))
            .unwrap_or(f64::MAX);
        // Snapshot the SA placement + its crossings so the whole pose+compact result can fall
        // back to it (the final safety net below).
        let sa_snap = crate::eval::save(&problem.items);
        let sa_crossings = eval.crossings(&problem.items).total();
        // 2. THE lever the SA never searches: re-pose each hub (+ its satellite cluster,
        //    moved rigidly), keeping a pose only when it strictly cuts shipped crossings.
        pose::search_hub_poses(&eval, &mut problem.items, &problem.inc, &out.ir);
        // 3. (Env-gated) de-sprawl floorplanner: lay each module out in isolation + pack, kept
        //    only when it strictly out-de-sprawls the SA without regressing the routed metrics.
        if std::env::var_os("CLUSTER_COMPACT").is_some() {
            compact::compact_clusters(
                &eval,
                design,
                &mut problem.items,
                &problem.inc,
                &out.ir,
                baseline_rendered,
            );
        }
        // 4. SAFETY NET: pose gates on gate-time (truthfulness, warnings, crossings), which is
        //    blind to the emit's orphan label-columns — so it can chase a phantom gate-time
        //    warning win that ships a more-sprawled sheet (54→78 on a dense board). Its genuine
        //    value is CROSSINGS; if the final result didn't cut crossings AND its rendered
        //    sprawl is worse than the SA's, pose/compact gave nothing but bloat — ship the SA.
        let final_crossings = eval.crossings(&problem.items).total();
        let final_rendered = eval
            .rendered_extent(design, &problem.items)
            .map(|r| compact::rendered_sprawl(&r, problem.items.len()))
            .unwrap_or(f64::MAX);
        if final_crossings >= sa_crossings && final_rendered > baseline_rendered + 1e-3 {
            crate::eval::restore(&mut problem.items, &sa_snap);
        }
        if let Some(b) = std::env::var_os("CLUSTER_DEBUG").map(|_| sa_crossings) {
            eprintln!(
                "[cluster] crossings {b} -> {final_crossings}  rendered {baseline_rendered:.1} -> {final_rendered:.1}"
            );
        }
        out.result = report(self.name(), &problem.items, &eval);
        out
    }
}

/// Measure the FINAL placement for the diagnostic [`PlaceResult`].
fn report(engine: &str, items: &[Item], eval: &RoutedEvaluator) -> PlaceResult {
    if items.is_empty() {
        return PlaceResult {
            engine: engine.to_string(),
            truthfulness_breaks: 0,
            warnings: 0,
            crossings: Crossings::default(),
            cost: 0.0,
        };
    }
    PlaceResult {
        engine: engine.to_string(),
        truthfulness_breaks: eval.truthfulness_breaks(items),
        warnings: eval.warnings(items),
        crossings: eval.crossings(items),
        cost: eval::cost(eval, items),
    }
}
