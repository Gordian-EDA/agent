//! The single 2-D point type shared across the schematic and PCB stacks.

use serde::{Deserialize, Serialize};
use std::ops::{Index, IndexMut};

/// A 2-D point in millimetres, y-down. The single point type across the
/// workspace (schematic sheet, PCB board, symbol pins after load).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Point2 {
    pub x: f64,
    pub y: f64,
}

impl Point2 {
    #[inline]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Squared euclidean distance (cheaper than [`Point2::dist`] for compares).
    #[inline]
    pub fn dist2(&self, other: Point2) -> f64 {
        let (dx, dy) = (self.x - other.x, self.y - other.y);
        dx * dx + dy * dy
    }

    /// Euclidean distance (mm).
    #[inline]
    pub fn dist(&self, other: Point2) -> f64 {
        self.dist2(other).sqrt()
    }

    /// Manhattan distance (`|dx| + |dy|`) in millimetres.
    #[inline]
    pub fn manhattan(&self, other: Point2) -> f64 {
        (self.x - other.x).abs() + (self.y - other.y).abs()
    }

    /// Coordinate-wise epsilon equality.
    #[inline]
    pub fn near_eq(self, other: Point2, eps: f64) -> bool {
        (self.x - other.x).abs() <= eps && (self.y - other.y).abs() <= eps
    }

    /// Round-scaled integer key for deterministic coordinate maps.
    #[inline]
    pub fn quantized_key(self, scale: f64) -> (i64, i64) {
        ((self.x * scale).round() as i64, (self.y * scale).round() as i64)
    }

    /// Plain `(x, y)` tuple for deterministic coordinate sorting.
    #[inline]
    pub fn sort_key(self) -> (f64, f64) {
        (self.x, self.y)
    }

    /// Orientation determinant of `(self, a, b)`: >0 ccw, <0 cw, 0 collinear.
    #[inline]
    pub fn orient(self, a: Point2, b: Point2) -> f64 {
        (a.x - self.x) * (b.y - self.y) - (a.y - self.y) * (b.x - self.x)
    }

    /// Rotate about the origin by `deg` (CCW-positive) in y-down space:
    /// `x' = x·cosθ + y·sinθ`, `y' = −x·sinθ + y·cosθ`. KiCAD's footprint/symbol
    /// rotation convention — the single rotation primitive workspace-wide.
    #[inline]
    pub fn rotate(self, deg: f64) -> Point2 {
        let (s, c) = deg.to_radians().sin_cos();
        Point2::new(self.x * c + self.y * s, -self.x * s + self.y * c)
    }

    /// Apply a placed instance's transform to this local offset: optional
    /// x-mirror, then [`Point2::rotate`]. Symbol-local geometry is y-down (the
    /// loader flips at the boundary), so this is the whole symbol→sheet
    /// transform — no special Y-flip. Translation to the instance position is
    /// the caller's job.
    #[inline]
    pub fn transform(self, deg: f64, mirror: bool) -> Point2 {
        let m = if mirror {
            Point2::new(-self.x, self.y)
        } else {
            self
        };
        m.rotate(deg)
    }

    /// Transform a KiCad symbol-local offset into schematic sheet space:
    /// optional x-mirror, symbol-space rotation, then y-flip into sheet space.
    #[inline]
    pub fn transform_offset(self, deg: f64, mirror: bool) -> Point2 {
        let (mut x, y) = (self.x, self.y);
        if mirror {
            x = -x;
        }
        let (s, c) = deg.to_radians().sin_cos();
        Point2::new(x * c - y * s, -(x * s + y * c))
    }
}

impl From<[f64; 2]> for Point2 {
    #[inline]
    fn from(a: [f64; 2]) -> Self {
        Self { x: a[0], y: a[1] }
    }
}

impl From<Point2> for [f64; 2] {
    #[inline]
    fn from(p: Point2) -> Self {
        [p.x, p.y]
    }
}

impl Index<usize> for Point2 {
    type Output = f64;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        match index {
            0 => &self.x,
            1 => &self.y,
            _ => panic!("Point2 index out of bounds: {index}"),
        }
    }
}

impl IndexMut<usize> for Point2 {
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        match index {
            0 => &mut self.x,
            1 => &mut self.y,
            _ => panic!("Point2 index out of bounds: {index}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_matches_kicad_convention() {
        let p = Point2::new(-2.475, 1.905).rotate(270.0);
        assert!(
            (p.x - -1.905).abs() < 1e-9 && (p.y - -2.475).abs() < 1e-9,
            "{p:?}"
        );
        let q = Point2::new(-2.475, 1.905).rotate(90.0);
        assert!(
            (q.x - 1.905).abs() < 1e-9 && (q.y - 2.475).abs() < 1e-9,
            "{q:?}"
        );
        let r = Point2::new(1.0, 2.0).rotate(180.0);
        assert!(
            (r.x - -1.0).abs() < 1e-9 && (r.y - -2.0).abs() < 1e-9,
            "{r:?}"
        );
    }

    #[test]
    fn transform_mirror_negates_x() {
        let p = Point2::new(1.0, 2.0).transform(0.0, true);
        assert_eq!(p, Point2::new(-1.0, 2.0));
    }

    #[test]
    fn transform_offset_flips_symbol_y_into_sheet_space() {
        let p = Point2::new(1.0, 2.0).transform_offset(0.0, false);
        assert_eq!(p, Point2::new(1.0, -2.0));
        let q = Point2::new(1.0, 2.0).transform_offset(90.0, true);
        assert!((q.x - -2.0).abs() < 1e-9 && (q.y - 1.0).abs() < 1e-9);
    }

    #[test]
    fn manhattan_distance() {
        assert_eq!(
            Point2::new(1.0, -2.0).manhattan(Point2::new(4.0, 3.0)),
            8.0
        );
    }

    #[test]
    fn near_eq_is_coordinate_wise() {
        let p = Point2::new(1.0, 2.0);
        assert!(p.near_eq(Point2::new(1.001, 1.999), 0.001));
        assert!(!p.near_eq(Point2::new(1.002, 2.0), 0.001));
    }

    #[test]
    fn quantized_key_rounds_scaled_coordinates() {
        assert_eq!(Point2::new(1.2344, -2.3456).quantized_key(1000.0), (1234, -2346));
    }

    #[test]
    fn json_is_camel_xy() {
        let j = serde_json::to_string(&Point2::new(1.5, -2.5)).unwrap();
        assert_eq!(j, r#"{"x":1.5,"y":-2.5}"#);
        let p: Point2 = serde_json::from_str(&j).unwrap();
        assert_eq!(p, Point2::new(1.5, -2.5));
    }
}
