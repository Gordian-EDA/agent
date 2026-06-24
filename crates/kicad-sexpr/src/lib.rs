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

/// KiCAD coordinate number formatting (shortest round-tripping decimal, `-0.0`
/// collapsed to `0`). The single owner; re-exported here so synthesis and other
/// emitters share one definition. See [`pcb::fmt_num`].
pub use pcb::fmt_num;
