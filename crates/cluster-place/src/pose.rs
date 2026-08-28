//! Hub POSE search — the lever the base engines never touch. Their move sets only
//! translate an anchor (carrying its block) and re-orient 2-pin satellites; a hub's
//! `angle`/`mirror` are seeded once from the IR and never searched. Yet which way an
//! IC faces — its rotation and its left↔right mirror — decides whether its pins meet
//! their neighbours head-on or force the wires to wrap around the body. This module
//! tries each hub's 8 poses, moving the hub AND its satellite cluster RIGIDLY so the
//! decoupling caps / pull-ups follow the rotated pins, and keeps the pose that lowers
//! the routed cost without breaking connectivity.
//!
//! The rigid transform is exact: a symbol's `(angle, mirror)` maps a local pin offset
//! to the sheet via [`geom::Point2::transform_offset`] (`x→−x` if mirror, then rotate
//! with the symbol-Y-into-sheet flip). Those 8 maps are orthogonal matrices with
//! entries in {−1,0,1} closed under composition, so the cluster's base→target transform
//! `Δ = M(target)·M(base)⁻¹` is itself one of the 8 — we apply Δ to every member's
//! position and compose it into each member's own pose, then decode back to KiCAD's
//! `(angle, mirror)`.

use std::collections::BTreeMap;

use geom::Point2;
use sch_place::ir::LayoutIr;
use sch_place::item::{Incidence, Item};

use sch_floorplan::contract::RoutedEvaluator;
use sch_floorplan::engine_support::{build_anchor_blocks, decongest};

use crate::eval::{restore, save, score};

type Mat = [[f64; 2]; 2];

/// The symbol-frame linear map for `(angle_deg, mirror)`, read straight off
/// `transform_offset`: column j is the image of basis vector e_j.
fn pose_mat(angle_deg: f64, mirror: bool) -> Mat {
    let (s, c) = angle_deg.to_radians().sin_cos();
    let mx = if mirror { -1.0 } else { 1.0 };
    // e1=(1,0) -> (mx*c, -mx*s); e2=(0,1) -> (-s, -c).
    [[mx * c, -s], [-mx * s, -c]]
}

fn matmul(a: Mat, b: Mat) -> Mat {
    let mut m = [[0.0; 2]; 2];
    for i in 0..2 {
        for j in 0..2 {
            m[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j];
        }
    }
    m
}

/// Inverse of an orthogonal pose matrix is its transpose.
fn transpose(a: Mat) -> Mat {
    [[a[0][0], a[1][0]], [a[0][1], a[1][1]]]
}

fn matvec(a: Mat, v: Point2) -> Point2 {
    Point2::new(a[0][0] * v.x + a[0][1] * v.y, a[1][0] * v.x + a[1][1] * v.y)
}

/// The 8 `(angle, mirror)` poses and their matrices, rounded to the exact {−1,0,1}
/// integer entries so floating sin/cos noise never breaks the match.
fn pose_table() -> Vec<(f64, bool, [[i8; 2]; 2])> {
    let mut t = Vec::new();
    for &a in &[0.0, 90.0, 180.0, 270.0] {
        for &m in &[false, true] {
            let mt = pose_mat(a, m);
            let mi = [
                [mt[0][0].round() as i8, mt[0][1].round() as i8],
                [mt[1][0].round() as i8, mt[1][1].round() as i8],
            ];
            t.push((a, m, mi));
        }
    }
    t
}

/// Decode a composed pose matrix back to KiCAD `(angle, mirror)`.
fn decode(l: Mat, table: &[(f64, bool, [[i8; 2]; 2])]) -> Option<(f64, bool)> {
    let li = [
        [l[0][0].round() as i8, l[0][1].round() as i8],
        [l[1][0].round() as i8, l[1][1].round() as i8],
    ];
    table
        .iter()
        .find(|(_, _, mi)| *mi == li)
        .map(|&(a, m, _)| (a, m))
}

