use geom::{Point2, Rect};
use serde::{Deserialize, Serialize};

/// Through-hole vs. surface-mount, derived from a pad's KiCAD `pad_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadTechnology {
    /// `smd` — surface-mount; lives on one copper face.
    Smd,
    /// `thru_hole` — drilled; spans the whole copper stack.
    ThruHole,
    /// `np_thru_hole` — non-plated mechanical hole (no copper).
    NpThruHole,
    /// `connect` or anything else KiCAD may add — treated conservatively as
    /// surface copper by consumers that must pick.
    Other,
}

/// How the courtyard bbox was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CourtyardSource {
    /// Bounding box of the `F.CrtYd` / `B.CrtYd` graphics actually present.
    Crtyd,
    /// No courtyard layer present: bbox of pads plus silkscreen graphics.
    PadSilkFallback,
}

/// One pad of a footprint, reference-designator-agnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FootprintPad {
    /// Pad number/name as a string (`"1"`, `"A1"`, `"GND"`); may repeat.
    pub number: String,
    /// Centre offset in the footprint frame, millimetres.
    pub at: Point2,
    /// Local pad rotation in degrees, if the file specifies one.
    pub rotation: f64,
    /// Pad copper size as width/height, millimetres.
    pub size: Point2,
    /// KiCAD pad shape token (`rect`, `roundrect`, `circle`, `oval`, ...).
    pub shape: String,
    /// Copper/technical layers the pad occupies (`F.Cu`, `*.Cu`, ...).
    pub layers: Vec<String>,
    /// Mounting technology (SMD / through-hole / ...).
    pub technology: PadTechnology,
    /// Drill diameter in millimetres for a through-hole pad, else `None`.
    pub drill: Option<f64>,
}

/// A parsed footprint: everything placement needs before board context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    /// Bare footprint name (the `.kicad_mod` stem).
    pub name: String,
    /// Free-text description from `(descr ...)`, if any.
    pub descr: Option<String>,
    /// Reference-designator-agnostic pad list.
    pub pads: Vec<FootprintPad>,
    /// Courtyard bounding box in the footprint frame.
    pub courtyard: Rect,
    /// How [`Self::courtyard`] was derived.
    pub courtyard_source: CourtyardSource,
    /// Overall bounding box over pads and graphics.
    pub bbox: Rect,
}

impl Footprint {
    /// Number of pads — the count placement validates against a symbol's pins.
    pub fn pad_count(&self) -> usize {
        self.pads.len()
    }
}
