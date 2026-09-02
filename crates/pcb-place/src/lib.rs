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
//! The routability oracle a placer ranks candidates against is INJECTED as
//! [`pcb_model::RouteProbe`], so this crate names no router.

mod api;
pub mod placement;

pub use api::*;
pub use placement::place_tuned;

use pcb_model::{Budget, PcbPlacer, RouteProbe};

/// Gordian's tuned placement leaf behind the [`PcbPlacer`] contract.
pub struct TunedPlacer;

impl PcbPlacer for TunedPlacer {
    fn name(&self) -> &'static str {
        "tuned"
    }

    /// The tuned search is deterministic and purely geometric: it optimises an
    /// explicit overlap/wirelength/cohesion cost rather than trial routes, so it
    /// consults neither `probe` nor `budget.seed`. An already-expired budget
    /// returns the untouched input placement instead of starting the search.
    fn place(
        &self,
        view: &PlacementView,
        hints: &PlacementHints,
        _probe: &dyn RouteProbe,
        budget: &Budget,
    ) -> PlaceResult {
        if budget.expired() {
            return placement::place_as_given(view);
        }
        place_tuned(view, hints)
    }
}
