//! Typed wrapper over the `kicad-cli` binary plus KiCAD environment discovery.
//!
//! `env` locates the KiCAD install + library tables; `cli` shells out to
//! `kicad-cli` for DRC/ERC/plot/export and parses its output.

pub mod cli;
pub mod env;
