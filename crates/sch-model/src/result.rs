//! Shared emission types and identity-property keys.
//!
//! These live in `sch-model` so the floorplan engine (`sch-floorplan`) and the
//! live schematic tools can share them.

/// Property key naming the functional block a symbol belongs to. Blocks exist in
/// a placement payload; this is what carries one onto the sheet, so a later call
/// can say `arrange{block}` about parts it did not place itself.
pub const AP_BLOCK: &str = "ap_block";

/// Property key marking a symbol as benched — on the sheet and on its nets, but
/// not laid out. `"1"` when set, cleared when the symbol is arranged.
pub const AP_BENCH: &str = "ap_bench";

/// A circuit idiom the engine RECOGNIZED purely from connectivity and co-placed as
/// one cohesive cluster (a crystal+its load caps, a decoupling bank, an op-amp
/// feedback resistor). Reported back so the LLM can confirm the layout matched its
/// intent — detection needs NO new YAML syntax, only the netlist.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct IdiomReport {
    /// `"crystal"` | `"decoupling"` | `"feedback"`.
    pub kind: String,
    /// The IC (anchor) refdes the cluster serves.
    pub anchor: String,
    /// The refdes of every part placed as part of this idiom.
    pub parts: Vec<String>,
}

/// The rendered schematic plus deterministic readability findings.
pub struct EmitOutput {
    /// The assembled `.kicad_sch` document text.
    pub sch: String,
    /// One human-readable warning per overlapping symbol/label pair (empty when
    /// the layout is clean). A side-channel only: it does not alter `sch`.
    pub layout_warnings: Vec<String>,
    /// The shipped sheet's body / IC / wire-crossing triple (ground truth, the same
    /// `fan_risers=true` measure the engines pick on). A vision critic systematically
    /// over-reports these, so they are the objective signal behind its complaints.
    pub crossings: crate::place::Crossings,
    /// Idioms the engine recognized + co-placed (crystal, decoupling, feedback),
    /// surfaced to the agent loop by schematic mutators.
    pub detected_idioms: Vec<IdiomReport>,
    /// One line per point where the realised sheet puts two nets — the shorts the
    /// netlist would show. Empty is the invariant; a non-empty list is an engine
    /// defect, not a payload one.
    pub net_shorts: Vec<String>,
    /// One name per authored net whose pins did NOT all land on one net of the
    /// finished sheet — an OPEN. The dual of [`Self::net_shorts`]: a short welds two
    /// nets, an open leaves one in islands, and both are invisible on the render.
    /// Empty is the invariant; a non-empty list is an engine defect.
    pub net_opens: Vec<String>,
}
