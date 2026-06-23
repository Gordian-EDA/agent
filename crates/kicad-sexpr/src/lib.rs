//! KiCAD s-expr file & library access: read `.kicad_pcb`, footprint and symbol
//! libraries, geometry helpers, and a fuzzy index over the libraries. The
//! `kicad-cli` wrapper lives in `kicad-cli-rs`; board synthesis in `pcb-synth`;
//! the Specctra/Freerouting bridge in `specctra`.

pub mod footlib;
pub mod geometry;
pub mod pcb;
pub mod provider;
pub mod search;
pub mod snapshot;
pub mod symlib;
