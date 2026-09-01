//! `kicad-symbol` — everything about KiCAD *symbols*: the pin/metadata vocabulary
//! (`PinMeta`/`SymbolMeta`), the concrete [`SymbolTable`] oracle backed by the
//! `.kicad_sym` library reader ([`symlib`]), the per-symbol drawing geometry +
//! embeddable definition ([`geometry`]), and cross-library [`search`].
//!
//! `sch-check` re-exports the metadata types (`sch_check::{PinType, …}`).

pub mod geometry;
pub mod search;
pub mod symlib;

mod table;
mod types;

pub use table::SymbolTable;
pub use types::{PinDir, PinMeta, PinType, SymbolMeta, find_pin};
