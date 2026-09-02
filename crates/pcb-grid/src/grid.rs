//! Routing grid: per-layer occupancy bitmaps with net-aware blocking.
//!
//! The naive grid router (slice 1) rasterizes the [`RoutingView`] onto a
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

use pcb_model::{MIN_PITCH_MM, Point2, Polygon, RoutingView};
use std::collections::BTreeMap;

/// Per-cell occupancy tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cell {
    /// Open copper — free for every connection.
    Free,
    /// Owned by connection index `usize` — free only for that connection.
    Net(usize),
    /// Passable only for the connections in the grid's shared-region table
    /// entry — a reserved corridor (a fine-pitch IC's escape ring) that the
    /// owning IC's nets may cross while foreign copper must detour.
    Shared(u16),
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
    /// Sorted connection-index sets backing [`Cell::Shared`].
    shared_regions: Vec<Vec<usize>>,
}

impl RouteGrid {
    /// Build the grid for `problem`.
    ///
    /// Pitch is `max(MIN_PITCH_MM, (min_trace_width + clearance) / 2)`. Every
    /// obstacle is inflated by `clearance + min_trace_width/2` (so a trace
    /// centre kept one cell away clears the obstacle by the full rule) and
    /// rasterized into each layer it occupies. Cells whose inflated disc would
    /// leave `bounds` are blocked for everyone (board edge).
    pub fn build(problem: &RoutingView) -> RouteGrid {
        Self::build_with_pitch(problem, problem.grid_pitch())
    }

    /// [`Self::build`] over the whole board at an explicit `pitch` (mm). The
    /// detailed router ([`crate::detail`]) uses a *finer* pitch than slice-1 so a
    /// trace's emitted cell-centre points sit closer to the ideal centreline,
    /// shrinking the grid-snap distortion that the exact-geometry lint measures at
    /// dense crossings. Slice-1 calls [`Self::build`] (the design pitch) and is
    /// unaffected. Obstacle/board-edge semantics are identical to [`Self::build`].
    pub fn build_with_pitch(problem: &RoutingView, pitch: f64) -> RouteGrid {
        let pitch = pitch.max(MIN_PITCH_MM);
        let inflation = problem.obstacle_inflation();

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
            shared_regions: Vec::new(),
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

        // Custom outline: block every cell outside the polygon (or within `inflation` of
        // an edge), so copper stays inside the true shape, not just its bounding box.
        if let Some(poly) = &problem.outline {
            grid.block_outside_polygon(poly, inflation);
        }

        // Rasterize obstacles.
        for ob in &problem.obstacles {
            grid.rasterize_obstacle(ob, inflation);
        }

        // Pad-copper own-net relief: re-assert each owned pad's UN-inflated copper
        // footprint as its own net (see [`Self::assert_pad_copper`]). This rescues a
        // fine-pitch pad whose own body cells were collapsed to `BlockedAll` by a
        // neighbour's *clearance halo* overlap — so the owning net can still seed and
        // walk out along its own pad. Foreign clearance is preserved (the cell stays
        // `Net(owner)`, which still blocks every other net).
        grid.assert_pad_copper(&problem.obstacles);
        grid.assert_fine_pitch_escape_lanes(problem, &problem.obstacles);

        grid
    }

    /// Build a [`RouteGrid`] covering only a sub-rectangle `window` of the
    /// board, at the same pitch and with the same obstacle/clearance semantics as
    /// [`Self::build`]. Used by the per-cell detailed router ([`crate::detail`])
    /// to route inside one quadtree leaf without rasterizing the whole board.
    ///
    /// The window is expected to be the leaf rect inflated by one track pitch and
    /// clamped to the board bounds (the caller does the inflation/clamp). Only the
    /// window is rasterized; only obstacles whose inflated footprint reaches the
    /// window are blocked.
    ///
    /// ## Boundary semantics (deliberate, and tested)
    ///
    /// - **True board edges block.** A cell within `obstacle_inflation` of the
    ///   *real* board bounds (`problem.bounds`) is blocked on every layer, exactly
    ///   as in [`Self::build`]: copper may not leave the board.
    /// - **Window edges do NOT block by inflation.** A window edge that is *not*
    ///   also a board edge is an artefact of the per-cell decomposition, not a
    ///   physical boundary, so cells at it are usable. This is required: crossing
    ///   points sit on leaf boundaries, which become window-interior cells after
    ///   the one-pitch inflation, and a route must be able to reach them.
    /// - **Routing still stays inside the window.** Movement beyond the grid is
    ///   impossible (out-of-range cells read [`Cell::BlockedAll`]), so a path
    ///   physically cannot leave the window even though the window edge itself is
    ///   routable. Other cells' copper is invisible here by construction — the
    ///   per-cell router marks only this cell's own routed copper into the window
    ///   grid, and stitching (Task 3) joins cells at the shared crossing points.
    ///
    /// The grid uses the window's own origin (`min` = clamped window min); the
    /// cross-cell stitching contract is upheld by the detailed router *snapping*
    /// each terminal endpoint to its exact mm position, not by aligning window
    /// lattices (floating-point floor mismatches make exact lattice alignment
    /// unreliable, and snapping is exact regardless).
    pub fn build_window(problem: &RoutingView, window: &pcb_model::Rect) -> RouteGrid {
        Self::build_window_with_pitch(problem, window, problem.grid_pitch())
    }

