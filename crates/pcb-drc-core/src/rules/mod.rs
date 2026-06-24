//! The in-house DRC rule set.
//!
//! One [`Rule`](crate::Rule) per module. [`standard_rules`] returns them in the
//! canonical reporting order — the exact order the legacy hardcoded `lint()`
//! produced, so the suite's findings are byte-identical.

mod geom;

pub mod board_edge;
pub mod clearance;
pub mod connectivity_rule;
pub mod hole_clearance;
pub mod invalid_layer;
pub mod out_of_bounds;
pub mod trace_width;
pub mod via_diameter;

pub use board_edge::BoardEdgeClearanceRule;
pub use clearance::PairClearanceRule;
pub use connectivity_rule::ConnectivityRule;
pub use hole_clearance::HoleClearanceRule;
pub use invalid_layer::InvalidLayerRule;
pub use out_of_bounds::OutOfBoundsRule;
pub use trace_width::TraceWidthRule;
pub use via_diameter::ViaDiameterRule;

use crate::Rule;

/// The canonical in-house rule set, in fixed reporting order:
/// invalid-layer (0), trace-width (1), out-of-bounds (2), board-edge (2b),
/// pairwise clearance (3), hole-clearance (3b), via-diameter (3c), then
/// connectivity (4) folded in last.
pub(crate) fn standard_rules() -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(InvalidLayerRule),
        Box::new(TraceWidthRule),
        Box::new(OutOfBoundsRule),
        Box::new(BoardEdgeClearanceRule),
        Box::new(PairClearanceRule),
        Box::new(HoleClearanceRule),
        Box::new(ViaDiameterRule),
        Box::new(ConnectivityRule),
    ]
}
