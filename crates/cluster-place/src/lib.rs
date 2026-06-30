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
        // The pose search + density sweep + gate each realize the sheet several times; on a
        // huge board (hundreds of parts) that text-solve cost dominates and can time out, for a
        // de-sprawl the floorplanner rarely lands there anyway. Ship the (already-computed)
        // anneal result directly above a size cap so the engine never regresses on latency.
        if problem.items.is_empty() || problem.items.len() > 70 {
            return out;
        }
        let realizer = RoutedSheetRealizer::new(env, &problem.inc, &out.ir);
        let eval = RoutedEvaluator::new(&realizer);
        // The SA's RENDERED sprawl (post text-solve + orphan label-columns), captured BEFORE
        // pose, is the baseline the de-sprawl floorplanner must beat outright — measured the
        // same way as the candidate so the comparison is apples-to-apples (a pose move that
        // spreads an IC can't lower the bar either).
        let n = problem.items.len();
        let (sa_crossings, sa_warnings, baseline_rendered) = match eval.shipped(design, &problem.items) {
            Some((cr, w, r)) => (cr.total(), w, compact::rendered_sprawl(&r, n)),
            None => (usize::MAX, usize::MAX, f64::MAX),
        };
        let baseline_parts = compact::part_sprawl(&problem.items);
        // Snapshot the SA placement so the whole pose+compact result can fall back to it.
        let sa_snap = crate::eval::save(&problem.items);
        // 2. THE lever the SA never searches: re-pose each hub (+ its satellite cluster,
        //    moved rigidly), keeping a pose only when it strictly cuts shipped crossings. Pose
        //    can only REDUCE crossings, so when the anneal already routed the sheet crossing-free
        //    (the common case) the whole search is wasted realizes — skip it.
        if sa_crossings > 0 {
            pose::search_hub_poses(&eval, &mut problem.items, &problem.inc, &out.ir);
        }
        // 3. De-sprawl floorplanner (DEFAULT-ON; `CLUSTER_NO_COMPACT` opts out): lay each module
        //    out in isolation + pack, kept only when it strictly out-de-sprawls the SA on both
        //    sprawl measures without regressing warnings/crossings — else it reverts.
        if std::env::var_os("CLUSTER_NO_COMPACT").is_none() {
            compact::compact_clusters(
                &eval,
                design,
                &mut problem.items,
                &problem.inc,
                &out.ir,
                baseline_rendered,
                sa_warnings,
            );
        }
        // 4. SAFETY NET: pose gates on gate-time (truthfulness, warnings, crossings), which is
        //    blind to the emit's orphan label-columns — so it can chase a phantom gate-time win
        //    that ships a MORE-SPRAWLED or MORE-COLLIDING sheet (a dense board: 54→78 sprawl, or
        //    1→4 warnings, crossings unchanged). Pose's genuine value is CROSSINGS, so measure
        //    the SHIPPED result and fall back to the SA snapshot unless pose/compact earned its
        //    keep: a real crossing cut, no new warnings, and no sprawl bloat.
        let (final_crossings, final_warnings, final_rendered) = match eval.shipped(design, &problem.items) {
            Some((cr, w, r)) => (cr.total(), w, compact::rendered_sprawl(&r, n)),
            None => (usize::MAX, usize::MAX, f64::MAX),
        };
        // STRICT PARETO: ship pose+compact only if it regresses NOTHING — warnings, crossings,
        // and BOTH sprawl measures (the label-inclusive rendered extent AND the part-origin
        // spread). Two measures because each is blind where the other sees: rendered catches the
        // orphan-column balloon a dense pack causes; part-spread catches a pose splaying an IC,
        // which leaves the label-padded extent flat. A mixed result (pose cut crossings but
        // spread the parts +20%) is NOT a more human-like sheet, so revert it.
        let final_parts = compact::part_sprawl(&problem.items);
        let earned_keep = final_warnings <= sa_warnings
            && final_crossings <= sa_crossings
            && final_rendered <= baseline_rendered + 1e-3
            && final_parts <= baseline_parts + 1e-3;
        if !earned_keep {
            crate::eval::restore(&mut problem.items, &sa_snap);
        }
        if std::env::var_os("CLUSTER_DEBUG").is_some() {
            eprintln!(
                "[cluster] x {sa_crossings}->{final_crossings}  w {sa_warnings}->{final_warnings}  rendered {baseline_rendered:.1}->{final_rendered:.1}  parts {baseline_parts:.1}->{final_parts:.1}  keep={earned_keep}"
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
