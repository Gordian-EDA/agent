//! The routability probe a placer ranks candidate placements with.
//!
//! This is the "the placer uses the router" edge turned inside out: `pcb-place`
//! knows only [`pcb_model::RouteProbe`], and the composition root hands it this
//! implementation.

use pcb_model::{Drc, RouteEstimate, RouteProbe, RouteQuality, RoutingView};

use crate::router;

/// Estimates routability by running the strict ORTHOGONAL grid pass over the
/// candidate board and scoring the result.
///
/// Orthogonal (rather than the octilinear default) so the layout choice stays
/// invariant to the router's diagonal policy, and a single pass rather than the
/// export router's order portfolio so ranking many candidates stays affordable.
pub struct GridRouteProbe<'a> {
    drc: &'a dyn Drc,
}

impl<'a> GridRouteProbe<'a> {
    /// Probe scoring against `drc` as its geometry authority.
    pub fn new(drc: &'a dyn Drc) -> Self {
        GridRouteProbe { drc }
    }
}

impl RouteProbe for GridRouteProbe<'_> {
    fn name(&self) -> &'static str {
        "grid-orthogonal"
    }

    fn estimate(&self, view: &RoutingView) -> RouteEstimate {
        let result = router::route_orthogonal_single_pass(self.drc, view);
        let geom = self.drc.geometry_violations(view, &result.solution);
        RouteQuality::of(view, &result, geom).into()
    }
}
