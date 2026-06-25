//! Ordered polyline with simplification + bbox queries.

use crate::consts::EPS;
use crate::point::Point2;
use crate::rect::Rect;

/// An ordered polyline (mm, y-down): a routed copper/wire path or a net's
/// terminal set. Owns simplification and bbox queries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Polyline(pub Vec<Point2>);

impl Polyline {
    #[inline]
    pub fn new(points: Vec<Point2>) -> Self {
        Self(points)
    }

    #[inline]
    pub fn points(&self) -> &[Point2] {
        &self.0
    }

    #[inline]
    pub fn into_points(self) -> Vec<Point2> {
        self.0
    }

    /// Tight bounding box, or `None` if empty.
    #[inline]
    pub fn bbox(&self) -> Option<Rect> {
        Rect::bounding(&self.0)
    }

    /// Half-perimeter (w + h) of the bbox; 0 if empty. The HPWL/net-order term.
    #[inline]
    pub fn half_perimeter(&self) -> f64 {
        self.bbox().map_or(0.0, |r| r.half_perimeter())
    }

    /// Dedup consecutive coincident points and merge collinear runs (orthogonal
    /// AND 45°). The single polyline simplifier for routed paths.
    pub fn simplify(self) -> Polyline {
        let mut pts: Vec<Point2> = Vec::with_capacity(self.0.len());
        for p in self.0 {
            if pts.last().map_or(true, |q: &Point2| q.dist2(p) > EPS * EPS) {
                pts.push(p);
            }
        }
        if pts.len() < 3 {
            return Polyline(pts);
        }
        let mut out: Vec<Point2> = vec![pts[0]];
        for i in 1..pts.len() - 1 {
            let (a, b, c) = (out[out.len() - 1], pts[i], pts[i + 1]);
            if a.orient(b, c).abs() > EPS {
                out.push(b);
            }
        }
        out.push(pts[pts.len() - 1]);
        Polyline(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplify_merges_collinear_and_dedups() {
        let path = vec![
            Point2::new(0.0, 0.0),
            Point2::new(0.0, 0.0), // dup
            Point2::new(1.0, 0.0),
            Point2::new(2.0, 0.0), // collinear → dropped
            Point2::new(2.0, 1.0),
        ];
        let out = Polyline::new(path).simplify().into_points();
        assert_eq!(
            out,
            vec![Point2::new(0.0, 0.0), Point2::new(2.0, 0.0), Point2::new(2.0, 1.0)]
        );
    }

    #[test]
    fn simplify_keeps_45_degree_collinear() {
        // A straight 45° run collapses to its endpoints.
        let path = vec![
            Point2::new(0.0, 0.0),
            Point2::new(1.0, 1.0),
            Point2::new(2.0, 2.0),
        ];
        let out = Polyline::new(path).simplify().into_points();
        assert_eq!(out, vec![Point2::new(0.0, 0.0), Point2::new(2.0, 2.0)]);
    }

    #[test]
    fn half_perimeter_of_bbox() {
        let pl = Polyline::new(vec![Point2::new(0.0, 0.0), Point2::new(3.0, 4.0)]);
        assert!((pl.half_perimeter() - 7.0).abs() < 1e-9);
        assert_eq!(Polyline::default().half_perimeter(), 0.0);
    }
}
