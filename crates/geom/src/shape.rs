//! Cardinal directions + axis-aligned segment math, shared across schematic
//! placement, wiring, and emit.

use crate::{EPS, point::Point2};

/// A pin's outward direction on the sheet, quantized to the four axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    East,
    West,
    North,
    South,
}

impl Dir {
    /// Sheet-space unit vector (sheet Y grows downward, so North is -y).
    pub fn vec(self) -> Point2 {
        match self {
            Dir::East => Point2::new(1.0, 0.0),
            Dir::West => Point2::new(-1.0, 0.0),
            Dir::North => Point2::new(0.0, -1.0),
            Dir::South => Point2::new(0.0, 1.0),
        }
    }
}

/// Apply a placed instance's symbol→sheet transform to a local offset: optional
/// X-mirror, rotation by `angle` (degrees, CCW in symbol space), then the Y-flip
/// into sheet space. Translation to the instance position is the caller's job.
/// NOTE: y-up symbol input; the loader flip + collapse to `Point2::transform` is Task 8.
pub fn transform_offset(local: Point2, angle: f64, mirror: bool) -> Point2 {
    let (mut x, y) = (local.x, local.y);
    if mirror {
        x = -x;
    }
    let phi = angle.to_radians();
    let (s, c) = phi.sin_cos();
    let rx = x * c - y * s;
    let ry = x * s + y * c;
    Point2::new(rx, -ry)
}

pub fn point_on_segment(p: Point2, a: Point2, b: Point2) -> bool {
    let cross = (p.y - a.y) * (b.x - a.x) - (p.x - a.x) * (b.y - a.y);
    if cross.abs() > EPS {
        return false;
    }
    let dot = (p.x - a.x) * (b.x - a.x) + (p.y - a.y) * (b.y - a.y);
    if dot < -EPS {
        return false;
    }
    let len2 = (b.x - a.x).powi(2) + (b.y - a.y).powi(2);
    dot <= len2 + EPS
}
