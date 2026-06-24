//! 2-D segment geometry kernel: point/segment/rect distances and segment
//! intersection, all on bare `[f64; 2]` coordinates (mm). Pure distance math —
//! no grid, no inflation, no problem model — so every clearance/connectivity
//! check shares one definition.

/// Geometric slop (mm) for the collinear/touching predicates. A point within
/// `EPS` of a segment's supporting line and span counts as on it.
const EPS: f64 = 1e-6;

/// Euclidean distance between two points.
pub fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    (dx * dx + dy * dy).sqrt()
}

/// Distance from point `p` to segment `ab` (a zero-length segment is a point).
pub fn point_seg_dist(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    if len2 <= f64::EPSILON {
        return dist(p, a);
    }
    let t = (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1]) / len2).clamp(0.0, 1.0);
    let proj = [a[0] + t * ab[0], a[1] + t * ab[1]];
    dist(p, proj)
}

/// Minimum distance between segments `ab` and `cd`. Zero when they intersect.
pub fn seg_seg_dist(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> f64 {
    if segments_intersect(a, b, c, d) {
        return 0.0;
    }
    // No crossing: the minimum is an endpoint-to-other-segment distance.
    point_seg_dist(a, c, d)
        .min(point_seg_dist(b, c, d))
        .min(point_seg_dist(c, a, b))
        .min(point_seg_dist(d, a, b))
}

/// Orientation determinant of `(o, a, b)`: >0 ccw, <0 cw, 0 collinear.
pub fn cross(o: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Do segments `ab` and `cd` intersect (including touching / collinear overlap)?
pub fn segments_intersect(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let d1 = cross(c, d, a);
    let d2 = cross(c, d, b);
    let d3 = cross(a, b, c);
    let d4 = cross(a, b, d);
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
    {
        return true;
    }
    // Collinear / touching cases.
    on_segment(c, d, a, d1)
        || on_segment(c, d, b, d2)
        || on_segment(a, b, c, d3)
        || on_segment(a, b, d, d4)
}

/// `p` lies on segment `ab` given the orientation determinant `cr` for `p`.
pub fn on_segment(a: [f64; 2], b: [f64; 2], p: [f64; 2], cr: f64) -> bool {
    cr.abs() <= EPS
        && p[0] >= a[0].min(b[0]) - EPS
        && p[0] <= a[0].max(b[0]) + EPS
        && p[1] >= a[1].min(b[1]) - EPS
        && p[1] <= a[1].max(b[1]) + EPS
}

/// Distance from point `p` to the axis-aligned rect `[min, max]`; 0 inside.
pub fn point_rect_dist(p: [f64; 2], min: [f64; 2], max: [f64; 2]) -> f64 {
    let dx = (min[0] - p[0]).max(0.0).max(p[0] - max[0]);
    let dy = (min[1] - p[1]).max(0.0).max(p[1] - max[1]);
    (dx * dx + dy * dy).sqrt()
}

/// Distance from segment `ab` to the axis-aligned rect `[min, max]`; 0 if the
/// segment enters or touches the rect.
pub fn seg_rect_dist(a: [f64; 2], b: [f64; 2], min: [f64; 2], max: [f64; 2]) -> f64 {
    if point_rect_dist(a, min, max) <= EPS || point_rect_dist(b, min, max) <= EPS {
        return 0.0;
    }
    // Closest approach is either an endpoint to an edge, or an intersection with
    // one of the four edges (distance 0). Test against each edge segment.
    let corners = [
        [min[0], min[1]],
        [max[0], min[1]],
        [max[0], max[1]],
        [min[0], max[1]],
    ];
    let mut best = f64::INFINITY;
    for i in 0..4 {
        let c = corners[i];
        let d = corners[(i + 1) % 4];
        best = best.min(seg_seg_dist(a, b, c, d));
    }
    best
}