/// Rigidly move hub `h` and its `members` from their CURRENT pose to `(target_angle,
/// target_mirror)`, pivoting on the hub's origin so its pins rotate but its centre
/// stays put. Returns false (no-op) if the composed pose can't be decoded (never
/// happens for the closed group, but keeps the caller total).
fn apply_pose(
    items: &mut [Item],
    h: usize,
    members: &[usize],
    target_angle: f64,
    target_mirror: bool,
    table: &[(f64, bool, [[i8; 2]; 2])],
) -> bool {
    let pivot = items[h].at;
    let base = pose_mat(items[h].angle, items[h].mirror);
    let delta = matmul(pose_mat(target_angle, target_mirror), transpose(base));
    // Decode each member's new pose up front; bail before mutating if any fails.
    let mut plan: Vec<(usize, Point2, f64, bool)> = Vec::with_capacity(members.len());
    for &m in members {
        let rel = Point2::new(items[m].at.x - pivot.x, items[m].at.y - pivot.y);
        let d = matvec(delta, rel);
        let new_at = Point2::new(pivot.x + d.x, pivot.y + d.y);
        let l = matmul(delta, pose_mat(items[m].angle, items[m].mirror));
        let Some((a, mir)) = decode(l, table) else {
            return false;
        };
        plan.push((m, new_at, a, mir));
    }
    for (m, at, a, mir) in plan {
        items[m].at = geom::GRID_50_MIL.snap_point(at);
        items[m].angle = a;
        items[m].mirror = mir;
    }
    true
}

/// Candidate poses for a hub. A WIDE multi-pin IC (a big MCU) is conventionally kept
/// upright — humans flip it left↔right but rarely stand it on end — so restrict it to
/// {0°,180°}×mirror; smaller hubs (connectors, 3–8 pin parts) may take any of the 8,
/// where a 90°/270° vertical pin row often faces a board edge.
fn candidate_poses(it: &Item) -> Vec<(f64, bool)> {
    let pins = it.geom.pins.len();
    let wide = {
        let xs: Vec<f64> = it.geom.pins.iter().map(|p| p.at.x).collect();
        let ys: Vec<f64> = it.geom.pins.iter().map(|p| p.at.y).collect();
        let w = xs.iter().cloned().fold(f64::MIN, f64::max)
            - xs.iter().cloned().fold(f64::MAX, f64::min);
        let h = ys.iter().cloned().fold(f64::MIN, f64::max)
            - ys.iter().cloned().fold(f64::MAX, f64::min);
        w > h
    };
    let angles: &[f64] = if pins >= 8 && wide {
        &[0.0, 180.0]
    } else {
        &[0.0, 90.0, 180.0, 270.0]
    };
    let mut out = Vec::new();
    for &a in angles {
        for &m in &[false, true] {
            out.push((a, m));
        }
    }
    out
}

/// Coordinate-descent over hub poses: sweep each hub, keep the pose that lowers the
/// base routed cost at zero truthfulness breaks (a flip can silently short two rails,
/// which the readability cost alone would miss), iterate to a fixpoint. The whole
/// cluster moves rigidly, so a satellite never strands — and `decongest` clears any
/// body overlap the rotation introduces before scoring.
pub(crate) fn search_hub_poses(
    eval: &RoutedEvaluator,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
) {
    let hubs: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3 && !items[i].frozen)
        .collect();
    if hubs.is_empty() {
        return;
    }
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
    let blocks: BTreeMap<usize, Vec<usize>> = build_anchor_blocks(items, inc, &hubs, &sats, ir);
    let table = pose_table();

    const MAX_SWEEPS: usize = 2;
    for _ in 0..MAX_SWEEPS {
        let mut improved = false;
        for &h in &hubs {
            // The rigid unit: the hub plus the satellites that tap it.
            let mut members = vec![h];
            if let Some(b) = blocks.get(&h) {
                members.extend(b.iter().copied().filter(|&m| !items[m].frozen));
            }
            let base = save(items);
            // Incumbent score on the SHIPPED (finalized) sheet — the pose search is
            // STRICTLY ADDITIVE against this, so it can never regress what ships.
            let mut best = score(eval, inc, ir, items);
            let mut best_snap = base.clone();
            let mut best_changed = false;
            for (a, mir) in candidate_poses(&items[h]) {
                if (a - items[h].angle).abs() < 1e-6 && mir == items[h].mirror {
                    continue; // current pose, already the incumbent
                }
                restore(items, &base);
                if !apply_pose(items, h, &members, a, mir, &table) {
                    continue;
                }
                decongest(items);
                let s = score(eval, inc, ir, items);
                // STRICT crossing/warning improvement only — pose's genuine win is making
                // pins meet neighbours head-on (fewer crossings/body-throughs). Accepting a
                // pose for a marginal straightness tiebreak moves the IC for no readable
                // gain and the vision critic can read the re-orientation as worse, so the
                // gate must see a real drop in the shipped (truthfulness, warnings, crossings).
                if (s.0, s.1, s.2) < (best.0, best.1, best.2) {
                    best = s;
                    best_snap = save(items);
                    best_changed = true;
                }
            }
            restore(items, &best_snap);
            improved |= best_changed;
        }
        if !improved {
            break;
        }
    }
}
