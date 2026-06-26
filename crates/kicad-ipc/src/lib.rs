//! Native Rust client for the KiCAD IPC API (KiCAD 9.0+).
//!
//! KiCAD hosts a per-instance API server (protobuf messages over an NNG REQ/REP
//! socket); this crate speaks that protocol directly — no Python. The generated
//! message types live under [`proto`]; the high-level board client is layered on
//! top (added incrementally). See the validated wire protocol in the project
//! memory `kicad-ipc-protocol`.

mod client;
mod documents;
mod edit;
mod error;
mod items;
mod nets;

/// Generated protobuf types — the full `kiapi::{common, board, ...}` module tree.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/_proto.rs"));
}

pub mod session;
pub mod snapshot;

pub use client::Kicad;
pub use edit::{FootprintMove, footprint_reference};
pub use error::Error;
pub use session::{Session, SessionManager};
