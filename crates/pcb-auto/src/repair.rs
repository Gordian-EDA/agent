//! Wire a stranded piece of ground pour back to the plane.
//!
//! Freerouting routes the netlist, not the pour, and the copper it lays cuts the plane into
//! islands. [`crate::stitch`] ties an island to the other layer with a via wherever one fits; what
//! is left are crumbs wedged between tracks with no room for a barrel. Those get a track instead:
//! a Dijkstra search on a millimetre-fraction lattice over both copper layers, from the crumb to
//! the plane, keeping every other net's clearance and paying for a layer change.

use std::collections::BinaryHeap;
use std::path::Path;

use kicad::KicadInstallation;

use crate::geom::{point_in_polygon, polygon_area, BBox, Point, UnionFind};
use crate::model::{Board, Rules};
use crate::stitch::{filled_islands, Island};

/// Lattice pitch. Fine enough to thread a gap the router left, coarse enough that a 30 x 55 mm
/// board is ~50 k cells per layer and the search is instant.
const GRID_MM: f64 = 0.2;
/// What a layer change costs, in lattice steps: enough that a path prefers to go round.
const VIA_COST: u32 = 12;
/// A repair that needs to cross the whole board is a placement problem, not a pour problem.
const MAX_STEPS: u32 = 4000;

struct Grid {
    origin: Point,
    nx: usize,
    ny: usize,
    layers: Vec<String>,
    /// `blocked[layer][y * nx + x]`: another net's copper, or too near the board edge.
    blocked: Vec<Vec<bool>>,
    /// Cells where a via of the board's barrel cannot sit: it needs more room than a track, and
    /// its drill owes every other hole a hole-to-hole gap.
    via_blocked: Vec<bool>,
}

impl Grid {
    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.nx + x
    }
    fn point(&self, x: usize, y: usize) -> Point {
        (
            self.origin.0 + x as f64 * GRID_MM,
            self.origin.1 + y as f64 * GRID_MM,
        )
    }
    fn cell(&self, p: Point) -> Option<(usize, usize)> {
        let x = ((p.0 - self.origin.0) / GRID_MM).round();
        let y = ((p.1 - self.origin.1) / GRID_MM).round();
        (x >= 0.0 && y >= 0.0 && (x as usize) < self.nx && (y as usize) < self.ny)
            .then_some((x as usize, y as usize))
    }
    fn node(&self, l: usize, x: usize, y: usize) -> usize {
        l * self.nx * self.ny + self.idx(x, y)
    }
}

