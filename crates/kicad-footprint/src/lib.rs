//! KiCAD footprint `.pretty` library access.
//!
//! This crate hides KiCAD's installed library layout, `.kicad_mod` parser gaps,
//! and search ranking details behind stable Rust types.
//!
//! # Contract
//!
//! - **Discovery & inventory.** [`FootprintCatalog`] indexes the `.pretty`
//!   libraries reachable from a *resolved* footprint root. Platform/version
//!   discovery lives in [`kicad_env`]; build with [`FootprintCatalog::from_env`]
//!   to honor `AUTO_PCB_FOOTPRINT_DIR` and known install paths, or
//!   [`FootprintCatalog::from_root`] for an explicit directory (tests, vendored
//!   fixtures).
//! - **Identity.** Footprints are addressed by [`FootprintId`] (`Nickname:Name`),
//!   validated once at the boundary instead of threading bare strings.
//! - **Raw vs parsed.** [`FootprintEntry::source`] returns verbatim
//!   `.kicad_mod` text (for board emission); [`FootprintEntry::parse`] /
//!   [`FootprintCatalog::footprint`] return a parsed [`Footprint`] (for
//!   placement geometry).
//! - **Errors.** Failure reasons that callers must distinguish are surfaced via
//!   [`Error`] — a missing id ([`Error::NotFound`]) is never confused with a
//!   malformed file ([`Error::Parse`]).
//!
//! # Library layout assumptions
//!
//! Footprint libraries are `*.pretty/` directories of `*.kicad_mod` files
//! (stable across KiCAD 6–10). `cli_version` is for diagnostics, not parser
//! switching: the `.kicad_mod` reader tolerates unknown tokens and approximates
//! geometry conservatively.

mod catalog;
mod discover;
mod error;
mod id;
mod parse;
mod search;
mod types;

pub use catalog::{FootprintCatalog, FootprintCatalogBuilder, FootprintEntry, FootprintLibrary};
pub use error::{Error, Result};
pub use id::{FootprintId, LibraryId};
pub use search::{FootprintSearchHit, SearchQuery};
pub use types::{CourtyardSource, Footprint, FootprintPad, PadTechnology};
