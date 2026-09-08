//! KiCad host integration: installation discovery and typed `kicad-cli`
//! operations. Gordian supports KiCad 10 and newer through the command-line
//! interface and saved design files.

mod cli;
mod export;
mod installation;
mod netlist;
mod reports;

pub use installation::KicadInstallation;
/// The name the PCB and schematic pipelines spell it.
pub use installation::KicadInstallation as Installation;
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
