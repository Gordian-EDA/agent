//! The legalizer (the spiral overlap-resolver + deterministic initial grid). The
//! exact-geometry legality check ([`is_legal`]) lives in the kernel
//! ([`pcb_model::place`]) so a third-party placer self-verifies with it; it is
//! re-exported here.
//!
//! `legalize` snaps movable parts to the grid then resolves residual courtyard
//! overlap by a deterministic nearest-free-cell spiral; `is_legal` then RE-VERIFIES
//! the result in exact geometry — the algorithm's verdict is never trusted.

use super::geometry::{PLACE_GRID, SPIRAL_MAX_RING, clamp_into_bounds, snap};
use super::model::PlaceProblem;
use crate::problem::{Point2, Rect};

pub(crate) use crate::problem::place::is_legal;

/// Outcome counters from [`legalize`].
pub(crate) struct LegalizeStats {
    pub(crate) overlaps_resolved: usize,
    pub(crate) out_of_bounds_clamps: usize,
}

/// Snap movable parts to the placement grid, then resolve residual courtyard
/// overlaps by a deterministic nearest-free-cell spiral, processing parts
/// area-descending (big parts seat first). Locked parts are fixed obstacles.
///
/// Records (and never trusts) — legality is re-checked by [`is_legal`] after.
pub(crate) fn legalize(
    problem: &PlaceProblem,
    half: &[(f64, f64)],
    margin: f64,
    pos: &mut [Point2],
) -> LegalizeStats {
    let n = problem.parts.len();
    let locked: Vec<bool> = problem.parts.iter().map(|p| p.locked.is_some()).collect();

    let mut out_of_bounds_clamps = 0;
    // Snap + clamp movable parts.
    for i in 0..n {
        if locked[i] {
            continue;
        }
        let before = pos[i].clone();
        pos[i].x = snap(pos[i].x);
        pos[i].y = snap(pos[i].y);
        clamp_into_bounds(&mut pos[i], &problem.bounds, half[i]);
        if (pos[i].x - before.x).abs() > PLACE_GRID || (pos[i].y - before.y).abs() > PLACE_GRID {
            // A real bounds clamp (more than a snap's worth of motion).
            out_of_bounds_clamps += 1;
        }
    }

    // Process order: area-descending, ties by reference (deterministic). Locked
    // parts are placed (immovable) first as obstacles by seeding `placed`.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        let aa = half[a].0 * half[a].1;
        let ab = half[b].0 * half[b].1;
        ab.partial_cmp(&aa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| problem.parts[a].reference.cmp(&problem.parts[b].reference))
    });

    let mut placed: Vec<usize> = Vec::new();
    // Seat locked parts first (they are immovable obstacles for everyone).
    for &i in &order {
        if locked[i] {
            placed.push(i);
        }
    }

    let mut overlaps_resolved = 0;
    for &i in &order {
        if locked[i] {
            continue;
        }
        // Does the snapped cell collide with anything already placed?
        if !collides(&pos[i], half[i], pos, half, margin, &placed) {
            placed.push(i);
            continue;
        }
        // Spiral out from the snapped cell for the nearest free grid cell.
        let origin = pos[i].clone();
        if let Some(found) = spiral_free_cell(problem, half, margin, i, &placed, &origin, pos) {
            pos[i] = found;
            overlaps_resolved += 1;
            placed.push(i);
        } else {
            // No legal cell within the cap: leave it (is_legal will flag the
            // board) and still mark it placed so others route around its spot.
            placed.push(i);
        }
    }

    LegalizeStats {
        overlaps_resolved,
        out_of_bounds_clamps,
    }
}

/// Spiral outward from `origin` (on the placement grid) for the nearest cell
/// where part `i` collides with none of `placed` and stays in bounds. Rings are
/// probed in increasing Chebyshev radius; within a ring, cells are visited in a
/// fixed (sorted) order for determinism. `None` if nothing is found within
/// [`SPIRAL_MAX_RING`] rings.
fn spiral_free_cell(
    problem: &PlaceProblem,
    half: &[(f64, f64)],
    margin: f64,
    i: usize,
    placed: &[usize],
    origin: &Point2,
    pos: &[Point2],
) -> Option<Point2> {
    for ring in 1..=SPIRAL_MAX_RING {
        // Collect this ring's offsets, sorted deterministically: by squared
        // distance, then dy, then dx — so the nearest cell (fixed tiebreak) wins.
        let mut cells: Vec<(i64, i64)> = Vec::new();
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs() == ring || dy.abs() == ring {
                    cells.push((dx, dy));
                }
            }
        }
        cells.sort_by_key(|&(dx, dy)| (dx * dx + dy * dy, dy, dx));
        for (dx, dy) in cells {
            let cand = Point2 {
                x: snap(origin.x + dx as f64 * PLACE_GRID),
                y: snap(origin.y + dy as f64 * PLACE_GRID),
            };
            // Must fit in bounds without clamping (clamping would move it off
            // the probed cell and could re-collide).
            if !problem
                .bounds
                .contains_rect_eps(&Rect::from_center_half(cand, half[i]), 1e-9)
            {
                continue;
            }
            if !collides(&cand, half[i], pos, half, margin, placed) {
                return Some(cand);
            }
        }
    }
    None
}

/// Does a part at `cand` with half-extent `cand_half` margin-overlap any part in
/// `placed` (whose positions are `pos[j]`, half-extents `half[j]`)?
pub(crate) fn collides(
    cand: &Point2,
    cand_half: (f64, f64),
    pos: &[Point2],
    half: &[(f64, f64)],
    margin: f64,
    placed: &[usize],
) -> bool {
    placed.iter().any(|&j| {
        let a = Rect::from_center_half(*cand, cand_half).inflate(margin / 2.0);
        let b = Rect::from_center_half(pos[j], half[j]).inflate(margin / 2.0);
        let (ox, oy) = a.axis_penetration(&b);
        ox > 0.0 && oy > 0.0
    })
}

/// Deterministic initial layout: parts (sorted by reference) on a near-square
/// grid sized to the largest courtyard, anchored at the board's top-left inset.
/// No RNG — the grid is a pure function of the parts.
pub(crate) fn initial_grid(problem: &PlaceProblem, half: &[(f64, f64)]) -> Vec<Point2> {
    let n = problem.parts.len();
    let mut pos = vec![Point2 { x: 0.0, y: 0.0 }; n];
    if n == 0 {
        return pos;
    }

    // Sort indices by reference for a deterministic cell assignment.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| problem.parts[a].reference.cmp(&problem.parts[b].reference));

    // Cell pitch = largest courtyard extent + a margin, snapped to the grid.
    let max_half = half.iter().map(|(w, h)| w.max(*h)).fold(0.0_f64, f64::max);
    let pitch = snap(
        (max_half * 2.0 + super::geometry::courtyard_margin(problem.clearance)).max(PLACE_GRID),
    ) + PLACE_GRID;

    let cols = (n as f64).sqrt().ceil().max(1.0) as usize;
    let b = &problem.bounds;
    let x0 = b.min_x + max_half + PLACE_GRID;
    let y0 = b.min_y + max_half + PLACE_GRID;

    for (rank, &i) in order.iter().enumerate() {
        let r = rank / cols;
        let c = rank % cols;
        let mut p = Point2 {
            x: x0 + c as f64 * pitch,
            y: y0 + r as f64 * pitch,
        };
        clamp_into_bounds(&mut p, b, half[i]);
        pos[i] = p;
    }
    pos
}
