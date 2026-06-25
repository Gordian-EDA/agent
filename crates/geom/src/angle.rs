//! Angle and rotated-extent helpers.

/// Half-extents (hw, hh) of a `w × h` rectangle rotated by `deg`.
#[inline]
pub fn rotated_aabb_half(w: f64, h: f64, deg: f64) -> (f64, f64) {
    let (s, c) = deg.to_radians().sin_cos();
    (
        (w / 2.0 * c).abs() + (h / 2.0 * s).abs(),
        (w / 2.0 * s).abs() + (h / 2.0 * c).abs(),
    )
}

/// Snap `deg` to the nearest quadrant (0/90/180/270), result in `[0, 360)`.
#[inline]
pub fn snap_quadrant(deg: f64) -> f64 {
    (deg / 90.0).round().rem_euclid(4.0) * 90.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aabb_half_unrotated_is_half_extents() {
        let (hw, hh) = rotated_aabb_half(4.0, 2.0, 0.0);
        assert!((hw - 2.0).abs() < 1e-9 && (hh - 1.0).abs() < 1e-9);
        // 90° swaps w/h.
        let (hw, hh) = rotated_aabb_half(4.0, 2.0, 90.0);
        assert!((hw - 1.0).abs() < 1e-9 && (hh - 2.0).abs() < 1e-9);
    }

    #[test]
    fn snap_quadrant_rounds_to_axes() {
        assert_eq!(snap_quadrant(43.0), 0.0);
        assert_eq!(snap_quadrant(46.0), 90.0);
        assert_eq!(snap_quadrant(-90.0), 270.0);
        assert_eq!(snap_quadrant(360.0), 0.0);
    }
}
