//! Routing grid: per-layer occupancy bitmaps with net-aware blocking.
//!
//! The naive grid router (slice 1) rasterizes the [`RouteProblem`] onto a
//! uniform square grid, one occupancy plane per copper layer, and asks A* the
//! single question "is this cell free for connection *c*?". A cell answers no
//! when it is blocked by foreign copper, by a keepout (copper with no net), or
//! by the board edge — but a pad never blocks cells for *its own* connection,
//! so a route can start and end inside its own pads.
//!
//! ## Determinism
//!
//! The cell↔mm mapping is floor-based with no float accumulation: cell `i`'s
//! lower edge is exactly `min + (i as f64) * pitch`, and a coordinate maps to
//! cell `((v - min) / pitch).floor()`. Grid dimensions, pitch, and the
//! connection-name index are all derived deterministically from the problem.
//!
//! ## Occupancy encoding
//!
//! Each cell holds a [`Cell`] tag:
//! - [`Cell::Free`] — open copper, blocks nothing.
//! - `Cell::Net(i)` — owned by connection index `i`; blocks every connection
//!   *except* `i`.
//! - [`Cell::BlockedAll`] — keepout, foreign copper with no net, or off-board;
//!   blocks every connection.
//!
//! A pad belonging to several connections (a shared/star pad) is modelled by
//! the *first* listed connection index; the conservative effect is that the
//! pad reads as owned by that one connection. Slice 1's fixtures have
//! single-owner pads, so this is exact for them; the DRC lint (a later task) is
//! the precision authority regardless.

use crate::problem::RouteProblem;
use std::collections::BTreeMap;

/// Minimum grid pitch, mm. Keeps the grid from exploding on tiny design rules.
const MIN_PITCH_MM: f64 = 0.1;

/// Per-cell occupancy tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cell {
    /// Open copper — free for every connection.
    Free,
    /// Owned by connection index `usize` — free only for that connection.
    Net(usize),
    /// Blocked for every connection (keepout, unowned copper, or off-board).
    BlockedAll,
}

/// A uniform square routing grid with one occupancy plane per copper layer.
#[derive(Debug, Clone)]
pub struct RouteGrid {
    /// Number of copper layers (planes), index 0 = top.
    pub layer_count: usize,
    /// Cells along x.
    pub nx: usize,
    /// Cells along y.
    pub ny: usize,
    /// Grid pitch, mm (square cells).
    pub pitch: f64,
    /// Lower-x edge of cell column 0, mm.
    pub min_x: f64,
    /// Lower-y edge of cell row 0, mm.
    pub min_y: f64,
    /// `cells[layer * (nx*ny) + iy * nx + ix]`.
    cells: Vec<Cell>,
    /// Connection name → dense index, built in `connections` order.
    name_index: BTreeMap<String, usize>,
}

impl RouteGrid {
    /// Build the grid for `problem`.
    ///
    /// Pitch is `max(MIN_PITCH_MM, (min_trace_width + clearance) / 2)`. Every
    /// obstacle is inflated by `clearance + min_trace_width/2` (so a trace
    /// centre kept one cell away clears the obstacle by the full rule) and
    /// rasterized into each layer it occupies. Cells whose inflated disc would
    /// leave `bounds` are blocked for everyone (board edge).
    pub fn build(problem: &RouteProblem) -> RouteGrid {
        let pitch = grid_pitch(problem);
        let inflation = obstacle_inflation(problem);

        let b = &problem.bounds;
        // Cell counts: cover [min, max] inclusive of the last partial cell.
        let nx = cells_along(b.min_x, b.max_x, pitch);
        let ny = cells_along(b.min_y, b.max_y, pitch);
        let layer_count = problem.layer_count.max(1) as usize;

        // Stable connection-name → index map (connections are already ordered).
        let mut name_index: BTreeMap<String, usize> = BTreeMap::new();
        for (i, conn) in problem.connections.iter().enumerate() {
            name_index.entry(conn.name.clone()).or_insert(i);
        }

        let plane = nx * ny;
        let cells = vec![Cell::Free; plane * layer_count];

        let mut grid = RouteGrid {
            layer_count,
            nx,
            ny,
            pitch,
            min_x: b.min_x,
            min_y: b.min_y,
            cells,
            name_index,
        };

        // Board-edge keepout: a cell whose centre is within `inflation` of a
        // board edge cannot host a trace centre without the copper leaving the
        // board. Block those cells on every layer.
        grid.block_board_edge(inflation, b.max_x, b.max_y);

        // Rasterize obstacles.
        for ob in &problem.obstacles {
            grid.rasterize_obstacle(ob, inflation);
        }

        grid
    }

