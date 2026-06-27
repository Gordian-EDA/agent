//! The placement cost the annealer minimizes (and the routability oracle's
//! secondary selection key). The cost carries a SILK-GAP term so parts keep room
//! for their reference designators (the recurring critic complaint). HPWL — the
//! cheap quality number every engine reports — lives in the kernel
//! ([`pcb_model::place::compute_hpwl`]) and is re-exported here.

use super::model::{LogicalNet, Pin, PlaceProblem};
use crate::problem::{Point2, Rect};

pub(crate) use crate::problem::place::compute_hpwl_with_rotations;

/// SA cost weights (mm units), scaled like the schematic floorplan cost.
pub(crate) const SA_OVERLAP_W: f64 = 1000.0; // hard: courtyard collision
pub(crate) const SA_BOUNDS_W: f64 = 1000.0; // hard: out of board bounds
pub(crate) const SA_KEEPOUT_W: f64 = 1000.0; // hard: part overlapping a signal-layer keep-out
pub(crate) const SA_SILK_W: f64 = 6.0; // soft: parts crowding each other's refdes
pub(crate) const SA_WL_W: f64 = 0.4; // half-perimeter wirelength over physical pads
pub(crate) const SA_CROSS_W: f64 = 4.0; // soft: two-pin ratline crossings (routeability proxy)
pub(crate) const SA_OBSTRUCT_W: f64 = 1.5; // soft: ratline through foreign body / keepout
pub(crate) const SA_SPREAD_W: f64 = 0.25; // mild whole-board compaction
pub(crate) const SA_COHERE_W: f64 = 8.0; // decoupling cap → nearest anchor power pad (hug the IC).
// Deliberately ABOVE SA_SILK_W (refdes-crowding): a bypass cap hugging its IC is an
// electrical necessity that must outrank silk aesthetics, else a big cap (1210) next to a
// small IC (SOIC-8) gets pushed away by the crowding penalty and strands (critic-caught on
// power-buck). Targeted to detected decoupling PAIRS only, so it does not perturb parts
// with normal net springs.
pub(crate) const SA_EDGE_W: f64 = 2.5; // connector → nearest board edge
/// Breathing room (mm) a refdes needs around a part before it crowds a neighbour.
pub(crate) const SA_SILK_GAP: f64 = 1.0;

/// Distance from a decoupling cap's origin to the NEAREST power pad of its anchor
/// (the proximity a bypass cap should minimize). 0 if the anchor shares no pad net.
fn cap_anchor_dist(
    problem: &PlaceProblem,
    pos: &[Point2],
    rotations: &[f64],
    cap: usize,
    ic: usize,
) -> f64 {
    let cap_nets: Vec<&str> = problem.parts[cap]
        .pads
        .iter()
        .filter_map(|p| p.net.as_deref())
        .collect();
    let mut best = f64::MAX;
    for pad in &problem.parts[ic].pads {
        if pad.net.as_deref().is_some_and(|nn| cap_nets.contains(&nn)) {
            let off = pad.offset.rotate(rotations[ic]);
            let pad_pos = Point2 {
                x: pos[ic].x + off.x,
                y: pos[ic].y + off.y,
            };
            best = best.min(pos[cap].dist(pad_pos));
        }
    }
    if best.is_finite() { best } else { 0.0 }
}

/// Physical world position of one logical pin's pad under the current placement.
fn pin_pos(problem: &PlaceProblem, pos: &[Point2], rotations: &[f64], pin: &Pin) -> Point2 {
    let part = &problem.parts[pin.part];
    let pad = &part.pads[pin.pad];
    let off = pad.offset.rotate(rotations[pin.part]);
    Point2 {
        x: pos[pin.part].x + off.x,
        y: pos[pin.part].y + off.y,
    }
}

