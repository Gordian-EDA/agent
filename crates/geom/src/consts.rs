//! Shared geometric tolerances. One name per *kind* of comparison so callers
//! never hand-roll an epsilon.

/// General geometric slop (mm): collinearity, touching, "are these equal".
pub const EPS: f64 = 1e-6;

/// Tight adjacency slop (mm): generated rect edges that should coincide exactly.
pub const STRICT_EPS: f64 = 1e-9;

/// Path-stitching vertex coincidence (mm): exact-dedup of routed polylines.
pub const JOIN_EPS: f64 = 1e-12;

/// Clearance kept between a schematic's drawn content and its page edge, in mm.
///
/// Half an inch: KiCAD's own drawing frame border is 10 mm, so this keeps content off the
/// border rule as well as off the paper edge. Every stage that frames a sheet uses it —
/// the typesetter starts its pack here, the writer sizes the page from it, and the
/// document re-fits to it — so it lives where all three can see it. Two of them declaring
/// their own 12.7 is how a pack and a page quietly stop agreeing.
pub const PAGE_MARGIN: f64 = 12.7;
