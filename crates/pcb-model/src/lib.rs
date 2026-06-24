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

pub mod geom2d;
pub mod hash;
pub mod place;
pub mod route;
pub mod union_find;
pub use hash::{fnv1a, uuid_v5};
pub use route::{
    failed_pad_weight, select, Capabilities, RouteMetrics, RouteQuality, RouteResult, Router,
};
pub use union_find::UnionFind;

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

    /// Resolve a pour/zone layer string to its `(zero-based index, KiCAD layer
    /// name)` on an `lc`-layer board — the inverse pairing the synthesizer needs
    /// to emit a `(layer "InN.Cu")` from a layer request.
    ///
    /// Accepts both the engine vocabulary (`"top"`, `"bottom"`, `"innerN"`) and
    /// the KiCAD names (`"F.Cu"`, `"B.Cu"`, `"InN.Cu"`):
    /// - `"top"` / `"F.Cu"` → `(0, "F.Cu")`
    /// - `"bottom"` / `"B.Cu"` → `(lc-1, "B.Cu")`
    /// - `"innerN"` / `"InN.Cu"` → `(N, "InN.Cu")` for `0 < N < lc-1`
    ///
    /// Returns `None` for an out-of-range or unparseable layer (e.g. `"inner3"`
    /// on a 2-layer board). Deterministic; never panics.
    pub fn resolve(layer: &str, lc: u32) -> Option<(u32, String)> {
        let idx = match layer {
            "top" | "F.Cu" => 0,
            "bottom" | "B.Cu" => lc.checked_sub(1)?,
            other => other
                .strip_prefix("inner")
                .or_else(|| other.strip_prefix("In").and_then(|s| s.strip_suffix(".Cu")))
                .and_then(|n| n.parse::<u32>().ok())?,
        };
        if idx >= lc {
            return None;
        }
        let name = if idx == 0 {
            "F.Cu".to_string()
        } else if idx == lc - 1 {
            "B.Cu".to_string()
        } else {
            format!("In{idx}.Cu")
        };
        Some((idx, name))
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

impl Point2 {
    /// Squared euclidean distance to `other` (cheaper than [`Point2::dist`] when
    /// only comparing magnitudes).
    #[inline]
    pub fn dist2(&self, other: &Point2) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        dx * dx + dy * dy
    }

    /// Euclidean distance to `other` (mm).
    #[inline]
    pub fn dist(&self, other: &Point2) -> f64 {
        self.dist2(other).sqrt()
    }
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
    /// Per-net trace width overrides (net name → mm). A net not listed uses
    /// `min_trace_width`. This is how power/high-current nets get fat copper while
    /// signals stay thin — the router emits each net at its width and (conservatively)
    /// spaces every net for the widest so the board stays DRC-clean. Empty = the old
    /// uniform-width behaviour.
    #[serde(default)]
    pub net_widths: std::collections::BTreeMap<String, f64>,
    /// Optional custom board OUTLINE (closed polygon, mm). When set, copper must stay
    /// inside it (the grid blocks cells outside the polygon or within clearance of an
    /// edge) — so concave shapes (a star) route inside the TRUE outline, not just its
    /// bounding box. None = the rectangular `bounds`.
    #[serde(default)]
    pub outline: Option<Vec<Point2>>,
    /// Inner-layer escape assignment: net → the inner SIGNAL copper layer that net's
    /// ENCLOSED fine-pitch ball must drop to (via-in-pad) and route out on. The agent
    /// computes this for dense BGA fields (by ring/quadrant depth) so each escape layer
    /// drains a disjoint wedge of balls — the structured fan-out a free maze can't find.
    /// A net listed here that is enclosed on its own face gets a forced via-in-pad to the
    /// assigned layer, and its A* is restricted to {top, bottom, assigned} so the ring→
    /// layer assignment holds. Empty (the default) = the old surface-only escape.
    #[serde(default)]
    pub escape_layers: std::collections::BTreeMap<String, u32>,
}