    /// [`Self::build_window`] at an explicit `pitch` (mm) — the detailed router
    /// uses a finer pitch than slice-1 for better geometric fidelity (smaller
    /// grid-snap distortion). See [`Self::build_with_pitch`].
    pub fn build_window_with_pitch(
        problem: &RoutingView,
        window: &pcb_model::Rect,
        pitch: f64,
    ) -> RouteGrid {
        let pitch = pitch.max(MIN_PITCH_MM);
        let inflation = problem.obstacle_inflation();
        let b = &problem.bounds;

        // Clamp the window to the board bounds defensively (the caller should
        // have done this, but a window cell must never sit off-board).
        let win_min_x = window.min_x.max(b.min_x);
        let win_min_y = window.min_y.max(b.min_y);
        let win_max_x = window.max_x.min(b.max_x);
        let win_max_y = window.max_y.min(b.max_y);

        // Window origin = window min. Window cells are this grid's own lattice;
        // the stitching contract relies on *terminal endpoint snapping* (the
        // detailed router replaces any terminal endpoint with its exact mm
        // position), NOT on cell-centre alignment between windows — so two
        // adjacent cells agree on a crossing point byte-for-byte regardless of
        // their independent lattices.
        let min_x = win_min_x;
        let min_y = win_min_y;

        let nx = cells_along(min_x, win_max_x, pitch);
        let ny = cells_along(min_y, win_max_y, pitch);
        let layer_count = problem.layer_count.max(1) as usize;

        let mut name_index: BTreeMap<String, usize> = BTreeMap::new();
        for (i, conn) in problem.connections.iter().enumerate() {
            name_index.entry(conn.name.clone()).or_insert(i);
        }

        let plane = nx * ny;
        let cells = vec![Cell::Free; plane * layer_count];

        let mut grid = RouteGrid {
            shared_regions: Vec::new(),
            layer_count,
            nx,
            ny,
            pitch,
            min_x,
            min_y,
            cells,
            name_index,
        };

        // Block cells within `inflation` of a TRUE board edge only (window edges
        // that are not board edges stay routable — see the doc comment).
        grid.block_true_board_edge(inflation, b);

        // Rasterize only obstacles whose inflated footprint reaches the window.
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
            Cell::Shared(region) => self.shared_regions[region as usize]
                .binary_search(&conn)
                .is_ok(),
            Cell::BlockedAll => false,
        }
    }

    /// Mark `(layer, ix, iy)` as copper owned by connection `conn`.
    ///
    /// Used by the router to turn a routed path into an obstacle for later
    /// Intern a sorted connection-index set as a shared region id.
    fn shared_region_id(&mut self, owners: Vec<usize>) -> u16 {
        if let Some(found) = self.shared_regions.iter().position(|r| *r == owners) {
            return found as u16;
        }
        self.shared_regions.push(owners);
        (self.shared_regions.len() - 1) as u16
    }

    /// [`combine`] with region awareness: a net cell inside its own region
    /// stays the (stricter) net cell; a foreign net collapses the cell.
    fn combine_cells(&self, existing: Cell, incoming: Cell) -> Cell {
        match (existing, incoming) {
            (Cell::Shared(region), Cell::Net(net)) | (Cell::Net(net), Cell::Shared(region)) => {
                if self.shared_regions[region as usize]
                    .binary_search(&net)
                    .is_ok()
                {
                    Cell::Net(net)
                } else {
                    Cell::BlockedAll
                }
            }
            (a, b) => combine(a, b),
        }
    }

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
            Cell::Shared(region) => {
                if self.shared_regions[region as usize]
                    .binary_search(&conn)
                    .is_ok()
                {
                    Cell::Net(conn)
                } else {
                    Cell::BlockedAll
                }
            }
        };
    }

    /// Force `(layer, ix, iy)` to be owned by `conn`, OVERRIDING a `BlockedAll`
    /// collapse (unlike [`Self::mark_net`], which leaves `BlockedAll` alone).
    ///
    /// Used only by the pre-routed escape stub, whose mm centre-line is DRC-legal by
    /// construction (it is collinear with its own elongated pad). The grid cell may
    /// read `BlockedAll` purely from the conservative box/grid-quantised halo of a
    /// perpendicular neighbour; forcing it to `Net(conn)` lets the stub's own net
    /// route through its legal corridor while still blocking every FOREIGN net there.
    /// A two-net real-copper overlap is never forced here (the stub only touches its
    /// own pad's centre-line), and `drop_violating_copper` is the final authority on
    /// the emitted mm copper regardless.
    pub fn force_mark_net(&mut self, layer: usize, ix: usize, iy: usize, conn: usize) {
        if layer >= self.layer_count || ix >= self.nx || iy >= self.ny {
            return;
        }
        let i = self.idx(layer, ix, iy);
        self.cells[i] = Cell::Net(conn);
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
    pub fn mark_net_halo(
        &mut self,
        layer: usize,
        ix: usize,
        iy: usize,
        conn: usize,
        radius_cells: usize,
    ) {
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

    /// Mark `(layer, ix, iy)` and every cell whose centre is closer than
    /// `min_dist` (mm, **Euclidean**) to that cell's centre as copper owned by
    /// `conn` — an exact-geometry clearance halo.
    ///
    /// This is the detailed stage's halo. Where [`Self::mark_net_halo`] uses a
    /// square Chebyshev ring of cells (conservative: it over-blocks the diagonal
    /// to ~`radius·√2·pitch` and blocks even a foreign cell sitting at *exactly*
    /// the legal clearance), this blocks a foreign cell only when its centre would
    /// sit strictly inside the legal centre-to-centre spacing `min_dist`
    /// (= `min_trace_width + clearance`). A foreign trace centred at exactly
    /// `min_dist` is legal (its edge gap equals `clearance`), so it is left
    /// routable — which is what lets dense boundary crossings spaced at one track
    /// pitch coexist. Slice-1's full-board router keeps the Chebyshev halo
    /// unchanged; this method is used only by [`crate::detail`].
    pub fn mark_net_halo_euclid(
        &mut self,
        layer: usize,
        ix: usize,
        iy: usize,
        conn: usize,
        min_dist: f64,
    ) {
        if layer >= self.layer_count {
            return;
        }
        // Cells within `min_dist` of the centre lie within a Chebyshev box of
        // `ceil(min_dist / pitch)` cells; test each by exact Euclidean distance.
        let r = (min_dist / self.pitch).ceil() as isize;
        // Strictly-inside test: a centre exactly `min_dist` away is legal copper.
        let thresh = min_dist - 1e-9;
        let thresh2 = thresh * thresh;
        for dy in -r..=r {
            for dx in -r..=r {
                let hx = ix as isize + dx;
                let hy = iy as isize + dy;
                if hx < 0 || hy < 0 || hx >= self.nx as isize || hy >= self.ny as isize {
                    continue;
                }
                let off_x = dx as f64 * self.pitch;
                let off_y = dy as f64 * self.pitch;
                if off_x * off_x + off_y * off_y <= thresh2 {
                    self.mark_net(layer, hx as usize, hy as usize, conn);
                }
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
        let ix = (((x - self.min_x) / self.pitch).floor() as isize).clamp(0, self.nx as isize - 1)
            as usize;
        let iy = (((y - self.min_y) / self.pitch).floor() as isize).clamp(0, self.ny as isize - 1)
            as usize;
        (ix, iy)
    }

    // ── rasterization ────────────────────────────────────────────────────────

    /// Block cells whose centre is within `inflation` of any board edge, on
    /// every layer. A trace centre placed in such a cell would push copper
    /// (half a trace width) outside the board.
    /// Block every cell whose centre is OUTSIDE `poly`, or within `inflation` of a
    /// polygon edge (copper-to-edge clearance), on every layer. This is the board-edge
    /// keep-out for a custom (possibly concave) outline.
    fn block_outside_polygon(&mut self, poly: &Polygon, inflation: f64) {
        for ix in 0..self.nx {
            for iy in 0..self.ny {
                let pt = Point2 {
                    x: self.cell_center_x(ix),
                    y: self.cell_center_y(iy),
                };
                if !poly.contains_point(pt) || poly.dist_to_edge(pt) < inflation {
                    for layer in 0..self.layer_count {
                        let i = self.idx(layer, ix, iy);
                        self.cells[i] = Cell::BlockedAll;
                    }
                }
            }
        }
    }

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

    /// Block cells whose centre is within `inflation` of a TRUE board edge (one
    /// of the four `bounds` edges), on every layer. Used by [`Self::build_window`]
    /// so a sub-window blocks only physical board edges, not the artificial
    /// window edges introduced by per-cell decomposition.
    fn block_true_board_edge(&mut self, inflation: f64, bounds: &pcb_model::Rect) {
        for ix in 0..self.nx {
            for iy in 0..self.ny {
                let cx = self.cell_center_x(ix);
                let cy = self.cell_center_y(iy);
                let near_board_edge = (cx - bounds.min_x) < inflation
                    || (bounds.max_x - cx) < inflation
                    || (cy - bounds.min_y) < inflation
                    || (bounds.max_y - cy) < inflation;
                if near_board_edge {
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
    fn rasterize_obstacle(&mut self, ob: &pcb_model::Obstacle, inflation: f64) {
        let hw = ob.width / 2.0 + inflation;
        let hh = ob.height / 2.0 + inflation;
        let min_x = ob.center.x - hw;
        let max_x = ob.center.x + hw;
        let min_y = ob.center.y - hh;
        let max_y = ob.center.y + hh;

        // The tag this obstacle contributes: its owning connection when it
        // has exactly one, a shared region when several connections may pass
        // (an escape-ring reservation), or BlockedAll when it owns no net
        // (keepout / foreign copper).
        let mut owners: Vec<usize> = ob
            .connected_to
            .iter()
            .filter_map(|n| self.connection_index(n))
            .collect();
        owners.sort_unstable();
        owners.dedup();
        let tag = match owners.as_slice() {
            [] => Cell::BlockedAll,
            [only] => Cell::Net(*only),
            _ => Cell::Shared(self.shared_region_id(owners.clone())),
        };

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
                    self.cells[i] = self.combine_cells(self.cells[i], tag);
                }
            }
        }
    }

    /// Re-assert every owned pad's **un-inflated** copper footprint as its own net,
    /// undoing a `BlockedAll` collapse that a *neighbour's clearance halo* caused.
    ///
    /// The inflation pass ([`Self::rasterize_obstacle`]) reserves `clearance +
    /// trace_half` around each pad and collapses two nets' overlapping halos to
    /// [`Cell::BlockedAll`]. At fine pitch (a 0.5 mm-pitch QFP, a 0.8 mm BGA) that
    /// halo overlap swallows a pad's OWN body cells, so the owning net cannot even
    /// seed or step off its pad — yet a trace on its own copper is perfectly legal.
    /// This pass walks each pad's real (un-inflated) rectangle and, where two
    /// **different** pads' real copper do not actually overlap, sets the cell to
    /// `Net(owner)`. Effects:
    /// - Frees the cell **for the owner** (it can route off its pad).
    /// - Still blocks **every other net** (`Net(owner)` is foreign to them), so the
    ///   neighbour's clearance is preserved — this only relaxes the owner against
    ///   its own copper, never opens a foreign clearance corridor.
    ///
    /// A true keepout / off-board cell (a `BlockedAll` NOT covered by this pad's own
    /// real copper) is untouched, and a genuine two-net copper overlap stays
    /// `BlockedAll` (a real short the lint must see).
    fn assert_pad_copper(&mut self, obstacles: &[pcb_model::Obstacle]) {
        for ob in obstacles {
            let Some(owner) = ob
                .connected_to
                .iter()
                .find_map(|n| self.connection_index(n))
            else {
                continue; // unowned (keepout / foreign copper) — never relax
            };
            let hw = ob.width / 2.0;
            let hh = ob.height / 2.0;
            let (min_x, max_x) = (ob.center.x - hw, ob.center.x + hw);
            let (min_y, max_y) = (ob.center.y - hh, ob.center.y + hh);
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
                        // Only rescue a cell the halo collapse blocked; never override a
                        // cell that genuinely belongs to a DIFFERENT net's real copper
                        // (that is a real short) or the owner's own (already free).
                        let i = self.idx(layer, ix, iy);
                        match self.cells[i] {
                            Cell::BlockedAll
                                if !self.foreign_pad_copper(obstacles, owner, layer, cx, cy) =>
                            {
                                self.cells[i] = Cell::Net(owner);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    /// Does any pad owned by a net OTHER than `owner` have un-inflated copper on
    /// `layer` covering the point `(cx, cy)`? Used by [`Self::assert_pad_copper`] to
    /// refuse to relax a cell where two pads' real copper genuinely overlap (a short).
    /// Open the straight-out escape lane in front of each fine-pitch pad.
    ///
    /// At sub-0.66mm pitch the neighbouring pads' clearance halos overlap the
    /// cells directly beyond a pad's tip, collapsing them to `BlockedAll` even
    /// though a minimum-width trace exiting straight out clears both
    /// neighbours by the design clearance. The A* then dead-ends on the pad
    /// itself and reports enclosure. Relax exactly those lane cells back to
    /// the owner where the trace genuinely clears every foreign obstacle's
    /// REAL copper; anything tighter stays blocked.
    fn assert_fine_pitch_escape_lanes(
        &mut self,
        problem: &RoutingView,
        obstacles: &[pcb_model::Obstacle],
    ) {
        const FINE_PITCH_MM: f64 = 0.66;
        let clear = problem.clearance + problem.min_trace_width / 2.0;
        let lane_len = clear + self.pitch;
        for ob in obstacles {
            let Some(owner) = ob
                .connected_to
                .iter()
                .find_map(|n| self.connection_index(n))
            else {
                continue;
            };
            // Fine-pitch membership: another owned pad's centre closer than the
            // pitch threshold.
            let fine = obstacles.iter().any(|other| {
                !std::ptr::eq(ob, other)
                    && !other.connected_to.is_empty()
                    && other.center.dist(ob.center) < FINE_PITCH_MM
            });
            if !fine {
                continue;
            }
            // Elongated pads escape along their long axis, both directions.
            let (long, short, horizontal) = if ob.width >= ob.height {
                (ob.width, ob.height, true)
            } else {
                (ob.height, ob.width, false)
            };
            if long < short * 1.3 {
                continue;
            }
            for dir in [-1.0f64, 1.0] {
                let (lane_cx, lane_cy, lane_w, lane_h) = if horizontal {
                    (
                        ob.center.x + dir * (long / 2.0 + lane_len / 2.0),
                        ob.center.y,
                        lane_len,
                        short,
                    )
                } else {
                    (
                        ob.center.x,
                        ob.center.y + dir * (long / 2.0 + lane_len / 2.0),
                        short,
                        lane_len,
                    )
                };
                let (min_x, max_x) = (lane_cx - lane_w / 2.0, lane_cx + lane_w / 2.0);
                let (min_y, max_y) = (lane_cy - lane_h / 2.0, lane_cy + lane_h / 2.0);
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
                            if self.cells[i] != Cell::BlockedAll {
                                continue;
                            }
                            let legal =
                                !obstacles.iter().any(|other| {
                                    let other_owner = other
                                        .connected_to
                                        .iter()
                                        .find_map(|n| self.connection_index(n));
                                    if other_owner == Some(owner) {
                                        return false;
                                    }
                                    if !other.layers.iter().any(|l| {
                                        l.index(self.layer_count as u32) == Some(layer as u32)
                                    }) {
                                        return false;
                                    }
                                    let dx = (cx - other.center.x).abs() - other.width / 2.0;
                                    let dy = (cy - other.center.y).abs() - other.height / 2.0;
                                    let gap = match (dx > 0.0, dy > 0.0) {
                                        (true, true) => (dx * dx + dy * dy).sqrt(),
                                        (true, false) => dx,
                                        (false, true) => dy,
                                        (false, false) => f64::NEG_INFINITY,
                                    };
                                    gap < clear
                                });
                            if legal {
                                self.cells[i] = Cell::Net(owner);
                            }
                        }
                    }
                }
            }
        }
    }

    fn foreign_pad_copper(
        &self,
        obstacles: &[pcb_model::Obstacle],
        owner: usize,
        layer: usize,
        cx: f64,
        cy: f64,
    ) -> bool {
        obstacles.iter().any(|ob| {
            let other = ob
                .connected_to
                .iter()
                .find_map(|n| self.connection_index(n));
            other != Some(owner)
                && other.is_some()
                && ob
                    .layers
                    .iter()
                    .any(|l| l.index(self.layer_count as u32) == Some(layer as u32))
                && (cx - ob.center.x).abs() <= ob.width / 2.0
                && (cy - ob.center.y).abs() <= ob.height / 2.0
        })
    }
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
        // Region interactions are resolved by RouteGrid::combine_cells, which
        // can consult the region table; a bare combine treats them strictly.
        (Cell::Shared(a), Cell::Shared(b)) if a == b => Cell::Shared(a),
        (Cell::Shared(_), _) | (_, Cell::Shared(_)) => Cell::BlockedAll,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{Connection, LayerRef, Obstacle, Point2, Rect, RoutePoint, RoutingView};

    fn problem(obstacles: Vec<Obstacle>) -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections: vec![
                conn("SIG", &[(1.0, 1.0, "top")]),
                conn("GND", &[(9.0, 9.0, "top")]),
            ],
            bounds: Rect {
                min_x: 0.0,
                max_x: 10.0,
                min_y: 0.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
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
        assert!((p.grid_pitch() - 0.2).abs() < 1e-12);
        // 0.2 + 0.2/2 = 0.3.
        assert!((p.obstacle_inflation() - 0.3).abs() < 1e-12);
    }

    #[test]
    fn pitch_respects_minimum_floor() {
        let mut p = problem(vec![]);
        p.min_trace_width = 0.05;
        p.clearance = 0.05;
        // (0.05+0.05)/2 = 0.05 < 0.1 floor.
        assert!((p.grid_pitch() - MIN_PITCH_MM).abs() < 1e-12);
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
        assert!(
            !g.is_free_for(0, cx + 1, cy, gnd),
            "inflation blocks GND nearby"
        );
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
    fn window_blocks_true_board_edges_but_not_interior_window_edges() {
        use pcb_model::Rect;
        // A 20x20 board. Take a window in the middle that touches the LEFT board
        // edge but whose right/top/bottom edges are interior to the board.
        let p = problem(vec![]);
        let win = Rect {
            min_x: 0.0, // touches the real left board edge
            min_y: 4.0, // interior (window edge, not board edge)
            max_x: 6.0, // interior
            max_y: 6.0, // interior
        };
        let g = RouteGrid::build_window(&p, &win);
        let sig = g.connection_index("SIG").unwrap();

        // A cell at the interior window edge (near max_x ~ 6.0, mid-height) must
        // be routable — it is NOT a board edge.
        let (ix, iy) = g.cell_of(5.9, 5.0);
        assert!(
            g.is_free_for(0, ix, iy, sig),
            "interior window edge cell must stay routable"
        );

        // A cell hugging the LEFT board edge (x ~ 0) is blocked: that IS a board
        // edge.
        let (lx, ly) = g.cell_of(0.05, 5.0);
        assert_eq!(
            g.cell(0, lx, ly),
            Cell::BlockedAll,
            "cell at the true board edge must be blocked"
        );
    }

    #[test]
    fn window_covers_its_rect_and_maps_interior_points() {
        use pcb_model::Rect;
        // A window must contain a cell for every interior point, and the cell↔mm
        // mapping round-trips within the window (the per-cell stitching contract
        // relies on endpoint snapping, not lattice alignment, so we only require
        // a consistent local mapping here).
        let p = problem(vec![]);
        let win = Rect {
            min_x: 3.3,
            min_y: 4.7,
            max_x: 8.1,
            max_y: 9.2,
        };
        let g = RouteGrid::build_window(&p, &win);
        for ix in 0..g.nx {
            for iy in 0..g.ny {
                let cx = g.cell_center_x(ix);
                let cy = g.cell_center_y(iy);
                assert_eq!(g.cell_of(cx, cy), (ix, iy), "window mapping round-trips");
            }
        }
        // An interior window point resolves to an in-range cell.
        let (ix, iy) = g.cell_of(5.0, 6.0);
        assert!(ix < g.nx && iy < g.ny);
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
