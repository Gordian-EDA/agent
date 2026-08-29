//! Concrete placement phase used by `pcb-engine`.
//!
//! Fan-out initialization, force relaxation, annealing, and legalization form
//! one tuned algorithm surfaced as [`place_tuned`].
//!
//! ```text
//! place_tuned(problem, hints):
//!   1. STRUCTURED fast-path  — fan-out (`placement::unified_fanout_place`)
//!        a dominant-IC board gets the textbook radial layout (IC centred,
//!        decoupling caps + series resistors ringed in IC-pad order, connectors
//!        on the edges). Overlap-free by construction.
//!        └ seats legally? → return it.
//!   2. OPTIMIZE              — force seed → anneal → legalize.
//!   3. post-pass: corner/edge seating (`seat_corner_seek_parts`).
//! ```
//!
//! This crate does not define an engine trait or select among placers.

mod api;
pub mod placement;

pub use api::*;
pub use placement::place_tuned;
