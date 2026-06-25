//! KiCAD footprint `.pretty` library access.
//!
//! This crate is the footprint-library analogue of `kicad-symbol`: it hides
//! KiCAD's installed library layout, `.kicad_mod` parser gaps, and search
//! ranking details behind stable Rust types.

mod discover;
mod parse;
mod search;
mod types;

pub use search::{FootprintHit, FootprintIndex};
pub use types::{CourtyardSource, Footprint, FootprintPad, PadTechnology};
