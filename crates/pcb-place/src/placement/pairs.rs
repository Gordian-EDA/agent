//! Engine-private co-placement glue over the kernel's pair detectors
//! ([`pcb_place_api::decoupling_pairs`]/[`pcb_place_api::series_pairs`]).
//!
//! Decoupling caps share BOTH nets with a ≥3-pad anchor (bypass a power rail);
//! series taps sit on a 2-pin net off a dense package (a breakout element). The
//! two sets are disjoint by construction, and feed both the fan-out fast-path and
//! the annealer's cohesion term.

use pcb_place_api::{PlaceProblem, decoupling_pairs, series_pairs};

/// Co-placement pairs the SA cohesion honours: decoupling caps (hug their IC) plus
/// series taps (hug their dense anchor). A part can appear once — [`decoupling_pairs`]
/// and [`series_pairs`] are disjoint by construction (both-nets-shared vs 2-pin-net).
pub(crate) fn coplacement_pairs(problem: &PlaceProblem) -> Vec<(usize, usize)> {
    let mut pairs = decoupling_pairs(problem);
    pairs.extend(series_pairs(problem));
    pairs
}
