//! Placement data types and the derived-net model.
//!
//! Pads carry net names; logical nets are derived ([`derive_nets`]) — the single
//! canonical net source, with no separate authoritative list to keep in sync.

use crate::problem::{Bounds, LayerRef, Point2};
/// Axis-aligned region/keep-out rectangle (mm) — the shared [`pcb_model::Rect`].
pub use crate::problem::Rect;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── problem ──────────────────────────────────────────────────────────────────

/// A placement problem: board bounds, design clearance, and the parts to place.
///
/// Logical nets are **not** a field — they are derived from per-pad net names
/// ([`derive_nets`]); pads are the single canonical net source. Unknown JSON
/// fields are rejected (schema drift fails loudly), like `RouteProblem`'s
/// solution types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceProblem {
    /// Board outline (mm, y-down).
    pub bounds: Bounds,
    /// Copper-to-copper clearance (mm); also floors the courtyard margin.
    #[serde(default = "default_clearance")]
    pub clearance: f64,
    /// Number of copper layers (carried into the emitted [`RouteProblem`]).
    #[serde(default = "default_layer_count")]
    pub layer_count: u32,
    /// Minimum trace width (mm), carried into the emitted [`RouteProblem`].
    #[serde(default = "default_min_trace_width")]
    pub min_trace_width: f64,
    /// The parts to place.
    pub parts: Vec<Part>,
    /// Rectangular keep-out regions on the SIGNAL layers (top/bottom) the placer
    /// must keep parts OUT of — a part dropped inside one would have its pads
    /// trapped (no track can leave without crossing the keep-out). Inner-only
    /// (plane) keep-outs are not included here. Empty for most boards.
    #[serde(default)]
    pub keepouts: Vec<Rect>,
    /// Optional custom board OUTLINE (closed polygon, mm). When set, a part is illegal if
    /// its courtyard falls outside the polygon — so concave shapes (a star) keep parts
    /// inside the TRUE outline, not just its bounding box. Carried into the [`RouteProblem`].
    #[serde(default)]
    pub outline: Option<Vec<Point2>>,
}

fn default_clearance() -> f64 {
    0.2
}
fn default_layer_count() -> u32 {
    2
}
fn default_min_trace_width() -> f64 {
    0.2
}

/// A footprint-shaped part: a reference, a courtyard rectangle (centered on the
/// part origin), its pads, and an optional locked position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Part {
    /// Schematic reference designator ("R1", "U2", "J1"). Must be unique;
    /// determinism sorts parts by this.
    pub reference: String,
    /// Courtyard width (x extent, mm), centered on the part origin at rotation 0.
    pub courtyard_w: f64,
    /// Courtyard height (y extent, mm), centered on the part origin at rotation 0.
    pub courtyard_h: f64,
    /// The part's pads (offsets are relative to the part origin at rotation 0).
    pub pads: Vec<PartPad>,
    /// If present, the part is pinned at this position/rotation and never moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<LockedAt>,
}

/// One pad: a number, an offset from the part origin (rotation 0), a size, the
/// copper layers it sits on, and the net name it belongs to (the canonical net
/// source).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PartPad {
    /// Pad number/name ("1", "2", "A1").
    pub number: String,
    /// Offset from the part origin at rotation 0 (mm).
    pub offset: Point2,
    /// Pad width (x, mm at rotation 0).
    pub width: f64,
    /// Pad height (y, mm at rotation 0).
    pub height: f64,
    /// Copper layers the pad sits on ("top", "bottom", … — thru-hole lists all).
    pub layers: Vec<LayerRef>,
    /// The net this pad belongs to, if any. `None` = unconnected pad.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
}

/// A locked (pinned) placement: an absolute position and a rotation in degrees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockedAt {
    /// Absolute part-origin position (mm).
    pub at: Point2,
    /// Rotation in degrees (0/90/180/270; other values are snapped).
    #[serde(default)]
    pub rotation: i32,
}

// ── hints ──────────────────────────────────────────────────────────────────────

