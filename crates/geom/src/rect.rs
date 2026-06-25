//! The single axis-aligned rectangle type (region / bounds / bbox).

use serde::{Deserialize, Serialize};

use crate::consts::EPS;
use crate::point::Point2;
use crate::segment::Segment;

/// An axis-aligned rectangle in mm, y-down, `[min, max]` per axis. The single
/// rect/bounds/bbox type across the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Rect {
    #[inline]
    pub const fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self { min_x, min_y, max_x, max_y }
    }

    /// Normalized bbox from two arbitrary corner points.
    #[inline]
    pub fn from_points(a: Point2, b: Point2) -> Self {
        Self {
            min_x: a.x.min(b.x),
            min_y: a.y.min(b.y),
            max_x: a.x.max(b.x),
            max_y: a.y.max(b.y),
        }
    }

    /// Tight bounding box of a point set, or `None` if empty. The canonical
    /// "bbox of points" constructor (used by [`crate::Polyline::bbox`] too).
    pub fn bounding(points: &[Point2]) -> Option<Rect> {
        let first = points.first()?;
        let mut r = Rect::new(first.x, first.y, first.x, first.y);
        for p in points {
            r.min_x = r.min_x.min(p.x);
            r.min_y = r.min_y.min(p.y);
            r.max_x = r.max_x.max(p.x);
            r.max_y = r.max_y.max(p.y);
        }
        Some(r)
    }

    #[inline]
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    #[inline]
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    /// Half-perimeter (width + height) — the HPWL term for a bbox.
    #[inline]
    pub fn half_perimeter(&self) -> f64 {
        self.width() + self.height()
    }

    #[inline]
    pub fn area(&self) -> f64 {
        self.width() * self.height()
    }

    #[inline]
    pub fn center(&self) -> Point2 {
        Point2::new((self.min_x + self.max_x) / 2.0, (self.min_y + self.max_y) / 2.0)
    }

    /// Inflate by `m` on every side (negative shrinks).
    #[inline]
    pub fn inflate(&self, m: f64) -> Rect {
        Rect::new(self.min_x - m, self.min_y - m, self.max_x + m, self.max_y + m)
    }

    /// Is `p` inside or on the boundary?
    #[inline]
    pub fn contains(&self, p: Point2) -> bool {
        p.x >= self.min_x && p.x <= self.max_x && p.y >= self.min_y && p.y <= self.max_y
    }

    /// Do the rects overlap with positive area? Open + `EPS`-eased: a shared
    /// edge (within float dust) is NOT an overlap. The single boolean overlap
    /// predicate for the whole workspace.
    #[inline]
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.min_x < other.max_x - EPS
            && self.max_x > other.min_x + EPS
            && self.min_y < other.max_y - EPS
            && self.max_y > other.min_y + EPS
    }

    /// The overlap rectangle, or `None` when they do not overlap.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let min_x = self.min_x.max(other.min_x);
        let max_x = self.max_x.min(other.max_x);
        let min_y = self.min_y.max(other.min_y);
        let max_y = self.max_y.min(other.max_y);
        (min_x < max_x && min_y < max_y).then_some(Rect { min_x, min_y, max_x, max_y })
    }

    /// Does the boundary of `other` cross the interior of `self`?
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

    /// Distance from `p` to this rect; 0 inside.
    pub fn dist_to_point(&self, p: Point2) -> f64 {
        let dx = (self.min_x - p.x).max(0.0).max(p.x - self.max_x);
        let dy = (self.min_y - p.y).max(0.0).max(p.y - self.max_y);
        (dx * dx + dy * dy).sqrt()
    }

    /// Min distance to another rect; 0 if they overlap or touch.
    pub fn dist_to_rect(&self, other: &Rect) -> f64 {
        let dx = (self.min_x - other.max_x).max(other.min_x - self.max_x).max(0.0);
        let dy = (self.min_y - other.max_y).max(other.min_y - self.max_y).max(0.0);
        (dx * dx + dy * dy).sqrt()
    }

    /// Min distance to a segment; 0 if it enters/touches. Mirror of
    /// [`Segment::dist_to_rect`].
    #[inline]
    pub fn dist_to_segment(&self, s: Segment) -> f64 {
        s.dist_to_rect(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounding_of_points() {
        let r = Rect::bounding(&[
            Point2::new(1.0, 5.0),
            Point2::new(-2.0, 3.0),
            Point2::new(4.0, -1.0),
        ])
        .unwrap();
        assert_eq!(r, Rect::new(-2.0, -1.0, 4.0, 5.0));
        assert!(Rect::bounding(&[]).is_none());
    }

    #[test]
    fn overlaps_is_open() {
        let a = Rect::new(0.0, 0.0, 2.0, 2.0);
        let edge = Rect::new(2.0, 0.0, 4.0, 2.0); // shares the x=2 edge
        assert!(!a.overlaps(&edge));
        let inside = Rect::new(1.0, 1.0, 3.0, 3.0);
        assert!(a.overlaps(&inside));
    }

    #[test]
    fn dist_to_point_zero_inside() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(r.dist_to_point(Point2::new(5.0, 5.0)), 0.0);
        assert!((r.dist_to_point(Point2::new(13.0, 5.0)) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn json_camel_round_trip() {
        let r = Rect::new(1.0, 2.0, 3.0, 4.0);
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(j, r#"{"minX":1.0,"minY":2.0,"maxX":3.0,"maxY":4.0}"#);
        // name-based: order-independent (tscircuit emits minX,maxX,minY,maxY).
        let p: Rect = serde_json::from_str(r#"{"minX":1.0,"maxX":3.0,"minY":2.0,"maxY":4.0}"#).unwrap();
        assert_eq!(p, r);
    }
}
