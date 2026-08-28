//! KiCad host integration: installation discovery and typed `kicad-cli`
//! operations.
//!
//! Live editor access remains in `kicad-ipc`; library parsing remains in
//! `kicad-symbol` and `kicad-footprint`.

mod cli;
mod export;
mod installation;
mod netlist;
mod reports;

pub use installation::KicadInstallation;
pub use netlist::{Net, NetComp, Netlist};
pub use reports::{DrcReport, ErcReport, Violation, ViolationItem};
