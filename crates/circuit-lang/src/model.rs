use indexmap::IndexMap;

pub type RefDes = String;
pub type NetName = String;
pub type BlockName = String;

/// Kernel design — post-desugar. This is the ONLY thing the
/// validator, reconciler, and lift operate on (spec §5 layering).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Design {
    pub name: Option<String>,
    pub description: Option<String>,
    pub blocks: IndexMap<BlockName, Block>,
    pub nets: IndexMap<NetName, NetAttrs>,
    /// Lint codes suppressed via the top-level `lint: {allow: [...]}` section.
    pub lint_allow: std::collections::BTreeSet<String>,
}

/// A module's author-facing placement grid (the per-block `layout:` 2D array).
/// Each row is a left→right list of cells; a cell names one of THIS block's
/// refdes, or is `None` for a `~` hole. Column index = x, row index = y
/// (ordinal). Empty = no grid → the engine arranges the block's parts by
/// inference. A refdes repeated down a column **spans** those rows and floats
/// within that span (a tall IC). Inter-block placement is always inferred — the
/// grid only controls a module's internal arrangement, the thing inference can't
/// derive from connectivity.
pub type LayoutGrid = Vec<Vec<Option<String>>>;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Block {
    pub note: Option<String>,
    pub components: IndexMap<RefDes, Component>,
    /// This module's internal placement grid (per-block `layout:`). See
    /// [`LayoutGrid`]. Empty = infer the block's internal arrangement.
    pub layout: LayoutGrid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub part: String, // full lib_id after alias desugar, e.g. "Device:R"
    pub value: Option<String>,
    pub footprint: Option<String>,
    pub dnp: bool,
    pub props: IndexMap<String, String>,
    /// Component-level pin map; resolves across units (spec §5.2).
    pub pins: IndexMap<String, PinTarget>,
    /// Multi-unit parts: unit letter -> pin map.
    pub units: IndexMap<String, IndexMap<String, PinTarget>>,
    pub origin: Origin,
}

impl Default for Component {
    fn default() -> Self {
        Self {
            part: String::new(),
            value: None,
            footprint: None,
            dnp: false,
            props: IndexMap::new(),
            pins: IndexMap::new(),
            units: IndexMap::new(),
            origin: Origin::Authored,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinTarget {
    Net(NetName),
    NoConnect,
}

/// Identity for reconciliation (spec §7): authored components match by
/// refdes; sugar-synthesized ones by (parent, role, index) — carried
/// into the sch file as ap_parent/ap_role/ap_index properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Authored,
    Synthesized {
        parent: RefDes,
        role: String,
        index: u32,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NetAttrs {
    pub power: bool,
    /// This net is a board I/O PORT — drawn with a global-label pennant at the sheet
    /// edge. DERIVED (like `power`) from author-placed `label:global` components, not
    /// stored in YAML; re-marked each desugar so the canonical round-trip is stable.
    pub port: bool,
    pub class: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_equality_is_structural() {
        let mut a = Design::default();
        a.blocks.insert("main".into(), Block::default());
        let mut b = Design::default();
        b.blocks.insert("main".into(), Block::default());
        assert_eq!(a, b);
        b.nets.insert(
            "GND".into(),
            NetAttrs {
                power: true,
                port: false,
                class: None,
            },
        );
        assert_ne!(a, b);
    }
}
