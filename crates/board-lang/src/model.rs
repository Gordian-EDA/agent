//! Kernel model for the board DSL — the PCB-side analog of circuit-lang's
//! `Design`. A `BoardDesign` is the full design intent for one board: its
//! outline + rules, the parts (footprint + pad→net + per-part placement
//! flags), and the placement-intent groups. It is GEOMETRY-FREE except for
//! optional explicit locks — the engine computes part coordinates and copper.
//!
//! This is the source of truth the agent authors and that round-trips with a
//! `.kicad_pcb`; it compiles down to the engine's `BoardDraft`.

use indexmap::IndexMap;

pub type RefDes = String;
pub type NetName = String;

#[derive(Debug, Clone, PartialEq)]
pub struct BoardDesign {
    pub name: Option<String>,
    pub board: BoardSpec,
    /// Parts keyed by reference designator, author order preserved.
    pub parts: IndexMap<RefDes, Part>,
    /// Placement-intent groups (cohesion / region / edge / surround).
    pub groups: IndexMap<String, Group>,
}

impl Default for BoardDesign {
    fn default() -> Self {
        Self {
            name: None,
            board: BoardSpec::default(),
            parts: IndexMap::new(),
            groups: IndexMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoardSpec {
    /// Copper layer count (2, 4, 6, 8).
    pub layers: u32,
    pub outline: Outline,
    pub rules: Rules,
}

impl Default for BoardSpec {
    fn default() -> Self {
        Self {
            layers: 2,
            outline: Outline::Rect { w: 40.0, h: 30.0 },
            rules: Rules::default(),
        }
    }
}

/// Board outline. `Rect` spans the origin to (w, h); `Circle` is centred in its
/// own bbox; `Polygon` is a closed point list (mm, y-down).
#[derive(Debug, Clone, PartialEq)]
pub enum Outline {
    Rect { w: f64, h: f64 },
    Circle { r: f64 },
    Polygon(Vec<(f64, f64)>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rules {
    pub clearance: f64,
    pub trace_width: f64,
    pub via_diameter: f64,
    pub via_drill: f64,
    /// Per-net trace-width overrides (net → mm): fat power, thin signal.
    pub net_widths: IndexMap<NetName, f64>,
    /// Copper pours / planes on a layer for a net.
    pub pours: Vec<Pour>,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            clearance: 0.2,
            trace_width: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: IndexMap::new(),
            pours: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pour {
    pub net: NetName,
    /// Layer name: `top` / `bottom` / `in1` / `in2` / ...
    pub layer: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    /// Fully-qualified footprint lib_id, e.g. `Resistor_SMD:R_0603_1608Metric`.
    pub footprint: String,
    /// Pad number → net name. An absent pad is unconnected.
    pub pads: IndexMap<String, NetName>,
    /// Pull this part to the nearest board edge (connectors, headers).
    pub edge: bool,
    /// Pull this part to the nearest board corner (mounting holes).
    pub corner: bool,
    /// Pin the part at an explicit position (mm) + rotation (0/90/180/270).
    pub lock: Option<Lock>,
}

impl Default for Part {
    fn default() -> Self {
        Self {
            footprint: String::new(),
            pads: IndexMap::new(),
            edge: false,
            corner: false,
            lock: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lock {
    pub x: f64,
    pub y: f64,
    pub rot: i32,
}

/// A placement-intent group. Mirrors the engine's `GroupHint`: members cohere,
/// and may be confined to a `region`, hug an `edge`, ring a `surround` target,
/// or tile as a `grid`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Group {
    pub members: Vec<RefDes>,
    /// [min_x, min_y, max_x, max_y] in mm.
    pub region: Option<[f64; 4]>,
    /// Board edge to hug: `n` / `s` / `e` / `w`.
    pub edge: Option<String>,
    /// Reference of a part to ring the members around (decoupling pattern).
    pub surround: Option<RefDes>,
    /// Tile members in a regular grid filling `region` (requires `region`).
    pub grid: bool,
}
