//! Symbol metadata vocabulary shared by parsers, providers, and consumers.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinType {
    PowerInput,
    PowerOutput,
    Passive,
    Other,
}

/// Signal DIRECTION of a pin, preserved from KiCAD's electrical type (which
/// [`PinType`] collapses): a net flows from its `Out` pin to its `In` pins, which lets
/// a dataflow-aware placer order parts left->right by signal flow. Orthogonal to
/// [`PinType`] -- kept as a separate field so existing `PinType` matches are
/// untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PinDir {
    In,
    Out,
    Bidir,
    Passive,
    Power,
    #[default]
    Unknown,
}

#[derive(Debug, Clone)]
pub struct PinMeta {
    pub number: String,
    pub name: String,
    pub etype: PinType,
    /// Signal direction (KiCAD electrical type), for dataflow layout.
    pub dir: PinDir,
    pub unit: u8, // 1-based; 1 for single-unit symbols
}

#[derive(Debug, Clone, Default)]
pub struct SymbolMeta {
    pub pins: Vec<PinMeta>,
    /// Human-readable KiCad library description, often including decisive
    /// electrical ratings such as current, voltage, or package.
    pub description: Option<String>,
    /// Manufacturer datasheet URL carried by the KiCad symbol.
    pub datasheet: Option<String>,
    /// Library-default footprint, when the symbol declares one.
    pub footprint: Option<String>,
    /// Search keywords supplied by the KiCad library.
    pub keywords: Option<String>,
}

/// Resolve a pin reference (`id`) within a pin list, matching by **number
/// first, then by name**. This is `sch-check`'s canonical pin-resolution
/// order; reuse it instead of hand-rolling the same `find().or_else(find())`.
pub fn find_pin<'a>(pins: &'a [PinMeta], id: &str) -> Option<&'a PinMeta> {
    pins.iter()
        .find(|p| p.number == id)
        .or_else(|| pins.iter().find(|p| p.name == id))
}
