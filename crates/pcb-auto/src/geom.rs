//! 2-D helpers in KiCad's board frame: millimetres, y grows downward, angles in degrees
//! counter-clockwise on screen.

pub type Point = (f64, f64);

/// Rotate about the origin by `deg` counter-clockwise as seen on screen.
pub fn rotate(p: Point, deg: f64) -> Point {
    if deg == 0.0 {
        return p;
    }
    let a = deg.to_radians();
    let (c, s) = (a.cos(), a.sin());
    (p.0 * c + p.1 * s, -p.0 * s + p.1 * c)
}

pub fn add(a: Point, b: Point) -> Point {
    (a.0 + b.0, a.1 + b.1)
}

pub fn dist(a: Point, b: Point) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// Z of (a - o) x (b - o).
pub fn cross(o: Point, a: Point, b: Point) -> f64 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

pub fn seg_point_foot(a: Point, b: Point, p: Point) -> Point {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let l2 = dx * dx + dy * dy;
    if l2 <= 1e-12 {
        return a;
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / l2).clamp(0.0, 1.0);
    (a.0 + t * dx, a.1 + t * dy)
}

pub fn seg_point_dist(a: Point, b: Point, p: Point) -> f64 {
    let q = seg_point_foot(a, b, p);
    dist(p, q)
}

/// Distance from `p` to the axis-aligned rectangle; 0 inside it.
pub fn point_rect_dist(p: Point, r: &BBox) -> f64 {
    let dx = (r.x0 - p.0).max(0.0).max(p.0 - r.x1);
    let dy = (r.y0 - p.1).max(0.0).max(p.1 - r.y1);
    dx.hypot(dy)
}

