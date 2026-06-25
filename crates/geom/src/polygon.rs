//! Closed polygon predicates and edge distances.

use crate::{Point2, Segment};

/// Is `pt` inside the closed polygon `poly` by the even-odd rule?
///
/// A polygon with fewer than three points is treated as absent and contains all
/// points. This preserves the optional-outline contract used by PCB callers.
pub fn point_in_polygon(pt: Point2, poly: &[Point2]) -> bool {
    let n = poly.len();
    if n < 3 {
        return true;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (pi, pj) = (poly[i], poly[j]);
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

/// Minimum distance from `pt` to any polygon edge.
pub fn dist_to_polygon_edge(pt: Point2, poly: &[Point2]) -> f64 {
    let n = poly.len();
    if n < 2 {
        return f64::INFINITY;
    }
    let mut best = f64::INFINITY;
    let mut j = n - 1;
    for i in 0..n {
        best = best.min(Segment::new(poly[j], poly[i]).dist_to_point(pt));
        j = i;
    }
    best
}

/// Minimum distance from `seg` to any polygon edge.
pub fn segment_dist_to_polygon_edge(seg: Segment, poly: &[Point2]) -> f64 {
    let n = poly.len();
    if n < 2 {
        return f64::INFINITY;
    }
    let mut best = f64::INFINITY;
    let mut j = n - 1;
    for i in 0..n {
        best = best.min(seg.dist_to_segment(Segment::new(poly[j], poly[i])));
        j = i;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> Vec<Point2> {
        vec![
            Point2::new(0.0, 0.0),
            Point2::new(10.0, 0.0),
            Point2::new(10.0, 10.0),
            Point2::new(0.0, 10.0),
        ]
    }

    #[test]
    fn contains_points_by_even_odd_rule() {
        let poly = square();
        assert!(point_in_polygon(Point2::new(5.0, 5.0), &poly));
        assert!(!point_in_polygon(Point2::new(15.0, 5.0), &poly));
        assert!(point_in_polygon(Point2::new(5.0, 5.0), &[]));
    }

    #[test]
    fn point_distance_to_edges() {
        let poly = square();
        assert!((dist_to_polygon_edge(Point2::new(5.0, 7.0), &poly) - 3.0).abs() < 1e-9);
        assert!((dist_to_polygon_edge(Point2::new(12.0, 5.0), &poly) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn segment_distance_to_edges() {
        let poly = square();
        let near = Segment::new(Point2::new(12.0, 2.0), Point2::new(12.0, 8.0));
        assert!((segment_dist_to_polygon_edge(near, &poly) - 2.0).abs() < 1e-9);
        let crossing = Segment::new(Point2::new(-1.0, 5.0), Point2::new(11.0, 5.0));
        assert_eq!(segment_dist_to_polygon_edge(crossing, &poly), 0.0);
    }
}
