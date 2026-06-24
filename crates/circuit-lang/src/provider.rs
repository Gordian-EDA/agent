//! The symbol-metadata contract now lives in the `symbol-contract` crate (a pure
//! types crate the low-level KiCAD reader can satisfy without depending on this
//! DSL). Re-exported here so `circuit_lang::provider::*` and the crate-root
//! `circuit_lang::{PinType, …}` paths resolve unchanged.

pub use symbol_contract::*;
