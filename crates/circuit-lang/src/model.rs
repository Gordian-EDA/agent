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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Block {
    pub note: Option<String>,
    pub layout: LayoutHint,
    pub components: IndexMap<RefDes, Component>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LayoutHint {
    pub edge: Option<Edge>,
    pub near: Option<BlockName>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Placement-relevant role inferred at desugar time (spec Phase 2 item 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutRole {
    /// A two-pin passive spanning at least one power net (e.g. rail↔ground, or
    /// rail↔rail). Drawn vertical; when one pin is ground it is flipped so the
    /// ground-side pin sits at the bottom (orientation policy lives in sch-engine).
    RailSpan,
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
    /// Placement role inferred at desugar time, if any.
    pub layout_role: Option<LayoutRole>,
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
            layout_role: None,
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
                class: None,
            },
        );
        assert_ne!(a, b);
    }
}
