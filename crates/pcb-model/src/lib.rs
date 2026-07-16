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

pub mod place;
pub mod route;
pub use geom::UnionFind;
pub use geom::{Point2, Polygon, Rect, Segment};
pub use route::{
    Capabilities, RouteMetrics, RouteQuality, RouteResult, Router, failed_pad_weight, select,
};

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
    pub bounds: Rect,
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
    pub outline: Option<Polygon>,
    /// Power nets carried by solid inner PLANES (net → plane layer index).
    /// The router fans each pad out with a via instead of routing the net as
    /// trace trees, and the connectivity oracle joins same-net vias through
    /// the plane. Empty on boards without planes.
    #[serde(default)]
    pub plane_nets: std::collections::BTreeMap<String, u32>,
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

impl RouteProblem {
    /// Trace width to emit for `net`: its per-net override, else the board minimum.
    pub fn net_width(&self, net: &str) -> f64 {
        self.net_widths
            .get(net)
            .copied()
            .unwrap_or(self.min_trace_width)
    }
    /// The widest trace any net may use — clearance/inflation are sized to this so a fat
    /// power trace never violates spacing. Defaults to `min_trace_width`.
    pub fn max_route_width(&self) -> f64 {
        self.net_widths
            .values()
            .copied()
            .fold(self.min_trace_width, f64::max)
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

impl Connection {
    /// Half-perimeter of this net's terminal bounding box.
    pub fn half_perimeter(&self) -> f64 {
        let pts: Vec<Point2> = self
            .points_to_connect
            .iter()
            .map(RoutePoint::point)
            .collect();
        Rect::bounding(&pts).map_or(0.0, |r| r.half_perimeter())
    }
}

/// A point on a specific layer that must be reached by a route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutePoint {
    pub x: f64,
    pub y: f64,
    pub layer: LayerRef,
}

impl RoutePoint {
    pub fn point(&self) -> Point2 {
        Point2::new(self.x, self.y)
    }
}

// ── FailedNet ────────────────────────────────────────────────────────────────

/// A net a router could not fully connect, with a human-readable cause.
///
/// The single failure-provenance type every [`Router`](crate::Router) reports in
/// [`RouteResult::failed`](crate::RouteResult) — shared across the grid and
/// negotiated-mesh engines so failures have one shape. Serializable: a router's
/// congestion report carries these as data.
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
/// of copper layers.
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

/// The inner copper layers that carry solid GND/VCC planes, centred in the
/// stack: 4-layer → {1,2}, 6-layer → {2,3}, odd or <4 → none. The routing
/// stack masks these against signal traces; `grid-astar` re-exports this.
pub fn plane_layers(layer_count: u32) -> Vec<u32> {
    if layer_count >= 4 && layer_count & 1 == 0 {
        vec![layer_count / 2 - 1, layer_count / 2]
    } else {
        Vec::new()
    }
}

/// Default plane-net assignment for a plane-carrying stackup: the most-padded
/// ground-named net takes the first plane, the most-padded supply-named net
/// the second. `nets` are (name, pad count) pairs. Empty when the stack has
/// no planes or no recognizable power nets.
pub fn default_plane_nets(
    layer_count: u32,
    nets: impl Iterator<Item = (String, usize)>,
) -> std::collections::BTreeMap<String, u32> {
    default_plane_nets_excluding(layer_count, nets, std::iter::empty())
}

/// Default plane-net assignment after reserving layers explicitly configured by
/// the caller. An authored full-board pour owns its physical layer, so an
/// automatic GND/supply default must not emit a duplicate or competing zone on
/// that layer. With no reserved layers this is identical to [`default_plane_nets`].
pub fn default_plane_nets_excluding(
    layer_count: u32,
    nets: impl Iterator<Item = (String, usize)>,
    reserved_layers: impl IntoIterator<Item = u32>,
) -> std::collections::BTreeMap<String, u32> {
    let planes = plane_layers(layer_count);
    let mut assigned = std::collections::BTreeMap::new();
    let [gnd_layer, pwr_layer] = planes.as_slice() else {
        return assigned;
    };
    let reserved: std::collections::BTreeSet<u32> = reserved_layers.into_iter().collect();
    let is_ground = |n: &str| {
        let u = n.to_ascii_uppercase();
        u == "GND" || u.ends_with("GND") || u == "VSS" || u == "AGND" || u == "PGND"
    };
    let is_supply = |n: &str| {
        let u = n.to_ascii_uppercase();
        u.starts_with("VCC")
            || u.starts_with("VDD")
            || u.starts_with("VBUS")
            || u.starts_with("+")
            || (u.starts_with('V') && u[1..].chars().all(|c| c.is_ascii_digit() || c == 'V'))
    };
    type RankedNet = Option<(String, usize)>;
    let (mut gnd, mut pwr): (RankedNet, RankedNet) = (None, None);
    for (name, pads) in nets {
        if is_ground(&name) {
            if gnd.as_ref().is_none_or(|(_, c)| pads > *c) {
                gnd = Some((name, pads));
            }
        } else if is_supply(&name) && pwr.as_ref().is_none_or(|(_, c)| pads > *c) {
            pwr = Some((name, pads));
        }
    }
    if !reserved.contains(gnd_layer)
        && let Some((name, _)) = gnd
    {
        assigned.insert(name, *gnd_layer);
    }
    if !reserved.contains(pwr_layer)
        && let Some((name, _)) = pwr
    {
        assigned.insert(name, *pwr_layer);
    }
    assigned
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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

    #[test]
    fn explicit_plane_layers_suppress_only_competing_defaults() {
        let nets = || [("GND".to_owned(), 10), ("V3V3".to_owned(), 8)].into_iter();

        assert_eq!(
            default_plane_nets_excluding(4, nets(), []),
            BTreeMap::from([("GND".to_owned(), 1), ("V3V3".to_owned(), 2)])
        );
        assert_eq!(
            default_plane_nets_excluding(4, nets(), [2]),
            BTreeMap::from([("GND".to_owned(), 1)])
        );
        assert!(default_plane_nets_excluding(4, nets(), [1, 2]).is_empty());
    }

    #[test]
    fn connection_half_perimeter_uses_terminal_bbox() {
        let layer = LayerRef::top();
        let empty = Connection {
            name: "EMPTY".into(),
            points_to_connect: Vec::new(),
        };
        assert_eq!(empty.half_perimeter(), 0.0);

        let conn = Connection {
            name: "N".into(),
            points_to_connect: vec![
                RoutePoint {
                    x: 1.0,
                    y: 4.0,
                    layer: layer.clone(),
                },
                RoutePoint {
                    x: 5.0,
                    y: -2.0,
                    layer,
                },
            ],
        };
        assert_eq!(conn.half_perimeter(), 10.0);
    }
}
