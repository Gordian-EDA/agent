//! Line segment with distance + intersection math. Owns the segment kernel so
//! callers say `seg.dist_to_rect(&r)` rather than reaching for free functions.

use crate::consts::EPS;
use crate::point::Point2;
use crate::rect::Rect;

/// A line segment between two points (mm, y-down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub a: Point2,
    pub b: Point2,
}

impl Segment {
    #[inline]
    pub const fn new(a: Point2, b: Point2) -> Self {
        Self { a, b }
    }

    #[inline]
    pub fn length(&self) -> f64 {
        self.a.dist(self.b)
    }

    #[inline]
    pub fn midpoint(&self) -> Point2 {
        Point2::new((self.a.x + self.b.x) / 2.0, (self.a.y + self.b.y) / 2.0)
    }

    /// Squared distance to point `p` (a zero-length segment is its point).
    pub fn dist2_to_point(&self, p: Point2) -> f64 {
        let ab = Point2::new(self.b.x - self.a.x, self.b.y - self.a.y);
        let len2 = ab.x * ab.x + ab.y * ab.y;
        if len2 <= f64::EPSILON {
            return p.dist2(self.a);
        }
        let t = (((p.x - self.a.x) * ab.x + (p.y - self.a.y) * ab.y) / len2).clamp(0.0, 1.0);
        p.dist2(Point2::new(self.a.x + t * ab.x, self.a.y + t * ab.y))
    }

    #[inline]
    pub fn dist_to_point(&self, p: Point2) -> f64 {
        self.dist2_to_point(p).sqrt()
    }

    /// Does `p` lie on this segment (endpoints included), within `EPS`?
    /// Collinear + within the bounding span.
    pub fn contains_point(&self, p: Point2) -> bool {
        self.a.orient(self.b, p).abs() <= EPS
            && p.x >= self.a.x.min(self.b.x) - EPS
            && p.x <= self.a.x.max(self.b.x) + EPS
            && p.y >= self.a.y.min(self.b.y) - EPS
            && p.y <= self.a.y.max(self.b.y) + EPS
    }

    /// Min distance to another segment; 0 if they intersect.
    pub fn dist_to_segment(&self, other: Segment) -> f64 {
        if self.intersects(other) {
            return 0.0;
        }
        self.dist_to_point(other.a)
            .min(self.dist_to_point(other.b))
            .min(other.dist_to_point(self.a))
            .min(other.dist_to_point(self.b))
    }

    /// Do the segments intersect (incl. touching / collinear overlap)?
    pub fn intersects(&self, other: Segment) -> bool {
        let (a, b, c, d) = (self.a, self.b, other.a, other.b);
        let d1 = c.orient(d, a);
        let d2 = c.orient(d, b);
        let d3 = a.orient(b, c);
        let d4 = a.orient(b, d);
        if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
            && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
        {
            return true;
        }
        // Collinear / touching fallback.
        other.contains_point(a)
            || other.contains_point(b)
            || self.contains_point(c)
            || self.contains_point(d)
    }

    /// Min distance to an axis-aligned rect; 0 if it enters/touches the rect.
    pub fn dist_to_rect(&self, r: &Rect) -> f64 {
        if r.dist_to_point(self.a) <= EPS || r.dist_to_point(self.b) <= EPS {
            return 0.0;
        }
        let c = [
            Point2::new(r.min_x, r.min_y),
            Point2::new(r.max_x, r.min_y),
            Point2::new(r.max_x, r.max_y),
            Point2::new(r.min_x, r.max_y),
        ];
        let mut best = f64::INFINITY;
        for i in 0..4 {
            best = best.min(self.dist_to_segment(Segment::new(c[i], c[(i + 1) % 4])));
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_to_segment() {
        let s = Segment::new(Point2::new(0.0, 0.0), Point2::new(10.0, 0.0));
        assert!((s.dist_to_point(Point2::new(5.0, 3.0)) - 3.0).abs() < 1e-9);
        assert!((s.dist_to_point(Point2::new(-4.0, 0.0)) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn crossing_segments_intersect() {
        let a = Segment::new(Point2::new(0.0, 0.0), Point2::new(2.0, 2.0));
        let b = Segment::new(Point2::new(0.0, 2.0), Point2::new(2.0, 0.0));
        assert!(a.intersects(b));
        assert_eq!(a.dist_to_segment(b), 0.0);
    }

    #[test]
    fn touching_endpoint_intersects() {
        let a = Segment::new(Point2::new(0.0, 0.0), Point2::new(2.0, 0.0));
        let b = Segment::new(Point2::new(2.0, 0.0), Point2::new(2.0, 2.0));
        assert!(a.intersects(b));
    }

    #[test]
    fn contains_point_accepts_segment_points() {
        let s = Segment::new(Point2::new(0.0, 0.0), Point2::new(4.0, 0.0));
        assert!(s.contains_point(Point2::new(2.0, 0.0)));
        assert!(!s.contains_point(Point2::new(2.0, 0.5)));
        assert!(!s.contains_point(Point2::new(5.0, 0.0)));
    }

    #[test]
    fn segment_to_rect() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        let outside = Segment::new(Point2::new(13.0, 5.0), Point2::new(13.0, 8.0));
        assert!((outside.dist_to_rect(&r) - 3.0).abs() < 1e-9);
        let through = Segment::new(Point2::new(-1.0, 5.0), Point2::new(11.0, 5.0));
        assert_eq!(through.dist_to_rect(&r), 0.0);
    }
}
