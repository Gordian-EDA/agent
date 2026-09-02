use serde::{Deserialize, Serialize};

use crate::{LayerRef, Point2, Polygon, Rect};

/// A footprint-local PCB-edge datum.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeDatum {
    pub start: Point2,
    pub end: Point2,
}

impl EdgeDatum {
    pub fn rotated(self, rotation: f64) -> Self {
        Self {
            start: self.start.rotate(rotation),
            end: self.end.rotate(rotation),
        }
    }

    pub fn midpoint(self) -> Point2 {
        Point2::new(
            (self.start.x + self.end.x) / 2.0,
            (self.start.y + self.end.y) / 2.0,
        )
    }

    pub fn is_horizontal(self) -> bool {
        (self.end.x - self.start.x).abs() >= (self.end.y - self.start.y).abs()
    }
}

/// One footprint pad in component-local coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PartPad {
    pub number: String,
    pub offset: Point2,
    pub width: f64,
    pub height: f64,
    pub layers: Vec<LayerRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
}

/// A fixed component placement supplied to the placement phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockedAt {
    pub at: Point2,
    #[serde(default)]
    pub rotation: f64,
}

/// A physical component to place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Part {
    pub reference: String,
    pub courtyard_w: f64,
    pub courtyard_h: f64,
    pub pads: Vec<PartPad>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_datum: Option<EdgeDatum>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<LockedAt>,
}

/// One component's position returned by the placement phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Placement {
    pub reference: String,
    pub at: Point2,
    pub rotation: f64,
}

/// The complete input to one placement invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlacementView {
    /// Board bounds in millimetres, with the y axis pointing down.
    pub bounds: Rect,
    /// Copper-to-copper clearance in millimetres.
    #[serde(default = "default_clearance")]
    pub clearance: f64,
    /// Number of copper layers carried into routing.
    #[serde(default = "default_layer_count")]
    pub layer_count: u32,
    /// Minimum trace width in millimetres carried into routing.
    #[serde(default = "default_min_trace_width")]
    pub min_trace_width: f64,
    /// Parts to place.
    pub parts: Vec<Part>,
    /// Signal-layer keepouts that must remain free of components.
    #[serde(default)]
    pub keepouts: Vec<Rect>,
    /// Custom board outline, when the board is not rectangular.
    #[serde(default)]
    pub outline: Option<Polygon>,
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

/// Optional placement guidance supplied independently of the board view.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlacementHints {
    /// Grouping, region, and edge hints.
    #[serde(default)]
    pub groups: Vec<GroupHint>,
    /// References pulled toward their nearest board edge.
    #[serde(default)]
    pub edge_seek: Vec<String>,
    /// References pulled toward their nearest board corner.
    #[serde(default)]
    pub corner_seek: Vec<String>,
}

/// A group of parts that should cohere or occupy a prescribed region.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupHint {
    /// Human-readable label used for diagnostics.
    pub name: String,
    /// References of the parts in this group.
    pub members: Vec<String>,
    /// Region the members should occupy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Rect>,
    /// Board edge the group should approach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<Edge>,
    /// Whether to tile members row-major inside the region.
    #[serde(default)]
    pub grid: bool,
    /// Quadrant rotation applied to locked grid members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<f64>,
    /// Reference around which the members should be placed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surround: Option<String>,
}

/// A board edge used by placement hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Edge {
    N,
    S,
    E,
    W,
}

/// The output of one placement invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceResult {
    /// One placement per input part, in input order.
    pub placements: Vec<Placement>,
    /// Whether exact geometry verifies the placement as legal.
    pub legal: bool,
    /// Placement quality and legalization diagnostics.
    pub report: PlaceReport,
}

/// Placement quality and legalization diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceReport {
    /// Parts moved by legalization.
    pub overlaps_resolved: usize,
    /// Parts clamped back into the board bounds.
    pub out_of_bounds_clamps: usize,
    /// Half-perimeter wirelength in millimetres.
    pub hpwl: f64,
    /// Full objective cost of the final placement.
    pub layout_cost: f64,
}
