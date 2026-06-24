//! `greedy-place` — the free-tier greedy hill-climb placement engine. Implements
//! `sch_model::place::PlacementEngine` against `sch-model` ALONE: it scores and
//! improves purely through the injected `PlacementCost`, so it never depends on the
//! incumbent layout crate. The cost evaluator (`sch-place-core`'s `RoutedCost`)
//! provides the routed `refine`/`polish` scaffold this engine drives; a third party
//! could supply any cost and reuse this same engine.

use sch_model::item::Item;
use sch_model::place::{Crossings, PlaceProblem, PlaceResult, PlacementEngine};

/// Greedy hill-climb (free tier): local, strictly-cost-improving moves only over
/// the seeded mm placement.
pub struct Greedy;

impl PlacementEngine for Greedy {
    fn name(&self) -> &'static str {
        "greedy"
    }

    fn place(&self, p: &PlaceProblem, items: &mut [Item]) -> PlaceResult {
        p.cost.refine(items);
        // The free tier uses the ROUTED polish at EVERY size: it is the truthfulness-
        // safe path (each move re-routes, so the cost sees a net merge / short — the
        // router-free proxy polish does NOT, and greedy has no candidate pick to reject
        // a mis-wire). References keep the exact refine→polish order → byte-identical.
        p.cost.polish(items);
        report(self.name(), p, items)
    }
}

/// Measure the FINAL placement against the injected cost, for the diagnostic
/// [`PlaceResult`]. Empty placements report all-zero.
fn report(engine: &str, p: &PlaceProblem, items: &[Item]) -> PlaceResult {
    if items.is_empty() {
        return PlaceResult {
            engine: engine.to_string(),
            truthfulness_breaks: 0,
            warnings: 0,
            crossings: Crossings::default(),
            cost: 0.0,
        };
    }
    let warnings = p.cost.warnings(items);
    PlaceResult {
        engine: engine.to_string(),
        truthfulness_breaks: p.cost.truthfulness_breaks(items),
        warnings,
        crossings: p.cost.crossings(items),
        cost: p.cost.cost(items),
    }
}
