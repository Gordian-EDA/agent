//! Shared geometric tolerances. One name per *kind* of comparison so callers
//! never hand-roll an epsilon.

/// General geometric slop (mm): collinearity, touching, "are these equal".
pub const EPS: f64 = 1e-6;

/// Tight adjacency slop (mm): generated rect edges that should coincide exactly.
pub const STRICT_EPS: f64 = 1e-9;

/// Path-stitching vertex coincidence (mm): exact-dedup of routed polylines.
pub const JOIN_EPS: f64 = 1e-12;
