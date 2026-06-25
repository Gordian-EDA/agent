//! Cardinal directions in y-down coordinates.

use crate::point::Point2;

/// One of the four axis directions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    East,
    West,
    North,
    South,
}

impl Dir {
    /// Unit vector, with north as negative y.
    pub fn vec(self) -> Point2 {
        match self {
            Dir::East => Point2::new(1.0, 0.0),
            Dir::West => Point2::new(-1.0, 0.0),
            Dir::North => Point2::new(0.0, -1.0),
            Dir::South => Point2::new(0.0, 1.0),
        }
    }
}
