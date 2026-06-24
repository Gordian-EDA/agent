//! The engine-PRIVATE geometry scaffold: grid snapping, bounds clamping, and edge
//! affinity — the helpers only the search drivers (force/legalize/anneal) need.
//!
//! The PURE placement geometry the SDK shares with a third-party placer (quadrant
//! rotation, centered-rect overlap, bounds fit, courtyard margin) now lives in the
//! kernel ([`pcb_model::place`]) and is re-exported here so every internal
//! `super::geometry::…` path resolves unchanged.

use super::model::Edge;
use crate::problem::{Bounds, Point2};

// Shared (kernel) geometry — re-exported VERBATIM.
pub(crate) use crate::problem::place::{
    courtyard_margin, courtyard_overlap, fits_in_bounds, pad_world, part_keepout_overlap,
    rect_overlap, rotate_offset, rotated_copper_bbox, rotated_courtyard_half, snap_rotation,
};

// ── engine-private design constants ──────────────────────────────────────────

/// Legalizer snap grid (mm). Placed positions land on multiples of this.
pub(crate) const PLACE_GRID: f64 = 0.5;

/// How deep the edge "band" extends from the board edge (mm) for edge affinity:
/// a part whose courtyard half-extent fits within this of the edge counts as
/// "on the edge". Also the target inset the edge pull aims for.
pub(crate) const EDGE_BAND: f64 = 2.0;

/// Spiral search cap: how many grid rings the legalizer probes before giving up
/// on a part (→ `legal: false`). Generous; a real board seats in a few rings.
pub(crate) const SPIRAL_MAX_RING: i64 = 400;

// ── grid / bounds (search-only) ──────────────────────────────────────────────

/// Snap a coordinate to the placement grid.
pub(crate) fn snap(v: f64) -> f64 {
    (v / PLACE_GRID).round() * PLACE_GRID
}

/// Clamp a part origin so its courtyard fits in bounds (best effort: if the part
/// is wider than the board, it is centered on that axis).
pub(crate) fn clamp_into_bounds(p: &mut Point2, b: &Bounds, h: (f64, f64)) {
    let (lo_x, hi_x) = (b.min_x + h.0, b.max_x - h.0);
    let (lo_y, hi_y) = (b.min_y + h.1, b.max_y - h.1);
    p.x = if lo_x <= hi_x {
        p.x.clamp(lo_x, hi_x)
    } else {
        (b.min_x + b.max_x) / 2.0
    };
    p.y = if lo_y <= hi_y {
        p.y.clamp(lo_y, hi_y)
    } else {
        (b.min_y + b.max_y) / 2.0
    };
}

/// A deterministic non-zero sign: +1 for ≥ 0, -1 for < 0 (so coincident parts
/// still get a fixed separating direction).
pub(crate) fn sign_nonzero(v: f64) -> f64 {
    if v < 0.0 {
        -1.0
    } else {
        1.0
    }
}

// ── edge affinity (search-only) ──────────────────────────────────────────────

/// The board edge a part should hug given its aspect: a part taller than wide
/// prefers the nearer SIDE edge (E/W) — its long axis then runs along the edge;
/// a wider part prefers the nearer top/bottom (N/S). Square parts fall back to
/// the overall nearest edge.
pub(crate) fn aspect_edge(p: &Point2, b: &Bounds, w: f64, h: f64) -> Edge {
    if h > w {
        if p.x - b.min_x <= b.max_x - p.x { Edge::W } else { Edge::E }
    } else if w > h {
        if p.y - b.min_y <= b.max_y - p.y { Edge::N } else { Edge::S }
    } else {
        nearest_edge(p, b)
    }
}

/// The board edge nearest to `p`. Ties break in N, S, W, E order (deterministic).
pub(crate) fn nearest_edge(p: &Point2, b: &Bounds) -> Edge {
    let d_n = p.y - b.min_y;
    let d_s = b.max_y - p.y;
    let d_w = p.x - b.min_x;
    let d_e = b.max_x - p.x;
    let mut best = Edge::N;
    let mut best_d = d_n;
    for (d, e) in [(d_s, Edge::S), (d_w, Edge::W), (d_e, Edge::E)] {
        if d < best_d {
            best_d = d;
            best = e;
        }
    }
    best
}

/// The pull target for an edge hint: a point on the edge band line, keeping the
/// part's other coordinate where it is (only the edge-normal coordinate matters).
pub(crate) fn edge_target(edge: Edge, b: &Bounds, h: (f64, f64)) -> f64 {
    match edge {
        Edge::N => b.min_y + h.1 + EDGE_BAND.min((b.max_y - b.min_y) / 2.0),
        Edge::S => b.max_y - h.1 - EDGE_BAND.min((b.max_y - b.min_y) / 2.0),
        Edge::W => b.min_x + h.0 + EDGE_BAND.min((b.max_x - b.min_x) / 2.0),
        Edge::E => b.max_x - h.0 - EDGE_BAND.min((b.max_x - b.min_x) / 2.0),
    }
}

/// The edge pull delta (only the edge-normal axis is driven; the tangential axis
/// is left to nets/groups).
pub(crate) fn edge_delta(edge: Edge, p: &Point2, target: f64) -> (f64, f64) {
    match edge {
        Edge::N | Edge::S => (0.0, target - p.y),
        Edge::E | Edge::W => (target - p.x, 0.0),
    }
}
