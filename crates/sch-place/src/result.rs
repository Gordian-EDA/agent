//! Shared emission types and identity-property keys.
//!
//! These live in `sch-place` so the floorplan engine (`sch-floorplan`) and the
//! round-trip reader (`sch-io::read`) can share them.

/// Property key for the block a component belongs to.
pub const AP_BLOCK: &str = "ap_block";
/// Property key for a synthesized component's role (absent / `"authored"` for
/// authored parts).
pub const AP_ROLE: &str = "ap_role";
/// Property key for a synthesized component's parent refdes.
pub const AP_PARENT: &str = "ap_parent";
/// Property key for a synthesized component's index within `(parent, role)`.
pub const AP_INDEX: &str = "ap_index";

/// The `ap_role` value written for authored components.
pub const ROLE_AUTHORED: &str = "authored";

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
    /// surfaced to the agent loop via `apply_design`.
    pub detected_idioms: Vec<IdiomReport>,
}
