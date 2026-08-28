//! `pcb-place` — the PCB placement engine.
//!
//! All of force / anneal / fan-out are *placement* — they decide where parts sit.
//! What matters is the PIPELINE they form, surfaced as the single entry [`place`]:
//!
//! ```text
//! place(problem, hints):
//!   1. STRUCTURED fast-path  — fan-out (`placement::unified_fanout_place`)
//!        a dominant-IC board gets the textbook radial layout (IC centred,
//!        decoupling caps + series resistors ringed in IC-pad order, connectors
//!        on the edges). Overlap-free by construction.
//!        └ seats legally? → return it.
//!   2. OPTIMIZE fallback     — `placement::place_best`
//!        a. generate candidates: force-directed seed (`force_layout`) → optional
//!           simulated-annealing refine (`anneal_placement`), plus decouple/edge
//!           idiom variants;
//!        b. route each candidate (routability ranking);
//!        c. keep the most routable.
//!   3. post-pass: corner/edge seating (`seat_corner_seek_parts`).
//! ```
//!
//! Every stage shares one legalizer + geometry scaffold (`legalize`, `collides`,
//! `rotated_*`, …) and the place types. Routing for the ranking comes from
//! `grid-astar`; DRC from `drc-lint`; shared geometry types from `pcb-model`.

pub mod placement;

pub use placement::place_board;
// The built-in engines a `place-model` RoutabilityOracle can drive.
pub use placement::{AnnealingPlacer, FanoutPlacer, GridAstarRanker, LegalizingPlacer};
