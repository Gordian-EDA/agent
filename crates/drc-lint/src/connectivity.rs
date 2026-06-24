//! The connectivity oracle, re-exported from [`pcb_drc_core::connectivity`].
//!
//! Kept as a module path so existing callers (`drc_lint::connectivity::check`,
//! `drc_lint::connectivity::Violation`) are unchanged; the implementation lives
//! in the engine kernel.

pub use pcb_drc_core::connectivity::{Violation, check};
