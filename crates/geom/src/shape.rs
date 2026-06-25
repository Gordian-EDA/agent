//! Cardinal directions shared across schematic placement, wiring, and emit.

use crate::point::Point2;

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