/// Everything the repair track must keep away from, rasterised once.
fn build_grid(board: &Board, gnd: i64, rules: &Rules, width: f64) -> Option<Grid> {
    let outline = board.outline_polygon()?;
    let bb = BBox::of_points(outline.iter().copied());
    let layers = board.copper_layers();
    let nx = (bb.w() / GRID_MM).ceil() as usize + 1;
    let ny = (bb.h() / GRID_MM).ceil() as usize + 1;
    let mut grid = Grid {
        origin: (bb.x0, bb.y0),
        nx,
        ny,
        blocked: vec![vec![false; nx * ny]; layers.len()],
        via_blocked: vec![false; nx * ny],
        layers,
    };
    // the board edge owes the track its edge clearance, measured from the track's own flank
    let edge = crate::geom::inset_polygon(&outline, rules.edge_clearance + width / 2.0);
    for y in 0..ny {
        for x in 0..nx {
            if !point_in_polygon(grid.point(x, y), &edge) {
                let i = grid.idx(x, y);
                for l in 0..grid.blocked.len() {
                    grid.blocked[l][i] = true;
                }
            }
        }
    }
    let keep = rules.clearance + width / 2.0;
    /// `layers` empty means "the via plane", which spans the whole stack anyway.
    fn block(grid: &mut Grid, area: BBox, layers: &[usize], hit: &dyn Fn(Point) -> bool) {
        let Some((x0, y0)) = grid.cell((area.x0, area.y0)) else {
            return;
        };
        let (x1, y1) = grid
            .cell((area.x1, area.y1))
            .unwrap_or((grid.nx - 1, grid.ny - 1));
        for y in y0..=y1.min(grid.ny - 1) {
            for x in x0..=x1.min(grid.nx - 1) {
                if hit(grid.point(x, y)) {
                    let i = grid.idx(x, y);
                    if layers.is_empty() {
                        grid.via_blocked[i] = true;
                    }
                    for &l in layers {
                        grid.blocked[l][i] = true;
                    }
                }
            }
        }
    }
    let names = grid.layers.clone();
    let index_of = |name: &str| names.iter().position(|l| l == name);
    let all: Vec<usize> = (0..names.len()).collect();

    for t in board.tracks() {
        if t.net_id == gnd {
            continue;
        }
        let Some(l) = index_of(&t.layer) else { continue };
        let r = keep + t.width / 2.0;
        let (a, b) = (t.start, t.end);
        let area = BBox::of_points([a, b]).inflate(r);
        block(&mut grid, area, &[l], &move |p| {
            crate::geom::seg_point_dist(a, b, p) <= r
        });
    }
    for v in board.vias() {
        if v.net_id == gnd {
            continue;
        }
        let r = keep + v.size / 2.0;
        let c = v.pos;
        block(&mut grid, BBox::new(c.0, c.1, c.0, c.1).inflate(r), &all, &move |p| {
            crate::geom::dist(c, p) <= r
        });
    }
    for f in board.footprints() {
        for pad in &f.pads {
            if pad.net_id == gnd {
                continue;
            }
            let bx = pad.bbox().inflate(keep);
            let lays: Vec<usize> = if pad.is_through() {
                all.clone()
            } else {
                pad.copper_layers().iter().filter_map(|l| index_of(l)).collect()
            };
            if lays.is_empty() {
                continue;
            }
            block(&mut grid, bx, &lays, &move |p| bx.contains(p));
        }
    }
    // A via needs more room than a track and drills a hole: mark where one may not go, so a
    // layer change is only ever offered somewhere the barrel is actually legal.
    let via_keep = rules.clearance.max(rules.hole_clearance) + rules.via_size / 2.0;
    let hole_keep = rules.hole_clearance + rules.via_drill / 2.0;
    for y in 0..grid.ny {
        for x in 0..grid.nx {
            let i = grid.idx(x, y);
            if grid.blocked.iter().any(|l| l[i]) {
                grid.via_blocked[i] = true;
            }
        }
    }
    for t in board.tracks() {
        if t.net_id == gnd {
            continue;
        }
        let r = via_keep + t.width / 2.0;
        let (a, b) = (t.start, t.end);
        block(&mut grid, BBox::of_points([a, b]).inflate(r), &[], &move |p| {
            crate::geom::seg_point_dist(a, b, p) <= r
        });
    }
    for v in board.vias() {
        let c = v.pos;
        let r = (via_keep + v.size / 2.0).max(hole_keep + v.drill / 2.0);
        block(&mut grid, BBox::new(c.0, c.1, c.0, c.1).inflate(r), &[], &move |p| {
            crate::geom::dist(c, p) <= r
        });
    }
    for f in board.footprints() {
        for pad in &f.pads {
            let drill = pad.drill.unwrap_or(0.0);
            if pad.net_id == gnd && drill <= 0.0 {
                continue;
            }
            let r = if pad.net_id == gnd { hole_keep + drill / 2.0 } else { via_keep };
            let bx = pad.bbox().inflate(r);
            block(&mut grid, bx, &[], &move |p| bx.contains(p));
        }
    }
    Some(grid)
}

