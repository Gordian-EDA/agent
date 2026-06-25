//! Simulated-annealing placement refinement.
//!
//! A direct analog of the schematic floorplan SA (`sch-floorplan::floorplan`): from
//! the force-directed seed, anneal part positions to minimize an explicit cost
//! ([`super::cost::place_cost`]), escaping the local minima the springs settle into.
//! Crucially the SA OWNS its cost (overlap included), so — unlike the reverted
//! spring/halo heuristics — the legalizer never has to fight it: the annealed state
//! is already near-legal and the final `legalize` only nudges.

use super::cost::place_cost;
use super::geometry::{clamp_into_bounds, snap};
use super::model::{LogicalNet, PlaceProblem, PlacementHints};
use super::pairs::coplacement_pairs;
use crate::problem::Point2;

/// Fixed seed — placement is deterministic (same board → same layout).
const SA_SEED: u64 = 0xB5AD_C0DE_1234_5678;

/// Deterministic SplitMix64 (no `rand`, no clock — reproducible placement).
pub(crate) struct SaRng(pub(crate) u64);
impl SaRng {
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub(crate) fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
    }
    pub(crate) fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }
    pub(crate) fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + self.unit() * (hi - lo)
    }
}

/// Anneal `pos` (the force-directed seed) to a lower [`place_cost`]. Metropolis
/// acceptance with a linearly-cooled temperature; move set = relocate a part,
/// swap two parts, or shift a whole decoupling cluster (anchor + its caps).
/// Locked parts never move. Deterministic.
pub(crate) fn anneal_placement(
    problem: &PlaceProblem,
    hints: &PlacementHints,
    nets: &[LogicalNet],
    half: &[(f64, f64)],
    margin: f64,
    rotations: &[f64],
    pos: &mut [Point2],
) {
    let n = problem.parts.len();
    let movable: Vec<usize> =
        (0..n).filter(|&i| problem.parts[i].locked.is_none()).collect();
    if movable.len() < 2 {
        return;
    }
    let pairs = coplacement_pairs(problem);
    let edge_idx: Vec<usize> = hints
        .edge_seek
        .iter()
        .filter_map(|r| problem.parts.iter().position(|p| &p.reference == r))
        .collect();
    // Clusters for the block move: anchor → its caps.
    let mut clusters: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for &(cap, ic) in &pairs {
        if problem.parts[cap].locked.is_none() {
            clusters.entry(ic).or_default().push(cap);
        }
    }
    let anchors: Vec<usize> = clusters.keys().copied().collect();

    let mut rng = SaRng(SA_SEED);
    let iters = (250 * movable.len()).clamp(1000, 8000);
    let t0 = 8.0;
    let cost_of = |p: &[Point2]| place_cost(problem, nets, half, margin, rotations, &pairs, &edge_idx, p);
    let mut cost = cost_of(pos);

    let mut restore: Vec<(usize, Point2)> = Vec::with_capacity(8);
    for it in 0..iters {
        let t = (t0 * (1.0 - it as f64 / iters as f64)).max(0.05);
        restore.clear();
        let kind = rng.below(10);
        if kind < 7 {
            // Relocate one part; amplitude shrinks as the board cools.
            let k = movable[rng.below(movable.len())];
            restore.push((k, pos[k].clone()));
            let amp = 0.5 + 5.0 * (t / t0);
            pos[k].x = snap(pos[k].x + rng.range(-amp, amp));
            pos[k].y = snap(pos[k].y + rng.range(-amp, amp));
            clamp_into_bounds(&mut pos[k], &problem.bounds, half[k]);
        } else if kind < 9 || anchors.is_empty() {
            // Swap two parts.
            let a = movable[rng.below(movable.len())];
            let b = movable[rng.below(movable.len())];
            if a == b {
                continue;
            }
            restore.push((a, pos[a].clone()));
            restore.push((b, pos[b].clone()));
            pos.swap(a, b);
            clamp_into_bounds(&mut pos[a], &problem.bounds, half[a]);
            clamp_into_bounds(&mut pos[b], &problem.bounds, half[b]);
        } else {
            // Shift a whole decoupling cluster (anchor + caps) rigidly.
            let ic = anchors[rng.below(anchors.len())];
            let amp = 0.5 + 3.0 * (t / t0);
            let (dx, dy) = (rng.range(-amp, amp), rng.range(-amp, amp));
            let mut members = vec![ic];
            members.extend(clusters.get(&ic).into_iter().flatten().copied());
            for &m in &members {
                restore.push((m, pos[m].clone()));
                pos[m].x = snap(pos[m].x + dx);
                pos[m].y = snap(pos[m].y + dy);
                clamp_into_bounds(&mut pos[m], &problem.bounds, half[m]);
            }
        }
        let new_cost = cost_of(pos);
        let d = new_cost - cost;
        if d < 0.0 || rng.unit() < (-d / t).exp() {
            cost = new_cost;
        } else {
            for (i, p) in restore.drain(..) {
                pos[i] = p;
            }
        }
    }
}
