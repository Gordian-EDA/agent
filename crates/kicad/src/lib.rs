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

/// Escape a string for embedding in a quoted KiCAD S-expression atom.
pub fn sexpr_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}
