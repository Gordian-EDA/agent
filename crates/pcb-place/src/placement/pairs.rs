//! Co-placement pair detection now lives in the kernel ([`pcb_model::place`]) so a
//! third-party engine can reuse it. This module re-exports
//! [`decoupling_pairs`]/[`series_pairs`]/[`series_fanout_order`] verbatim and adds
//! the engine-private glue ([`coplacement_pairs`]) the search drivers use.
//!
//! Decoupling caps share BOTH nets with a ≥3-pad anchor (bypass a power rail);
//! series taps sit on a 2-pin net off a dense package (a breakout element). The
//! two sets are disjoint by construction, and feed both the fan-out fast-path and
//! the annealer's cohesion term.

pub use crate::problem::place::{decoupling_pairs, series_fanout_order, series_pairs};

use super::model::PlaceProblem;

/// Co-placement pairs the SA cohesion honours: decoupling caps (hug their IC) plus
/// series taps (hug their dense anchor). A part can appear once — [`decoupling_pairs`]
/// and [`series_pairs`] are disjoint by construction (both-nets-shared vs 2-pin-net).
pub(crate) fn coplacement_pairs(problem: &PlaceProblem) -> Vec<(usize, usize)> {
    let mut pairs = decoupling_pairs(problem);
    pairs.extend(series_pairs(problem));
    pairs
}
