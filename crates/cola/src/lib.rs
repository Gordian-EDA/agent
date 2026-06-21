//! Constraint-based layout — a Rust subset-port of adaptagrams `libcola` / `libvpsc`.
//!
//! Layer 1 (this module set): [`vpsc`] — the 1-D Variable Placement with Separation
//! Constraints solver. Layer 2 (planned): constrained stress-majorization 2-D placement
//! built on top of it (alternating x/y axes, each majorization step projected through
//! VPSC). See `docs/specs/constraint-placement-and-vlm-structure.md`.

pub mod vpsc;

pub use vpsc::{Constraint, Solver};
