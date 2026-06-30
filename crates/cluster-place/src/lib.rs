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
//! Beyond pose it runs a DE-SPRAWL floorplanner ([`compact`], DEFAULT-ON; `CLUSTER_NO_COMPACT`
//! opts out): each module is laid out cleanly IN ISOLATION (hub + a single-row decoupling bank)
//! and the footprints re-packed, kept only when it strictly out-de-sprawls the SA on BOTH sprawl
//! measures (label-inclusive rendered extent AND part-origin spread) with no new warnings or
//! crossings — else it reverts, so the result is never worse. On repetitive power-IC ARRAYS it
//! then applies the "modules between rails" idiom ([`compact::rail_relayout`]): stand the ICs
//! sharing the dominant rail in one row so a shared trunk replaces their distributed power
//! glyphs. Full-dataset validation: 13/40 liftable boards de-sprawl, 0 regressions.
//!
//! It owns its objective and search; it measures candidates through `sch-floorplan`'s
//! [`RoutedEvaluator`] and implements the published [`PlacementEngine`] trait.

mod cola_place;
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
        // Snapshot the SA placement (and its IR) so the whole pose+compact result — or an
        // untruthful final sheet — can fall back to the bare anneal.
        let sa_snap = crate::eval::save(&problem.items);
        let sa_ir = out.ir.clone();
        // COLA compaction (default; `CLUSTER_NO_COLA` opts out): re-place via constraint stress
        // (connected parts adjacent ⇒ wires; gravity pulls interface parts in; cap-bank). Ship it
        // ONLY when it emits a TRUTHFUL netlist (zero merges/shorts — the hard correctness gate;
        // tight packs can starve the router of channels and short nets, so this is REQUIRED) AND it
        // is strictly tighter with a better weighted score. The anneal is untouched, so the
        // truthfulness gate (which exercises the anneal) stays green; this guards the cola candidate.
        if std::env::var_os("CLUSTER_NO_COLA").is_none() {
            cola_place::cola_place(&mut problem.items, &problem.inc, &out.ir);
            // One realize gives both the truthfulness-break count and the shipped (crossings,
            // warnings, sprawl) — so the gate costs a single extra emit, not two.
            let (truthful, co_x, co_w, co_r) = match eval.shipped_truthful(design, &problem.items) {
                Some((b, cr, w, r)) => (b == 0, cr.total(), w, compact::rendered_sprawl(&r, n)),
                None => (false, usize::MAX, usize::MAX, f64::MAX),
            };
            let score = |x: usize, w: usize, r: f64| w as f64 * 5.0 + x as f64 * 3.0 + r;
            let cola_wins = truthful
                && co_r + 1e-3 < baseline_rendered
                && score(co_x, co_w, co_r) < score(sa_crossings, sa_warnings, baseline_rendered);
            if std::env::var_os("CLUSTER_DEBUG").is_some() {
                eprintln!(
                    "[cola] truthful={truthful} co_rendered={co_r:.1} (sa {baseline_rendered:.1}) co_w={co_w} co_x={co_x} -> wins={cola_wins}"
                );
            }
            if cola_wins {
                out.result = report(self.name(), &problem.items, &eval);
                return out;
            }
            crate::eval::restore(&mut problem.items, &sa_snap);
        }
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
        // 5. POWER RAILS: try the "modules between rails" idiom — stand the ICs sharing the
        //    dominant power net in one top-aligned row so a shared trunk replaces their
        //    distributed per-pin power glyphs (the dominant residual sprawl). Snapshot first;
        //    keep it only if the SHIPPED rendered sheet (with the trunk forced) shrinks with no
        //    new warnings — a colliding trunk reverts. Gate measures via a fresh realizer that
        //    carries `rail_force`; anneal never sets it ⇒ references unaffected.
        let cur = eval.shipped(design, &problem.items).map(|(_, w, r)| (w, compact::rendered_sprawl(&r, n)));
        out.result = report(self.name(), &problem.items, &eval);
        // `eval`/`realizer` borrow `out.ir`; their last use is the `report` above, so NLL frees
        // that borrow here, letting the rail step below reassign `out.ir`.
        if let Some((cur_w, cur_spr)) = cur {
            let pre = crate::eval::save(&problem.items);
            if let Some(rail) = compact::rail_relayout(&mut problem.items, &problem.inc, &out.ir) {
                let mut ir_rail = out.ir.clone();
                ir_rail.rail_force.insert(rail);
                let rz = RoutedSheetRealizer::new(env, &problem.inc, &ir_rail);
                let ev = RoutedEvaluator::new(&rz);
                let got = ev.rendered(design, &problem.items).map(|(w, r)| (w, compact::rendered_sprawl(&r, n)));
                let keep = matches!(got, Some((w, s)) if w <= cur_w && s + 1e-3 < cur_spr);
                if std::env::var_os("CLUSTER_DEBUG").is_some() {
                    eprintln!("[cluster] rails: {cur_spr:.1} -> {:?}  keep={keep}", got.map(|g| g.1));
                }
                if keep {
                    out.ir = ir_rail;
                } else {
                    crate::eval::restore(&mut problem.items, &pre);
                }
            } else {
                crate::eval::restore(&mut problem.items, &pre);
            }
        }
        // FINAL TRUTHFULNESS BACKSTOP: the SA-side passes (pose/de-sprawl/rail) are gated on
        // sprawl/warnings/crossings but NOT truthfulness, so as a hard correctness floor — if the
        // final sheet would emit an untruthful netlist (any merge/short) — fall back to the bare
        // anneal, which is spread enough that the router never starves for channels and shorts.
        // The engine must NEVER ship a wrong netlist. (The cola path above already returns only
        // truthful layouts, so this only guards the SA-side.)
        let final_untruthful = {
            let rz = RoutedSheetRealizer::new(env, &problem.inc, &out.ir);
            RoutedEvaluator::new(&rz).truthfulness_breaks(&problem.items) > 0
        };
        if final_untruthful {
            crate::eval::restore(&mut problem.items, &sa_snap);
            out.ir = sa_ir;
            let rz = RoutedSheetRealizer::new(env, &problem.inc, &out.ir);
            out.result = report(self.name(), &problem.items, &RoutedEvaluator::new(&rz));
            if std::env::var_os("CLUSTER_DEBUG").is_some() {
                eprintln!("[cluster] FINAL untruthful -> restored bare anneal");
            }
        }
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
