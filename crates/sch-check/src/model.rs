use indexmap::IndexMap;

pub use sch_model::item::refdes_key;

pub type RefDes = String;
pub type NetName = String;
pub type BlockName = String;

/// The kernel design: parts, their pin→net map, and net attributes.
///
/// The one thing the checkers, the placement engines, and the writer operate
/// on — whether built from a live `.kicad_sch` or a tool call. It holds
/// connectivity and intent, never geometry.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Design {
    pub name: Option<String>,
    pub description: Option<String>,
    pub blocks: IndexMap<BlockName, Block>,
    pub nets: IndexMap<NetName, NetAttrs>,
    /// Lint codes this design suppresses as deliberate exceptions.
    pub lint_allow: std::collections::BTreeSet<String>,
}


#[derive(Debug, Default, Clone, PartialEq)]
pub struct Block {
    /// Caption drawn on the block's frame. Defaults to the block's own name.
    pub title: Option<String>,
    /// One line under the frame explaining a decision a reader would otherwise
    /// have to reverse-engineer — what a human writes on a schematic.
    pub note: Option<String>,
    pub components: IndexMap<RefDes, Component>,
    /// How this module is arranged: the row/col tree its author composed
    /// (`layout:`). `None` = nobody composed one, and the typesetter falls back to
    /// a single row.
    pub layout: Option<sch_model::tree::Tree>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub part: String, // full lib_id after alias desugar, e.g. "Device:R"
    pub value: Option<String>,
    pub footprint: Option<String>,
    pub dnp: bool,
    pub props: IndexMap<String, String>,
    /// Component-level pin map; resolves across units.
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

/// Identity for reconciliation: authored components match by refdes;
/// sugar-synthesized ones by (parent, role, index) — carried into the sch file
/// as ap_parent/ap_role/ap_index properties.
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