    /// Total cells per layer plane.
    #[inline]
    fn plane(&self) -> usize {
        self.nx * self.ny
    }

    #[inline]
    fn idx(&self, layer: usize, ix: usize, iy: usize) -> usize {
        layer * self.plane() + iy * self.nx + ix
    }

    /// The occupancy tag at `(layer, ix, iy)`. Out-of-range cells read as
    /// [`Cell::BlockedAll`].
    #[inline]
    pub fn cell(&self, layer: usize, ix: usize, iy: usize) -> Cell {
        if layer >= self.layer_count || ix >= self.nx || iy >= self.ny {
            return Cell::BlockedAll;
        }
        self.cells[self.idx(layer, ix, iy)]
    }

    /// The dense index of a connection name, if known.
    pub fn connection_index(&self, name: &str) -> Option<usize> {
        self.name_index.get(name).copied()
    }

    /// Is `(layer, ix, iy)` usable by connection `conn`?
    ///
    /// `conn` is the dense connection index from [`Self::connection_index`].
    /// A cell owned by `conn` is free for it; foreign or all-blocked cells are
    /// not. Out-of-range cells are never free.
    #[inline]
    pub fn is_free_for(&self, layer: usize, ix: usize, iy: usize, conn: usize) -> bool {
        match self.cell(layer, ix, iy) {
            Cell::Free => true,
            Cell::Net(owner) => owner == conn,
            Cell::BlockedAll => false,
        }
    }

    /// Mark `(layer, ix, iy)` as copper owned by connection `conn`.
    ///
    /// Used by the router to turn a routed path into an obstacle for later
    /// nets. Never overwrites a [`Cell::BlockedAll`] (a keepout or board edge
    /// stays blocked) and a cell already owned by another net is upgraded to
    /// `BlockedAll` (two nets' copper at one cell is a hard block for everyone
    /// else). Out-of-range writes are ignored.
    pub fn mark_net(&mut self, layer: usize, ix: usize, iy: usize, conn: usize) {
        if layer >= self.layer_count || ix >= self.nx || iy >= self.ny {
            return;
        }
        let i = self.idx(layer, ix, iy);
        self.cells[i] = match self.cells[i] {
            Cell::BlockedAll => Cell::BlockedAll,
            Cell::Free => Cell::Net(conn),
            Cell::Net(owner) if owner == conn => Cell::Net(conn),
            Cell::Net(_) => Cell::BlockedAll,
        };
    }

    /// Mark `(layer, ix, iy)` and every cell within `radius_cells` of it (a
    /// square Chebyshev halo) as copper owned by `conn`.
    ///
    /// The halo enforces clearance between this net's copper and *other* nets:
    /// a foreign trace centred in a haloed cell would sit too close to this
    /// net's trace. Because the halo is written as `Net(conn)`, the owning net
    /// can still route through it (it never blocks itself), while foreign nets
    /// are kept the full clearance away. See [`Self::mark_net`] for the
    /// per-cell tag-combine rules.
    pub fn mark_net_halo(&mut self, layer: usize, ix: usize, iy: usize, conn: usize, radius_cells: usize) {
        if layer >= self.layer_count {
            return;
        }
        let r = radius_cells as isize;
        for dy in -r..=r {
            for dx in -r..=r {
                let hx = ix as isize + dx;
                let hy = iy as isize + dy;
                if hx < 0 || hy < 0 || hx >= self.nx as isize || hy >= self.ny as isize {
                    continue;
                }
                self.mark_net(layer, hx as usize, hy as usize, conn);
            }
        }
    }

    // ── coordinate mapping ───────────────────────────────────────────────────