#[derive(PartialEq, Eq)]
struct Step(u32, usize);
impl Ord for Step {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        o.0.cmp(&self.0) // min-heap
    }
}
impl PartialOrd for Step {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

/// Shortest path from any of `sources` to any of `targets`, over both layers.
fn search(grid: &Grid, sources: &[usize], targets: &[bool]) -> Option<Vec<usize>> {
    let n = grid.nx * grid.ny * grid.layers.len();
    let mut cost = vec![u32::MAX; n];
    let mut from = vec![usize::MAX; n];
    let mut heap = BinaryHeap::new();
    for &s in sources {
        if cost[s] != 0 {
            cost[s] = 0;
            heap.push(Step(0, s));
        }
    }
    let plane = grid.nx * grid.ny;
    while let Some(Step(c, node)) = heap.pop() {
        if c > cost[node] || c > MAX_STEPS {
            continue;
        }
        if targets[node] {
            let mut path = vec![node];
            let mut cur = node;
            while from[cur] != usize::MAX {
                cur = from[cur];
                path.push(cur);
            }
            path.reverse();
            return Some(path);
        }
        let l = node / plane;
        let cell = node % plane;
        let (x, y) = (cell % grid.nx, cell / grid.nx);
        let relax = |to: usize, step: u32, heap: &mut BinaryHeap<Step>, cost: &mut Vec<u32>, from: &mut Vec<usize>| {
            if !grid.blocked[to / plane][to % plane] && c + step < cost[to] {
                cost[to] = c + step;
                from[to] = node;
                heap.push(Step(c + step, to));
            }
        };
        if x > 0 {
            relax(grid.node(l, x - 1, y), 10, &mut heap, &mut cost, &mut from);
        }
        if x + 1 < grid.nx {
            relax(grid.node(l, x + 1, y), 10, &mut heap, &mut cost, &mut from);
        }
        if y > 0 {
            relax(grid.node(l, x, y - 1), 10, &mut heap, &mut cost, &mut from);
        }
        if y + 1 < grid.ny {
            relax(grid.node(l, x, y + 1), 10, &mut heap, &mut cost, &mut from);
        }
        for other in 0..grid.layers.len() {
            if other != l && !grid.via_blocked[cell] {
                relax(grid.node(other, x, y), VIA_COST * 10, &mut heap, &mut cost, &mut from);
            }
        }
    }
    None
}

/// Which island each pour piece belongs to, as a component id per island.
fn components(board: &Board, islands: &[Island], gnd: i64) -> (Vec<usize>, Option<usize>) {
    let mut uf = UnionFind::new(islands.len());
    let hits = |p: Point, layer: Option<&str>| -> Vec<usize> {
        islands
            .iter()
            .enumerate()
            .filter(|(_, i)| layer.is_none_or(|l| i.layer == l) && point_in_polygon(p, &i.poly))
            .map(|(k, _)| k)
            .collect()
    };
    let mut ties: Vec<Point> = board
        .vias()
        .iter()
        .filter(|v| v.net_id == gnd)
        .map(|v| v.pos)
        .collect();
    for f in board.footprints() {
        for p in &f.pads {
            if p.net_id == gnd && p.is_through() {
                ties.push(p.pos);
            }
        }
    }
    for t in &ties {
        let h = hits(*t, None);
        for w in h.windows(2) {
            uf.join(w[0], w[1]);
        }
    }
    // see the note in `stitch`: only a via or a plated through-hole counts as a join
    let main = (0..islands.len())
        .max_by(|a, b| {
            polygon_area(&islands[*a].poly)
                .abs()
                .partial_cmp(&polygon_area(&islands[*b].poly).abs())
                .unwrap()
        })
        .map(|k| uf.find(k));
    ((0..islands.len()).map(|i| uf.find(i)).collect(), main)
}

/// Route every stranded piece of the ground pour back to the plane. Returns the pieces joined.
pub fn repair_pour(
    kicad: &KicadInstallation,
    board: &mut Board,
    pcb: &Path,
    net: &str,
    rules: &Rules,
) -> anyhow::Result<usize> {
    let islands = filled_islands(kicad, pcb, net)?;
    repair_islands(board, &islands, net, rules)
}

/// The same repair against a fill somebody else already paid for. A refill costs several seconds
/// of `kicad-cli`, so the stitcher and the router share one.
pub fn repair_islands(
    board: &mut Board,
    islands: &[Island],
    net: &str,
    rules: &Rules,
) -> anyhow::Result<usize> {
    let Some(gnd) = board.net_by_name(net) else {
        return Ok(0);
    };
    if islands.len() < 2 {
        return Ok(0);
    }
    let (comp, main) = components(board, islands, gnd.id);
    let Some(main) = main else { return Ok(0) };
    let stranded: Vec<usize> = (0..islands.len()).filter(|&i| comp[i] != main).collect();
    if stranded.is_empty() {
        return Ok(0);
    }
    let width = crate::rules::signal_track_width(rules, None);
    let Some(grid) = build_grid(board, gnd.id, rules, width) else {
        return Ok(0);
    };
    let plane = grid.nx * grid.ny;
    let names = grid.layers.clone();
    let layer_index = |name: &str| names.iter().position(|l| l == name);

    // every cell the plane already covers is somewhere a repair may end
    let mut targets = vec![false; plane * grid.layers.len()];
    for (i, isl) in islands.iter().enumerate() {
        if comp[i] != main {
            continue;
        }
        let Some(l) = layer_index(&isl.layer) else { continue };
        for y in 0..grid.ny {
            for x in 0..grid.nx {
                if point_in_polygon(grid.point(x, y), &isl.poly) {
                    targets[grid.node(l, x, y)] = true;
                }
            }
        }
    }

    let mut joined = 0usize;
    let mut done: Vec<usize> = Vec::new();
    for i in stranded {
        if done.contains(&comp[i]) {
            continue;
        }
        let isl = &islands[i];
        let Some(l) = layer_index(&isl.layer) else { continue };
        let mut sources = Vec::new();
        let Some((x0, y0)) = grid.cell((isl.bbox.x0, isl.bbox.y0)) else {
            continue;
        };
        let (x1, y1) = grid
            .cell((isl.bbox.x1, isl.bbox.y1))
            .unwrap_or((grid.nx - 1, grid.ny - 1));
        for y in y0..=y1.min(grid.ny - 1) {
            for x in x0..=x1.min(grid.nx - 1) {
                let node = grid.node(l, x, y);
                if !grid.blocked[l][grid.idx(x, y)] && point_in_polygon(grid.point(x, y), &isl.poly)
                {
                    sources.push(node);
                }
            }
        }
        if sources.is_empty() {
            continue;
        }
        let Some(path) = search(&grid, &sources, &targets) else {
            continue;
        };
        // lay the path as straight runs, one via per layer change
        let mut run_start = path[0];
        for w in path.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (la, lb) = (a / plane, b / plane);
            let (ca, cb) = (a % plane, b % plane);
            let turn = la != lb
                || {
                    let (sx, sy) = (run_start % plane % grid.nx, run_start % plane / grid.nx);
                    let (ax, ay) = (ca % grid.nx, ca / grid.nx);
                    let (bx, by) = (cb % grid.nx, cb / grid.nx);
                    (ax as i64 - sx as i64) * (by as i64 - ay as i64)
                        != (ay as i64 - sy as i64) * (bx as i64 - ax as i64)
                };
            if turn {
                let (sx, sy) = (run_start % plane % grid.nx, run_start % plane / grid.nx);
                let (ax, ay) = (ca % grid.nx, ca / grid.nx);
                if (sx, sy) != (ax, ay) {
                    board.add_track(
                        grid.point(sx, sy),
                        grid.point(ax, ay),
                        width,
                        &grid.layers[la],
                        gnd.id,
                    );
                }
                if la != lb {
                    board.add_via(
                        grid.point(ax, ay),
                        rules.via_size,
                        rules.via_drill,
                        gnd.id,
                        (&grid.layers[la], &grid.layers[lb]),
                    );
                }
                run_start = b;
            }
        }
        let last = *path.last().unwrap();
        let (sx, sy) = (run_start % plane % grid.nx, run_start % plane / grid.nx);
        let (lx, ly) = (last % plane % grid.nx, last % plane / grid.nx);
        if (sx, sy) != (lx, ly) {
            board.add_track(
                grid.point(sx, sy),
                grid.point(lx, ly),
                width,
                &grid.layers[last / plane],
                gnd.id,
            );
        }
        // the copper just laid is somewhere the next repair may finish, and nowhere it may cross
        for &node in &path {
            targets[node] = true;
        }
        done.push(comp[i]);
        joined += 1;
    }
    Ok(joined)
}
