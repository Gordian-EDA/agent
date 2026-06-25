//! The single axis-aligned rectangle type (region / bounds / bbox).

use serde::{Deserialize, Serialize};
use std::ops::Index;

use crate::consts::{EPS, STRICT_EPS};
use crate::point::Point2;
use crate::segment::Segment;

/// Axis direction of a shared rectangle boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryAxis {
    Vertical,
    Horizontal,
}

/// Positive-length boundary segment shared by two abutting rectangles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SharedBoundary {
    pub axis: BoundaryAxis,
    pub coord: f64,
    pub lo: f64,
    pub hi: f64,
}

impl SharedBoundary {
    #[inline]
    pub fn len(&self) -> f64 {
        self.hi - self.lo
    }

    #[inline]
    pub fn point_at(&self, t: f64) -> Point2 {
        match self.axis {
            BoundaryAxis::Vertical => Point2::new(self.coord, t),
            BoundaryAxis::Horizontal => Point2::new(t, self.coord),
        }
    }

    #[inline]
    pub fn midpoint(&self) -> Point2 {
        self.point_at((self.lo + self.hi) / 2.0)
    }
}

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
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    #[inline]
    pub const fn zero() -> Self {
        Self::new(0.0, 0.0, 0.0, 0.0)
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

    /// Rect from a center point and half-extents.
    #[inline]
    pub fn from_center_half(center: Point2, half: (f64, f64)) -> Self {
        Self::new(
            center.x - half.0,
            center.y - half.1,
            center.x + half.0,
            center.y + half.1,
        )
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
        Point2::new(
            (self.min_x + self.max_x) / 2.0,
            (self.min_y + self.max_y) / 2.0,
        )
    }

    /// Clamp a centered rectangle's center so its half-extents fit in `self`.
    ///
    /// If the half-extents are wider than `self` on an axis, that axis is
    /// centered in `self`.
    #[inline]
    pub fn clamp_center_for_half(&self, center: Point2, half: (f64, f64)) -> Point2 {
        let (lo_x, hi_x) = (self.min_x + half.0, self.max_x - half.0);
        let (lo_y, hi_y) = (self.min_y + half.1, self.max_y - half.1);
        let bounds_center = self.center();
        Point2::new(
            if lo_x <= hi_x {
                center.x.clamp(lo_x, hi_x)
            } else {
                bounds_center.x
            },
            if lo_y <= hi_y {
                center.y.clamp(lo_y, hi_y)
            } else {
                bounds_center.y
            },
        )
    }

    /// Inflate by `m` on every side (negative shrinks).
    #[inline]
    pub fn inflate(&self, m: f64) -> Rect {
        Rect::new(
            self.min_x - m,
            self.min_y - m,
            self.max_x + m,
            self.max_y + m,
        )
    }

    #[inline]
    pub fn clamped_to(&self, bounds: &Rect) -> Rect {
        Rect::new(
            self.min_x.max(bounds.min_x),
            self.min_y.max(bounds.min_y),
            self.max_x.min(bounds.max_x),
            self.max_y.min(bounds.max_y),
        )
    }

    #[inline]
    pub fn inflate_clamped_to(&self, m: f64, bounds: &Rect) -> Rect {
        self.inflate(m).clamped_to(bounds)
    }

    /// Is `p` inside or on the boundary?
    #[inline]
    pub fn contains(&self, p: Point2) -> bool {
        p.x >= self.min_x && p.x <= self.max_x && p.y >= self.min_y && p.y <= self.max_y
    }

    /// Does this rect contain all of `other`, with `eps` slack on each edge?
    #[inline]
    pub fn contains_rect_eps(&self, other: &Rect, eps: f64) -> bool {
        other.min_x >= self.min_x - eps
            && other.max_x <= self.max_x + eps
            && other.min_y >= self.min_y - eps
            && other.max_y <= self.max_y + eps
    }

    /// Per-axis distance `other` extends outside `self`.
    #[inline]
    pub fn containment_overshoot(&self, other: &Rect) -> (f64, f64) {
        (
            (self.min_x - other.min_x).max(0.0) + (other.max_x - self.max_x).max(0.0),
            (self.min_y - other.min_y).max(0.0) + (other.max_y - self.max_y).max(0.0),
        )
    }

    /// How far a disc of `radius` centered at `p` extends past this rect.
    ///
    /// Returns `0.0` when the disc is fully inside or touching the boundary.
    #[inline]
    pub fn disc_overshoot(&self, p: Point2, radius: f64) -> f64 {
        let left = (self.min_x - (p.x - radius)).max(0.0);
        let right = ((p.x + radius) - self.max_x).max(0.0);
        let top = (self.min_y - (p.y - radius)).max(0.0);
        let bottom = ((p.y + radius) - self.max_y).max(0.0);
        left.max(right).max(top).max(bottom)
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
        if !self.overlaps(other) {
            return None;
        }
        let min_x = self.min_x.max(other.min_x);
        let max_x = self.max_x.min(other.max_x);
        let min_y = self.min_y.max(other.min_y);
        let max_y = self.max_y.min(other.max_y);
        Some(Rect {
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }

    /// Positive overlap width and height, or `None` when no area overlaps.
    pub fn overlap_size(&self, other: &Rect) -> Option<(f64, f64)> {
        self.intersection(other).map(|r| (r.width(), r.height()))
    }

    /// Per-axis overlap depth; positive on an axis means overlap, zero means
    /// touching, negative means separation.
    pub fn axis_penetration(&self, other: &Rect) -> (f64, f64) {
        (
            self.max_x.min(other.max_x) - self.min_x.max(other.min_x),
            self.max_y.min(other.max_y) - self.min_y.max(other.min_y),
        )
    }

    /// Positive-length boundary shared with an abutting rect.
    pub fn shared_boundary(&self, other: &Rect) -> Option<SharedBoundary> {
        let touch_v = |left: &Rect, right: &Rect| -> Option<SharedBoundary> {
            if (left.max_x - right.min_x).abs() < STRICT_EPS {
                let lo = left.min_y.max(right.min_y);
                let hi = left.max_y.min(right.max_y);
                if hi - lo > STRICT_EPS {
                    return Some(SharedBoundary {
                        axis: BoundaryAxis::Vertical,
                        coord: left.max_x,
                        lo,
                        hi,
                    });
                }
            }
            None
        };
        let touch_h = |top: &Rect, bottom: &Rect| -> Option<SharedBoundary> {
            if (top.max_y - bottom.min_y).abs() < STRICT_EPS {
                let lo = top.min_x.max(bottom.min_x);
                let hi = top.max_x.min(bottom.max_x);
                if hi - lo > STRICT_EPS {
                    return Some(SharedBoundary {
                        axis: BoundaryAxis::Horizontal,
                        coord: top.max_y,
                        lo,
                        hi,
                    });
                }
            }
            None
        };
        touch_v(self, other)
            .or_else(|| touch_v(other, self))
            .or_else(|| touch_h(self, other))
            .or_else(|| touch_h(other, self))
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
        let dx = (self.min_x - other.max_x)
            .max(other.min_x - self.max_x)
            .max(0.0);
        let dy = (self.min_y - other.max_y)
            .max(other.min_y - self.max_y)
            .max(0.0);
        (dx * dx + dy * dy).sqrt()
    }

    /// Min distance to a segment; 0 if it enters/touches. Mirror of
    /// [`Segment::dist_to_rect`].
    #[inline]
    pub fn dist_to_segment(&self, s: Segment) -> f64 {
        s.dist_to_rect(self)
    }

    /// Area of the union of axis-aligned rectangles.
    pub fn union_area(rects: &[Rect]) -> f64 {
        if rects.is_empty() {
            return 0.0;
        }
        let mut xs: Vec<f64> = Vec::with_capacity(rects.len() * 2);
        let mut ys: Vec<f64> = Vec::with_capacity(rects.len() * 2);
        for r in rects {
            xs.push(r.min_x);
            xs.push(r.max_x);
            ys.push(r.min_y);
            ys.push(r.max_y);
        }
        xs.sort_by(|a, b| a.total_cmp(b));
        xs.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
        ys.sort_by(|a, b| a.total_cmp(b));
        ys.dedup_by(|a, b| (*a - *b).abs() < 1e-12);

        let mut area = 0.0;
        for xi in 0..xs.len().saturating_sub(1) {
            let (x0, x1) = (xs[xi], xs[xi + 1]);
            let cx = (x0 + x1) / 2.0;
            for yi in 0..ys.len().saturating_sub(1) {
                let (y0, y1) = (ys[yi], ys[yi + 1]);
                let cy = (y0 + y1) / 2.0;
                if rects.iter().any(|r| r.contains(Point2::new(cx, cy))) {
                    area += (x1 - x0) * (y1 - y0);
                }
            }
        }
        area
    }
}

impl From<[f64; 4]> for Rect {
    #[inline]
    fn from(r: [f64; 4]) -> Self {
        Rect::new(r[0], r[1], r[2], r[3])
    }
}

impl Index<usize> for Rect {
    type Output = f64;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        match index {
            0 => &self.min_x,
            1 => &self.min_y,
            2 => &self.max_x,
            3 => &self.max_y,
            _ => panic!("Rect index out of bounds: {index}"),
        }
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
        assert!(a.intersection(&edge).is_none());
        let inside = Rect::new(1.0, 1.0, 3.0, 3.0);
        assert!(a.overlaps(&inside));
        assert_eq!(a.intersection(&inside), Some(Rect::new(1.0, 1.0, 2.0, 2.0)));
    }

    #[test]
    fn clamps_center_for_half_extents() {
        let bounds = Rect::new(0.0, 0.0, 10.0, 6.0);
        assert_eq!(
            bounds.clamp_center_for_half(Point2::new(-5.0, 20.0), (2.0, 1.0)),
            Point2::new(2.0, 5.0)
        );
        assert_eq!(
            bounds.clamp_center_for_half(Point2::new(8.0, 3.0), (6.0, 1.0)),
            Point2::new(5.0, 3.0)
        );
    }

    #[test]
    fn containment_overshoot_sums_outside_edges() {
        let bounds = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(
            bounds.containment_overshoot(&Rect::new(-1.0, 2.0, 12.0, 9.0)),
            (3.0, 0.0)
        );
        assert_eq!(
            bounds.containment_overshoot(&Rect::new(1.0, 2.0, 3.0, 4.0)),
            (0.0, 0.0)
        );
    }

    #[test]
    fn clamps_inflated_rect_to_bounds() {
        let r = Rect::new(2.0, 3.0, 5.0, 7.0);
        let b = Rect::new(0.0, 0.0, 6.0, 6.0);
        assert_eq!(r.inflate_clamped_to(2.0, &b), Rect::new(0.0, 1.0, 6.0, 6.0));
    }

    #[test]
    fn union_area_counts_overlaps_once() {
        let rects = [Rect::new(0.0, 0.0, 2.0, 2.0), Rect::new(1.0, 0.0, 3.0, 2.0)];
        assert_eq!(Rect::union_area(&rects), 6.0);
        assert_eq!(Rect::union_area(&[]), 0.0);
    }

    #[test]
    fn shared_boundary_vertical() {
        let a = Rect::new(0.0, 0.0, 2.0, 4.0);
        let b = Rect::new(2.0, 1.0, 5.0, 3.0);
        let shared = a.shared_boundary(&b).unwrap();
        assert_eq!(
            shared,
            SharedBoundary {
                axis: BoundaryAxis::Vertical,
                coord: 2.0,
                lo: 1.0,
                hi: 3.0,
            }
        );
        assert_eq!(shared.len(), 2.0);
        assert_eq!(shared.midpoint(), Point2::new(2.0, 2.0));
    }

    #[test]
    fn shared_boundary_horizontal() {
        let a = Rect::new(0.0, 0.0, 4.0, 2.0);
        let b = Rect::new(1.0, 2.0, 3.0, 5.0);
        let shared = a.shared_boundary(&b).unwrap();
        assert_eq!(shared.axis, BoundaryAxis::Horizontal);
        assert_eq!(shared.coord, 2.0);
        assert_eq!(shared.lo, 1.0);
        assert_eq!(shared.hi, 3.0);
        assert_eq!(shared.point_at(1.5), Point2::new(1.5, 2.0));
    }

    #[test]
    fn shared_boundary_requires_positive_overlap() {
        let a = Rect::new(0.0, 0.0, 2.0, 2.0);
        let corner = Rect::new(2.0, 2.0, 4.0, 4.0);
        let gap = Rect::new(2.01, 0.0, 4.0, 2.0);
        let overlap = Rect::new(1.5, 0.0, 4.0, 2.0);
        assert!(a.shared_boundary(&corner).is_none());
        assert!(a.shared_boundary(&gap).is_none());
        assert!(a.shared_boundary(&overlap).is_none());
    }

    #[test]
    fn overlap_size_reports_penetration_dims() {
        let a = Rect::new(0.0, 0.0, 5.0, 4.0);
        let b = Rect::new(3.0, 1.0, 7.0, 3.0);
        assert_eq!(a.overlap_size(&b), Some((2.0, 2.0)));
        assert_eq!(a.overlap_size(&Rect::new(5.0, 0.0, 6.0, 4.0)), None);
    }

    #[test]
    fn centered_rect_contains_and_penetration() {
        let bounds = Rect::new(0.0, 0.0, 10.0, 10.0);
        let r = Rect::from_center_half(Point2::new(5.0, 5.0), (2.0, 1.0));
        assert_eq!(r, Rect::new(3.0, 4.0, 7.0, 6.0));
        assert!(bounds.contains_rect_eps(&r, 1e-9));
        assert!(!bounds.contains_rect_eps(
            &Rect::from_center_half(Point2::new(9.0, 5.0), (2.0, 1.0)),
            1e-9
        ));

        let inflated_a = r.inflate(0.5);
        let inflated_b = Rect::from_center_half(Point2::new(8.0, 5.0), (1.0, 1.0)).inflate(0.5);
        assert_eq!(inflated_a.axis_penetration(&inflated_b), (1.0, 3.0));
        assert_eq!(
            r.axis_penetration(&Rect::new(8.0, 0.0, 9.0, 1.0)),
            (-1.0, -3.0)
        );
    }

    #[test]
    fn dist_to_point_zero_inside() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(r.dist_to_point(Point2::new(5.0, 5.0)), 0.0);
        assert!((r.dist_to_point(Point2::new(13.0, 5.0)) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn disc_overshoot_reports_axis_boundary_excess() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);

        assert_eq!(r.disc_overshoot(Point2::new(5.0, 5.0), 2.0), 0.0);
        assert_eq!(r.disc_overshoot(Point2::new(1.0, 5.0), 1.0), 0.0);
        assert_eq!(r.disc_overshoot(Point2::new(0.5, 5.0), 1.0), 0.5);
        assert_eq!(r.disc_overshoot(Point2::new(11.0, -2.0), 0.25), 2.25);
    }

    #[test]
    fn json_camel_round_trip() {
        let r = Rect::new(1.0, 2.0, 3.0, 4.0);
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(j, r#"{"minX":1.0,"minY":2.0,"maxX":3.0,"maxY":4.0}"#);
        // name-based: order-independent (tscircuit emits minX,maxX,minY,maxY).
        let p: Rect =
            serde_json::from_str(r#"{"minX":1.0,"maxX":3.0,"minY":2.0,"maxY":4.0}"#).unwrap();
        assert_eq!(p, r);
    }
}
