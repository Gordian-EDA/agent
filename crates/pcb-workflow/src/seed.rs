//! Board-construction input types used by `sync_board`.

use serde::{Deserialize, Serialize};

/// Board-level design rules for the initial `.kicad_pcb`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Pad attachment policy. Thermal relief is the production-safe default;
    /// solid is opt-in for requests that explicitly require it.
    #[serde(default, rename = "connect")]
    pub pad_connection: PourPadConnection,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PourPadConnection {
    #[default]
    Thermal,
    Solid,
}

fn default_layers() -> u32 {
    2
}

impl Default for BoardSeedRules {
    fn default() -> Self {
        BoardSeedRules {
            clearance: 0.15,
            min_trace_width: 0.15,
            via_diameter: 0.6,
            via_drill: 0.3,
            layer_count: 2,
            net_widths: std::collections::BTreeMap::new(),
            pours: Vec::new(),
        }
    }
}