fn net_hpwl(problem: &PlaceProblem, rotations: &[f64], net: &LogicalNet, pos: &[Point2]) -> f64 {
    if net.pins.len() < 2 {
        return 0.0;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for pin in &net.pins {
        let p = pin_pos(problem, pos, rotations, pin);
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    (x1 - x0) + (y1 - y0)
}

/// Cheap routeability proxy: count strict crossings between ratline tree edges
/// over physical pad positions. HPWL alone cannot distinguish an untangled X from
/// a clean parallel pairing with the same bounding boxes; this term gives the
/// annealer a local signal that usually correlates with fewer detailed-router
/// conflicts. Pad coordinates matter here: off-centre pads and rotations can
/// cross even when part centres do not.
///
/// Two-pin nets stay one segment. Multi-pin nets use a deterministic
/// nearest-neighbour tree, which is a cheap stand-in for the connection tree a
/// router will eventually grow; without it a three-pin/common net can cut across
/// unrelated signals with no crossing penalty at all.
pub(crate) fn ratline_crossings(
    problem: &PlaceProblem,
    rotations: &[f64],
    nets: &[LogicalNet],
    pos: &[Point2],
) -> usize {
    let pin_positions = ratline_pin_positions(problem, rotations, nets, pos);
    let segs = ratline_tree_segments(nets, &pin_positions);

    let mut crossings = 0usize;
    for i in 0..segs.len() {
        let (net_a, a0, a1, sa) = segs[i];
        for &(net_b, b0, b1, sb) in &segs[i + 1..] {
            if net_a == net_b {
                continue;
            }
            // Nets sharing a component naturally meet at that component; do not
            // charge those as crossings.
            if a0 == b0 || a0 == b1 || a1 == b0 || a1 == b1 {
                continue;
            }
            if sa.intersects(sb) {
                crossings += 1;
            }
        }
    }
    crossings
}

/// Cheap congestion proxy: count ratline tree edges that run through a foreign
/// component courtyard or signal keepout. This catches the common placement shape
/// where HPWL is short and ratlines do not cross each other, but the straight
/// route corridor is occupied by another part and the router must detour.
pub(crate) fn ratline_obstruction_pressure(
    problem: &PlaceProblem,
    rotations: &[f64],
    nets: &[LogicalNet],
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
) -> usize {
    let pin_positions = ratline_pin_positions(problem, rotations, nets, pos);
    let segs = ratline_tree_segments(nets, &pin_positions);
    let mut pressure = 0usize;

    for &(_, a_part, b_part, seg) in &segs {
        for part_idx in 0..problem.parts.len() {
            if part_idx == a_part || part_idx == b_part {
                continue;
            }
            let courtyard =
                Rect::from_center_half(pos[part_idx], half[part_idx]).inflate(margin / 2.0);
            if courtyard.dist_to_segment(seg) <= geom::EPS {
                pressure += 1;
            }
        }
        for keepout in &problem.keepouts {
            if keepout.dist_to_segment(seg) <= geom::EPS {
                pressure += 1;
            }
        }
    }

    pressure
}

fn ratline_pin_positions(
    problem: &PlaceProblem,
    rotations: &[f64],
    nets: &[LogicalNet],
    pos: &[Point2],
) -> Vec<Vec<Point2>> {
    nets.iter()
        .map(|net| {
            net.pins
                .iter()
                .map(|pin| pin_pos(problem, pos, rotations, pin))
                .collect()
        })
        .collect()
}

fn ratline_tree_segments(
    nets: &[LogicalNet],
    pin_positions: &[Vec<Point2>],
) -> Vec<(usize, usize, usize, geom::Segment)> {
    let mut segs = Vec::new();
    for (net_idx, net) in nets.iter().enumerate() {
        match net.pins.as_slice() {
            [] | [_] => {}
            [a, b] => push_ratline_segment(
                &mut segs,
                net_idx,
                a,
                b,
                pin_positions[net_idx][0],
                pin_positions[net_idx][1],
            ),
            pins => {
                let mut in_tree = vec![false; pins.len()];
                in_tree[0] = true;
                for _ in 1..pins.len() {
                    let mut best: Option<(usize, usize, f64)> = None;
                    for (ai, _) in pins.iter().enumerate() {
                        if !in_tree[ai] {
                            continue;
                        }
                        let pa = pin_positions[net_idx][ai];
                        for (bi, _) in pins.iter().enumerate() {
                            if in_tree[bi] {
                                continue;
                            }
                            let dist = pa.dist(pin_positions[net_idx][bi]);
                            let replace = best.is_none_or(|(old_a, old_b, old_dist)| {
                                dist < old_dist - 1e-9
                                    || ((dist - old_dist).abs() <= 1e-9
                                        && (ai, bi) < (old_a, old_b))
                            });
                            if replace {
                                best = Some((ai, bi, dist));
                            }
                        }
                    }
                    let Some((ai, bi, _)) = best else {
                        break;
                    };
                    in_tree[bi] = true;
                    push_ratline_segment(
                        &mut segs,
                        net_idx,
                        &pins[ai],
                        &pins[bi],
                        pin_positions[net_idx][ai],
                        pin_positions[net_idx][bi],
                    );
                }
            }
        }
    }
    segs
}

fn push_ratline_segment(
    segs: &mut Vec<(usize, usize, usize, geom::Segment)>,
    net_idx: usize,
    a: &Pin,
    b: &Pin,
    a_pos: Point2,
    b_pos: Point2,
) {
    if a.part == b.part {
        return;
    }
    segs.push((net_idx, a.part, b.part, geom::Segment::new(a_pos, b_pos)));
}

/// The placement cost the SA minimizes (also the [`crate::placement::place_best`]
/// selection key, so the variant that genuinely lays out best is the one chosen).
/// Lower is better.
#[allow(clippy::too_many_arguments)] // internal SA cost kernel; arg-struct adds indirection without value
pub(crate) fn place_cost(
    problem: &PlaceProblem,
    nets: &[LogicalNet],
    half: &[(f64, f64)],
    margin: f64,
    rotations: &[f64],
    pairs: &[(usize, usize)],
    edge_idx: &[usize],
    pos: &[Point2],
) -> f64 {
    let n = problem.parts.len();
    let mut cost = 0.0;

    // Pairwise courtyard overlap (hard) + a soft silk gap so refdes don't crowd.
    for i in 0..n {
        let courtyard_i = Rect::from_center_half(pos[i], half[i]);
        for j in (i + 1)..n {
            let courtyard_j = Rect::from_center_half(pos[j], half[j]);
            let (ox, oy) = courtyard_i
                .inflate(margin / 2.0)
                .axis_penetration(&courtyard_j.inflate(margin / 2.0));
            if ox > 0.0 && oy > 0.0 {
                cost += SA_OVERLAP_W * ox.min(oy);
            } else {
                let silk = (margin + 2.0 * SA_SILK_GAP) / 2.0;
                let (sx, sy) = courtyard_i
                    .inflate(silk)
                    .axis_penetration(&courtyard_j.inflate(silk));
                if sx > 0.0 && sy > 0.0 {
                    cost += SA_SILK_W * sx.min(sy);
                }
            }
        }
    }

    // Out-of-bounds (hard).
    let b = &problem.bounds;
    for i in 0..n {
        let courtyard = Rect::from_center_half(pos[i], half[i]);
        let (dx, dy) = b.containment_overshoot(&courtyard);
        cost += SA_BOUNDS_W * (dx + dy);
    }

    // Keep-out overlap (hard): a part inside a signal-layer keep-out has trapped
    // pads. Penalize the penetration depth so the SA pushes parts clear.
    for i in 0..n {
        let courtyard = Rect::from_center_half(pos[i], half[i]);
        for k in &problem.keepouts {
            let (ox, oy) = courtyard.axis_penetration(k);
            if ox > 0.0 && oy > 0.0 {
                cost += SA_KEEPOUT_W * ox.min(oy);
            }
        }
    }

    // Half-perimeter wirelength over part centres + whole-board spread.
    let (mut gx0, mut gy0, mut gx1, mut gy1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in pos {
        gx0 = gx0.min(p.x);
        gy0 = gy0.min(p.y);
        gx1 = gx1.max(p.x);
        gy1 = gy1.max(p.y);
    }
    if gx1 >= gx0 {
        cost += SA_SPREAD_W * ((gx1 - gx0) + (gy1 - gy0));
    }
    for net in nets {
        if net.pins.len() < 2 {
            continue;
        }
        cost += SA_WL_W * net_hpwl(problem, rotations, net, pos);
    }
    cost += SA_CROSS_W * ratline_crossings(problem, rotations, nets, pos) as f64;
    cost += SA_OBSTRUCT_W
        * ratline_obstruction_pressure(problem, rotations, nets, half, margin, pos) as f64;

    // Decoupling cohesion + connector edge-seek.
    for &(cap, ic) in pairs {
        cost += SA_COHERE_W * cap_anchor_dist(problem, pos, rotations, cap, ic);
    }
    for &i in edge_idx {
        let h = half[i];
        let dl = (pos[i].x - h.0) - b.min_x;
        let dr = b.max_x - (pos[i].x + h.0);
        let dt = (pos[i].y - h.1) - b.min_y;
        let db = b.max_y - (pos[i].y + h.1);
        cost += SA_EDGE_W * dl.min(dr).min(dt).min(db).max(0.0);
    }
    cost
}
