//! Native Rust client for the KiCAD IPC API (KiCAD 9.0+).
//!
//! KiCAD hosts a per-instance API server (protobuf messages over an NNG REQ/REP
//! socket); this crate speaks that protocol directly — no Python. The generated
//! generated message types are an implementation detail; callers use the
//! high-level board client and bridge-owned DTOs. Routing and placement model
//! conversion belongs to callers, so this transport crate stays a dependency
//! leaf with respect to engine SDKs.

mod client;
mod documents;
mod edit;
mod error;
mod items;
mod nets;

#[allow(dead_code, clippy::all)]
mod proto {
    include!(concat!(env!("OUT_DIR"), "/_proto.rs"));
}

pub mod session;
pub mod snapshot;
pub mod units;

pub use client::Kicad;
pub use edit::{
    CopperDeleteRequest, CopperHit, CopperKind, FootprintMove, FootprintPosition, RouteWrite,
};
pub use error::Error;
pub use session::{Session, SessionManager};
