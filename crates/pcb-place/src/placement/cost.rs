//! The placement cost the annealer minimizes (and the routability oracle's
//! secondary selection key). The cost carries a SILK-GAP term so parts keep room
//! for their reference designators (the recurring critic complaint). HPWL — the
//! cheap quality number every engine reports — lives in the kernel
//! ([`place_model::compute_hpwl`]) and is re-exported here.

use super::geometry::{
    part_edge_distance, part_placement_bounds_envelope, placement_envelope_at, rotated_copper_bbox,
};
use place_model::{LogicalNet, Pin, PlaceProblem};
use crate::problem::{LayerRef, Point2, Rect};

pub(crate) use place_model::compute_hpwl_with_rotations;

/// SA cost weights (mm units), scaled like the schematic floorplan cost.
pub(crate) const SA_OVERLAP_W: f64 = 1000.0; // hard: courtyard collision
pub(crate) const SA_BOUNDS_W: f64 = 1000.0; // hard: out of board bounds
pub(crate) const SA_KEEPOUT_W: f64 = 1000.0; // hard: part overlapping a signal-layer keep-out
pub(crate) const SA_SILK_W: f64 = 6.0; // soft: parts crowding each other's refdes
pub(crate) const SA_WL_W: f64 = 0.4; // half-perimeter wirelength over physical pads
pub(crate) const SA_CROSS_W: f64 = 4.0; // soft: two-pin ratline crossings (routeability proxy)
pub(crate) const SA_OBSTRUCT_W: f64 = 1.5; // soft: ratline through foreign body / keepout
pub(crate) const SA_LAYER_CHANGE_W: f64 = 2.0; // soft: prefer same-layer pad pairings before vias
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
#[cfg(test)]
pub(crate) fn ratline_crossings(
    problem: &PlaceProblem,
    rotations: &[f64],
    nets: &[LogicalNet],
    pos: &[Point2],
) -> usize {
    let pin_positions = ratline_pin_positions(problem, rotations, nets, pos);
    let segs = ratline_tree_segments(problem, nets, &pin_positions);

    ratline_crossings_from_segments(&segs)
}

