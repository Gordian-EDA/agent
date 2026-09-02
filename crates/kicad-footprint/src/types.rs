use geom::{Point2, Rect};
use serde::{Deserialize, Serialize};

use crate::id::FootprintId;

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

impl PadTechnology {
    /// Stable lowercase token, matching the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            PadTechnology::Smd => "smd",
            PadTechnology::ThruHole => "thru_hole",
            PadTechnology::NpThruHole => "np_thru_hole",
            PadTechnology::Other => "other",
        }
    }
}

/// How a footprint's courtyard bounding box was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CourtyardSource {
    /// Bounding box of the `F.CrtYd` / `B.CrtYd` graphics actually present.
    ExplicitCourtyard,
    /// No courtyard layer present: estimated from pads plus silkscreen graphics.
    EstimatedFromPadsAndSilkscreen,
}

impl CourtyardSource {
    /// Stable lowercase token, matching the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            CourtyardSource::ExplicitCourtyard => "explicit_courtyard",
            CourtyardSource::EstimatedFromPadsAndSilkscreen => "estimated_from_pads_and_silkscreen",
        }
    }
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

/// A footprint-local line explicitly identifying where the finished PCB edge belongs.
///
/// KiCad connector footprints commonly draw this on `Dwgs.User` beside a
/// `PCB Edge` user-text marker. It is mechanical placement metadata, not part of
/// the footprint courtyard or copper geometry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PcbEdgeDatum {
    pub start: Point2,
    pub end: Point2,
}

impl FootprintPad {
    /// The COPPER layers this pad occupies — `F.Cu`, `B.Cu`, `In2.Cu`, `*.Cu`.
    ///
    /// A pad may also list `F.Paste` / `F.Mask`; those are stencil and solder
    /// resist, not copper, and a caller reasoning about clearance or nets must
    /// not see them. A paste-only pad has no copper layers at all.
    pub fn copper_layers(&self) -> impl Iterator<Item = &str> {
        self.layers
            .iter()
            .map(String::as_str)
            .filter(|layer| layer.ends_with(".Cu"))
    }

    /// Whether the pad is a drilled through-hole pad (plated or not).
    pub fn is_through_hole(&self) -> bool {
        matches!(
            self.technology,
            PadTechnology::ThruHole | PadTechnology::NpThruHole
        )
    }
}

/// A parsed footprint: everything placement needs before board context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    /// The library id this footprint was resolved under, when known.
    ///
    /// `None` for a standalone parse ([`Footprint::from_file`] /
    /// [`Footprint::parse_str`]); set when obtained via
    /// [`crate::FootprintCatalog::footprint`].
    pub id: Option<FootprintId>,
    /// Bare footprint name (the `.kicad_mod` stem).
    pub name: String,
    /// Free-text description from `(descr ...)`, if any.
    pub descr: Option<String>,
    /// Reference-designator-agnostic pad list.
    pub pads: Vec<FootprintPad>,
    /// Courtyard bounding box in the footprint frame (placement keep-out).
    pub courtyard: Rect,
    /// How [`Self::courtyard`] was derived.
    pub courtyard_source: CourtyardSource,
    /// Overall bounding box over every pad and graphic element.
    pub bounds: Rect,
    /// Explicit footprint-local PCB-edge line, when the library footprint
    /// supplies a labelled `Dwgs.User` datum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcb_edge_datum: Option<PcbEdgeDatum>,
}

impl Footprint {
    /// Number of pads — the count placement validates against a symbol's pins.
    pub fn pad_count(&self) -> usize {
        self.pads.len()
    }
}
