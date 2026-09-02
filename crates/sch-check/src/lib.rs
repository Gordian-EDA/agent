//! sch-check: the kernel circuit model plus everything that judges it.
//!
//! The model ([`Design`]) is authoring-agnostic — it is what a `.kicad_sch`
//! extractor or a bulk-create tool call reduces to — and
//! the checkers ([`lint`], [`erc`]) run over it without ever seeing source text.
//! Pure: no I/O.

pub mod authored;
pub mod completeness;
pub mod decouple;
pub mod diag;
pub mod erc;
pub mod lint;
pub mod model;
pub mod nets;
pub mod pins;
pub mod place_parts;

pub use diag::{Diagnostic, Diagnostics, Severity, Span};
pub use kicad_symbol::{PinDir, PinMeta, PinType, SymbolMeta, SymbolTable, find_pin};
pub use model::{Block, Component, Design, LayoutGrid, NetAttrs, Origin, PinTarget};
pub use place_parts::{
    DanglingPin, DuplicateRef, ExistingSheet, Intent, PayloadAudit, PlacePartsInput, into_design,
    place_parts_input_schema,
};
