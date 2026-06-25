//! Regular 2-D grid snapping.

use crate::point::Point2;

/// A square grid with millimetre pitch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    pitch: f64,
}

impl Grid {
    #[inline]
    pub const fn new(pitch: f64) -> Self {
        Self { pitch }
    }

    #[inline]
    pub const fn pitch(self) -> f64 {
        self.pitch
    }

    /// Snap `v` to the nearest grid line, rounding half away from zero.
    pub fn snap(self, v: f64) -> f64 {
        let q = v / self.pitch;
        let rounded = if q >= 0.0 {
            (q + 0.5).floor()
        } else {
            (q - 0.5).ceil()
        };
        rounded * self.pitch
    }

    #[inline]
    pub fn snap_down(self, v: f64) -> f64 {
        (v / self.pitch).floor() * self.pitch
    }

    #[inline]
    pub fn snap_up(self, v: f64) -> f64 {
        (v / self.pitch).ceil() * self.pitch
    }

    #[inline]
    pub fn snap_point(self, p: impl Into<Point2>) -> Point2 {
        let p = p.into();
        Point2::new(self.snap(p.x), self.snap(p.y))
    }
}

/// The 50 mil grid used by schematic placement.
pub const GRID_50_MIL: Grid = Grid::new(1.27);

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < EPS
    }

    #[test]
    fn snaps_to_1_27mm_grid() {
        let grid = GRID_50_MIL;
        // Multiples of 1.27 are fixed points.
        assert!(close(grid.snap(0.0), 0.0));
        assert!(close(grid.snap(1.27), 1.27));
        assert!(close(grid.snap(2.54), 2.54));
        assert!(close(grid.snap(-3.81), -3.81));

        // Arbitrary values round to the nearest multiple of 1.27.
        // 1.9 / 1.27 = 1.496 -> nearest multiple is 1 -> 1.27
        // (1.9 is 0.63 above 1.27 but 0.64 below 2.54, so 1.27 is nearer).
        assert!(
            close(grid.snap(1.9), 1.27),
            "snap(1.9) should be 1.27, got {}",
            grid.snap(1.9)
        );
        // 1.2 / 1.27 = 0.945 -> nearest multiple is 1 -> 1.27
        assert!(close(grid.snap(1.2), 1.27));
        // 3.0 sits between 2.54 (2x) and 3.81 (3x); 3.0-2.54=0.46 < 3.81-3.0=0.81 -> 2.54
        assert!(close(grid.snap(3.0), 2.54));
    }

    #[test]
    fn rounds_half_away_from_zero() {
        let grid = GRID_50_MIL;
        // The exact half-grid point 0.635 (= 1.27/2) rounds away from zero.
        assert!(
            close(grid.snap(0.635), 1.27),
            "snap(0.635) = {}",
            grid.snap(0.635)
        );
        assert!(
            close(grid.snap(-0.635), -1.27),
            "snap(-0.635) = {}",
            grid.snap(-0.635)
        );
        // Negative non-half values snap to the nearest multiple, sign preserved.
        assert!(
            close(grid.snap(-1.9), -1.27),
            "snap(-1.9) = {}",
            grid.snap(-1.9)
        );
    }

    #[test]
    fn multiples_are_fixed_points() {
        let grid = GRID_50_MIL;
        for k in -10..=10 {
            let m = k as f64 * grid.pitch();
            assert!(
                close(grid.snap(m), m),
                "snap({m}) should be a fixed point, got {}",
                grid.snap(m)
            );
        }
    }

    #[test]
    fn snap_point_snaps_both_axes() {
        let grid = GRID_50_MIL;
        let p = grid.snap_point(Point2::new(1.9, 1.2));
        assert!(close(p.x, 1.27) && close(p.y, 1.27), "{p:?}");
        // Already-on-grid points are unchanged.
        assert_eq!(
            grid.snap_point(Point2::new(0.0, 2.54)),
            Point2::new(0.0, 2.54)
        );
    }

    #[test]
    fn directional_snap_uses_adjacent_grid_lines() {
        let grid = GRID_50_MIL;
        assert!(close(grid.snap_down(1.9), 1.27));
        assert!(close(grid.snap_up(1.9), 2.54));
        assert!(close(grid.snap_down(-1.9), -2.54));
        assert!(close(grid.snap_up(-1.9), -1.27));
    }
}