    /// Centre of cell column `ix`, mm. Floor-based: lower edge is
    /// `min_x + ix*pitch`, centre adds half a pitch.
    #[inline]
    pub fn cell_center_x(&self, ix: usize) -> f64 {
        self.min_x + (ix as f64) * self.pitch + self.pitch / 2.0
    }

    /// Centre of cell row `iy`, mm.
    #[inline]
    pub fn cell_center_y(&self, iy: usize) -> f64 {
        self.min_y + (iy as f64) * self.pitch + self.pitch / 2.0
    }

    /// The cell column/row a coordinate falls in, clamped to the grid. Maps
    /// `((v - min) / pitch).floor()`.
    #[inline]
    pub fn cell_of(&self, x: f64, y: f64) -> (usize, usize) {
        let ix = (((x - self.min_x) / self.pitch).floor() as isize)
            .clamp(0, self.nx as isize - 1) as usize;
        let iy = (((y - self.min_y) / self.pitch).floor() as isize)
            .clamp(0, self.ny as isize - 1) as usize;
        (ix, iy)
    }

    // ── rasterization ────────────────────────────────────────────────────────

    /// Block cells whose centre is within `inflation` of any board edge, on
    /// every layer. A trace centre placed in such a cell would push copper
    /// (half a trace width) outside the board.
    fn block_board_edge(&mut self, inflation: f64, max_x: f64, max_y: f64) {
        for ix in 0..self.nx {
            for iy in 0..self.ny {
                let cx = self.cell_center_x(ix);
                let cy = self.cell_center_y(iy);
                let near_edge = (cx - self.min_x) < inflation
                    || (max_x - cx) < inflation
                    || (cy - self.min_y) < inflation
                    || (max_y - cy) < inflation;
                if near_edge {
                    for layer in 0..self.layer_count {
                        let i = self.idx(layer, ix, iy);
                        self.cells[i] = Cell::BlockedAll;
                    }
                }
            }
        }
    }

    /// Rasterize one obstacle inflated by `inflation` into each layer it
    /// occupies. The obstacle is treated as an axis-aligned rect (oval → its
    /// bounding rect, per the v1 model); a cell is blocked when its centre lies
    /// within the inflated rect.
    fn rasterize_obstacle(&mut self, ob: &crate::problem::Obstacle, inflation: f64) {
        let hw = ob.width / 2.0 + inflation;
        let hh = ob.height / 2.0 + inflation;
        let min_x = ob.center.x - hw;
        let max_x = ob.center.x + hw;
        let min_y = ob.center.y - hh;
        let max_y = ob.center.y + hh;

        // The tag this obstacle contributes: its first owning connection's
        // index, or BlockedAll when it owns no net (keepout / foreign copper).
        let tag = ob
            .connected_to
            .iter()
            .find_map(|n| self.connection_index(n))
            .map(Cell::Net)
            .unwrap_or(Cell::BlockedAll);

        // Cell index span touching the inflated rect.
        let (ix0, ix1) = cell_span(self.min_x, min_x, max_x, self.pitch, self.nx);
        let (iy0, iy1) = cell_span(self.min_y, min_y, max_y, self.pitch, self.ny);

        for layer_ref in &ob.layers {
            let Some(layer) = layer_ref.index(self.layer_count as u32) else {
                continue;
            };
            let layer = layer as usize;
            for ix in ix0..=ix1 {
                let cx = self.cell_center_x(ix);
                if cx < min_x || cx > max_x {
                    continue;
                }
                for iy in iy0..=iy1 {
                    let cy = self.cell_center_y(iy);
                    if cy < min_y || cy > max_y {
                        continue;
                    }
                    let i = self.idx(layer, ix, iy);
                    self.cells[i] = combine(self.cells[i], tag);
                }
            }
        }
    }
}

/// Grid pitch for `problem`: `max(MIN_PITCH_MM, (min_trace_width+clearance)/2)`.
pub fn grid_pitch(problem: &RouteProblem) -> f64 {
    ((problem.min_trace_width + problem.clearance) / 2.0).max(MIN_PITCH_MM)
}

