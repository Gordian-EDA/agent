//! Shared emission types and identity-property keys.
//!
//! These live in `sch-layout` so the modern floorplan engine ([`crate::floorplan`])
//! and the round-trip lifter ([`crate::lift`]) can use them without depending on
//! the legacy reconcile path. The legacy `sch-engine` crate re-imports them.

/// Property key for the block a component belongs to.
pub const AP_BLOCK: &str = "ap_block";
/// Property key for a synthesized component's role (absent / `"authored"` for
/// authored parts).
pub const AP_ROLE: &str = "ap_role";
/// Property key for a synthesized component's parent refdes.
pub const AP_PARENT: &str = "ap_parent";
/// Property key for a synthesized component's index within `(parent, role)`.
pub const AP_INDEX: &str = "ap_index";

/// Property key recording the layout-revision a component was placed under.
pub const AP_LAYOUT_REV: &str = "ap_layout_rev";

/// The `ap_role` value written for authored components.
pub const ROLE_AUTHORED: &str = "authored";

/// Which prior placements to discard on re-emit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Relayout {
    /// Honor every surviving prior placement whose layout-rev still matches.
    #[default]
    None,
    /// Discard all prior placements; the placer lays out everything fresh.
    All,
    /// Discard prior placements only for the named blocks.
    Blocks(std::collections::BTreeSet<String>),
}

impl Relayout {
    /// Whether prior placements for `block_name` must be discarded (placer lays
    /// the block out fresh). The single source of the relayout decision — shared
    /// by `resolve_placement` and the cluster-origin loop.
    pub fn forces(&self, block_name: &str) -> bool {
        match self {
            Relayout::All => true,
            Relayout::Blocks(names) => names.contains(block_name),
            Relayout::None => false,
        }
    }
}

/// The rendered schematic plus deterministic readability findings.
pub struct EmitOutput {
    /// The assembled `.kicad_sch` document text.
    pub sch: String,
    /// One human-readable warning per overlapping symbol/label pair (empty when
    /// the layout is clean). A side-channel only: it does not alter `sch`.
    pub layout_warnings: Vec<String>,
    /// Per-block count of components re-placed this emit (rev changed or a
    /// `Relayout` forced it). Empty when every surviving placement was preserved.
    pub relayout_blocks: std::collections::BTreeMap<String, usize>,
}
