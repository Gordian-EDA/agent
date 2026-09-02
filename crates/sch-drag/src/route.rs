//! Orthogonal L/Z routing for the wire a dragged pin has to re-draw.
//!
//! The rules are the ones a human draws by: leave the pin along its own
//! direction, turn at most twice, never cross a part body, never run on top of
//! another wire, and never pass through a connection point that belongs to
//! somebody else's net — that last one is what makes the drag truthful rather
//! than merely tidy.

use std::collections::HashMap;

use geom::{Point2, Segment};

use crate::sheet::{Sheet, key};

/// KiCAD's schematic grid. Every point this module invents lands on it.
pub const GRID: f64 = 1.27;

/// Snap to [`GRID`].
pub fn snap(v: f64) -> f64 {
    (v / GRID).round() * GRID
}

/// Snap a point to [`GRID`].
pub fn snap_point(p: Point2) -> Point2 {
    Point2::new(snap(p.x), snap(p.y))
}

/// The sheet as the router must respect it: what is already drawn, minus the
/// wires this drag is about to delete.
pub struct Obstacles<'a> {
    sheet: &'a Sheet,
    /// Horizontal segments by quantised y, as `(x0, x1, net)`.
    horizontal: HashMap<i64, Vec<(f64, f64, &'a str)>>,
    /// Vertical segments by quantised x.
    vertical: HashMap<i64, Vec<(f64, f64, &'a str)>>,
    /// Connection points by quantised y and by quantised x.
    nodes_by_y: HashMap<i64, Vec<(f64, &'a str)>>,
    nodes_by_x: HashMap<i64, Vec<(f64, &'a str)>>,
    bodies: Vec<geom::Rect>,
}

fn q(v: f64) -> i64 {
    (v * 1000.0).round() as i64
}

fn between(v: f64, a: f64, b: f64) -> bool {
    v > a.min(b) - geom::EPS && v < a.max(b) + geom::EPS
}

fn strictly_between(v: f64, a: f64, b: f64) -> bool {
    v > a.min(b) + geom::EPS && v < a.max(b) - geom::EPS
}

impl<'a> Obstacles<'a> {
    /// Index everything the router must avoid. `removed` names wire UUIDs the
    /// drag has retracted, which are no longer scenery.
    pub fn new(sheet: &'a Sheet, removed: &[String]) -> Obstacles<'a> {
        let mut horizontal: HashMap<i64, Vec<(f64, f64, &str)>> = HashMap::new();
        let mut vertical: HashMap<i64, Vec<(f64, f64, &str)>> = HashMap::new();
        let mut live_nodes: Vec<(Point2, &str)> = Vec::new();
        for seg in &sheet.wires {
            if removed.contains(&seg.uuid) {
                continue;
            }
            if seg.horizontal() {
                horizontal.entry(q(seg.a.y)).or_default().push((
                    seg.a.x,
                    seg.b.x,
                    seg.net.as_str(),
                ));
            } else if seg.vertical() {
                vertical
                    .entry(q(seg.a.x))
                    .or_default()
                    .push((seg.a.y, seg.b.y, seg.net.as_str()));
            }
            live_nodes.push((seg.a, seg.net.as_str()));
            live_nodes.push((seg.b, seg.net.as_str()));
        }
        // A pin is a connection point whether or not a wire reaches it.
        for pin in &sheet.pins {
            if let Some(net) = sheet.net_at(pin.at) {
                live_nodes.push((pin.at, net));
            }
        }
        for (node, net) in sheet.node_net.iter() {
            if sheet.fixtures.contains(node) {
                live_nodes.push((
                    Point2::new(node.0 as f64 / 1000.0, node.1 as f64 / 1000.0),
                    net.as_str(),
                ));
            }
        }

        let mut nodes_by_y: HashMap<i64, Vec<(f64, &str)>> = HashMap::new();
        let mut nodes_by_x: HashMap<i64, Vec<(f64, &str)>> = HashMap::new();
        for (p, net) in live_nodes {
            nodes_by_y.entry(q(p.y)).or_default().push((p.x, net));
            nodes_by_x.entry(q(p.x)).or_default().push((p.y, net));
        }

        let bodies = sheet.part_bodies().map(|b| b.rect.inflate(-0.05)).collect();

        Obstacles {
            sheet,
            horizontal,
            vertical,
            nodes_by_y,
            nodes_by_x,
            bodies,
        }
    }

    /// Register a path this drag has just drawn, so the next pin routes around it.
    pub fn add_path(&mut self, path: &[Point2], net: &'a str) {
        for pair in path.windows(2) {
            if (pair[0].y - pair[1].y).abs() < geom::EPS {
                self.horizontal
                    .entry(q(pair[0].y))
                    .or_default()
                    .push((pair[0].x, pair[1].x, net));
            } else {
                self.vertical
                    .entry(q(pair[0].x))
                    .or_default()
                    .push((pair[0].y, pair[1].y, net));
            }
        }
        for p in path {
            self.nodes_by_y.entry(q(p.y)).or_default().push((p.x, net));
            self.nodes_by_x.entry(q(p.x)).or_default().push((p.y, net));
        }
    }

    fn hits_body(&self, a: Point2, b: Point2) -> bool {
        let seg = Segment::new(a, b);
        self.bodies
            .iter()
            .any(|r| seg.axis_aligned_hits_rect_interior(r))
    }

    /// Whether a segment runs along another wire, or through a connection point
    /// of a foreign net.
    fn conflicts(&self, a: Point2, b: Point2, net: &str) -> bool {
        let horizontal = (a.y - b.y).abs() < geom::EPS;
        let (lane, others) = if horizontal {
            (q(a.y), &self.horizontal)
        } else {
            (q(a.x), &self.vertical)
        };
        let (p0, p1) = if horizontal { (a.x, b.x) } else { (a.y, b.y) };
        if let Some(list) = others.get(&lane)
            && list.iter().any(|(s0, s1, _)| {
                p0.min(p1) < s0.max(*s1) - geom::EPS && s0.min(*s1) < p0.max(p1) - geom::EPS
            })
        {
            return true;
        }
        let nodes = if horizontal {
            self.nodes_by_y.get(&lane)
        } else {
            self.nodes_by_x.get(&lane)
        };
        if let Some(nodes) = nodes
            && nodes
                .iter()
                .any(|(v, n)| *n != net && strictly_between(*v, p0, p1))
        {
            return true;
        }
        false
    }

    /// Whether a corner would land on a foreign net's wire or point, which
    /// reads as a connection that is not one.
    fn corner_blocked(&self, p: Point2, net: &str) -> bool {
        if let Some(n) = self.sheet.net_at(p)
            && n != net
        {
            return true;
        }
        let on_foreign = |list: Option<&Vec<(f64, f64, &str)>>, v: f64| {
            list.is_some_and(|l| {
                l.iter()
                    .any(|(s0, s1, n)| *n != net && between(v, *s0, *s1))
            })
        };
        on_foreign(self.horizontal.get(&q(p.y)), p.x) || on_foreign(self.vertical.get(&q(p.x)), p.y)
    }

    fn path_ok(&self, path: &[Point2], net: &str) -> bool {
        for pair in path.windows(2) {
            if pair[0].near_eq(pair[1], geom::EPS) {
                continue;
            }
            let orthogonal = (pair[0].x - pair[1].x).abs() < geom::EPS
                || (pair[0].y - pair[1].y).abs() < geom::EPS;
            if !orthogonal
                || self.hits_body(pair[0], pair[1])
                || self.conflicts(pair[0], pair[1], net)
            {
                return false;
            }
        }
        path[1..path.len() - 1]
            .iter()
            .all(|corner| !self.corner_blocked(*corner, net))
    }
}

/// The path a re-drawn connection takes, as corner points from pin to target.
pub type Path = Vec<Point2>;

fn dedup(mut path: Path) -> Path {
    path.dedup_by(|a, b| a.near_eq(*b, geom::EPS));
    // Drop a corner that does not turn.
    let mut out: Path = Vec::with_capacity(path.len());
    for p in path {
        if out.len() >= 2 {
            let (a, b) = (out[out.len() - 2], out[out.len() - 1]);
            let straight = ((a.x - b.x).abs() < geom::EPS && (b.x - p.x).abs() < geom::EPS)
                || ((a.y - b.y).abs() < geom::EPS && (b.y - p.y).abs() < geom::EPS);
            if straight {
                out.pop();
            }
        }
        out.push(p);
    }
    out
}

/// What the router pays for a path: its length, its turns, and a penalty for
/// leaving the pin sideways instead of the way the pin faces.
pub fn path_cost(path: &Path, out_dir: Point2) -> f64 {
    let length: f64 = path.windows(2).map(|w| w[0].manhattan(w[1])).sum();
    let bends = path.len().saturating_sub(2) as f64;
    let leaves_along_pin = path.len() > 1 && {
        let d = Point2::new(path[1].x - path[0].x, path[1].y - path[0].y);
        d.x * out_dir.x + d.y * out_dir.y > geom::EPS
    };
    length + 6.0 * bends + if leaves_along_pin { 0.0 } else { 12.0 }
}

/// Route from a pin to a point on its own net.
///
/// Candidates are the straight run, both L shapes and the Z shapes that turn on
/// a grid line between the two ends; the cheapest legal one wins, where cheap
/// means short, few bends, and leaving the pin the way the pin faces.
pub fn route(
    obstacles: &Obstacles,
    from: Point2,
    out_dir: Point2,
    to: Point2,
    net: &str,
) -> Option<Path> {
    let mut candidates: Vec<Path> = Vec::new();
    candidates.push(vec![from, to]);
    candidates.push(vec![from, Point2::new(to.x, from.y), to]);
    candidates.push(vec![from, Point2::new(from.x, to.y), to]);

    let steps = 12;
    for i in 1..steps {
        let t = i as f64 / steps as f64;
        let x = snap(from.x + (to.x - from.x) * t);
        let y = snap(from.y + (to.y - from.y) * t);
        candidates.push(vec![from, Point2::new(x, from.y), Point2::new(x, to.y), to]);
        candidates.push(vec![from, Point2::new(from.x, y), Point2::new(to.x, y), to]);
    }
    // Detours that first step off the pin along its own direction, for a target
    // that sits behind the body.
    for reach in [2.54_f64, 5.08, 7.62] {
        let stub = Point2::new(
            snap(from.x + out_dir.x * reach),
            snap(from.y + out_dir.y * reach),
        );
        candidates.push(vec![stub, Point2::new(to.x, stub.y), to]);
        candidates.push(vec![stub, Point2::new(stub.x, to.y), to]);
        for c in candidates.len() - 2..candidates.len() {
            candidates[c].insert(0, from);
        }
    }

    candidates
        .into_iter()
        .map(dedup)
        .filter(|path| path.len() >= 2 && obstacles.path_ok(path, net))
        .min_by(|a, b| {
            path_cost(a, out_dir)
                .partial_cmp(&path_cost(b, out_dir))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Whether a point already carries a connection node.
pub fn is_node(sheet: &Sheet, p: Point2) -> bool {
    sheet.node_net.contains_key(&key(p))
}
