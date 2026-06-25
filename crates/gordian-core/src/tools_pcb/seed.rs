//! Seed-board input types used before the live KiCAD IPC board exists.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use pcb_model::{Point2, Rect};
use pcb_place::placement::{LockedAt, PlacementHints};

use super::create::{parse_group_hint, parse_keepout};

/// Validated input for synthesizing the initial KiCAD board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BoardSeed {
    /// Board outline bounds in millimetres, y-down.
    pub bounds: Rect,
    #[serde(default)]
    pub rules: BoardSeedRules,
    /// Footprints and per-pad net assignments for the first board file.
    pub parts: Vec<BoardSeedPart>,
    /// Test-harness routing keepouts. Active board tools read keepouts from IPC.
    #[serde(default)]
    pub keepouts: Vec<Keepout>,
    /// Test-harness placement hints.
    #[serde(default)]
    pub hints: PlacementHints,
    /// Optional closed Edge.Cuts polygon in millimetres.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<Vec<Point2>>,
}

/// Board-level design rules for the initial board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BoardSeedRules {
    /// Copper-to-copper clearance in millimetres.
    pub clearance: f64,
    pub min_trace_width: f64,
    pub via_diameter: f64,
    pub via_drill: f64,
    #[serde(default = "default_layers")]
    pub layer_count: u32,
    /// Per-net trace-width overrides in millimetres.
    #[serde(default)]
    pub net_widths: std::collections::BTreeMap<String, f64>,
    /// Signal-layer copper pours requested at board creation.
    #[serde(default)]
    pub pours: Vec<PourSpec>,
}

/// A copper pour request on a signal layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PourSpec {
    pub net: String,
    pub layer: String,
}

fn default_layers() -> u32 {
    2
}

impl Default for BoardSeedRules {
    fn default() -> Self {
        BoardSeedRules {
            clearance: 0.2,
            min_trace_width: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            layer_count: 2,
            net_widths: std::collections::BTreeMap::new(),
            pours: Vec::new(),
        }
    }
}

/// One footprint in the initial board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BoardSeedPart {
    pub reference: String,
    pub footprint: String,
    /// Pad number to net name. Missing pads are left unconnected.
    #[serde(default)]
    pub pad_nets: BTreeMap<String, String>,
    /// Optional initial fixed placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<LockedAt>,
}

/// A rectangular routing keepout used by deterministic harness specs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Keepout {
    pub rect: pcb_place::placement::Rect,
    pub layers: Vec<pcb_model::LayerRef>,
}

/// Test-harness support for keepouts and placement groups in circuit specs.
pub fn apply_seed_extras(seed: &mut BoardSeed, spec: &Value) {
    if let Some(kos) = spec.get("keepouts").and_then(Value::as_array) {
        let (bounds, layers) = (seed.bounds.clone(), seed.rules.layer_count);
        seed.keepouts = kos
            .iter()
            .enumerate()
            .filter_map(|(i, k)| parse_keepout(k, &bounds, layers, i).ok())
            .collect();
    }
    if let Some(groups) = spec
        .get("hints")
        .and_then(|h| h.get("groups"))
        .and_then(Value::as_array)
    {
        let known: Vec<&str> = seed.parts.iter().map(|p| p.reference.as_str()).collect();
        seed.hints.groups = groups
            .iter()
            .filter_map(|g| parse_group_hint(g, &known).ok())
            .collect();
    }
}
