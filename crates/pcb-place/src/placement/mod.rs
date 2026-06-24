//! Deterministic placement: force-directed seed + legalizer, optional SA refine,
//! and a structured radial fan-out fast-path.
//!
//! Turns a bag of footprint-shaped [`Part`]s carrying per-pad net names into legal
//! board positions, optionally steered by LLM-authored [`PlacementHints`] (group
//! cohesion, region containment, edge affinity). The engine is **pure and
//! deterministic** — no clock, fixed RNG seed, no I/O beyond serde — and the LLM
//! never emits coordinates: it emits hints (data), and [`place_board`] does the
//! geometry.
//!
//! ## The pipeline
//!
//! ```text
//! place_board(problem, hints):
//!   1. STRUCTURED fast-path  — hints::unified_fanout_place
//!        a dominant-IC board gets the textbook radial layout (IC centred,
//!        decoupling caps + series resistors ringed in IC-pad order, connectors
//!        on the edges). Overlap-free by construction.
//!   2. OPTIMIZE fallback     — route::place_best
//!        a. generate candidates: force-directed seed (force::force_layout) →
//!           optional simulated-annealing refine (anneal::anneal_placement), plus
//!           decouple/edge idiom variants;
//!        b. route each candidate (routability ranking);
//!        c. keep the most routable.
//!   3. post-pass: corner seating for mounting holes (corner_seek).
//! ```
//!
//! ## The engine-SDK seam
//!
//! The placement TRAIT seam — the [`Placer`] contract, the [`PlaceProblem`] it
//! reads, the [`PlaceResult`] it returns, the [`RouteRanker`] evaluator, and the
//! [`RoutabilityOracle`] selector — lives in the KERNEL ([`pcb_model::place`]) so a
//! third party implements it against `pcb-model` ALONE. This crate supplies the
//! BUILT-IN implementations: [`LegalizingPlacer`], [`AnnealingPlacer`],
//! [`FanoutPlacer`], and the grid-astar-backed [`GridAstarRanker`]. The model types
//! are re-exported VERBATIM so every external `pcb_place::placement::…` path resolves
//! unchanged.
//!
//! ## Module layout
//!
//! - [`model`]   — re-exports the kernel problem/result types + the derived-net model.
//! - [`geometry`] — the engine-PRIVATE scaffold (grid snap, edge affinity), with the
//!   shared kernel geometry re-exported.
//! - [`pairs`]   — re-exports kernel co-placement detection + the engine-private glue.
//! - [`hints`]   — hint-driven & structured LOCKING (grid, surround, edge-lock,
//!   fan-out rings, the unified fan-out fast-path).
//! - [`force`]   — the force-directed seed + the cap-to-anchor-ring snap.
//! - [`cost`]    — the SA cost the annealer minimizes (and the oracle selects on);
//!   re-exports the kernel HPWL.
//! - [`anneal`]  — the SA driver + its deterministic `SaRng`.
//! - [`legalize`] — the legalizer driver; re-exports the kernel legality check.
//! - [`route`]   — the pipeline entries ([`place`], [`place_best`], [`place_board`])
//!   + the built-in [`Placer`]s and the [`GridAstarRanker`].
//!
//! Routing for the ranking comes from `grid-astar`; DRC from `drc-lint`; the engine
//! kernel + shared geometry from `pcb-model`.

mod anneal;
mod cost;
mod force;
mod geometry;
mod hints;
mod legalize;
mod model;
mod pairs;
mod route;

// The engine-SDK seam lives in the kernel — re-exported so a `pcb-place` caller can
// reach the trait + oracle without a second `pcb_model::place` import.
pub use crate::problem::place::{compute_hpwl, is_legal, Placer, RouteRanker, RoutabilityOracle};

// Public surface — re-exported VERBATIM so every external `pcb_place::placement::…`
// path resolves unchanged.
pub use model::{
    derive_nets, Edge, GroupHint, LockedAt, LogicalNet, Part, PartPad, PlaceProblem, PlaceReport,
    PlaceResult, Placement, PlacementHints, Pin, Rect,
};
pub use pairs::{decoupling_pairs, series_fanout_order, series_pairs};
pub use hints::{apply_edge_lock, apply_grid_hints, apply_surround, fan_out_rings, unified_fanout_place};
pub use route::{place, place_best, place_board, to_route_problem};
pub use route::{AnnealingPlacer, FanoutPlacer, GridAstarRanker, LegalizingPlacer};

#[cfg(test)]
mod tests;
