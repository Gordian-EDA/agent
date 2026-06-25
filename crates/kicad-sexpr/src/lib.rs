//! KiCAD s-expr **PCB-side** file access: read/write `.kicad_pcb` ([`pcb`]) and
//! footprint `.kicad_mod` libraries ([`footlib`]). Symbol concerns (`.kicad_sym`
//! parsing, pin metadata, drawing geometry, search) live in the `kicad-symbol`
//! crate; the `kicad-cli` wrapper in `kicad-cli`; and the Freerouting bridge in
//! `specctra`.

pub mod footlib;
pub mod pcb;

/// KiCAD coordinate number formatting (shortest round-tripping decimal, `-0.0`
/// collapsed to `0`). The single owner; re-exported here so synthesis and other
/// emitters share one definition. See [`pcb::fmt_num`].
pub use pcb::fmt_num;
