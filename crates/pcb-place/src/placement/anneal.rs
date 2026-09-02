//! Simulated-annealing placement refinement.
//!
//! A direct analog of the schematic floorplan SA (`sch-floorplan::floorplan`): from
//! the force-directed seed, anneal part positions to minimize an explicit cost
//! ([`super::cost::place_cost`]), escaping the local minima the springs settle into.
//! Crucially the SA OWNS its cost (overlap included), so — unlike the reverted
//! spring/halo heuristics — the legalizer never has to fight it: the annealed state
//! is already near-legal and the final `legalize` only nudges.

use super::cost::{CostTerms, place_cost};
use super::geometry::{PLACEMENT_GRID, courtyard_margin, rotated_courtyard_half};
use super::pairs::coplacement_pairs;
use crate::{LogicalNet, Pin, PlacementHints, PlacementView};
use pcb_model::{LayerRef, Point2, Rect};

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
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
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
/// swap two parts, or shift a whole decoupling cluster (anchor + its caps). A final
/// deterministic swap polish accepts any pairwise swap that still lowers the same
/// cost after the random search cools. Locked parts never move. Deterministic.
pub(crate) fn anneal_placement(
    problem: &PlacementView,
    hints: &PlacementHints,
    nets: &[LogicalNet],
    half: &[(f64, f64)],
    margin: f64,
    rotations: &[f64],
    pos: &mut [Point2],
) {
    let n = problem.parts.len();
    let movable: Vec<usize> = (0..n)
        .filter(|&i| problem.parts[i].locked.is_none())
        .collect();
    if movable.len() < 2 {
        return;
    }
    let pairs = coplacement_pairs(problem);
    let terms = CostTerms::new(problem, hints, pairs.clone());
    // Clusters for the block move: anchor → its caps.
    let mut clusters: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for &(cap, ic) in &pairs {
        // A rigid cluster move includes the anchor and its caps. Only build it
        // when the anchor is movable; a locked IC may still attract caps through
        // the cohesion term and ordinary cap relocations, but it must never be
        // translated by a block move.
        if problem.parts[cap].locked.is_none() && problem.parts[ic].locked.is_none() {
            clusters.entry(ic).or_default().push(cap);
        }
    }
    let anchors: Vec<usize> = clusters.keys().copied().collect();

    let mut rng = SaRng(SA_SEED);
    let iters = (250 * movable.len()).clamp(1000, 8000);
    let t0 = 8.0;
    let cost_of =
        |p: &[Point2]| place_cost(problem, nets, half, margin, rotations, &terms, p);
    let mut cost = cost_of(pos);

    let mut restore: Vec<(usize, Point2)> = Vec::with_capacity(8);
    for it in 0..iters {
        let t = (t0 * (1.0 - it as f64 / iters as f64)).max(0.05);
        restore.clear();
        let kind = rng.below(10);
        if kind < 7 {
            // Relocate one part; amplitude shrinks as the board cools.
            let k = movable[rng.below(movable.len())];
            restore.push((k, pos[k]));
            let amp = 0.5 + 5.0 * (t / t0);
            pos[k].x = PLACEMENT_GRID.snap(pos[k].x + rng.range(-amp, amp));
            pos[k].y = PLACEMENT_GRID.snap(pos[k].y + rng.range(-amp, amp));
            pos[k] = problem.bounds.clamp_center_for_half(pos[k], half[k]);
        } else if kind < 9 || anchors.is_empty() {
            // Swap two parts.
            let a = movable[rng.below(movable.len())];
            let b = movable[rng.below(movable.len())];
            if a == b {
                continue;
            }
            restore.push((a, pos[a]));
            restore.push((b, pos[b]));
            pos.swap(a, b);
            pos[a] = problem.bounds.clamp_center_for_half(pos[a], half[a]);
            pos[b] = problem.bounds.clamp_center_for_half(pos[b], half[b]);
        } else {
            // Shift a whole decoupling cluster (anchor + caps) rigidly.
            let ic = anchors[rng.below(anchors.len())];
            let amp = 0.5 + 3.0 * (t / t0);
            let (dx, dy) = (rng.range(-amp, amp), rng.range(-amp, amp));
            let mut members = vec![ic];
            members.extend(clusters.get(&ic).into_iter().flatten().copied());
            for &m in &members {
                restore.push((m, pos[m]));
                pos[m].x = PLACEMENT_GRID.snap(pos[m].x + dx);
                pos[m].y = PLACEMENT_GRID.snap(pos[m].y + dy);
                pos[m] = problem.bounds.clamp_center_for_half(pos[m], half[m]);
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

    let swap_order = routing_aware_swap_order(problem, nets, rotations, pos, &movable);
    greedy_swap_polish_with_order(problem, half, &swap_order, pos, &mut cost, cost_of);
}

/// Deterministic 2-opt polish: SA can cool with two movable parts assigned to
/// crossed ratlines or suboptimal sides of a local cluster. Sweep the given pairs
/// and keep only strict cost improvements. Two passes are enough to cascade a
/// local improvement without turning this into another O(n^3) search.
pub(crate) fn greedy_swap_polish_with_order<F>(
    problem: &PlacementView,
    half: &[(f64, f64)],
    swap_order: &[(usize, usize)],
    pos: &mut [Point2],
    cost: &mut f64,
    cost_of: F,
) where
    F: Fn(&[Point2]) -> f64,
{
    for _ in 0..2 {
        let mut improved = false;
        for &(a, b) in swap_order {
            let old_a = pos[a];
            let old_b = pos[b];
            pos.swap(a, b);
            pos[a] = problem.bounds.clamp_center_for_half(pos[a], half[a]);
            pos[b] = problem.bounds.clamp_center_for_half(pos[b], half[b]);
            let new_cost = cost_of(pos);
            if new_cost + 1e-9 < *cost {
                *cost = new_cost;
                improved = true;
            } else {
                pos[a] = old_a;
                pos[b] = old_b;
            }
        }
        if !improved {
            break;
        }
    }
}

pub(crate) fn routing_aware_swap_order(
    problem: &PlacementView,
    nets: &[LogicalNet],
    rotations: &[f64],
    pos: &[Point2],
    movable: &[usize],
) -> Vec<(usize, usize)> {
    let movable_set: std::collections::BTreeSet<usize> = movable.iter().copied().collect();
    let mut connected = std::collections::BTreeSet::new();
    for net in nets {
        for i in 0..net.pins.len() {
            for j in i + 1..net.pins.len() {
                push_pair_if_movable(
                    &mut connected,
                    &movable_set,
                    net.pins[i].part,
                    net.pins[j].part,
                );
            }
        }
    }

    let edges: Vec<_> = nets
        .iter()
        .enumerate()
        .flat_map(|(net_idx, net)| ratline_tree_edges(problem, rotations, pos, net_idx, net))
        .collect();
    let mut crossing_related = std::collections::BTreeSet::new();
    for i in 0..edges.len() {
        let a = &edges[i];
        for b in &edges[i + 1..] {
            if a.net_idx == b.net_idx {
                continue;
            }
            if a.a_part == b.a_part
                || a.a_part == b.b_part
                || a.b_part == b.a_part
                || a.b_part == b.b_part
            {
                continue;
            }
            if ratline_edge_layers_overlap(a, b)
                && geom::Segment::new(a.a_pos, a.b_pos)
                    .intersects(geom::Segment::new(b.a_pos, b.b_pos))
            {
                for pa in [a.a_part, a.b_part] {
                    for pb in [b.a_part, b.b_part] {
                        push_pair_if_movable(&mut crossing_related, &movable_set, pa, pb);
                    }
                }
            }
        }
    }

    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(rotations)
        .map(|(part, &rotation)| rotated_courtyard_half(part, rotation))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let mut obstructing_parts = std::collections::BTreeSet::new();
    for edge in &edges {
        let segment = geom::Segment::new(edge.a_pos, edge.b_pos);
        for &part_idx in movable {
            if part_idx == edge.a_part || part_idx == edge.b_part {
                continue;
            }
            let obstacle =
                Rect::from_center_half(pos[part_idx], half[part_idx]).inflate(margin / 2.0);
            if obstacle.dist_to_segment(segment) <= geom::EPS {
                obstructing_parts.insert(part_idx);
            }
        }
    }

    let mut pairs = Vec::new();
    for ai in 0..movable.len() {
        for bi in ai + 1..movable.len() {
            let a = movable[ai];
            let b = movable[bi];
            let key = ordered_pair(a, b);
            let rank = if connected.contains(&key) {
                0
            } else if crossing_related.contains(&key) {
                1
            } else if obstructing_parts.contains(&a) || obstructing_parts.contains(&b) {
                2
            } else {
                3
            };
            pairs.push((rank, a, b));
        }
    }
    pairs.sort_unstable();
    pairs.into_iter().map(|(_, a, b)| (a, b)).collect()
}

#[derive(Debug, Clone)]
struct RatlineEdge {
    net_idx: usize,
    a_part: usize,
    b_part: usize,
    layers: Vec<LayerRef>,
    a_pos: Point2,
    b_pos: Point2,
}

fn ratline_tree_edges<'a>(
    problem: &'a PlacementView,
    rotations: &'a [f64],
    pos: &'a [Point2],
    net_idx: usize,
    net: &'a LogicalNet,
) -> Vec<RatlineEdge> {
    match net.pins.as_slice() {
        [] | [_] => Vec::new(),
        [a, b] => ratline_edge(problem, rotations, pos, net_idx, a, b)
            .into_iter()
            .collect(),
        pins => {
            let pin_positions: Vec<Point2> = pins
                .iter()
                .map(|pin| pin_world_pos(problem, rotations, pos, pin))
                .collect();
            let mut edges = Vec::with_capacity(pins.len().saturating_sub(1));
            let mut in_tree = vec![false; pins.len()];
            in_tree[0] = true;
            for _ in 1..pins.len() {
                let mut best: Option<(usize, usize)> = None;
                for (ai, _) in pins.iter().enumerate() {
                    if !in_tree[ai] {
                        continue;
                    }
                    for (bi, _) in pins.iter().enumerate() {
                        if in_tree[bi] {
                            continue;
                        }
                        let replace = best.is_none_or(|(old_a, old_b)| {
                            ratline_tree_edge_better(
                                problem,
                                pins,
                                &pin_positions,
                                ai,
                                bi,
                                old_a,
                                old_b,
                            )
                        });
                        if replace {
                            best = Some((ai, bi));
                        }
                    }
                }
                let Some((ai, bi)) = best else {
                    break;
                };
                in_tree[bi] = true;
                if let Some(edge) =
                    ratline_edge(problem, rotations, pos, net_idx, &pins[ai], &pins[bi])
                {
                    edges.push(edge);
                }
            }
            edges
        }
    }
}

fn ratline_tree_edge_better(
    problem: &PlacementView,
    pins: &[Pin],
    pin_positions: &[Point2],
    a: usize,
    b: usize,
    old_a: usize,
    old_b: usize,
) -> bool {
    let dist = pin_positions[a].dist(pin_positions[b]);
    let old_dist = pin_positions[old_a].dist(pin_positions[old_b]);
    dist < old_dist - 1e-9
        || ((dist - old_dist).abs() <= 1e-9
            && ratline_tree_edge_tiebreak(problem, pins, a, b, old_a, old_b))
}

fn ratline_tree_edge_tiebreak(
    problem: &PlacementView,
    pins: &[Pin],
    a: usize,
    b: usize,
    old_a: usize,
    old_b: usize,
) -> bool {
    let layer_change = ratline_edge_requires_layer_change(problem, &pins[a], &pins[b]);
    let old_layer_change = ratline_edge_requires_layer_change(problem, &pins[old_a], &pins[old_b]);
    if layer_change != old_layer_change {
        !layer_change
    } else {
        (a, b) < (old_a, old_b)
    }
}

fn ratline_edge(
    problem: &PlacementView,
    rotations: &[f64],
    pos: &[Point2],
    net_idx: usize,
    a: &Pin,
    b: &Pin,
) -> Option<RatlineEdge> {
    if a.part == b.part {
        return None;
    }
    Some(RatlineEdge {
        net_idx,
        a_part: a.part,
        b_part: b.part,
        layers: ratline_edge_layers(problem, a, b),
        a_pos: pin_world_pos(problem, rotations, pos, a),
        b_pos: pin_world_pos(problem, rotations, pos, b),
    })
}

fn ratline_edge_requires_layer_change(problem: &PlacementView, a: &Pin, b: &Pin) -> bool {
    let a_layers = &problem.parts[a.part].pads[a.pad].layers;
    let b_layers = &problem.parts[b.part].pads[b.pad].layers;
    !a_layers
        .iter()
        .any(|layer| b_layers.iter().any(|other| other == layer))
}

fn ratline_edge_layers(problem: &PlacementView, a: &Pin, b: &Pin) -> Vec<LayerRef> {
    let mut layers = Vec::new();
    for pin in [a, b] {
        for layer in &problem.parts[pin.part].pads[pin.pad].layers {
            if !layers.iter().any(|existing| existing == layer) {
                layers.push(layer.clone());
            }
        }
    }
    layers
}

fn ratline_edge_layers_overlap(a: &RatlineEdge, b: &RatlineEdge) -> bool {
    a.layers
        .iter()
        .any(|layer| b.layers.iter().any(|other| other == layer))
}

fn pin_world_pos(problem: &PlacementView, rotations: &[f64], pos: &[Point2], pin: &Pin) -> Point2 {
    let off = problem.parts[pin.part].pads[pin.pad]
        .offset
        .rotate(rotations[pin.part]);
    Point2 {
        x: pos[pin.part].x + off.x,
        y: pos[pin.part].y + off.y,
    }
}

fn push_pair_if_movable(
    pairs: &mut std::collections::BTreeSet<(usize, usize)>,
    movable: &std::collections::BTreeSet<usize>,
    a: usize,
    b: usize,
) {
    if a != b && movable.contains(&a) && movable.contains(&b) {
        pairs.insert(ordered_pair(a, b));
    }
}

fn ordered_pair(a: usize, b: usize) -> (usize, usize) {
    if a < b { (a, b) } else { (b, a) }
}
