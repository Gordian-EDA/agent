//! The phase leaves: what a placer, a router, and a routability probe are.
//!
//! Each trait is a self-contained Input → Output leaf. Collaborators are passed
//! in (`&dyn RouteProbe`, and `&dyn Drc` inside a concrete router) rather than
//! reached for through a crate dependency, so every algorithm crate can be
//! built, benched, and tested on its own.

use std::time::{Duration, Instant};

use crate::{PlaceResult, PlacementHints, PlacementView, RouteQuality, RouteResult, RoutingView};

/// The resource envelope a phase must respect.
///
/// `deadline` is wall-clock: a phase checks [`Budget::expired`] at its own
/// natural interruption points and returns the best valid result it has. `seed`
/// makes every randomized search reproducible, and `effort` scales the search
/// budget of phases that have one (1.0 = the tuned default).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// Wall-clock instant after which the phase should stop searching.
    pub deadline: Option<Instant>,
    /// Search-effort multiplier; `None` means the phase's tuned default.
    pub effort: Option<f64>,
    /// Seed for every randomized decision the phase makes.
    pub seed: u64,
}

impl Budget {
    /// No deadline, default effort, seed 0 — the production envelope.
    pub const fn unlimited() -> Self {
        Budget {
            deadline: None,
            effort: None,
            seed: 0,
        }
    }

    /// An envelope that expires `after` from now.
    pub fn within(after: Duration) -> Self {
        Budget {
            deadline: Some(Instant::now() + after),
            effort: None,
            seed: 0,
        }
    }

    /// Whether the deadline has passed.
    pub fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// The routability signal a placer optimises against: how well a trial route of
/// a candidate placement's [`RoutingView`] turned out.
///
/// The fields are exactly what a placement search ranks candidates by:
/// routability first ([`RouteEstimate::faults`]), then the stranded-net count,
/// then the manufacturability tiebreakers (vias, then wirelength).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteEstimate {
    /// Unconnected-pad weight of the nets the trial route could not finish.
    pub fault_weight: usize,
    /// Geometry DRC violations of the trial route's copper.
    pub geometry_violations: usize,
    /// Number of nets left unrouted (not pad-weighted).
    pub failed_nets: usize,
    /// Number of vias the trial route spent.
    pub via_count: usize,
    /// Total copper wirelength of the trial route, mm.
    pub wirelength: f64,
}

impl RouteEstimate {
    /// Total routability cost: unconnected pads + geometry violations. The
    /// primary ranking key — never traded for tidiness.
    pub fn faults(&self) -> usize {
        self.fault_weight + self.geometry_violations
    }
}

impl From<RouteQuality> for RouteEstimate {
    fn from(q: RouteQuality) -> Self {
        RouteEstimate {
            fault_weight: q.fault_weight,
            geometry_violations: q.geom,
            failed_nets: q.failed_nets,
            via_count: q.via_count,
            wirelength: q.wirelength,
        }
    }
}

/// A cheap routability oracle over a candidate placement's [`RoutingView`].
///
/// This is the "the placer uses the router" edge, turned into an injection
/// point: a placer takes `&dyn RouteProbe` and never names a router crate.
pub trait RouteProbe {
    /// A stable, human-readable identifier for this probe.
    fn name(&self) -> &'static str;
    /// Estimate how routable `view` is. Deterministic and side-effect free.
    fn estimate(&self, view: &RoutingView) -> RouteEstimate;
}

/// A component-placement phase: board + hints + routability oracle → positions.
pub trait PcbPlacer {
    /// A stable, human-readable identifier for this placer.
    fn name(&self) -> &'static str;
    /// Place every part in `view` exactly once, leaving locked parts unmoved.
    fn place(
        &self,
        view: &PlacementView,
        hints: &PlacementHints,
        probe: &dyn RouteProbe,
        budget: &Budget,
    ) -> PlaceResult;
}

/// A copper-routing phase: a routing problem → traces, vias, and honest failures.
pub trait PcbRouter {
    /// A stable, human-readable identifier for this router.
    fn name(&self) -> &'static str;
    /// Route `view` within `budget`. Every emitted trace is on a layer the board
    /// declares and belongs to a connection `view` asked for.
    fn route(&self, view: &RoutingView, budget: &Budget) -> RouteResult;
}
