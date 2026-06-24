//! Shared geometry scaffold: grid snapping, courtyard overlap, rotation, edge
//! affinity. Every placement stage builds on these pure helpers.

use super::model::{Edge, Part, Pin, PlaceProblem, Rect};
use crate::problem::{Bounds, Point2};

// ── design constants ──────────────────────────────────────────────────────────

/// Legalizer snap grid (mm). Placed positions land on multiples of this.
pub(crate) const PLACE_GRID: f64 = 0.5;

/// Minimum courtyard-to-courtyard gap (mm). The effective margin is
/// `max(clearance, COURTYARD_MARGIN_MIN)`.
pub(crate) const COURTYARD_MARGIN_MIN: f64 = 0.25;

/// How deep the edge "band" extends from the board edge (mm) for edge affinity:
/// a part whose courtyard half-extent fits within this of the edge counts as
/// "on the edge". Also the target inset the edge pull aims for.
pub(crate) const EDGE_BAND: f64 = 2.0;

/// Spiral search cap: how many grid rings the legalizer probes before giving up
/// on a part (→ `legal: false`). Generous; a real board seats in a few rings.
pub(crate) const SPIRAL_MAX_RING: i64 = 400;

/// KiCAD's copper-to-board-edge clearance (its default). A part's PADS must clear the board
/// outline by this — otherwise an edge-seeking connector lands a pad on the Edge.Cuts and trips
/// `copper_edge_clearance`. (The COURTYARD may still overhang — only copper is constrained.)
pub(crate) const EDGE_CLEAR_PLACE_MM: f64 = 0.5;

// ── overlap / bounds ───────────────────────────────────────────────────────────

/// Margin-inflated overlap of two parts' courtyards, per axis (mm; >0 on both
/// axes ⇒ overlapping). Each courtyard is inflated by `margin/2` per side so the
/// required *gap* between courtyards is `margin`.
pub(crate) fn courtyard_overlap(
    pos: &[Point2],
    half: &[(f64, f64)],
    margin: f64,
    i: usize,
    j: usize,
) -> (f64, f64) {
    rect_overlap(&pos[i], half[i], &pos[j], half[j], margin)
}

/// Margin-inflated axis overlaps of two centered rects.
pub(crate) fn rect_overlap(
    ci: &Point2,
    hi: (f64, f64),
    cj: &Point2,
    hj: (f64, f64),
    margin: f64,
) -> (f64, f64) {
    let m = margin / 2.0;
    let ox = (hi.0 + m + hj.0 + m) - (ci.x - cj.x).abs();
    let oy = (hi.1 + m + hj.1 + m) - (ci.y - cj.y).abs();
    (ox, oy)
}

/// Does a part's courtyard fit fully within `bounds`?
pub(crate) fn fits_in_bounds(p: &Point2, b: &Bounds, h: (f64, f64)) -> bool {
    p.x - h.0 >= b.min_x - 1e-9
        && p.x + h.0 <= b.max_x + 1e-9
        && p.y - h.1 >= b.min_y - 1e-9
        && p.y + h.1 <= b.max_y + 1e-9
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

/// Overlap `(ox, oy)` of a part's courtyard (centre `p`, half-extents `h`) with a
/// keep-out rect; both strictly positive means the part intrudes into the keep-out.
pub(crate) fn part_keepout_overlap(p: &Point2, h: (f64, f64), k: &Rect) -> (f64, f64) {
    let ox = (p.x + h.0).min(k.max_x) - (p.x - h.0).max(k.min_x);
    let oy = (p.y + h.1).min(k.max_y) - (p.y - h.1).max(k.min_y);
    (ox, oy)
}

// ── grid / rotation ─────────────────────────────────────────────────────────────

/// Snap a coordinate to the placement grid.
pub(crate) fn snap(v: f64) -> f64 {
    (v / PLACE_GRID).round() * PLACE_GRID
}

/// The effective courtyard margin: `max(clearance, COURTYARD_MARGIN_MIN)`.
pub(crate) fn courtyard_margin(clearance: f64) -> f64 {
    clearance.max(COURTYARD_MARGIN_MIN)
}

/// Snap an arbitrary rotation (degrees) to the nearest quadrant in 0/90/180/270.
pub(crate) fn snap_rotation(deg: i32) -> i32 {
    let r = deg.rem_euclid(360);
    (((r + 45) / 90) * 90) % 360
}

/// Courtyard half-extents after a quadrant rotation (90/270 swap w/h).
pub(crate) fn rotated_courtyard_half(part: &Part, rot: i32) -> (f64, f64) {
    let (w, h) = (part.courtyard_w / 2.0, part.courtyard_h / 2.0);
    match rot {
        90 | 270 => (h, w),
        _ => (w, h),
    }
}

/// Half-extents of the part's PAD (copper) bounding box after a quadrant rotation. Bounds ONLY
/// the copper — so the outline check can keep pads inside the board while a part's courtyard
/// (its non-copper margin) is still free to overhang a notch (the mounting-hole allowance).
pub(crate) fn rotated_copper_bbox(part: &Part, rot: i32) -> (f64, f64, f64, f64) {
    let (mut xmin, mut ymin, mut xmax, mut ymax) =
        (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for pad in &part.pads {
        let off = rotate_offset(&pad.offset, rot);
        let (pw, ph) = match rot.rem_euclid(360) {
            90 | 270 => (pad.height / 2.0, pad.width / 2.0),
            _ => (pad.width / 2.0, pad.height / 2.0),
        };
        // TRUE (asymmetric) bbox relative to the part origin — a connector's pads are OFF-CENTRE
        // (origin at pin 1, not the courtyard centre), so a symmetric centre±max|offset| box would
        // be ~2× too large on the empty side and FALSE-REJECT a connector that actually clears the
        // edge. Track real min/max so the outline check is exact.
        xmin = xmin.min(off.x - pw);
        xmax = xmax.max(off.x + pw);
        ymin = ymin.min(off.y - ph);
        ymax = ymax.max(off.y + ph);
    }
    if xmin > xmax {
        (0.0, 0.0, 0.0, 0.0) // no pads
    } else {
        (xmin, ymin, xmax, ymax)
    }
}

/// A pad offset rotated by a quadrant (degrees), y-down.
pub(crate) fn rotate_offset(off: &Point2, rot: i32) -> Point2 {
    // KiCAD footprint-rotation convention (y-down board coords): a pad's local
    // offset under a footprint rotated by `rot` lands at these world offsets.
    // Verified against kicad-cli: a 270° footprint maps local (x,y) → (-y, x).
    // (The 90 and 270 cases were previously swapped, which placed the engine's
    // routing targets on the WRONG physical pad for any rotated part → shorts.)
    match rot.rem_euclid(360) {
        90 => Point2 { x: off.y, y: -off.x },
        180 => Point2 { x: -off.x, y: -off.y },
        270 => Point2 { x: -off.y, y: off.x },
        _ => off.clone(),
    }
}

/// World position of a pin's pad center given current part positions.
pub(crate) fn pad_world(problem: &PlaceProblem, pos: &[Point2], pin: &Pin) -> Point2 {
    let part = &problem.parts[pin.part];
    let rot = part.locked.as_ref().map(|l| snap_rotation(l.rotation)).unwrap_or(0);
    let off = rotate_offset(&part.pads[pin.pad].offset, rot);
    Point2 {
        x: pos[pin.part].x + off.x,
        y: pos[pin.part].y + off.y,
    }
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

// ── edge affinity ───────────────────────────────────────────────────────────────

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
