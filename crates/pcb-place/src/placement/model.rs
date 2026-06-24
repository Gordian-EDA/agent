//! The placement data model now lives in the kernel ([`pcb_model::place`]) so a
//! third-party engine reads it without depending on `pcb-place`. This module
//! re-exports it VERBATIM, so every internal `super::model::…` path resolves
//! unchanged.

pub use crate::problem::place::{
    derive_nets, Edge, GroupHint, LockedAt, LogicalNet, Part, PartPad, Pin, PlaceProblem,
    PlaceReport, PlaceResult, Placement, PlacementHints,
};
/// Axis-aligned region/keep-out rectangle (mm) — the shared [`pcb_model::Rect`].
pub use crate::problem::Rect;
