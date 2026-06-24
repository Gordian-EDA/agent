//! `anneal-place` — the premium simulated-annealing schematic placement engine.
//!
//! A thin [`sch_model::place::PlacementEngine`] (the free build defaults to Greedy):
//! it declares the premium tier and delegates the whole search to
//! [`sch_place_core::floorplan::place::anneal_place`], where the SA move-set + proxy
//! costs live in ONE home alongside the env-free scaffold they drive
//! (cohesion/anchor/idiom-align/decongest). Scoring still goes entirely through the
//! injected [`sch_model::place::PlacementCost`] — the engine never imports the
//! incumbent crate's scorers nor a KiCAD CLI.

use sch_model::item::Item;
use sch_model::place::{EngineCaps, PlaceProblem, PlaceResult, PlacementEngine, Tier};
use sch_place_core::floorplan::place::anneal_place;

/// Simulated annealing (paid tier): a seeded refine→anneal AND a broad anneal from
/// the raw seed, keeping whichever the cost prefers (today's multi-start best-of).
pub struct Anneal;
impl PlacementEngine for Anneal {
    fn name(&self) -> &'static str {
        "anneal"
    }
    fn caps(&self) -> EngineCaps {
        EngineCaps { tier: Tier::Premium }
    }
    fn place(&self, p: &PlaceProblem, items: &mut [Item]) -> PlaceResult {
        anneal_place(p, items, self.name())
    }
}
