//! The Layout IR — the geometry-free "frame" the LLM emits: which nets are rails
//! (and their band), which nets exit as ports, and the row/col layout tree per
//! block. `sch-floorplan` turns this into exact millimetre placement; the LLM
//! never sees a coordinate. This module is just the data vocabulary; the
//! inference that *produces* an IR lives in `sch-floorplan`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::tree::Trees;

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
    /// `above` is what a caller writes when the neighbouring relation kinds are
    /// `above`/`below`; it means this edge.
    #[serde(alias = "above", alias = "up")]
    Top,
    #[serde(alias = "below", alias = "down")]
    Bottom,
}

/// The geometry-free floorplan. Four keys; everything else is inferred from
/// connectivity by the compiler's fixed rule set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutIr {
    /// Net → band. Nets drawn as spanning rails.
    #[serde(default)]
    pub rails: BTreeMap<String, Band>,
    /// Net → edge side. Nets that exit as labelled ports.
    #[serde(default)]
    pub ports: BTreeMap<String, Side>,
    /// Power nets the author wants drawn as DISTRIBUTED LOCAL grounds/supplies — one
    /// power symbol per pin (the professional "drop a GND triangle at each pin" style)
    /// — instead of one sheet-spanning rail. The signal is the author declaring ≥2
    /// power symbols for the net (`GND1`, `GND2`, …); a board with one keeps the rail.
    /// Tames the long-rail sprawl of a dense MCU. `#[serde(default)]` so sidecars (one
    /// symbol per rail) deserialize empty and the tuned references stay rails.
    #[serde(default)]
    pub rail_locals: BTreeSet<String>,
    /// Block name → the row/col arrangement its author composed
    /// ([`crate::tree::Tree`]). This is the layout: the typesetter measures the
    /// symbols and computes every coordinate from it. A block absent here is
    /// arranged as one default row.
    #[serde(default)]
    pub trees: Trees,
}

impl LayoutIr {
    /// Deserialize an IR from JSON (the subagent's structured output / a test
    /// fixture sidecar).
    pub fn from_json(s: &str) -> serde_json::Result<LayoutIr> {
        serde_json::from_str(s)
    }
}
