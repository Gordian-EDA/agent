//! The Layout IR — the geometry-free "frame" the LLM emits: the global flow,
//! which nets are rails (and their band), where the anchors (ICs) sit, which nets
//! exit as ports. The engine (`sch-floorplan`) turns this into exact millimetre
//! placement; the LLM never sees a coordinate. This module is just the data
//! vocabulary; the inference that *produces* an IR lives in the engine.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::result::IdiomReport;

/// Global signal-flow direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Flow {
    /// Left → right (signals flow horizontally). The common case.
    #[default]
    Lr,
    /// Top → bottom.
    Tb,
}

/// Which horizontal band a rail net occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Band {
    Top,
    Bottom,
}

/// Which sheet edge a port net exits toward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// Orientation of a 2-pin part, stated as the direction its pins run — from its
/// first connected net (pin 1) toward its second (pin 2). The engine works out
/// the exact rotation from the symbol's own pin geometry, so the LLM never
/// reasons about a symbol's native axis or KiCAD angles; it just says which way
/// the part points. `down` (pin 1 on top, e.g. a divider leg from VCC down to
/// GND) is the common default. ICs/connectors ignore this (they stay at 0°; use
/// `mirror` to flip them left-to-right).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Orient {
    /// Pin 1 at the bottom, pin 2 at the top.
    Up,
    /// Pin 1 at the top, pin 2 at the bottom (the usual passive orientation).
    #[default]
    Down,
    /// Pin 1 on the right, pin 2 on the left.
    Left,
    /// Pin 1 on the left, pin 2 on the right (a series element along the flow).
    Right,
}

/// A coarse, unitless placement cell + orientation. The engine maps the
/// (col,row) grid to mm — each column sized to its widest part, each row to its
/// tallest — and places the symbol at the cell centre. `col` grows right, `row`
/// grows down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub col: i32,
    pub row: i32,
    #[serde(default)]
    pub orient: Orient,
}

/// A titled section frame the emitter draws around a region (dashed box +
/// bold name, the human "functional section" idiom). Engine-computed from the
/// FINAL placement; rect is `[min_x, min_y, max_x, max_y]` in sheet mm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SectionBox {
    pub name: String,
    pub rect: [f64; 4],
}

/// The geometry-free floorplan. Four keys; everything else is inferred from
/// connectivity by the compiler's fixed rule set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutIr {
    #[serde(default)]
    pub flow: Flow,
    /// Net → band. Nets drawn as spanning rails.
    #[serde(default)]
    pub rails: BTreeMap<String, Band>,
    /// Refdes → coarse cell. Usually only ICs; any refdes may be pinned.
    #[serde(default)]
    pub place: BTreeMap<String, Cell>,
    /// Net → edge side. Nets that exit as labelled ports.
    #[serde(default)]
    pub ports: BTreeMap<String, Side>,
    /// Anchors (ICs) to flip left-to-right, so the pins facing their neighbours
    /// point the right way (e.g. a level translator's B-side toward a connector).
    #[serde(default)]
    pub mirror: BTreeSet<String>,
    /// Refdes → authored grid bounding box `[col_min, row_min, col_max, row_max]`
    /// in composed grid-ordinal coords (from the per-block `layout:`). The search
    /// holds gridded parts in this RELATIVE order — left/right by column, top/bottom
    /// by row — so the author's arrangement is "relatively rigid"; a part spanning a
    /// column range floats within it. Empty on the sidecar/baseline paths (no
    /// authored grid ⇒ no ordering constraint, so tuned references are unaffected).
    #[serde(default)]
    pub grid: BTreeMap<String, [i32; 4]>,
    /// Idioms the engine recognized from connectivity and co-placed as cohesive
    /// clusters (crystal+load-caps, decoupling bank, op-amp feedback). Surfaced to
    /// the agent via `EmitOutput.detected_idioms`. `#[serde(default)]` so existing
    /// sidecar `layout.json` files (which never carry it) still deserialize.
    #[serde(default)]
    pub idioms: Vec<IdiomReport>,
    /// Refdes the placement search must NOT move — an idiom cluster's members,
    /// pinned so their recognized arrangement ships intact.
    #[serde(default)]
    pub frozen: BTreeSet<String>,
    /// Power nets the author wants drawn as DISTRIBUTED LOCAL grounds/supplies — one
    /// power symbol per pin (the professional "drop a GND triangle at each pin" style)
    /// — instead of one sheet-spanning rail. The signal is the author declaring ≥2
    /// power symbols for the net (`GND1`, `GND2`, …); a board with one keeps the rail.
    /// Tames the long-rail sprawl of a dense MCU. `#[serde(default)]` so sidecars (one
    /// symbol per rail) deserialize empty and the tuned references stay rails.
    #[serde(default)]
    pub rail_locals: BTreeSet<String>,
    /// The inverse of [`Self::rail_locals`]: power nets to draw as ONE shared trunk even when
    /// the per-pin heuristic would distribute them. The cluster engine sets this for a power
    /// net whose pins it laid in a single aligned row (the "modules between rails" idiom), then
    /// keeps it only if its safety net confirms the trunk de-sprawls without colliding. Empty
    /// elsewhere ⇒ references byte-identical.
    #[serde(default)]
    pub rail_force: BTreeSet<String>,
    /// Titled section frames to draw around aligned regions (motif grids, the
    /// strap column, connector banks). Empty everywhere except engines that
    /// compute them ⇒ existing paths byte-identical.
    #[serde(default)]
    pub sections: Vec<SectionBox>,
    /// HYBRID VLM placement: refdes → a COARSE target position as a fraction of the
    /// board bbox, `[fx, fy]` in 0..1 (fx: 0=left,1=right; fy: 0=top,1=bottom). A vision
    /// LLM is good at rough DIRECTION ("power left, MCU centre") but not millimetre
    /// positions, so this is applied as a SOFT bias in the annealer's placement cost (its
    /// `zbias` term), NOT a forced cell — the engine still does the precise placement, just
    /// nudged toward the LLM's zones. Empty on every existing path ⇒ no bias ⇒ unchanged.
    #[serde(default)]
    pub zone: BTreeMap<String, [f64; 2]>,
}

impl LayoutIr {
    /// Deserialize an IR from JSON (the subagent's structured output / a test
    /// fixture sidecar).
    pub fn from_json(s: &str) -> serde_json::Result<LayoutIr> {
        serde_json::from_str(s)
    }
}
