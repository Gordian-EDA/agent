//! Shared emission types and identity-property keys.
//!
//! These live in `sch-model` so the floorplan engine (`sch-floorplan`) and the
//! live schematic tools can share them.

/// Property key naming the functional block a symbol belongs to. Blocks exist in
/// a placement payload; this is what carries one onto the sheet, so a later call
/// can say `arrange{block}` about parts it did not place itself.
pub const AP_BLOCK: &str = "ap_block";

/// The region every part joins when its payload names none.
pub const DEFAULT_BLOCK: &str = "main";

/// The region a whole-sheet re-arrange lifts the existing drawing into.
pub const SHEET_BLOCK: &str = "$sheet";

/// Whether a region name was synthesized by the tools rather than chosen by the author.
/// Neither synthesized name is a functional block, so neither is tagged onto a symbol
/// ([`AP_BLOCK`]) nor drawn as a frame.
pub fn synthesized_block(name: &str) -> bool {
    name == DEFAULT_BLOCK || name == SHEET_BLOCK
}

/// Property key recording the authored net of the symbol's pins whose name the
/// drawing itself does not carry. A block wires its own nets and labels none of
/// them, so the name the author gave one is gone the moment KiCAD re-derives it —
/// and the next block, which joins by name, finds nothing. This is where that name
/// survives between calls. Written as `pin=net|pin=net`.
pub const AP_NETS: &str = "ap_nets";

/// Property key marking a symbol as benched — on the sheet and on its nets, but
/// not laid out. `"1"` when set, cleared when the symbol is arranged.
pub const AP_BENCH: &str = "ap_bench";

/// The rendered schematic plus deterministic readability findings.
pub struct EmitOutput {
    /// The assembled `.kicad_sch` document text.
    pub sch: String,
    /// One human-readable warning per overlapping symbol/label pair (empty when
    /// the layout is clean). A side-channel only: it does not alter `sch`.
    pub layout_warnings: Vec<String>,
    /// The shipped sheet's body / IC / wire-crossing triple (ground truth). A vision
    /// critic systematically over-reports these, so they are the objective signal
    /// behind its complaints.
    pub crossings: crate::place::Crossings,
    /// One line per point where the realised sheet puts two nets — the shorts the
    /// netlist would show. Empty is the invariant; a non-empty list is a typesetter
    /// defect, not a payload one.
    pub net_shorts: Vec<String>,
    /// One name per authored net whose pins did NOT all land on one net of the
    /// finished sheet — an OPEN. The dual of [`Self::net_shorts`]: a short welds two
    /// nets, an open leaves one in islands, and both are invisible on the render.
    /// Empty is the invariant; a non-empty list is a typesetter defect.
    pub net_opens: Vec<String>,
}
