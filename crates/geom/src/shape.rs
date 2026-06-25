//! Cardinal directions + axis-aligned segment math, shared across schematic
//! placement, wiring, and emit.

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
    pub fn vec(self) -> [f64; 2] {
        match self {
            Dir::East => [1.0, 0.0],
            Dir::West => [-1.0, 0.0],
            Dir::North => [0.0, -1.0],
            Dir::South => [0.0, 1.0],
        }
    }
}

/// Apply a placed instance's symbol→sheet transform to a local offset: optional
/// X-mirror, rotation by `angle` (degrees, CCW in symbol space), then the Y-flip
/// into sheet space. Translation to the instance position is the caller's job.
pub fn transform_offset(local: [f64; 2], angle: f64, mirror: bool) -> [f64; 2] {
    let (mut x, y) = (local[0], local[1]);
    if mirror {
        x = -x;
    }
    let phi = angle.to_radians();
    let (s, c) = phi.sin_cos();
    let rx = x * c - y * s;
    let ry = x * s + y * c;
    [rx, -ry]
}

/// Whether point `p` lies on the axis-aligned segment `a`–`b` (endpoints
/// included), within grid-snap floating-point dust.
///
/// All stub/power wires are horizontal or vertical, so the test reduces to: `p`
/// is collinear with the segment's constant axis and within its varying-axis
/// span. Endpoints count as "on". A non-axis-aligned segment (should not occur
/// for our wires) falls back to a collinearity + bounding-box test.
pub fn point_on_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> bool {
    const EPS: f64 = 1e-6;
    let within = |v: f64, lo: f64, hi: f64| v >= lo - EPS && v <= hi + EPS;
    if (a[0] - b[0]).abs() < EPS {
        // Vertical segment: x constant.
        (p[0] - a[0]).abs() < EPS && within(p[1], a[1].min(b[1]), a[1].max(b[1]))
    } else if (a[1] - b[1]).abs() < EPS {
        // Horizontal segment: y constant.
        (p[1] - a[1]).abs() < EPS && within(p[0], a[0].min(b[0]), a[0].max(b[0]))
    } else {
        let cross = (p[0] - a[0]) * (b[1] - a[1]) - (p[1] - a[1]) * (b[0] - a[0]);
        cross.abs() < EPS
            && within(p[0], a[0].min(b[0]), a[0].max(b[0]))
            && within(p[1], a[1].min(b[1]), a[1].max(b[1]))
    }
}
