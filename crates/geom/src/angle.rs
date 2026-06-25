//! Angle helpers.

/// Snap `deg` to the nearest quadrant (0/90/180/270), result in `[0, 360)`.
#[inline]
pub fn snap_quadrant(deg: f64) -> f64 {
    (deg / 90.0).round().rem_euclid(4.0) * 90.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_quadrant_rounds_to_axes() {
        assert_eq!(snap_quadrant(43.0), 0.0);
        assert_eq!(snap_quadrant(46.0), 90.0);
        assert_eq!(snap_quadrant(-90.0), 270.0);
        assert_eq!(snap_quadrant(360.0), 0.0);
    }
}
