//! Concrete routing algorithms used by the production routing policy.
//!
//! [`pipeline::route_tuned`] is the production entry point. It combines a
//! deterministic grid pass with targeted rip-up/rescue phases. The capacity-mesh,
//! crossing, and detailed-routing modules are lower-level implementation and
//! diagnostic primitives, not selectable routing engines.

#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod channel;
pub mod copper;
pub mod deps;
pub mod crossing;
pub mod detail;
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod direct;
pub(crate) mod heuristics;
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod layer_hop;
pub mod mesh;
pub mod pathing;
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod pattern;
pub mod pipeline;
#[cfg(test)]
pub(crate) mod quality;
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod sequential;
pub(crate) mod via_cleanup;
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod via_escape;