pub fn norm_angle(deg: f64) -> f64 {
    let d = deg.rem_euclid(360.0);
    if d > 180.0 { d - 360.0 } else { d }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl BBox {
    pub fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self { x0, y0, x1, y1 }
    }

    pub fn empty() -> Self {
        Self::new(f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY)
    }

    pub fn of_points(pts: impl IntoIterator<Item = Point>) -> Self {
        let mut b = Self::empty();
        for p in pts {
            b.add_point(p);
        }
        b
    }

    pub fn valid(&self) -> bool {
        self.x0 <= self.x1 && self.y0 <= self.y1
    }
    pub fn w(&self) -> f64 {
        self.x1 - self.x0
    }
    pub fn h(&self) -> f64 {
        self.y1 - self.y0
    }
    pub fn center(&self) -> Point {
        ((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0)
    }

    pub fn add_point(&mut self, p: Point) {
        self.x0 = self.x0.min(p.0);
        self.y0 = self.y0.min(p.1);
        self.x1 = self.x1.max(p.0);
        self.y1 = self.y1.max(p.1);
    }

    pub fn add_bbox(&mut self, o: &BBox) {
        if o.valid() {
            self.add_point((o.x0, o.y0));
            self.add_point((o.x1, o.y1));
        }
    }

    pub fn inflate(&self, d: f64) -> BBox {
        BBox::new(self.x0 - d, self.y0 - d, self.x1 + d, self.y1 + d)
    }

    pub fn overlaps(&self, o: &BBox) -> bool {
        self.x0 < o.x1 && o.x0 < self.x1 && self.y0 < o.y1 && o.y0 < self.y1
    }

    pub fn contains(&self, p: Point) -> bool {
        self.x0 <= p.0 && p.0 <= self.x1 && self.y0 <= p.1 && p.1 <= self.y1
    }

    pub fn corners(&self) -> [Point; 4] {
        [
            (self.x0, self.y0),
            (self.x1, self.y0),
            (self.x1, self.y1),
            (self.x0, self.y1),
        ]
    }
}

pub fn box_in_polygon(b: &BBox, poly: &[Point]) -> bool {
    b.corners().iter().all(|c| point_in_polygon(*c, poly))
}

/// Does the segment a-c touch the axis-aligned box? Liang-Barsky slab clip.
pub fn seg_hits_box(a: Point, c: Point, box_: &BBox) -> bool {
    if box_.contains(a) || box_.contains(c) {
        return true;
    }
    let (dx, dy) = (c.0 - a.0, c.1 - a.1);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (num, den) in [
        (a.0 - box_.x0, -dx),
        (box_.x1 - a.0, dx),
        (a.1 - box_.y0, -dy),
        (box_.y1 - a.1, dy),
    ] {
        if den.abs() < 1e-12 {
            if num < 0.0 {
                return false;
            }
        } else if den > 0.0 {
            t1 = t1.min(num / den);
        } else {
            t0 = t0.max(num / den);
        }
        if t0 > t1 {
            return false;
        }
    }
    true
}

/// Polyline approximation of a three-point arc, including both endpoints.
pub fn arc_points(start: Point, mid: Point, end: Point, max_seg: f64) -> Vec<Point> {
    let ((ax, ay), (bx, by), (cx, cy)) = (start, mid, end);
    let d = 2.0 * (ax * (by - cy) + bx * (cy - ay) + cx * (ay - by));
    if d.abs() < 1e-9 {
        return vec![start, end];
    }
    let ux = ((ax * ax + ay * ay) * (by - cy)
        + (bx * bx + by * by) * (cy - ay)
        + (cx * cx + cy * cy) * (ay - by))
        / d;
    let uy = ((ax * ax + ay * ay) * (cx - bx)
        + (bx * bx + by * by) * (ax - cx)
        + (cx * cx + cy * cy) * (bx - ax))
        / d;
    let r = (ax - ux).hypot(ay - uy);
    let a0 = (ay - uy).atan2(ax - ux);
    let am = (by - uy).atan2(bx - ux);
    let a1 = (cy - uy).atan2(cx - ux);
    let tau = std::f64::consts::TAU;
    let ccw = |f: f64, t: f64| (t - f).rem_euclid(tau);
    let span = if ccw(a0, am) <= ccw(a0, a1) {
        ccw(a0, a1)
    } else {
        -ccw(a1, a0)
    };
    let n = ((span.abs() * r / max_seg) as usize + 1).max(2);
    (0..=n)
        .map(|i| {
            let a = a0 + span * i as f64 / n as f64;
            (ux + r * a.cos(), uy + r * a.sin())
        })
        .collect()
}

pub fn circle_points(center: Point, r: f64, n: usize) -> Vec<Point> {
    (0..n)
        .map(|i| {
            let a = std::f64::consts::TAU * i as f64 / n as f64;
            (center.0 + r * a.cos(), center.1 + r * a.sin())
        })
        .collect()
}

pub fn polygon_area(pts: &[Point]) -> f64 {
    let n = pts.len();
    if n < 3 {
        return 0.0;
    }
    (0..n)
        .map(|i| {
            let a = pts[i];
            let b = pts[(i + 1) % n];
            a.0 * b.1 - b.0 * a.1
        })
        .sum::<f64>()
        / 2.0
}

pub fn point_in_polygon(p: Point, pts: &[Point]) -> bool {
    let (x, y) = p;
    let mut inside = false;
    let n = pts.len();
    for i in 0..n {
        let (x0, y0) = pts[i];
        let (x1, y1) = pts[(i + 1) % n];
        if (y0 > y) != (y1 > y) {
            let xi = x0 + (y - y0) * (x1 - x0) / (y1 - y0);
            if x < xi {
                inside = !inside;
            }
        }
    }
    inside
}

/// The part of closed polygon `poly` inside `box_`; empty if they do not meet (Sutherland-Hodgman).
pub fn clip_polygon_rect(poly: &[Point], box_: &BBox) -> Vec<Point> {
    if poly.len() < 3 || !box_.valid() {
        return vec![];
    }
    let mut ring: Vec<Point> = poly.to_vec();
    for (axis, limit, sign) in [
        (0usize, box_.x0, 1.0f64),
        (0, box_.x1, -1.0),
        (1, box_.y0, 1.0),
        (1, box_.y1, -1.0),
    ] {
        if ring.len() < 3 {
            return vec![];
        }
        let at = |p: Point| if axis == 0 { p.0 } else { p.1 };
        let src = std::mem::take(&mut ring);
        let mut prev = *src.last().unwrap();
        let mut prev_in = (at(prev) - limit) * sign >= 0.0;
        for cur in src {
            let cur_in = (at(cur) - limit) * sign >= 0.0;
            if cur_in != prev_in {
                let d = at(cur) - at(prev);
                let t = if d.abs() > 1e-12 {
                    (limit - at(prev)) / d
                } else {
                    0.0
                };
                ring.push((prev.0 + (cur.0 - prev.0) * t, prev.1 + (cur.1 - prev.1) * t));
            }
            if cur_in {
                ring.push(cur);
            }
            prev = cur;
            prev_in = cur_in;
        }
    }
    let n = ring.len();
    let out: Vec<Point> = (0..n)
        .filter(|&i| dist(ring[i], ring[(i + n - 1) % n]) > 1e-9)
        .map(|i| ring[i])
        .collect();
    if out.len() >= 3 { out } else { vec![] }
}

/// Join loose segments end to end into closed or open polylines.
pub fn chain_segments(segments: &[(Point, Point)], tol: f64) -> Vec<Vec<Point>> {
    let mut remaining: Vec<Vec<Point>> = segments.iter().map(|s| vec![s.0, s.1]).collect();
    let mut chains = Vec::new();
    while !remaining.is_empty() {
        let mut chain = remaining.remove(0);
        let mut grew = true;
        while grew {
            grew = false;
            for i in 0..remaining.len() {
                let (a, b) = (remaining[i][0], remaining[i][1]);
                let last = *chain.last().unwrap();
                let first = chain[0];
                if dist(last, a) < tol {
                    chain.push(b);
                } else if dist(last, b) < tol {
                    chain.push(a);
                } else if dist(first, b) < tol {
                    chain.insert(0, a);
                } else if dist(first, a) < tol {
                    chain.insert(0, b);
                } else {
                    continue;
                }
                remaining.remove(i);
                grew = true;
                break;
            }
        }
        chains.push(chain);
    }
    chains
}

/// Cheap inset toward the centroid; good enough for the convex-ish outlines a pour follows.
pub fn inset_polygon(poly: &[Point], d: f64) -> Vec<Point> {
    if poly.is_empty() {
        return vec![];
    }
    let n = poly.len() as f64;
    let cx = poly.iter().map(|p| p.0).sum::<f64>() / n;
    let cy = poly.iter().map(|p| p.1).sum::<f64>() / n;
    poly.iter()
        .map(|&(x, y)| {
            let (vx, vy) = (x - cx, y - cy);
            let l = vx.hypot(vy).max(1e-9);
            (x - vx / l * d, y - vy / l * d)
        })
        .collect()
}

/// Union-find with path compression.
pub struct UnionFind {
    p: Vec<usize>,
}

impl UnionFind {
    pub fn new(n: usize) -> Self {
        Self {
            p: (0..n).collect(),
        }
    }
    pub fn find(&mut self, mut a: usize) -> usize {
        while self.p[a] != a {
            self.p[a] = self.p[self.p[a]];
            a = self.p[a];
        }
        a
    }
    pub fn join(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.p[rb] = ra;
        }
    }
}