fn ratline_crossings_from_segments(segs: &[RatlineSegment]) -> usize {
    let mut crossings = 0usize;
    for i in 0..segs.len() {
        let a = &segs[i];
        for b in &segs[i + 1..] {
            if a.net_idx == b.net_idx {
                continue;
            }
            // Nets sharing a component naturally meet at that component; do not
            // charge those as crossings.
            if a.a_part == b.a_part
                || a.a_part == b.b_part
                || a.b_part == b.a_part
                || a.b_part == b.b_part
            {
                continue;
            }
            if ratline_layers_overlap(a, b) && a.segment.intersects(b.segment) {
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
#[cfg(test)]
pub(crate) fn ratline_obstruction_pressure(
    problem: &PlaceProblem,
    rotations: &[f64],
    nets: &[LogicalNet],
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
) -> usize {
    let pin_positions = ratline_pin_positions(problem, rotations, nets, pos);
    let segs = ratline_tree_segments(problem, nets, &pin_positions);

    ratline_obstruction_pressure_from_segments(problem, half, margin, pos, &segs)
}

#[cfg(test)]
pub(crate) fn ratline_layer_change_pressure(
    problem: &PlaceProblem,
    rotations: &[f64],
    nets: &[LogicalNet],
    pos: &[Point2],
) -> usize {
    let pin_positions = ratline_pin_positions(problem, rotations, nets, pos);
    let segs = ratline_tree_segments(problem, nets, &pin_positions);

    ratline_layer_change_pressure_from_segments(&segs)
}

fn ratline_layer_change_pressure_from_segments(segs: &[RatlineSegment]) -> usize {
    segs.iter().filter(|seg| seg.requires_layer_change).count()
}

fn ratline_obstruction_pressure_from_segments(
    problem: &PlaceProblem,
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
    segs: &[RatlineSegment],
) -> usize {
    let mut pressure = 0usize;

    for seg in segs {
        for part_idx in 0..problem.parts.len() {
            if part_idx == seg.a_part || part_idx == seg.b_part {
                continue;
            }
            let courtyard =
                Rect::from_center_half(pos[part_idx], half[part_idx]).inflate(margin / 2.0);
            if courtyard.dist_to_segment(seg.segment) <= geom::EPS {
                pressure += 1;
            }
        }
        for keepout in &problem.keepouts {
            if keepout.dist_to_segment(seg.segment) <= geom::EPS {
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

#[derive(Clone)]
struct RatlineSegment {
    net_idx: usize,
    a_part: usize,
    b_part: usize,
    layers: Vec<LayerRef>,
    requires_layer_change: bool,
    segment: geom::Segment,
}

fn ratline_tree_segments(
    problem: &PlaceProblem,
    nets: &[LogicalNet],
    pin_positions: &[Vec<Point2>],
) -> Vec<RatlineSegment> {
    let mut segs = Vec::new();
    for (net_idx, net) in nets.iter().enumerate() {
        match net.pins.as_slice() {
            [] | [_] => {}
            [a, b] => push_ratline_segment(
                &mut segs,
                problem,
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
                                    &pin_positions[net_idx],
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
                    push_ratline_segment(
                        &mut segs,
                        problem,
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

fn ratline_tree_edge_better(
    problem: &PlaceProblem,
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
    problem: &PlaceProblem,
    pins: &[Pin],
    a: usize,
    b: usize,
    old_a: usize,
    old_b: usize,
) -> bool {
    let layer_change = ratline_segment_requires_layer_change(problem, &pins[a], &pins[b]);
    let old_layer_change =
        ratline_segment_requires_layer_change(problem, &pins[old_a], &pins[old_b]);
    if layer_change != old_layer_change {
        !layer_change
    } else {
        (a, b) < (old_a, old_b)
    }
}

fn push_ratline_segment(
    segs: &mut Vec<RatlineSegment>,
    problem: &PlaceProblem,
    net_idx: usize,
    a: &Pin,
    b: &Pin,
    a_pos: Point2,
    b_pos: Point2,
) {
    if a.part == b.part {
        return;
    }
    segs.push(RatlineSegment {
        net_idx,
        a_part: a.part,
        b_part: b.part,
        layers: ratline_segment_layers(problem, a, b),
        requires_layer_change: ratline_segment_requires_layer_change(problem, a, b),
        segment: geom::Segment::new(a_pos, b_pos),
    });
}

fn ratline_segment_layers(problem: &PlaceProblem, a: &Pin, b: &Pin) -> Vec<LayerRef> {
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

fn ratline_segment_requires_layer_change(problem: &PlaceProblem, a: &Pin, b: &Pin) -> bool {
    let a_layers = &problem.parts[a.part].pads[a.pad].layers;
    let b_layers = &problem.parts[b.part].pads[b.pad].layers;
    !a_layers
        .iter()
        .any(|layer| b_layers.iter().any(|other| other == layer))
}

fn ratline_layers_overlap(a: &RatlineSegment, b: &RatlineSegment) -> bool {
    a.layers
        .iter()
        .any(|layer| b.layers.iter().any(|other| other == layer))
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
        let bounds_shape = if problem.parts[i].edge_datum.is_some() {
            let copper = rotated_copper_bbox(&problem.parts[i], rotations[i]);
            let envelope = part_placement_bounds_envelope(&problem.parts[i], half[i], copper);
            placement_envelope_at(pos[i], envelope)
        } else {
            Rect::from_center_half(pos[i], half[i])
        };
        let (dx, dy) = b.containment_overshoot(&bounds_shape);
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
    let pin_positions = ratline_pin_positions(problem, rotations, nets, pos);
    let ratline_segments = ratline_tree_segments(problem, nets, &pin_positions);
    cost += SA_CROSS_W * ratline_crossings_from_segments(&ratline_segments) as f64;
    cost += SA_OBSTRUCT_W
        * ratline_obstruction_pressure_from_segments(problem, half, margin, pos, &ratline_segments)
            as f64;
    cost +=
        SA_LAYER_CHANGE_W * ratline_layer_change_pressure_from_segments(&ratline_segments) as f64;

    // Decoupling cohesion + connector edge-seek.
    for &(cap, ic) in pairs {
        cost += SA_COHERE_W * cap_anchor_dist(problem, pos, rotations, cap, ic);
    }
    for &i in edge_idx {
        cost += SA_EDGE_W * part_edge_distance(&problem.parts[i], rotations[i], pos[i], b, half[i]);
    }
    cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement::geometry::courtyard_margin;
    use place_model::{Part, PartPad};
    use geom::Rect;
    use crate::problem::LayerRef;

    fn part(reference: &str, net: &str) -> Part {
        part_on(reference, net, vec![LayerRef::top()])
    }

    fn part_on(reference: &str, net: &str, layers: Vec<LayerRef>) -> Part {
        Part {
            reference: reference.to_owned(),
            courtyard_w: 1.0,
            courtyard_h: 1.0,
            pads: vec![PartPad {
                number: "1".to_owned(),
                offset: Point2 { x: 0.0, y: 0.0 },
                width: 0.4,
                height: 0.4,
                layers,
                net: Some(net.to_owned()),
            }],
            edge_datum: None,
            locked: None,
        }
    }

    #[test]
    fn shared_ratline_segments_match_public_pressure_proxies() {
        let problem = PlaceProblem {
            bounds: Rect {
                min_x: 0.0,
                max_x: 30.0,
                min_y: 0.0,
                max_y: 30.0,
            },
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            keepouts: vec![],
            parts: vec![
                part("A", "N1"),
                part("B", "N1"),
                part("C", "N2"),
                part("D", "N2"),
                part("X", "FLOAT"),
            ],
            outline: None,
        };
        let nets = place_model::derive_nets(&problem);
        let rotations = vec![0.0; problem.parts.len()];
        let half = vec![(0.5, 0.5); problem.parts.len()];
        let margin = courtyard_margin(problem.clearance);
        let pos = vec![
            Point2 { x: 5.0, y: 5.0 },
            Point2 { x: 25.0, y: 25.0 },
            Point2 { x: 5.0, y: 25.0 },
            Point2 { x: 25.0, y: 5.0 },
            Point2 { x: 15.0, y: 15.0 },
        ];
        let pin_positions = ratline_pin_positions(&problem, &rotations, &nets, &pos);
        let segments = ratline_tree_segments(&problem, &nets, &pin_positions);

        assert_eq!(
            ratline_crossings_from_segments(&segments),
            ratline_crossings(&problem, &rotations, &nets, &pos)
        );
        assert_eq!(
            ratline_obstruction_pressure_from_segments(&problem, &half, margin, &pos, &segments),
            ratline_obstruction_pressure(&problem, &rotations, &nets, &half, margin, &pos)
        );
    }

    #[test]
    fn ratline_crossings_ignore_disjoint_pad_layers() {
        let problem = PlaceProblem {
            bounds: Rect {
                min_x: 0.0,
                max_x: 30.0,
                min_y: 0.0,
                max_y: 30.0,
            },
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            keepouts: vec![],
            parts: vec![
                part_on("A", "TOP", vec![LayerRef::top()]),
                part_on("B", "TOP", vec![LayerRef::top()]),
                part_on("C", "BOT", vec![LayerRef::bottom()]),
                part_on("D", "BOT", vec![LayerRef::bottom()]),
                part_on("E", "MIXED", vec![LayerRef::top()]),
                part_on("F", "MIXED", vec![LayerRef::bottom()]),
            ],
            outline: None,
        };
        let nets = place_model::derive_nets(&problem);
        let rotations = vec![0.0; problem.parts.len()];
        let pos = vec![
            Point2 { x: 5.0, y: 15.0 },
            Point2 { x: 25.0, y: 15.0 },
            Point2 { x: 15.0, y: 5.0 },
            Point2 { x: 15.0, y: 25.0 },
            Point2 { x: 5.0, y: 5.0 },
            Point2 { x: 25.0, y: 25.0 },
        ];

        assert_eq!(
            ratline_crossings(&problem, &rotations, &nets, &pos),
            2,
            "top/bottom-only crossing should be ignored, while mixed-layer ratline crossings still count"
        );
    }

    #[test]
    fn ratline_layer_change_pressure_counts_disjoint_pad_layers() {
        let problem = PlaceProblem {
            bounds: Rect {
                min_x: 0.0,
                max_x: 30.0,
                min_y: 0.0,
                max_y: 30.0,
            },
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            keepouts: vec![],
            parts: vec![
                part_on("A", "VIA", vec![LayerRef::top()]),
                part_on("B", "VIA", vec![LayerRef::bottom()]),
                part_on("C", "SAME", vec![LayerRef::top()]),
                part_on("D", "SAME", vec![LayerRef::top()]),
                part_on("E", "THRU", vec![LayerRef::top(), LayerRef::bottom()]),
                part_on("F", "THRU", vec![LayerRef::bottom()]),
            ],
            outline: None,
        };
        let nets = place_model::derive_nets(&problem);
        let rotations = vec![0.0; problem.parts.len()];
        let pos = vec![
            Point2 { x: 5.0, y: 5.0 },
            Point2 { x: 25.0, y: 5.0 },
            Point2 { x: 5.0, y: 15.0 },
            Point2 { x: 25.0, y: 15.0 },
            Point2 { x: 5.0, y: 25.0 },
            Point2 { x: 25.0, y: 25.0 },
        ];

        assert_eq!(
            ratline_layer_change_pressure(&problem, &rotations, &nets, &pos),
            1,
            "only the top-to-bottom SMD pair should require an unavoidable layer change"
        );
    }

    #[test]
    fn multi_pin_ratline_tree_prefers_same_layer_edge_on_distance_tie() {
        let problem = PlaceProblem {
            bounds: Rect {
                min_x: 0.0,
                max_x: 30.0,
                min_y: 0.0,
                max_y: 30.0,
            },
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            keepouts: vec![],
            parts: vec![
                part_on("ROOT", "BUS", vec![LayerRef::top()]),
                part_on("BOTTOM", "BUS", vec![LayerRef::bottom()]),
                part_on("TOP", "BUS", vec![LayerRef::top()]),
            ],
            outline: None,
        };
        let nets = place_model::derive_nets(&problem);
        let rotations = vec![0.0; problem.parts.len()];
        let pos = vec![
            Point2 { x: 5.0, y: 5.0 },
            Point2 { x: 15.0, y: 5.0 },
            Point2 { x: 5.0, y: 15.0 },
        ];
        let pin_positions = ratline_pin_positions(&problem, &rotations, &nets, &pos);

        let segments = ratline_tree_segments(&problem, &nets, &pin_positions);

        assert_eq!(
            segments[0].b_part, 2,
            "equal-distance BUS tree should first connect the same-layer pad"
        );
        assert!(
            !segments[0].requires_layer_change,
            "the preferred equal-distance edge should not spend a via"
        );
    }

    #[test]
    fn place_cost_penalizes_layer_change_ratlines() {
        let same_layer = PlaceProblem {
            bounds: Rect {
                min_x: 0.0,
                max_x: 20.0,
                min_y: 0.0,
                max_y: 10.0,
            },
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            keepouts: vec![],
            parts: vec![
                part_on("A", "N", vec![LayerRef::top()]),
                part_on("B", "N", vec![LayerRef::top()]),
            ],
            outline: None,
        };
        let mut split_layer = same_layer.clone();
        split_layer.parts[1].pads[0].layers = vec![LayerRef::bottom()];

        let rotations = vec![0.0, 0.0];
        let half = vec![(0.5, 0.5), (0.5, 0.5)];
        let margin = courtyard_margin(same_layer.clearance);
        let pos = vec![Point2 { x: 5.0, y: 5.0 }, Point2 { x: 15.0, y: 5.0 }];
        let same_nets = place_model::derive_nets(&same_layer);
        let split_nets = place_model::derive_nets(&split_layer);

        let same_cost = place_cost(
            &same_layer,
            &same_nets,
            &half,
            margin,
            &rotations,
            &[],
            &[],
            &pos,
        );
        let split_cost = place_cost(
            &split_layer,
            &split_nets,
            &half,
            margin,
            &rotations,
            &[],
            &[],
            &pos,
        );

        assert!(
            split_cost > same_cost,
            "otherwise identical placement should prefer a same-layer route: {same_cost} vs {split_cost}"
        );
        assert!(
            (split_cost - same_cost - SA_LAYER_CHANGE_W).abs() < 1e-9,
            "layer-change pressure should add the configured soft via penalty"
        );
    }
}
