//! `greedy-place` — the free-tier greedy hill-climb placement engine. Implements
//! sch-layout's `PlacementEngine`: a seeded refine→polish over the place scaffold.
//! The engine-agnostic core lives in `sch-layout`; this and `anneal-place` are the
//! pluggable engines the agent selects between.

use sch_layout::floorplan::place::{polish, refine_items, PlaceProblem, PlacementEngine};
use sch_model::item::Item;

/// Greedy hill-climb (free tier): local, strictly-cost-improving moves only over
/// the seeded mm placement.
pub struct Greedy;
impl PlacementEngine for Greedy {
    fn name(&self) -> &'static str {
        "greedy"
    }
    fn place(&self, p: &PlaceProblem, items: &mut [Item]) {
        refine_items(p.env, items, p.inc, p.ir, p.needs_flag);
        // The free tier uses the ROUTED polish at EVERY size: it is the truthfulness-
        // safe path (each move re-routes, so the cost sees a net merge / short — the
        // router-free proxy polish does NOT, and greedy has no candidate pick to reject
        // a mis-wire). References keep the exact refine→polish order → byte-identical.
        polish(p.env, items, p.inc, p.ir, p.needs_flag);
    }
}
