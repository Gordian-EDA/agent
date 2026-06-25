//! Typed wrapper over the `kicad-cli` binary.
//!
//! `cli` shells out to `kicad-cli` for DRC/ERC/plot/export and parses its
//! output. `env` is a compatibility re-export of the standalone `kicad-env`
//! crate.

pub mod cli;
pub mod env;
