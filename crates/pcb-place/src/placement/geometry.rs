//! Engine-private placement geometry constants and edge affinity.

use pcb_model::{Point2, Rect};
use place_model::{Edge, EdgeDatum, Part};

// Shared placement geometry.
pub(crate) use place_model::{
    clamp_center_for_envelope, courtyard_margin, pad_world, part_edge_distance,
    part_placement_bounds_envelope, placement_envelope_at, rotated_copper_bbox,
    rotated_courtyard_half,
};

// ── engine-private design constants ──────────────────────────────────────────

/// Legalizer snap grid (mm). Placed positions land on multiples of this.
pub(crate) const PLACE_GRID: f64 = 0.5;
pub(crate) const PLACEMENT_GRID: geom::Grid = geom::Grid::new(PLACE_GRID);

/// How deep the edge "band" extends from the board edge (mm) for edge affinity:
/// a part whose courtyard half-extent fits within this of the edge counts as
/// "on the edge". Also the target inset the edge pull aims for.
pub(crate) const EDGE_BAND: f64 = 2.0;

/// Spiral search cap: how many grid rings the legalizer probes before giving up
/// on a part (→ `legal: false`). Generous; a real board seats in a few rings.
pub(crate) const SPIRAL_MAX_RING: i64 = 400;

/// A deterministic non-zero sign: +1 for ≥ 0, -1 for < 0 (so coincident parts
/// still get a fixed separating direction).
pub(crate) fn sign_nonzero(v: f64) -> f64 {
    if v < 0.0 { -1.0 } else { 1.0 }
}

// ── edge affinity (search-only) ──────────────────────────────────────────────

/// The board edge a part should hug given its aspect: a part taller than wide
/// prefers the nearer SIDE edge (E/W) — its long axis then runs along the edge;
/// a wider part prefers the nearer top/bottom (N/S). Square parts fall back to
/// the overall nearest edge.
pub(crate) fn aspect_edge(p: &Point2, b: &Rect, w: f64, h: f64) -> Edge {
    if h > w {
        if p.x - b.min_x <= b.max_x - p.x {
            Edge::W
        } else {
            Edge::E
        }
    } else if w > h {
        if p.y - b.min_y <= b.max_y - p.y {
            Edge::N
        } else {
            Edge::S
        }
    } else {
        nearest_edge(p, b)
    }
}

/// The board edge nearest to `p`. Ties break in N, S, W, E order (deterministic).
pub(crate) fn nearest_edge(p: &Point2, b: &Rect) -> Edge {
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
pub(crate) fn edge_target(edge: Edge, b: &Rect, h: (f64, f64)) -> f64 {
    match edge {
        Edge::N => b.min_y + h.1 + EDGE_BAND.min((b.max_y - b.min_y) / 2.0),
        Edge::S => b.max_y - h.1 - EDGE_BAND.min((b.max_y - b.min_y) / 2.0),
        Edge::W => b.min_x + h.0 + EDGE_BAND.min((b.max_x - b.min_x) / 2.0),
        Edge::E => b.max_x - h.0 - EDGE_BAND.min((b.max_x - b.min_x) / 2.0),
    }
}

/// Origin coordinate that puts a part's explicit PCB-edge datum exactly on the
/// selected rectangular board edge. Returns `None` when the rotated datum is
/// not tangent to that edge; callers then use ordinary courtyard affinity.
pub(crate) fn datum_edge_target(part: &Part, rotation: f64, edge: Edge, b: &Rect) -> Option<f64> {
    let datum = part.edge_datum?.rotated(rotation);
    if !datum_tangent_to_edge(datum, edge) {
        return None;
    }
    let midpoint = datum.midpoint();
    match edge {
        Edge::N => Some(b.min_y - midpoint.y),
        Edge::S => Some(b.max_y - midpoint.y),
        Edge::W => Some(b.min_x - midpoint.x),
        Edge::E => Some(b.max_x - midpoint.x),
    }
}

fn datum_tangent_to_edge(datum: EdgeDatum, edge: Edge) -> bool {
    let dx = (datum.end.x - datum.start.x).abs();
    let dy = (datum.end.y - datum.start.y).abs();
    if dx <= geom::EPS && dy <= geom::EPS {
        return false;
    }
    match edge {
        Edge::N | Edge::S => dy <= geom::EPS,
        Edge::E | Edge::W => dx <= geom::EPS,
    }
}

pub(crate) fn part_edge_target(
    part: &Part,
    rotation: f64,
    edge: Edge,
    b: &Rect,
    half: (f64, f64),
) -> f64 {
    datum_edge_target(part, rotation, edge, b).unwrap_or_else(|| edge_target(edge, b, half))
}

/// Quadrant rotation that makes the datum tangent to `edge` while preserving
/// its library orientation whenever no rotation is needed.
pub(crate) fn datum_rotation_for_edge(part: &Part, edge: Edge) -> f64 {
    if part.edge_datum.is_none() {
        return 0.0;
    }
    // A tangent line alone leaves a 180° ambiguity. Resolve it mechanically:
    // choose the quadrant that puts the pad-copper centroid on the board side
    // of the datum, so a north-edge receptacle does not point out of the board.
    [0.0, 90.0, 180.0, 270.0]
        .into_iter()
        .filter_map(|rotation| {
            let rotated_datum = part.edge_datum?.rotated(rotation);
            if !datum_tangent_to_edge(rotated_datum, edge) {
                return None;
            }
            let datum = rotated_datum.midpoint();
            let copper = rotated_copper_bbox(part, rotation).center();
            let inward = match edge {
                Edge::N => copper.y - datum.y,
                Edge::S => datum.y - copper.y,
                Edge::W => copper.x - datum.x,
                Edge::E => datum.x - copper.x,
            };
            Some((inward, rotation))
        })
        .max_by(|(a_score, a_rot), (b_score, b_rot)| {
            a_score
                .total_cmp(b_score)
                .then_with(|| b_rot.total_cmp(a_rot))
        })
        .map(|(_, rotation)| rotation)
        .unwrap_or(0.0)
}

/// The edge pull delta (only the edge-normal axis is driven; the tangential axis
/// is left to nets/groups).
pub(crate) fn edge_delta(edge: Edge, p: &Point2, target: f64) -> (f64, f64) {
    match edge {
        Edge::N | Edge::S => (0.0, target - p.y),
        Edge::E | Edge::W => (target - p.x, 0.0),
    }
}
