//! Points, boxes and a union-find, in sheet millimetres.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// Quantised to 1 µm so float dust never splits a connection node.
    pub fn key(self) -> (i64, i64) {
        (
            (self.x * 1000.0).round() as i64,
            (self.y * 1000.0).round() as i64,
        )
    }

    /// A symbol-space offset in sheet space: KiCad rotates counter-clockwise in
    /// symbol coordinates and flips y on the way out, then mirrors the result.
    pub fn to_sheet_dir(self, rot: f64, mirror: Mirror) -> Point {
        let (s, c) = rot.to_radians().sin_cos();
        let x = self.x * c - self.y * s;
        let y = -(self.x * s + self.y * c);
        match mirror {
            Mirror::None => Point::new(x, y),
            Mirror::X => Point::new(x, -y),
            Mirror::Y => Point::new(-x, y),
        }
    }

    pub fn to_sheet(self, at: Point, rot: f64, mirror: Mirror) -> Point {
        let d = self.to_sheet_dir(rot, mirror);
        Point::new(at.x + d.x, at.y + d.y)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mirror {
    None,
    X,
    Y,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Rect {
    pub fn bounding(points: &[Point]) -> Option<Rect> {
        let first = points.first()?;
        let mut rect = Rect {
            min_x: first.x,
            min_y: first.y,
            max_x: first.x,
            max_y: first.y,
        };
        for point in points {
            rect.min_x = rect.min_x.min(point.x);
            rect.min_y = rect.min_y.min(point.y);
            rect.max_x = rect.max_x.max(point.x);
            rect.max_y = rect.max_y.max(point.y);
        }
        Some(rect)
    }

    /// Overlap with a positive shared area; boxes that merely touch do not.
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.min_x < other.max_x - EPS
            && other.min_x < self.max_x - EPS
            && self.min_y < other.max_y - EPS
            && other.min_y < self.max_y - EPS
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min_x - EPS
            && p.x <= self.max_x + EPS
            && p.y >= self.min_y - EPS
            && p.y <= self.max_y + EPS
    }

    /// Whether the segment `a`–`b` enters the box's interior.
    pub fn crossed_by(&self, a: Point, b: Point) -> bool {
        let shrunk = Rect {
            min_x: self.min_x + EPS,
            min_y: self.min_y + EPS,
            max_x: self.max_x - EPS,
            max_y: self.max_y - EPS,
        };
        if shrunk.min_x >= shrunk.max_x || shrunk.min_y >= shrunk.max_y {
            return false;
        }
        // Liang-Barsky against the shrunk box.
        let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
        let deltas = [
            (-(b.x - a.x), a.x - shrunk.min_x),
            (b.x - a.x, shrunk.max_x - a.x),
            (-(b.y - a.y), a.y - shrunk.min_y),
            (b.y - a.y, shrunk.max_y - a.y),
        ];
        for (p, q) in deltas {
            if p == 0.0 {
                if q < 0.0 {
                    return false;
                }
                continue;
            }
            let t = q / p;
            if p < 0.0 {
                lo = lo.max(t);
            } else {
                hi = hi.min(t);
            }
        }
        lo < hi
    }
}

pub const EPS: f64 = 1e-6;

/// Whether `p` lies on the closed segment `a`–`b`, to within a micron.
pub fn on_segment(p: Point, a: Point, b: Point) -> bool {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let (px, py) = (p.x - a.x, p.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len == 0.0 || (dx * py - dy * px).abs() / len > 0.001 {
        return false;
    }
    let t = (px * dx + py * dy) / (len * len);
    (-1e-9..=1.0 + 1e-9).contains(&t)
}

#[derive(Debug, Clone)]
pub struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    pub fn new(size: usize) -> UnionFind {
        UnionFind {
            parent: (0..size).collect(),
        }
    }

    pub fn find(&mut self, mut node: usize) -> usize {
        while self.parent[node] != node {
            self.parent[node] = self.parent[self.parent[node]];
            node = self.parent[node];
        }
        node
    }

    pub fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[a] = b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_matches_kicad_orientations() {
        let at = Point::new(100.0, 50.0);
        let p = Point::new(0.0, 3.81);
        assert_eq!(p.to_sheet(at, 0.0, Mirror::None), Point::new(100.0, 46.19));
        assert_eq!(
            Point::new(2.54, 0.0).to_sheet(at, 0.0, Mirror::Y),
            Point::new(97.46, 50.0)
        );
        assert_eq!(p.to_sheet(at, 0.0, Mirror::X), Point::new(100.0, 53.81));
        let rotated = p.to_sheet(Point::new(0.0, 0.0), 90.0, Mirror::None);
        assert!((rotated.x + 3.81).abs() < 1e-9 && rotated.y.abs() < 1e-9);
    }

    #[test]
    fn a_segment_along_an_edge_does_not_cross() {
        let rect = Rect {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 2.0,
            max_y: 2.0,
        };
        assert!(rect.crossed_by(Point::new(-1.0, 1.0), Point::new(3.0, 1.0)));
        assert!(!rect.crossed_by(Point::new(-1.0, 0.0), Point::new(3.0, 0.0)));
    }
}
