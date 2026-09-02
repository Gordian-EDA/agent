//! `sch-doc` — the editable `.kicad_sch` document and its connectivity.
//!
//! A schematic is the source of truth for the design, so this crate opens one
//! *without losing anything*: [`SchDoc`] decodes typed fields for exactly what
//! tools touch — symbols, wires, junctions, no-connects, labels, text, sheets
//! and the embedded `lib_symbols` — and retains every other node verbatim, in
//! document order. Buses, images, rule areas, netclass flags and whatever KiCAD
//! adds next survive an edit untouched.
//!
//! ## Why the CST and not a typed AST
//!
//! `kiutils_kicad` parses `.kicad_sch` into a typed AST, but that AST is
//! read-only over the CST by design, its mutators cover only header scalars and
//! symbol properties, and its "lossless" write re-emits the original bytes
//! while its editing path *canonicalises the whole file* onto one line per
//! node. Neither end is usable for in-place editing. The layer underneath it —
//! `kiutils_sexpr`'s `Node`/`Span` CST — is exactly right, so the typed model
//! here sits directly on that.
//!
//! ## Write stability
//!
//! Each item remembers the byte span it was parsed from and forgets it the
//! moment an edit invalidates it. Untouched items are re-emitted from those
//! bytes, so parsing and writing an unedited file reproduces it byte for byte,
//! and an edited file differs only at the items that changed. Rewritten items
//! use the KiCad printer dialect (tab indent, one child per line, four
//! decimals), which is a fixed point of itself.
//!
//! ## Connectivity
//!
//! [`connect::extract`] derives the net partition from geometry alone — no
//! `kicad-cli` round trip — and [`Netlist::diff`] turns two extractions into the
//! [`NetDelta`] every editing tool reports back.
//!
//! Its scope is one file: a hierarchical sheet's pins are connection points but
//! do not pull in the child's connectivity. What it cannot model it says so
//! about, in [`Netlist::warnings`] — buses, a `lib_id` with no embedded
//! definition, and a sheet placed more than once (whose reference designators
//! are ambiguous outside the hierarchy). It never guesses.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use sch_doc::{SchDoc, connect};
//!
//! let mut doc = SchDoc::read("board.kicad_sch")?;
//! let before = connect::extract(&doc);
//! let undo = doc.snapshot();
//! doc.move_symbol("R1", 127.0, 63.5)?;
//! let delta = connect::Netlist::diff(&before, &connect::extract(&doc));
//! if !delta.is_empty() {
//!     doc.restore(undo)?;
//! }
//! doc.write("board.kicad_sch")?;
//! # Ok(()) }
//! ```

pub mod body;
pub mod connect;
mod doc;
mod edit;
mod error;
mod libsyms;
mod model;
mod pins;
mod sexpr;
mod text;

pub use body::{body_rect, body_rects};
pub use connect::{Net, NetDelta, NetSource, Netlist, PinRef, Scene};
pub use doc::{SchDoc, SnapshotId};
pub use edit::is_drawing;
pub use error::{Error, Result};
pub use libsyms::SymbolSource;
pub use model::{
    Field, Item, Junction, Label, LabelKind, LibSymbols, Mirror, NoConnect, Pose, Retained, Sheet,
    SheetPin, SymbolInst, Text, Wire,
};
pub use pins::{PlacedPin, placed_pins};
pub use text::{escape, unescape};

/// Pretty-print one top-level item, bypassing its retained bytes — the path an
/// edited item takes on write. Exposed so the round-trip gate can force every
/// item through the printer.
#[doc(hidden)]
pub fn print_item(item: &Item, out: &mut String) {
    sexpr::print(&item.encode(), 1, out);
}
