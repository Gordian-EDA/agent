//! KiCAD placement grid (1.27 mm = 50 mil) snapping.
//!
//! KiCAD's default schematic grid is 50 mil = 1.27 mm. Pin connection points
//! land cleanly only when symbol instances and labels sit on this grid, so we
//! snap every emitted coordinate to the nearest multiple of [`GRID_MM`].

use crate::point::Point2;

/// The schematic grid pitch in millimetres (50 mil).
pub const GRID_MM: f64 = 1.27;

/// Snap `v` to the nearest multiple of [`GRID_MM`], rounding half away from zero.
pub fn snap(v: f64) -> f64 {
    let q = v / GRID_MM;
    // Round half away from zero (symmetric about 0) rather than to-even.
    let rounded = if q >= 0.0 {
        (q + 0.5).floor()
    } else {
        (q - 0.5).ceil()
    };
    rounded * GRID_MM
}

/// Snap both coordinates of a point to the grid.
pub fn snap_point(p: impl Into<Point2>) -> Point2 {
    let p = p.into();
    Point2::new(snap(p.x), snap(p.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < EPS
    }

    #[test]
    fn snaps_to_1_27mm_grid() {
        // Multiples of 1.27 are fixed points.
        assert!(close(snap(0.0), 0.0));
        assert!(close(snap(1.27), 1.27));
        assert!(close(snap(2.54), 2.54));
        assert!(close(snap(-3.81), -3.81));

        // Arbitrary values round to the nearest multiple of 1.27.
        // 1.9 / 1.27 = 1.496 -> nearest multiple is 1 -> 1.27
        // (1.9 is 0.63 above 1.27 but 0.64 below 2.54, so 1.27 is nearer).
        assert!(
            close(snap(1.9), 1.27),
            "snap(1.9) should be 1.27, got {}",
            snap(1.9)
        );
        // 1.2 / 1.27 = 0.945 -> nearest multiple is 1 -> 1.27
        assert!(close(snap(1.2), 1.27));
        // 3.0 sits between 2.54 (2x) and 3.81 (3x); 3.0-2.54=0.46 < 3.81-3.0=0.81 -> 2.54
        assert!(close(snap(3.0), 2.54));
    }

    #[test]
    fn rounds_half_away_from_zero() {
        // The exact half-grid point 0.635 (= 1.27/2) rounds away from zero.
        assert!(close(snap(0.635), 1.27), "snap(0.635) = {}", snap(0.635));
        assert!(
            close(snap(-0.635), -1.27),
            "snap(-0.635) = {}",
            snap(-0.635)
        );
        // Negative non-half values snap to the nearest multiple, sign preserved.
        assert!(close(snap(-1.9), -1.27), "snap(-1.9) = {}", snap(-1.9));
    }

    #[test]
    fn multiples_are_fixed_points() {
        for k in -10..=10 {
            let m = k as f64 * GRID_MM;
            assert!(
                close(snap(m), m),
                "snap({m}) should be a fixed point, got {}",
                snap(m)
            );
        }
    }

    #[test]
    fn snap_point_snaps_both_axes() {
        let p = snap_point(Point2::new(1.9, 1.2));
        assert!(close(p.x, 1.27) && close(p.y, 1.27), "{p:?}");
        // Already-on-grid points are unchanged.
        assert_eq!(snap_point(Point2::new(0.0, 2.54)), Point2::new(0.0, 2.54));
    }
}