/// LLM-authored placement hints. **Empty hints are valid** and must produce a
/// legal placement (hints improve, never gate). Data only — slice 5's LLM
/// integration is a serde/prompt problem, not an engine change.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlacementHints {
    /// Grouping/region/edge hints.
    #[serde(default)]
    pub groups: Vec<GroupHint>,
    /// References that should be pulled to their NEAREST board edge (connectors,
    /// headers, mounting holes — parts a cable or the enclosure reaches from
    /// outside). Unlike a group `edge` hint, the engine picks each part's nearest
    /// edge automatically, so the caller need not know the final layout. A
    /// professional board puts these at the perimeter, not stranded in the
    /// interior with copper wrapping around them.
    #[serde(default)]
    pub edge_seek: Vec<String>,
    /// References pulled to their NEAREST board CORNER (mounting holes — mechanical
    /// fixings belong at the corners, where screws clear the components). Stronger
    /// and more specific than [`Self::edge_seek`] (a corner, not anywhere along an
    /// edge), so a board's 2–4 mounting holes settle one per corner instead of
    /// stranding in the interior or bunching mid-edge.
    #[serde(default)]
    pub corner_seek: Vec<String>,
}

/// A group of parts that should cohere, optionally pulled into a region and/or
/// toward a board edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupHint {
    /// Human label (for provenance/debug; not load-bearing).
    pub name: String,
    /// References of the parts in this group.
    pub members: Vec<String>,
    /// Optional rectangle the members should land inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Rect>,
    /// Optional board edge the group should hug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<Edge>,
    /// Tile the members in a regular GRID filling [`Self::region`] (row-major, in
    /// member order), locking each at its cell. For repetitive arrays the agent
    /// wants laid out tidily (LED matrices, resistor networks) rather than the
    /// general annealer's scatter. Requires `region`; ignored without it.
    #[serde(default)]
    pub grid: bool,
    /// Ring the members tightly around the perimeter of this target part (by
    /// reference) — the decoupling-cap pattern: caps hug their IC instead of
    /// scattering. The target must be LOCKED (the agent fixes the IC first) so its
    /// position is known when the ring is laid out. Ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surround: Option<String>,
}

/// A board edge for edge-affinity hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Edge {
    N,
    S,
    E,
    W,
}

// ── result ───────────────────────────────────────────────────────────────────

/// The result of [`crate::placement::place`]: per-part placements, a legality
/// verdict (verified by exact geometry), and a quality/diagnostic report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceResult {
    /// Placed parts (one per input part, in input order).
    pub placements: Vec<Placement>,
    /// True iff no courtyard overlap (with margin) and all parts in bounds —
    /// verified by exact geometry at the end, not trusted from the algorithm.
    pub legal: bool,
    /// Diagnostics + the HPWL quality number.
    pub report: PlaceReport,
}

/// One part's final placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Placement {
    /// The part this places.
    pub reference: String,
    /// Final part-origin position (mm).
    pub at: Point2,
    /// Final rotation (degrees; 0/90/180/270).
    pub rotation: i32,
}

/// Placement diagnostics: how much legalization happened and the HPWL metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceReport {
    /// How many parts the spiral legalizer had to move off their snapped cell.
    pub overlaps_resolved: usize,
    /// How many parts were clamped because the force layout pushed them out of
    /// bounds.
    pub out_of_bounds_clamps: usize,
    /// Half-perimeter wirelength over net bounding boxes (mm) — the cheap
    /// placement-quality number (lower is tighter).
    pub hpwl: f64,
    /// The full `place_cost` of the final placement (overlap + wirelength +
    /// compaction + decoupling cohesion + silk gap). `place_best` selects the
    /// variant with the lowest layout_cost among those that route as cleanly, so
    /// the annealer's layout-quality gains are actually chosen.
    pub layout_cost: f64,
}

// ── derived nets ─────────────────────────────────────────────────────────────

/// A pin site: which part, which pad.
#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    /// Index into `problem.parts`.
    pub part: usize,
    /// Index into that part's `pads`.
    pub pad: usize,
}

/// A derived logical net: a name and the pin sites that share it.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalNet {
    pub name: String,
    pub pins: Vec<Pin>,
}

/// Derive logical nets from per-pad net names — the canonical source. Pins are
/// grouped by net name in deterministic (name, part, pad) order. Single-pin
/// nets are kept (callers filter: a 1-pin net has nothing to connect).
pub fn derive_nets(problem: &PlaceProblem) -> Vec<LogicalNet> {
    let mut by_name: BTreeMap<String, Vec<Pin>> = BTreeMap::new();
    for (pi, part) in problem.parts.iter().enumerate() {
        for (di, pad) in part.pads.iter().enumerate() {
            if let Some(net) = &pad.net {
                by_name
                    .entry(net.clone())
                    .or_default()
                    .push(Pin { part: pi, pad: di });
            }
        }
    }
    by_name
        .into_iter()
        .map(|(name, pins)| LogicalNet { name, pins })
        .collect()
}
