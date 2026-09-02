//! Orthogonal L/Z routing for the wire a dragged pin has to re-draw.
//!
//! The rules are the ones a human draws by: leave the pin along its own
//! direction, turn at most twice, never cross a part body, never run on top of
//! another wire, and never pass through a connection point that belongs to
//! somebody else's net — that last one is what makes the drag truthful rather
//! than merely tidy.

use std::collections::HashMap;

use geom::{Point2, Segment};

use crate::sheet::Sheet;

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
    /// Bodies bucketed on a coarse lattice, so a point test looks at the few
    /// that could possibly contain it rather than at all of them.
    buckets: HashMap<(i64, i64), Vec<usize>>,
}

/// Side of one obstacle bucket, in mm.
const BUCKET: f64 = 8.0;

fn bucket_of(p: Point2) -> (i64, i64) {
    ((p.x / BUCKET).floor() as i64, (p.y / BUCKET).floor() as i64)
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
    /// Index everything the router must avoid.
    pub fn new(sheet: &'a Sheet) -> Obstacles<'a> {
        let mut horizontal: HashMap<i64, Vec<(f64, f64, &str)>> = HashMap::new();
        let mut vertical: HashMap<i64, Vec<(f64, f64, &str)>> = HashMap::new();
        let mut live_nodes: Vec<(Point2, &str)> = Vec::new();
        for seg in &sheet.wires {
            if seg.horizontal() {
                horizontal.entry(q(seg.a.y)).or_default().push((
                    seg.a.x,
                    seg.b.x,
                    seg.net.as_str(),
                ));
            } else {
                // A wire drawn off-axis still blocks its own line; treating it
                // as vertical keeps it visible to the span test either way.
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

        // Every body, power rail glyphs included: a wire ruled through a GND
        // symbol reads exactly as badly as one ruled through an IC. Text is an
        // obstacle too, or the router keeps buying collisions the score pays for.
        let bodies = sheet
            .bodies
            .iter()
            .map(|b| b.rect.inflate(-0.05))
            .chain(sheet.texts.iter().map(|t| t.inflate(-0.05)))
            .collect();

        let mut buckets: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (index, rect) in (&bodies as &Vec<geom::Rect>).iter().enumerate() {
            let (lo, hi) = (
                bucket_of(Point2::new(rect.min_x - GRID, rect.min_y - GRID)),
                bucket_of(Point2::new(rect.max_x + GRID, rect.max_y + GRID)),
            );
            for bx in lo.0..=hi.0 {
                for by in lo.1..=hi.1 {
                    buckets.entry((bx, by)).or_default().push(index);
                }
            }
        }

        Obstacles {
            sheet,
            horizontal,
            vertical,
            nodes_by_y,
            nodes_by_x,
            bodies,
            buckets,
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

    /// Whether a point sits inside a body or a text run, allowing half a grid
    /// step of clearance — the cell test a lattice search needs.
    pub(crate) fn blocks_point(&self, p: Point2) -> bool {
        self.buckets.get(&bucket_of(p)).is_some_and(|near| {
            near.iter()
                .any(|i| self.bodies[*i].inflate(GRID / 2.0 - 0.05).contains(p))
        })
    }

    pub(crate) fn hits_body(&self, a: Point2, b: Point2) -> bool {
        let seg = Segment::new(a, b);
        self.bodies
            .iter()
            .any(|r| seg.axis_aligned_hits_rect_interior(r))
    }

    /// Whether a segment runs along another wire, or through a connection point
    /// of a foreign net.
    pub(crate) fn conflicts(&self, a: Point2, b: Point2, net: &str) -> bool {
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
    pub(crate) fn corner_blocked(&self, p: Point2, net: &str) -> bool {
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

    /// Whether a whole path is legal — the check the label fallback needs too.
    pub fn path_ok(&self, path: &[Point2], net: &str) -> bool {
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

/// Whether a coordinate sits on [`GRID`].
fn on_grid(v: f64) -> bool {
    (v - snap(v)).abs() < 1e-6
}

/// What the router pays for a path: its length, its turns, a corner that misses
/// the grid, and leaving the pin sideways instead of the way the pin faces.
///
/// The grid term matters where a symbol puts its pin tip off the 50-mil lattice:
/// an L drawn straight from such a pin carries the offset into its corner, which
/// KiCAD reports as an off-grid endpoint. Stepping onto the grid first costs a
/// couple of millimetres and reads the way a person draws it.
pub fn path_cost(path: &Path, out_dir: Point2) -> f64 {
    let length: f64 = path.windows(2).map(|w| w[0].manhattan(w[1])).sum();
    let bends = path.len().saturating_sub(2) as f64;
    let off_grid = path[1..path.len().saturating_sub(1)]
        .iter()
        .filter(|c| !on_grid(c.x) || !on_grid(c.y))
        .count() as f64;
    let leaves_along_pin = path.len() > 1 && {
        let d = Point2::new(path[1].x - path[0].x, path[1].y - path[0].y);
        d.x * out_dir.x + d.y * out_dir.y > geom::EPS
    };
    length + 6.0 * bends + 20.0 * off_grid + if leaves_along_pin { 0.0 } else { 12.0 }
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

/// A lattice search for a path the L/Z candidates cannot see.
///
/// The enumerated shapes never leave the box spanned by the two ends, so on a
/// crowded sheet — where the only free channel runs *around* a part — they all
/// fail and the caller is forced into a label. This finds the detour: a
/// bend-penalised shortest path on the [`GRID`] lattice through `from`, inside
/// a window generous enough to go around a part and tight enough to stay cheap.
pub fn maze(
    obstacles: &Obstacles,
    pin: Point2,
    out_dir: Point2,
    to: Point2,
    net: &str,
) -> Option<Path> {
    let from = pin;
    // The lattice is the sheet's own grid, not one hung off the pin: a symbol
    // whose pin tip misses the 50-mil grid would otherwise put every corner of
    // every route off it too, which is what KiCAD reports as an off-grid end.
    let origin = snap_point(from);
    let escape = !origin.near_eq(from, 1e-6);
    if escape && (origin.x - from.x).abs() > 1e-6 && (origin.y - from.y).abs() > 1e-6 {
        return None;
    }
    let from = origin;
    let steps = |v: f64| (v / GRID).round() as i32;
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    if (dx - steps(dx) as f64 * GRID).abs() > 1e-6 || (dy - steps(dy) as f64 * GRID).abs() > 1e-6 {
        return None;
    }
    const MARGIN: i32 = 10;
    let (goal_x, goal_y) = (steps(dx), steps(dy));
    let (lo_x, hi_x) = (goal_x.min(0) - MARGIN, goal_x.max(0) + MARGIN);
    let (lo_y, hi_y) = (goal_y.min(0) - MARGIN, goal_y.max(0) + MARGIN);
    let (width, height) = ((hi_x - lo_x + 1) as usize, (hi_y - lo_y + 1) as usize);
    let point = |x: i32, y: i32| Point2::new(from.x + x as f64 * GRID, from.y + y as f64 * GRID);
    let cell_of = |x: i32, y: i32| (y - lo_y) as usize * width + (x - lo_x) as usize;
    let index = |x: i32, y: i32, d: usize| cell_of(x, y) * 4 + d;

    // Testing every step against every body and every text run is what makes a
    // lattice search expensive; testing every *cell* once, against rects grown
    // by half a step, is the same answer for two orders of magnitude less work.
    let mut solid: Vec<bool> = Vec::with_capacity(width * height);
    for y in lo_y..=hi_y {
        for x in lo_x..=hi_x {
            solid.push(obstacles.blocks_point(point(x, y)));
        }
    }
    solid[cell_of(0, 0)] = false;
    solid[cell_of(goal_x, goal_y)] = false;

    const DIRS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
    let bend = 6.0;
    let mut best = vec![f64::INFINITY; width * height * 4];
    let mut from_state: Vec<u32> = vec![u32::MAX; width * height * 4];
    let mut queue: std::collections::BinaryHeap<(std::cmp::Reverse<u64>, u32)> = Default::default();
    let encode = |cost: f64| std::cmp::Reverse((cost * 64.0) as u64);

    for (d, (sx, sy)) in DIRS.iter().enumerate() {
        let straight = *sx as f64 * out_dir.x + *sy as f64 * out_dir.y > 0.5;
        let cost = if straight { 0.0 } else { bend * 2.0 };
        let state = index(0, 0, d) as u32;
        best[state as usize] = cost;
        queue.push((encode(cost), state));
    }

    let mut goal = None;
    while let Some((std::cmp::Reverse(raw), state)) = queue.pop() {
        let cost = raw as f64 / 64.0;
        if cost > best[state as usize] + 1e-9 {
            continue;
        }
        let d = state as usize % 4;
        let cell = state as usize / 4;
        let (x, y) = ((cell % width) as i32 + lo_x, (cell / width) as i32 + lo_y);
        if (x, y) == (goal_x, goal_y) {
            goal = Some(state);
            break;
        }
        for (nd, (sx, sy)) in DIRS.iter().enumerate() {
            let (nx, ny) = (x + sx, y + sy);
            if nx < lo_x || nx > hi_x || ny < lo_y || ny > hi_y {
                continue;
            }
            if solid[cell_of(nx, ny)] {
                continue;
            }
            let (a, b) = (point(x, y), point(nx, ny));
            if obstacles.conflicts(a, b, net) {
                continue;
            }
            // Only a cell the path actually turns at reads as a connection;
            // one it runs straight through is just a crossing.
            if nd != d && obstacles.corner_blocked(point(x, y), net) {
                continue;
            }
            let next = cost + GRID + if nd == d { 0.0 } else { bend };
            let slot = index(nx, ny, nd);
            if next + 1e-9 < best[slot] {
                best[slot] = next;
                from_state[slot] = state;
                queue.push((encode(next), slot as u32));
            }
        }
    }

    let mut state = goal?;
    let mut cells = Vec::new();
    loop {
        let cell = state as usize / 4;
        cells.push(point(
            (cell % width) as i32 + lo_x,
            (cell / width) as i32 + lo_y,
        ));
        let previous = from_state[state as usize];
        if previous == u32::MAX {
            break;
        }
        state = previous;
    }
    cells.reverse();
    if escape {
        cells.insert(0, pin);
    }
    Some(dedup(cells))
}
