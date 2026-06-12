//! Data model for PCB routing problems and solutions.
//!
//! `RouteProblem` is wire-compatible with tscircuit's `SimpleRouteJson`
//! (camelCase, same field names/shapes) so the archived benchmark dataset
//! parses without transformation.  Extension fields (`clearance`,
//! `via_diameter`, `via_drill`) are optional with sane defaults so upstream
//! fixtures that omit them still parse.
//!
//! `RouteSolution` is our own format; unknown fields are rejected so any
//! schema drift is caught immediately.

use serde::{Deserialize, Serialize};

// ── defaults for extension fields ────────────────────────────────────────────

fn default_clearance() -> f64 {
    0.2
}
fn default_via_diameter() -> f64 {
    0.6
}
fn default_via_drill() -> f64 {
    0.3
}

// ── LayerRef ─────────────────────────────────────────────────────────────────

/// A PCB layer name ("top", "bottom", "inner1", …).
///
/// Kept as a string so upstream SimpleRouteJson fixtures ("top"/"bottom") pass
/// through without mapping.  Call [`LayerRef::index`] at solve time to convert
/// to a zero-based numeric index.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LayerRef(pub String);

impl LayerRef {
    /// The top copper layer ("F.Cu" in KiCAD terms).
    pub fn top() -> Self {
        Self("top".to_owned())
    }

    /// The bottom copper layer ("B.Cu" in KiCAD terms).
    pub fn bottom() -> Self {
        Self("bottom".to_owned())
    }

    /// Map to a zero-based layer index.
    ///
    /// - `"top"` → `0`
    /// - `"bottom"` → `layer_count - 1`
    /// - `"inner1"` → `1`, `"inner2"` → `2`, … (up to `layer_count - 2`)
    ///
    /// Returns `None` for unknown names or indices that would be out of range
    /// (e.g. "inner3" on a 2-layer board).
    pub fn index(&self, layer_count: u32) -> Option<u32> {
        match self.0.as_str() {
            "top" => Some(0),
            "bottom" => {
                if layer_count == 0 {
                    None
                } else {
                    Some(layer_count - 1)
                }
            }
            s if s.starts_with("inner") => {
                let n: u32 = s["inner".len()..].parse().ok()?;
                // inner1 → index 1, inner2 → index 2, …
                // Valid only if the index is strictly between top (0) and bottom (layer_count-1).
                if layer_count >= 2 && n >= 1 && n <= layer_count - 2 {
                    Some(n)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

// ── Point2 ───────────────────────────────────────────────────────────────────

/// A 2-D point in millimetres, y-down (KiCAD PCB convention).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Point2 {
    pub x: f64,
    pub y: f64,
}

// ── RouteProblem ─────────────────────────────────────────────────────────────

/// A PCB routing problem, wire-compatible with tscircuit `SimpleRouteJson`.
///
/// Unknown JSON fields are silently ignored (upstream files carry keys we do
/// not model).  Extension fields default to sensible values when absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteProblem {
    pub layer_count: u32,
    pub min_trace_width: f64,
    pub obstacles: Vec<Obstacle>,
    pub connections: Vec<Connection>,
    pub bounds: Bounds,
    // ---- extensions (absent from upstream SimpleRouteJson fixtures) ----------
    #[serde(default = "default_clearance")]
    pub clearance: f64,
    #[serde(default = "default_via_diameter")]
    pub via_diameter: f64,
    #[serde(default = "default_via_drill")]
    pub via_drill: f64,
}

/// A rectangular (or oval, treated as rect in v1) copper obstacle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Obstacle {
    /// Shape kind: `"rect"` or `"oval"` (oval treated as bounding rect in v1).
    #[serde(rename = "type")]
    pub kind: String,
    pub layers: Vec<LayerRef>,
    pub center: Point2,
    pub width: f64,
    pub height: f64,
    /// Connection names whose copper this obstacle belongs to.
    /// A router never treats an obstacle as blocking its own net.
    pub connected_to: Vec<String>,
}

/// A net that must be connected: a name and the set of points to join.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub name: String,
    pub points_to_connect: Vec<RoutePoint>,
}

/// A point on a specific layer that must be reached by a route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutePoint {
    pub x: f64,
    pub y: f64,
    pub layer: LayerRef,
}

/// Board outline bounding box (mm).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bounds {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

// ── RouteSolution ─────────────────────────────────────────────────────────────

/// The result of routing a [`RouteProblem`]: copper traces and vias.
///
/// Unknown JSON fields are rejected — any schema drift is caught immediately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteSolution {
    pub traces: Vec<Trace>,
    pub vias: Vec<Via>,
}

/// A single-layer copper trace segment for one connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Trace {
    pub connection: String,
    pub layer: LayerRef,
    pub width: f64,
    /// Ordered polyline points (mm, y-down).
    pub path: Vec<Point2>,
}

/// A via joining all copper layers at a board position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Via {
    pub connection: String,
    pub at: Point2,
    pub diameter: f64,
    pub drill: f64,
}
