//! The symbol-metadata contract lives in the `kicad-symbol` crate. Re-exported
//! here so `circuit_lang::provider::*` and the crate-root `circuit_lang::{PinType,
//! …}` paths resolve unchanged.

pub use kicad_symbol::*;
