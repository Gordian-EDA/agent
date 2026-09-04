//! `Item` — one placed symbol instance, the unit every placement engine moves, plus
//! the `Incidence` net→pins index. Shared by the infer, place, and wire stages, so
//! it lives in the model layer (below all of them).

use std::collections::BTreeMap;

use ::geom::Point2;
use kicad_symbol::geometry::SymbolGeometry;

/// The side of the symbol body a pin sits on, from its local geometry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinSide {
    East,
    West,
    North,
    South,
}

/// Classify a pin's local `(x, y)` offset into the body side it sits on.
pub fn pin_side(at: Point2) -> PinSide {
    if at.x.abs() >= at.y.abs() {
        if at.x >= 0.0 {
            PinSide::East
        } else {
            PinSide::West
        }
    } else if at.y >= 0.0 {
        PinSide::North // symbol-local +y is up; the pin points up = top side
    } else {
        PinSide::South
    }
}

/// One placed component plus the data the engine needs about it.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Item {
    pub refdes: String,
    /// The design region this part joined. Placement never reads it; the realiser
    /// draws one dashed frame per region, which is how a human sheet says where
    /// one functional block ends and the next begins. Empty when the caller has no
    /// regions (a live sheet lifted back off the document).
    #[serde(default)]
    pub block: String,
    pub part: String,
    pub value: String,
    /// Footprint lib_id from the kernel `Component`, carried to emit so the
    /// `.kicad_sch` symbol records its assignment. Multi-unit parts set this on the
    /// FIRST emitted unit only (like `value`) to avoid duplicate fields.
    pub footprint: Option<String>,
    pub geom: SymbolGeometry,
    /// (pin number, pin name, net or None for NC). For a multi-unit part this
    /// holds only the pins of THIS item's `unit` (each unit is its own Item).
    pub pins: Vec<(String, String, Option<String>)>,
    pub at: Point2,
    pub angle: f64,
    /// 1-based symbol unit this Item places. Single-unit parts are 1; a multi-unit
    /// part (op-amp/FPGA) splits into one Item per used unit, all sharing `refdes`
    /// but emitted as distinct `(unit N)` instances.
    pub unit: u8,
    /// Whether this symbol is flipped left↔right. Seeded from `ir.mirror`; lifted
    /// onto the Item so the placement search can flip it as a move and the cost
    /// sees exactly what ships.
    pub mirror: bool,
    /// This item arrived with a LIVE pose its caller owns (the region adapter's fixed
    /// neighbours, lifted off a real sheet), so seeding must leave `at`/`angle` alone.
    /// Every whole-sheet path builds items at the origin with this clear.
    pub preseeded: bool,
    /// The refdes this part was SYNTHESIZED to support — the parent of a `decouple`
    /// cap. Such a part exists only because the sugar expanded, so its author never saw
    /// it and could not have given it a place in the layout tree; the typesetter seats it
    /// beside the part it supports rather than in the leftovers row.
    #[serde(default)]
    pub supports: Option<String>,
}

/// Natural refdes sort key: alpha prefix + numeric suffix, so `J2` < `J10`.
/// Malformed suffixes sort last within their prefix.
pub fn refdes_key(r: &str) -> (&str, u64) {
    let split = r.find(|c: char| c.is_ascii_digit()).unwrap_or(r.len());
    let (alpha, num) = r.split_at(split);
    (alpha, num.parse().unwrap_or(u64::MAX))
}

/// Net name → the `(item index, pin number)` pairs incident on it.
pub type Incidence = BTreeMap<String, Vec<(usize, String)>>;