/// Obstacle inflation for `problem`: `clearance + min_trace_width/2`.
pub fn obstacle_inflation(problem: &RouteProblem) -> f64 {
    problem.clearance + problem.min_trace_width / 2.0
}

/// Number of cells needed to cover `[min, max]` at `pitch` (at least 1).
fn cells_along(min: f64, max: f64, pitch: f64) -> usize {
    (((max - min) / pitch).ceil() as usize).max(1)
}

/// Inclusive cell-index span `[i0, i1]` whose cells could touch `[lo, hi]`,
/// clamped to `[0, n-1]`.
fn cell_span(origin: f64, lo: f64, hi: f64, pitch: f64, n: usize) -> (usize, usize) {
    let i0 = (((lo - origin) / pitch).floor() as isize).clamp(0, n as isize - 1) as usize;
    let i1 = (((hi - origin) / pitch).floor() as isize).clamp(0, n as isize - 1) as usize;
    (i0, i1)
}

/// Combine an existing cell tag with a new obstacle's tag. Two different owners
/// (or any owner over a keepout) collapse to [`Cell::BlockedAll`].
fn combine(existing: Cell, incoming: Cell) -> Cell {
    match (existing, incoming) {
        (Cell::BlockedAll, _) | (_, Cell::BlockedAll) => Cell::BlockedAll,
        (Cell::Free, other) => other,
        (other, Cell::Free) => other,
        (Cell::Net(a), Cell::Net(b)) if a == b => Cell::Net(a),
        (Cell::Net(_), Cell::Net(_)) => Cell::BlockedAll,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problem::{Bounds, Connection, LayerRef, Obstacle, Point2, RoutePoint, RouteProblem};

    fn problem(obstacles: Vec<Obstacle>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections: vec![
                conn("SIG", &[(1.0, 1.0, "top")]),
                conn("GND", &[(9.0, 9.0, "top")]),
            ],
            bounds: Bounds {
                min_x: 0.0,
                max_x: 10.0,
                min_y: 0.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
        }
    }

    fn conn(name: &str, pts: &[(f64, f64, &str)]) -> Connection {
        Connection {
            name: name.to_owned(),
            points_to_connect: pts
                .iter()
                .map(|&(x, y, l)| RoutePoint {
                    x,
                    y,
                    layer: LayerRef(l.to_owned()),
                })
                .collect(),
        }
    }

    fn pad(connected_to: &[&str], center: (f64, f64), w: f64, h: f64, layers: &[&str]) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: layers.iter().map(|l| LayerRef((*l).to_owned())).collect(),
            center: Point2 {
                x: center.0,
                y: center.1,
            },
            width: w,
            height: h,
            connected_to: connected_to.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn pitch_and_inflation_match_the_design_constants() {
        let p = problem(vec![]);
        // (0.2 + 0.2)/2 = 0.2 > 0.1 floor.
        assert!((grid_pitch(&p) - 0.2).abs() < 1e-12);
        // 0.2 + 0.2/2 = 0.3.
        assert!((obstacle_inflation(&p) - 0.3).abs() < 1e-12);
    }

    #[test]
    fn pitch_respects_minimum_floor() {
        let mut p = problem(vec![]);
        p.min_trace_width = 0.05;
        p.clearance = 0.05;
        // (0.05+0.05)/2 = 0.05 < 0.1 floor.
        assert!((grid_pitch(&p) - MIN_PITCH_MM).abs() < 1e-12);
    }

    #[test]
    fn mapping_round_trips() {
        let g = RouteGrid::build(&problem(vec![]));
        for ix in 0..g.nx {
            for iy in 0..g.ny {
                let cx = g.cell_center_x(ix);
                let cy = g.cell_center_y(iy);
                assert_eq!(g.cell_of(cx, cy), (ix, iy), "cell ({ix},{iy}) center");
            }
        }
        // Floor-based: no float accumulation drift across the whole row.
        let last = g.nx - 1;
        let expected = g.min_x + (last as f64) * g.pitch + g.pitch / 2.0;
        assert!((g.cell_center_x(last) - expected).abs() < 1e-12);
    }

    #[test]
    fn pad_blocks_neighbors_on_its_layer_only_and_passes_own_net() {
        // A SIG pad in the middle of the top layer.
        let g = RouteGrid::build(&problem(vec![pad(
            &["SIG"],
            (5.0, 5.0),
            1.0,
            1.0,
            &["top"],
        )]));
        let sig = g.connection_index("SIG").unwrap();
        let gnd = g.connection_index("GND").unwrap();
        let (cx, cy) = g.cell_of(5.0, 5.0);

        // The pad cell is owned by SIG: free for SIG, blocked for GND, on top.
        assert!(g.is_free_for(0, cx, cy, sig), "own net passes over its pad");
        assert!(!g.is_free_for(0, cx, cy, gnd), "foreign net is blocked");

        // A neighbouring cell within inflation is also SIG-owned (blocks GND).
        // Inflated half-extent = 0.5 + 0.3 = 0.8 mm → reaches the next cell.
        assert!(!g.is_free_for(0, cx + 1, cy, gnd), "inflation blocks GND nearby");
        assert!(g.is_free_for(0, cx + 1, cy, sig), "but SIG still passes");

        // Bottom layer is untouched by a top-only pad.
        assert!(g.is_free_for(1, cx, cy, gnd), "bottom layer is clear");
    }

    #[test]
    fn keepout_blocks_every_connection() {
        // Unowned copper (connected_to empty) is a hard block on its layer.
        let g = RouteGrid::build(&problem(vec![pad(&[], (5.0, 5.0), 1.0, 1.0, &["top"])]));
        let sig = g.connection_index("SIG").unwrap();
        let (cx, cy) = g.cell_of(5.0, 5.0);
        assert_eq!(g.cell(0, cx, cy), Cell::BlockedAll);
        assert!(!g.is_free_for(0, cx, cy, sig));
    }

    #[test]
    fn board_edge_and_out_of_range_are_blocked() {
        let g = RouteGrid::build(&problem(vec![]));
        let sig = g.connection_index("SIG").unwrap();
        // Cell (0,0) sits in the corner, within inflation of two edges.
        assert_eq!(g.cell(0, 0, 0), Cell::BlockedAll);
        assert!(!g.is_free_for(0, 0, 0, sig));
        // Out-of-range reads BlockedAll.
        assert_eq!(g.cell(0, g.nx, g.ny), Cell::BlockedAll);
        assert!(!g.is_free_for(5, 0, 0, sig));
    }

    #[test]
    fn mark_net_makes_cell_an_obstacle_for_others() {
        let mut g = RouteGrid::build(&problem(vec![]));
        let sig = g.connection_index("SIG").unwrap();
        let gnd = g.connection_index("GND").unwrap();
        let (cx, cy) = g.cell_of(5.0, 5.0);
        assert!(g.is_free_for(0, cx, cy, gnd));
        g.mark_net(0, cx, cy, sig);
        assert!(g.is_free_for(0, cx, cy, sig), "owner still free");
        assert!(!g.is_free_for(0, cx, cy, gnd), "foreign now blocked");
        // A second net over the same cell hard-blocks it for everyone else.
        g.mark_net(0, cx, cy, gnd);
        assert_eq!(g.cell(0, cx, cy), Cell::BlockedAll);
    }

    #[test]
    fn mark_net_halo_blocks_a_chebyshev_ring_for_others() {
        let mut g = RouteGrid::build(&problem(vec![]));
        let sig = g.connection_index("SIG").unwrap();
        let gnd = g.connection_index("GND").unwrap();
        let (cx, cy) = g.cell_of(5.0, 5.0);
        g.mark_net_halo(0, cx, cy, sig, 1);
        // Every cell within Chebyshev radius 1 is SIG-owned (foreign blocked,
        // owner free); a cell two away is untouched.
        for dx in -1isize..=1 {
            for dy in -1isize..=1 {
                let (hx, hy) = ((cx as isize + dx) as usize, (cy as isize + dy) as usize);
                assert!(!g.is_free_for(0, hx, hy, gnd), "halo cell blocks foreign");
                assert!(g.is_free_for(0, hx, hy, sig), "halo cell free for owner");
            }
        }
        assert!(g.is_free_for(0, cx + 2, cy, gnd), "outside halo stays free");
    }
}
