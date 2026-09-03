//! Shared evaluation: the SHIPPED-sheet lexicographic score every search step in this
//! engine gates on, the base objective, and item snapshot/restore. Centralised so the pose
//! search and the cluster-compaction step judge a candidate identically — on the sheet as it
//! will ship, never the raw pre-finalize geometry.

use sch_model::engine::{CandidateEvaluator, RawMetrics};

use geom::Point2;
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};

use sch_model::idiom::{align_idiom_clusters, align_led_chains};
use sch_model::refine::decongest;

/// Cohesion pull on a multi-unit part's units (same refdes, no shared net).
const SIB_COHESION: f64 = 3.0;

/// The base routed objective: the 18 raw routed-sheet terms weighted into one straightness
/// scalar — the engine's tiebreak once truthfulness/warnings/crossings tie. The known-good
/// base weighting. Build failure (saturated length) ⇒ ∞.
pub(crate) fn base_cost(m: &RawMetrics) -> f64 {
    if !m.length.is_finite() {
        return f64::INFINITY;
    }
    let correctness = 2000.0 * m.merges as f64
        + 1500.0 * m.overlaps as f64
        + 1000.0 * m.fallbacks as f64
        + 30.0 * m.body_cross as f64
        + 12.0 * m.orient_viol as f64
        + 10.0 * m.spine_viol as f64;
    let base = correctness
        + 5.0 * m.crossings as f64
        + 7.0 * m.congestion as f64
        + 7.0 * m.corners as f64
        + m.junctions as f64
        + 0.7 * m.stray
        + 0.15 * m.length
        + 0.45 * m.spread;
    base + SIB_COHESION * m.sib_spread
}

/// Score `items` under the base objective by measuring the routed sheet.
pub(crate) fn cost(eval: &dyn CandidateEvaluator, items: &[Item]) -> f64 {
    base_cost(&eval.measure(items))
}

/// The shipped score of a candidate: finalize a clone with the same unconditional emit
/// passes (decongest + idiom/LED re-seat) the writer runs after `place()`, then read
/// `(truthfulness_breaks, warnings, total_crossings, straightness)`. Lower is better,
/// compared lexicographically on the first three with straightness as the tiebreak.
/// Measuring the raw geometry would rank candidates the emit then re-orders.
pub(crate) fn score(
    eval: &dyn CandidateEvaluator,
    inc: &Incidence,
    ir: &LayoutIr,
    cand: &[Item],
) -> (usize, usize, usize, f64) {
    let mut m = cand.to_vec();
    decongest(&mut m);
    if align_idiom_clusters(&mut m, ir) {
        decongest(&mut m);
    }
    if align_led_chains(&mut m, inc, ir) {
        decongest(&mut m);
    }
    let tb = eval.truthfulness_breaks(&m);
    let w = eval.warnings(&m);
    let cr = eval.crossings(&m);
    (tb, w, cr.total(), base_cost(&eval.measure(&m)))
}

/// A saved `(at, angle, mirror, frozen)` snapshot of every item, for trial/restore. Frozen
/// is included so a reverted compaction also restores the original freeze state (the
/// floorplanner freezes its placed items so the emit's gather pile can't re-arrange them).
pub(crate) type Snap = Vec<(Point2, f64, bool, bool)>;

pub(crate) fn save(items: &[Item]) -> Snap {
    items
        .iter()
        .map(|it| (it.at, it.angle, it.mirror, it.frozen))
        .collect()
}

pub(crate) fn restore(items: &mut [Item], snap: &Snap) {
    for (it, &(at, a, m, f)) in items.iter_mut().zip(snap) {
        it.at = at;
        it.angle = a;
        it.mirror = m;
        it.frozen = f;
    }
}
