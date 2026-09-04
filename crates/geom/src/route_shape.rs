//! How much a drawn orthogonal route departs from the ideal run between its ends.
//!
//! A person draws a connection when it can be drawn cleanly and names it otherwise, and
//! "cleanly" is a judgement about SHAPE, not about length: a 70 mm straight run reads
//! better than a 20 mm snake. This is the one place that judgement is written down, so
//! the router that picks a path and the caller that decides whether to keep it are
//! ranking the same thing.

use crate::{GRID_50_MIL, Point2};

/// Grid units charged for one corner.
const BEND: f64 = 6.0;
/// Grid units charged for one crossing over a foreign net.
pub const CROSSING_COST: f64 = 20.0;

/// A route's departure from the straight run between its ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteShape {
    /// Millimetres the drawn path runs beyond the direct Manhattan gap between its ends.
    pub detour_mm: f64,
    /// Corners in the simplified path.
    pub bends: usize,
    /// Places it passes over a foreign net's wire.
    pub crossings: usize,
}

/// What a clean LONG hop may cost: straight, or one corner with no detour. Past this a
/// long connection reads as a snake and a net label is cleaner.
pub const SHAPE_SIMPLE: f64 = 8.5;
/// What a LOCAL hop may cost: a Z, or one crossing on an otherwise direct run.
pub const SHAPE_GENERAL: f64 = 30.5;

impl RouteShape {
    /// Measure `path` (corner points, both ends inclusive) with a crossing count the
    /// caller took against its own scene.
    pub fn of(path: &[Point2], crossings: usize) -> Self {
        let drawn: f64 = path.windows(2).map(|w| w[0].manhattan(w[1])).sum();
        let direct = match (path.first(), path.last()) {
            (Some(a), Some(b)) => a.manhattan(*b),
            _ => 0.0,
        };
        RouteShape {
            detour_mm: (drawn - direct).max(0.0),
            bends: path.len().saturating_sub(2),
            crossings,
        }
    }

    /// The shape cost, in grid units: the detour plus [`BEND`] a corner and
    /// [`CROSSING`] a crossing. Comparable against [`SHAPE_SIMPLE`] / [`SHAPE_GENERAL`],
    /// and orderable on its own as a router's selection key.
    pub fn cost(&self) -> f64 {
        self.detour_mm / GRID_50_MIL.pitch()
            + BEND * self.bends as f64
            + CROSSING_COST * self.crossings as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_straight_run_has_no_shape_cost_however_long() {
        let path = [Point2::new(0.0, 0.0), Point2::new(120.0, 0.0)];
        assert_eq!(RouteShape::of(&path, 0).cost(), 0.0);
    }

    #[test]
    fn an_l_is_simple_and_a_z_is_not() {
        let l = [
            Point2::new(0.0, 0.0),
            Point2::new(20.0, 0.0),
            Point2::new(20.0, 10.0),
        ];
        assert!(RouteShape::of(&l, 0).cost() <= SHAPE_SIMPLE);
        let z = [
            Point2::new(0.0, 0.0),
            Point2::new(10.0, 0.0),
            Point2::new(10.0, 10.0),
            Point2::new(20.0, 10.0),
        ];
        let shape = RouteShape::of(&z, 0);
        assert!(shape.cost() > SHAPE_SIMPLE && shape.cost() <= SHAPE_GENERAL);
    }

    #[test]
    fn one_crossing_fits_a_local_hop_and_two_do_not() {
        let straight = [Point2::new(0.0, 0.0), Point2::new(40.0, 0.0)];
        assert!(RouteShape::of(&straight, 1).cost() <= SHAPE_GENERAL);
        assert!(RouteShape::of(&straight, 2).cost() > SHAPE_GENERAL);
    }

    #[test]
    fn a_wraparound_detour_is_charged_in_grid_units() {
        let around = [
            Point2::new(0.0, 0.0),
            Point2::new(0.0, 25.4),
            Point2::new(12.7, 25.4),
            Point2::new(12.7, 0.0),
        ];
        // 63.5 mm drawn, 12.7 mm direct: 50.8 mm of detour = 40 grid, plus two bends.
        assert!((RouteShape::of(&around, 0).cost() - 52.0).abs() < 1e-9);
    }
}
