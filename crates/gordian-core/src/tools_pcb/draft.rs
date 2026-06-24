//! The persisted board draft (`.gordian/board.json`) and its serde model — the
//! PCB analog of the schematic `draft.circuit.yaml`. Tools mutate it; place/route
//! read it. The serde shape reuses `pcb-place` types directly so a draft
//! round-trips straight into a `PlaceProblem` without a translation layer.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use pcb_place::placement::{LockedAt, PlacementHints, Placement};
use pcb_model::{Bounds, Point2};

use crate::tools::PcbToolCtx;

use super::create::{parse_group_hint, parse_keepout};

/// The persisted board draft (`.gordian/board.json`) — the PCB analog of the
/// schematic `draft.circuit.yaml`. Tools mutate it; place/route read it.
///
/// Unknown JSON fields are rejected (`deny_unknown_fields`) so a schema drift
/// fails loudly, matching the engine's `PlaceProblem`/solution types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BoardDraft {
    /// Board outline (mm, y-down) — the placement/routing extent.
    pub bounds: Bounds,
    /// Board-level design rules (clearance, trace width, via geometry).
    #[serde(default)]
    pub rules: DraftRules,
    /// The parts on the board (footprint + per-pad nets + optional lock).
    pub parts: Vec<DraftPart>,
    /// Rectangular keepouts (routing obstacles; v1 honored by route_board).
    #[serde(default)]
    pub keepouts: Vec<Keepout>,
    /// LLM-authored placement hints (reused verbatim from `pcb-place`).
    #[serde(default)]
    pub hints: PlacementHints,
    /// The last placement produced by `place_board`, if any (set in Task 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_placement: Option<Vec<Placement>>,
    /// Whether the last placement was geometrically ILLEGAL (courtyard overlap or
    /// out-of-bounds — the board is too tight for the parts). Export refuses an
    /// illegal placement so the engine never ships a board that fails DRC.
    #[serde(default)]
    pub last_place_illegal: bool,
    /// Optional custom board OUTLINE (closed polygon, mm) — circle, square, star, any
    /// shape. When set it becomes the Edge.Cuts at export (so the render shows the real
    /// shape and the agent can iterate); `bounds` stays the polygon's bounding box for
    /// placement/routing. None = the default rectangular outline from `bounds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<Vec<Point2>>,
}

/// Board-level design rules. Defaults are the engine's own
/// (`PlaceProblem`/`RouteProblem` defaults): 0.2 mm clearance & trace width,
/// 0.6/0.3 mm via diameter/drill — so an omitted `rules` matches what the
/// router and oracle already expect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftRules {
    /// Copper-to-copper clearance (mm); also floors the courtyard margin.
    pub clearance: f64,
    /// Minimum trace width (mm).
    pub min_trace_width: f64,
    /// Via copper diameter (mm).
    pub via_diameter: f64,
    /// Via drill diameter (mm).
    pub via_drill: f64,
    /// Copper layer count (2 or 4). 4 lets dense / fine-pitch parts (BGAs) fan
    /// out their inner pins onto inner layers; 2 is the default for simple boards.
    #[serde(default = "default_layers")]
    pub layer_count: u32,
    /// Per-net trace-width overrides (net name → mm) — fat copper for power/high-current
    /// nets, thin for signals. A net not listed uses `min_trace_width`.
    #[serde(default)]
    pub net_widths: std::collections::BTreeMap<String, f64>,
    /// Copper POURS on signal layers: a flood of a net (usually GND) on "top"/"bottom",
    /// carved around foreign copper. The HF return-path / shielding case (distinct from
    /// the inner 4-layer power planes). Empty = no signal-layer pours.
    #[serde(default)]
    pub pours: Vec<PourSpec>,
}

/// A copper pour request: flood `net` on signal layer `layer` ("top" or "bottom").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PourSpec {
    pub net: String,
    pub layer: String,
}

fn default_layers() -> u32 {
    2
}

impl Default for DraftRules {
    fn default() -> Self {
        DraftRules {
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

/// One part on the board: a reference, the footprint `Lib:Name` lib_id, the
/// per-pad net assignment (pad number → net name), and an optional locked
/// position (set via a part `lock` in the Board-DSL).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftPart {
    /// Schematic reference designator ("R1", "U2", "J1"). Unique per board.
    pub reference: String,
    /// Fully-qualified footprint id ("Resistor_SMD:R_0603_1608Metric").
    pub footprint: String,
    /// Pad number → net name. A pad absent from the map is left unconnected.
    #[serde(default)]
    pub pad_nets: BTreeMap<String, String>,
    /// If present, the part is pinned here and the engine never moves it
    /// (reuses `pcb-place`'s [`LockedAt`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<LockedAt>,
}

/// A rectangular keepout on a set of copper layers. v1 affects ROUTING only —
/// keepouts become BLOCKED obstacles in the draft→RouteProblem path (Task 2),
/// not placement no-go regions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Keepout {
    /// The keepout rectangle (mm), reusing `pcb-place`'s [`Rect`](pcb_place::placement::Rect).
    pub rect: pcb_place::placement::Rect,
    /// Copper layers the keepout blocks ("top", "bottom", …).
    pub layers: Vec<pcb_model::LayerRef>,
}

impl BoardDraft {
    /// Load the persisted board draft, if one exists and parses.
    pub fn load(ctx: &PcbToolCtx) -> Option<BoardDraft> {
        let raw = ctx.workspace().read_board()?;
        serde_json::from_str(&raw).ok()
    }

    /// Persist this draft to `.gordian/board.json` (pretty-printed for the
    /// human reader, mirroring how the schematic draft stays inspectable).
    pub fn save(&self, ctx: &PcbToolCtx) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        ctx.workspace().write_board(&json)?;
        Ok(())
    }
}

/// TEST-HARNESS support (NOT an agent tool): set a draft's keepouts + placement-hint
/// groups from a circuit-spec JSON (`{keepouts: [{rect, layers}], hints: {groups: […]}}`),
/// reusing the same parsers the engine uses. The agent authors keepouts/groups in the
/// Board-DSL; this lets the deterministic harnesses seed them on a built draft.
pub fn apply_spec_extras(draft: &mut BoardDraft, spec: &Value) {
    if let Some(kos) = spec.get("keepouts").and_then(Value::as_array) {
        let (bounds, layers) = (draft.bounds.clone(), draft.rules.layer_count);
        draft.keepouts = kos
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
        let known: Vec<&str> = draft.parts.iter().map(|p| p.reference.as_str()).collect();
        draft.hints.groups = groups
            .iter()
            .filter_map(|g| parse_group_hint(g, &known).ok())
            .collect();
    }
}