/// Is `pt` inside the closed polygon `poly` (ray-casting, even-odd rule)? A polygon of
/// fewer than 3 points is treated as "no outline" → always inside.
pub fn point_in_polygon(pt: &Point2, poly: &[Point2]) -> bool {
    let n = poly.len();
    if n < 3 {
        return true;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (pi, pj) = (&poly[i], &poly[j]);
        if (pi.y > pt.y) != (pj.y > pt.y) {
            let x_int = pi.x + (pt.y - pi.y) / (pj.y - pi.y) * (pj.x - pi.x);
            if pt.x < x_int {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// Minimum distance from `pt` to the boundary of polygon `poly` (any edge). Used with
/// [`point_in_polygon`] to enforce copper-to-edge clearance on a custom outline.
pub fn dist_to_polygon_edge(pt: &Point2, poly: &[Point2]) -> f64 {
    let n = poly.len();
    if n < 2 {
        return f64::INFINITY;
    }
    let mut best = f64::INFINITY;
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (&poly[j], &poly[i]);
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len2 = dx * dx + dy * dy;
        let t = if len2 > 0.0 {
            (((pt.x - a.x) * dx + (pt.y - a.y) * dy) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (cx, cy) = (a.x + t * dx, a.y + t * dy);
        let d = ((pt.x - cx).powi(2) + (pt.y - cy).powi(2)).sqrt();
        best = best.min(d);
        j = i;
    }
    best
}

impl RouteProblem {
    /// Trace width to emit for `net`: its per-net override, else the board minimum.
    pub fn net_width(&self, net: &str) -> f64 {
        self.net_widths.get(net).copied().unwrap_or(self.min_trace_width)
    }
    /// The widest trace any net may use — clearance/inflation are sized to this so a fat
    /// power trace never violates spacing. Defaults to `min_trace_width`.
    pub fn max_route_width(&self) -> f64 {
        self.net_widths.values().copied().fold(self.min_trace_width, f64::max)
    }
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

// ── FailedNet ────────────────────────────────────────────────────────────────

/// A net a router could not fully connect, with a human-readable cause.
///
/// Shared by the slice-1 grid router ([`crate::router`]) and the slice-2 global
/// router ([`crate::pathing`]) so failure provenance has one type across stages.
/// Serializable: the global router's congestion report carries these as data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FailedNet {
    /// Connection name.
    pub connection: String,
    /// Human-readable cause.
    pub reason: String,
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

/// The copper-layer span of a via. `Through` (the default) pierces the full stack
/// (F.Cu → B.Cu); `Partial` is an HDI blind/buried or micro via spanning a sub-range
/// of copper layers. See `docs/specs/hdi-microvia-feasibility.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ViaSpan {
    /// Full-stack through via (every existing via is this).
    #[default]
    Through,
    /// A via spanning copper layers `[from, to]` (0-based indices into the layer stack,
    /// 0 = top). `micro` emits KiCAD's `micro` keyword (a laser microvia, used for an
    /// adjacent-layer span / via-in-pad escape) vs `blind` (a mechanically-drilled
    /// blind/buried via). KiCAD encodes the type as a BARE keyword after `via`.
    Partial { from: u32, to: u32, micro: bool },
}

/// A via joining copper layers at a board position. `span` defaults to `Through`
/// (the full stack); a `Partial` span is an HDI blind/micro via.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Via {
    pub connection: String,
    pub at: Point2,
    pub diameter: f64,
    pub drill: f64,
    /// The via's copper-layer span. Omitted in older route JSON → defaults to `Through`.
    #[serde(default)]
    pub span: ViaSpan,
}

/// An axis-aligned rectangle in board mm (y-down), `[min, max]` per axis.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Rect {
    #[inline]
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }
    #[inline]
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }
    #[inline]
    pub fn area(&self) -> f64 {
        self.width() * self.height()
    }
    #[inline]
    pub fn center(&self) -> Point2 {
        Point2 {
            x: (self.min_x + self.max_x) / 2.0,
            y: (self.min_y + self.max_y) / 2.0,
        }
    }
    /// Is `p` inside (or on the boundary of) this rect?
    #[inline]
    pub fn contains(&self, p: &Point2) -> bool {
        p.x >= self.min_x && p.x <= self.max_x && p.y >= self.min_y && p.y <= self.max_y
    }
    /// Does this rect overlap `other` with positive area?
    #[inline]
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.min_x < other.max_x
            && self.max_x > other.min_x
            && self.min_y < other.max_y
            && self.max_y > other.min_y
    }
    /// The overlap rectangle with `other`, or `None` if they do not overlap.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let min_x = self.min_x.max(other.min_x);
        let max_x = self.max_x.min(other.max_x);
        let min_y = self.min_y.max(other.min_y);
        let max_y = self.max_y.min(other.max_y);
        if min_x < max_x && min_y < max_y {
            Some(Rect { min_x, min_y, max_x, max_y })
        } else {
            None
        }
    }
    /// Does the *boundary* of `other` cross the interior of `self`? True when the
    /// rects overlap but `other` does not wholly contain `self`.
    pub fn boundary_crosses(&self, other: &Rect) -> bool {
        if !self.overlaps(other) {
            return false;
        }
        let covers = other.min_x <= self.min_x
            && other.max_x >= self.max_x
            && other.min_y <= self.min_y
            && other.max_y >= self.max_y;
        !covers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Rect` is the single shared region/keep-out type (pcb-place reuses it). External
    /// JSON (agent keepouts) carries it by camelCase NAME, so deserialization is
    /// field-order-independent — this guards the de-duplication against any future
    /// reshuffle of the struct's field declaration order.
    #[test]
    fn rect_round_trips_camel_case_order_independent() {
        let r = Rect { min_x: 1.0, min_y: 2.0, max_x: 3.0, max_y: 4.0 };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"minX":1.0,"minY":2.0,"maxX":3.0,"maxY":4.0}"#);
        assert_eq!(serde_json::from_str::<Rect>(&json).unwrap(), r);
        // Keys in any order parse the same (name-based, not positional).
        let shuffled = r#"{"maxX":3.0,"minX":1.0,"maxY":4.0,"minY":2.0}"#;
        assert_eq!(serde_json::from_str::<Rect>(shuffled).unwrap(), r);
    }

    #[test]
    fn rect_contains_includes_boundary() {
        let r = Rect { min_x: 0.0, min_y: 0.0, max_x: 10.0, max_y: 10.0 };
        assert!(r.contains(&Point2 { x: 5.0, y: 5.0 }));
        assert!(r.contains(&Point2 { x: 0.0, y: 10.0 }), "boundary is inside");
        assert!(!r.contains(&Point2 { x: 11.0, y: 5.0 }));
        assert_eq!(r.center(), Point2 { x: 5.0, y: 5.0 });
    }

    #[test]
    fn layer_ref_resolve_maps_index_and_name() {
        // Engine vocabulary and KiCAD names both resolve, on a 6-layer board.
        assert_eq!(LayerRef::resolve("top", 6), Some((0, "F.Cu".into())));
        assert_eq!(LayerRef::resolve("F.Cu", 6), Some((0, "F.Cu".into())));
        assert_eq!(LayerRef::resolve("bottom", 6), Some((5, "B.Cu".into())));
        assert_eq!(LayerRef::resolve("B.Cu", 6), Some((5, "B.Cu".into())));
        assert_eq!(LayerRef::resolve("inner1", 6), Some((1, "In1.Cu".into())));
        assert_eq!(LayerRef::resolve("In4.Cu", 6), Some((4, "In4.Cu".into())));
        // Out of range / unparseable / degenerate stackup → None.
        assert_eq!(LayerRef::resolve("inner3", 2), None);
        assert_eq!(LayerRef::resolve("nope", 4), None);
        assert_eq!(LayerRef::resolve("bottom", 0), None);
    }
}
