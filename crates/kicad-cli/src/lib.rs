//! Typed wrapper over the `kicad-cli` binary.
//!
//! `cli` shells out to `kicad-cli` for DRC/ERC/plot/export and parses its output.

pub mod cli;
mod export;
pub mod netlist;
pub mod reports;

pub use cli::KicadCli;
pub use netlist::{Net, NetComp, Netlist};
pub use reports::{DrcReport, ErcReport, Violation, ViolationItem};
