//! The single tuned placement phase and its internal optimization passes.

use super::anneal::anneal_placement;
use super::cost::{CostTerms, compute_hpwl_with_rotations, place_cost};
use super::force::{force_layout, snap_caps_to_anchor_ring};
use super::geometry::{
    PLACEMENT_GRID, clamp_center_for_envelope, courtyard_margin, datum_edge_target,
    part_edge_target, part_placement_bounds_envelope, rotated_copper_bbox, rotated_courtyard_half,
};
use super::hints::{apply_grid_hints, unified_fanout_place};
use super::legalize::{initial_grid, is_legal, legalize};
use crate::decoupling_pairs;
use crate::{
    Edge, Pin, PlaceReport, PlaceResult, Placement, PlacementHints, PlacementView, derive_nets,
};
use pcb_model::{LayerRef, Point2, Rect};

/// Internal placement-phase tuning switches.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PlaceOpts {
    /// Pull each decoupling cap to hug its IC ([`super::pairs::decoupling_pairs`]) via
    /// the force spring, AND snap each cap to the nearest free ring slot around its
    /// anchor in the seed ([`snap_caps_to_anchor_ring`]). Off in the baseline, so
    /// the oracle keeps an unsnapped candidate to fall back to when snapping hurts
    /// routability.
    pub(crate) decouple: bool,
    /// Bias edge-seeking by part aspect: a tall connector goes to a side edge so
    /// its pad column lies along it, not the top where it pokes inward.
    pub(crate) aspect_edge: bool,
    /// Refine the force-directed seed with simulated annealing ([`anneal_placement`]):
    /// escapes local minima the springs settle into, and optimizes an explicit
    /// cost (overlap + wirelength + compactness + decoupling cohesion + a SILK GAP
    /// so reference designators don't collide). Mirrors the schematic floorplan SA.
    pub(crate) anneal: bool,
}

/// Gordian's single tuned placement algorithm.
pub fn place_tuned(problem: &PlacementView, hints: &PlacementHints) -> PlaceResult {
    let mut initialized = problem.clone();
    let structured = unified_fanout_place(&mut initialized, hints);
    if !structured {
        apply_grid_hints(&mut initialized, hints);
    }
    let mut result = place_variant(
        &initialized,
        hints,
        PlaceOpts {
            decouple: !structured,
            aspect_edge: true,
            anneal: !structured,
        },
    );
    seat_corner_seek_parts(problem, hints, &mut result);
    result
}

/// Move up to four `corner_seek` parts to a maximum-cardinality set of distinct,
/// legal board corners. The assignment is solved as a set instead of greedily:
/// one hole's old edge-seek position must not make another hole falsely reject a
/// corner that becomes free once both holes move. Among equally complete legal
/// assignments, maximize pairwise corner separation (two holes choose a
/// diagonal), then prefer the least total movement and stable corner order.
///
/// Safe on any placement: corner-seek parts (mounting holes) have no nets, so this
/// cannot change connectivity or routing.
pub(crate) fn seat_corner_seek_parts(
    problem: &PlacementView,
    hints: &PlacementHints,
    best: &mut PlaceResult,
) {
    if !best.legal {
        return;
    }
    let regioned = hints
        .groups
        .iter()
        .filter(|group| group.region.is_some())
        .flat_map(|group| group.members.iter().map(String::as_str))
        .collect::<std::collections::BTreeSet<_>>();
    let mut corner_idx: Vec<usize> = hints
        .corner_seek
        .iter()
        .filter_map(|r| problem.parts.iter().position(|p| &p.reference == r))
        .filter(|&i| {
            problem.parts[i].locked.is_none()
                && !regioned.contains(problem.parts[i].reference.as_str())
        })
        .collect();
    corner_idx.sort_by(|&a, &b| problem.parts[a].reference.cmp(&problem.parts[b].reference));
    corner_idx.dedup();
    if corner_idx.is_empty() {
        return;
    }
    let margin = courtyard_margin(problem.clearance);
    let rots: Vec<f64> = best.placements.iter().map(|p| p.rotation).collect();
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rots)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rots)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let mut pos: Vec<Point2> = best.placements.iter().map(|p| p.at).collect();
    // The exhaustive assignment is tiny (at most 5^4 including "leave in place").
    // More than four corner seekers cannot all occupy distinct corners, so keep the
    // first four stable refdes eligible and leave the remainder at their legal seats.
    corner_idx.truncate(4);
    let original = pos.clone();
    let targets: Vec<[Point2; 4]> = corner_idx
        .iter()
        .map(|&i| {
            let envelope =
                part_placement_bounds_envelope(&problem.parts[i], half[i], copper_bbox[i]);
            let b = &problem.bounds;
            let left = b.min_x - envelope.min_x;
            let right = b.max_x - envelope.max_x;
            let top = b.min_y - envelope.min_y;
            let bottom = b.max_y - envelope.max_y;
            [
                Point2 { x: left, y: top },
                Point2 { x: right, y: top },
                Point2 { x: left, y: bottom },
                Point2 {
                    x: right,
                    y: bottom,
                },
            ]
        })
        .collect();
    let mut assignment = vec![None; corner_idx.len()];
    let mut winner: Option<CornerAssignment> = None;
    search_corner_assignments(
        problem,
        &half,
        &copper_bbox,
        margin,
        &corner_idx,
        &targets,
        &original,
        &mut pos,
        &mut assignment,
        0,
        0,
        &mut winner,
    );
    if let Some(winner) = winner {
        pos = winner.positions;
    }
    for (p, np) in best.placements.iter_mut().zip(&pos) {
        p.at = *np;
    }
}

struct CornerAssignment {
    seated: usize,
    separation: u32,
    movement: f64,
    choices: Vec<Option<usize>>,
    positions: Vec<Point2>,
}

#[allow(clippy::too_many_arguments)]
fn search_corner_assignments(
    problem: &PlacementView,
    half: &[(f64, f64)],
    copper_bbox: &[Rect],
    margin: f64,
    corner_idx: &[usize],
    targets: &[[Point2; 4]],
    original: &[Point2],
    pos: &mut [Point2],
    assignment: &mut [Option<usize>],
    depth: usize,
    used: u8,
    winner: &mut Option<CornerAssignment>,
) {
    if depth == corner_idx.len() {
        if !is_legal(problem, half, copper_bbox, margin, pos) {
            return;
        }
        let seated = assignment.iter().flatten().count();
        let choices = assignment.iter().flatten().copied().collect::<Vec<_>>();
        let separation = choices
            .iter()
            .enumerate()
            .flat_map(|(i, &a)| choices.iter().skip(i + 1).map(move |&b| (a, b)))
            .map(|(a, b)| if a ^ b == 3 { 2 } else { 1 })
            .sum();
        let movement: f64 = assignment
            .iter()
            .enumerate()
            .filter_map(|(slot, choice)| {
                choice.map(|_| {
                    original[corner_idx[slot]]
                        .dist(pos[corner_idx[slot]])
                        .powi(2)
                })
            })
            .sum();
        let better = winner.as_ref().is_none_or(|best| {
            seated > best.seated
                || (seated == best.seated
                    && (separation > best.separation
                        || (separation == best.separation
                            && (movement < best.movement - 1e-9
                                || ((movement - best.movement).abs() <= 1e-9
                                    && &*assignment < best.choices.as_slice())))))
        });
        if better {
            *winner = Some(CornerAssignment {
                seated,
                separation,
                movement,
                choices: assignment.to_vec(),
                positions: pos.to_vec(),
            });
        }
        return;
    }

    let part = corner_idx[depth];
    for corner in 0..4 {
        let bit = 1 << corner;
        if used & bit != 0 {
            continue;
        }
        assignment[depth] = Some(corner);
        pos[part] = targets[depth][corner];
        search_corner_assignments(
            problem,
            half,
            copper_bbox,
            margin,
            corner_idx,
            targets,
            original,
            pos,
            assignment,
            depth + 1,
            used | bit,
            winner,
        );
    }
    assignment[depth] = None;
    pos[part] = original[part];
    search_corner_assignments(
        problem,
        half,
        copper_bbox,
        margin,
        corner_idx,
        targets,
        original,
        pos,
        assignment,
        depth + 1,
        used,
        winner,
    );
}

/// Place `problem`'s parts under `hints`, deterministically.
///
/// Runs the force-directed seed then the legalizer; locked parts never move;
/// empty hints are fully supported. The returned `legal` flag is verified by
/// exact geometry. Never panics: an impossible board returns `legal: false`
/// with a report rather than overlapping silently or aborting. This is the
/// baseline: no idiom variants, which [`place_tuned`] turns on.
pub fn place(problem: &PlacementView, hints: &PlacementHints) -> PlaceResult {
    place_variant(problem, hints, PlaceOpts::default())
}

/// [`place`] with a specific set of idiom variant toggles.
pub(crate) fn place_variant(
    problem: &PlacementView,
    hints: &PlacementHints,
    opts: PlaceOpts,
) -> PlaceResult {
    let n = problem.parts.len();
    let nets = derive_nets(problem);
    let margin = courtyard_margin(problem.clearance);
    let terms = CostTerms::new(problem, hints, decoupling_pairs(problem));

    // Locked parts use their locked rotation (snapped to a quadrant); unlocked
    // parts start at 0 and may be polished after position legalization if a
    // quadrant rotation lowers pad-level routeability cost without breaking
    // exact legality.
    let mut rotations: Vec<f64> = problem
        .parts
        .iter()
        .map(|p| {
            p.locked
                .as_ref()
                .map(|l| geom::snap_quadrant(l.rotation))
                .unwrap_or(0.0)
        })
        .collect();

    // Rotated courtyard half-extents per part (rotation only swaps w/h here).
    let mut half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &rot)| rotated_courtyard_half(p, rot))
        .collect();
    let mut copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &rot)| rotated_copper_bbox(p, rot))
        .collect();

    // 1. Deterministic initial grid (sorted by reference), seeding positions.
    let mut pos = initial_grid(problem, &half);
    // Locked parts override with their pinned position immediately.
    for (i, part) in problem.parts.iter().enumerate() {
        if let Some(l) = &part.locked {
            pos[i] = l.at;
        }
    }

    // 2. Force-directed relaxation (skips locked parts).
    force_layout(problem, hints, &nets, &half, margin, opts, &mut pos);

    // 2a. Snap every unlocked decoupling cap to the nearest FREE ring slot around its
    //     placed anchor IC (DECOUPLE variant only). A cheap, high-value seed fix: the
    //     force seed can strand a bypass cap tens of mm from its IC on a multi-IC board
    //     (the net springs split it between IC and the far power net), and the legalizer
    //     never recovers that. Snapping it tight first means SA only has to polish a good
    //     start. Skips parts under an explicit group/surround hint (the agent placed those
    //     deliberately). GATED on `opts.decouple` so the baseline (`PlaceOpts::default()`)
    //     stays UNSNAPPED: the snap helps some boards' supply loops and hurts others'
    //     routability, so only the tuned variant asks for it.
    if opts.decouple {
        snap_caps_to_anchor_ring(problem, hints, &half, margin, &mut pos);
    }

    // 2b. SA refinement (variant-gated): escape the springs' local minima and
    //     optimize the explicit cost (overlap + wirelength + compaction +
    //     decoupling cohesion + a silk gap so refdes don't collide).
    if opts.anneal {
        anneal_placement(problem, hints, &nets, &half, margin, &rotations, &mut pos);
    }

    // 3. Legalize: snap + spiral-resolve overlaps + clamp. Locked immovable.
    let leg = legalize(problem, &half, &copper_bbox, margin, &mut pos);

    polish_rotations(
        problem,
        &nets,
        margin,
        &terms,
        &pos,
        &mut rotations,
        &mut half,
        &mut copper_bbox,
    );
    polish_positions(
        problem,
        &nets,
        margin,
        &terms,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );
    polish_swaps(
        problem,
        &nets,
        margin,
        &terms,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );
    polish_rotations(
        problem,
        &nets,
        margin,
        &terms,
        &pos,
        &mut rotations,
        &mut half,
        &mut copper_bbox,
    );

    // 4. Build placements (input order) and the report.
    let placements: Vec<Placement> = (0..n)
        .map(|i| Placement {
            reference: problem.parts[i].reference.clone(),
            at: pos[i],
            rotation: rotations[i],
        })
        .collect();

    // 5. Verify legality by EXACT geometry — never trust the algorithm.
    let legal = is_legal(problem, &half, &copper_bbox, margin, &pos);

    let hpwl = compute_hpwl_with_rotations(problem, &nets, &pos, &rotations);
    let layout_cost = place_cost(
        problem, &nets, &half, margin, &rotations, &terms, &pos,
    );

    PlaceResult {
        placements,
        legal,
        report: PlaceReport {
            overlaps_resolved: leg.overlaps_resolved,
            out_of_bounds_clamps: leg.out_of_bounds_clamps,
            hpwl,
            layout_cost,
        },
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn polish_positions(
    problem: &PlacementView,
    nets: &[crate::LogicalNet],
    margin: f64,
    terms: &CostTerms,
    rotations: &[f64],
    half: &[(f64, f64)],
    copper_bbox: &[Rect],
    pos: &mut [Point2],
) {
    let mut cost = place_cost(problem, nets, half, margin, rotations, terms, pos);
    let step = PLACEMENT_GRID.pitch();
    let mut moves = Vec::new();
    for scale in [4.0, 2.0, 1.0] {
        let d = step * scale;
        moves.extend([
            (-d, 0.0),
            (d, 0.0),
            (0.0, -d),
            (0.0, d),
            (-d, -d),
            (-d, d),
            (d, -d),
            (d, d),
        ]);
    }

    for _ in 0..4 {
        let mut improved = false;
        let baseline_order: Vec<usize> = (0..problem.parts.len())
            .filter(|&idx| problem.parts[idx].locked.is_none())
            .collect();
        let pressure_order =
            position_polish_part_order(problem, nets, rotations, half, margin, pos);
        for order in [baseline_order, pressure_order] {
            if polish_positions_in_order(
                problem,
                nets,
                margin,
                terms,
                rotations,
                half,
                copper_bbox,
                pos,
                &moves,
                &order,
                &mut cost,
            ) {
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn polish_positions_in_order(
    problem: &PlacementView,
    nets: &[crate::LogicalNet],
    margin: f64,
    terms: &CostTerms,
    rotations: &[f64],
    half: &[(f64, f64)],
    copper_bbox: &[Rect],
    pos: &mut [Point2],
    moves: &[(f64, f64)],
    order: &[usize],
    cost: &mut f64,
) -> bool {
    let mut improved = false;
    for &i in order {
        let old = pos[i];
        let mut best = old;
        let mut best_cost = *cost;
        let ratline_edges = ratline_tree_edge_list(problem, nets, rotations, pos);
        let mut candidates: Vec<Point2> = moves
            .iter()
            .map(|&(dx, dy)| Point2 {
                x: old.x + dx,
                y: old.y + dy,
            })
            .collect();
        candidates.extend(net_centroid_position_candidates(
            problem, nets, rotations, pos, i,
        ));
        candidates.extend(ratline_crossing_position_candidates_from_edges(
            problem,
            rotations,
            i,
            &ratline_edges,
        ));
        candidates.extend(ratline_obstruction_position_candidates_from_edges(
            problem,
            rotations,
            half,
            margin,
            pos,
            i,
            &ratline_edges,
        ));
        candidates.extend(obstructing_part_position_candidates_from_edges(
            problem,
            half,
            margin,
            pos,
            i,
            &ratline_edges,
        ));
        candidates.extend(edge_seek_position_candidates(
            problem,
            half,
            rotations[i],
            pos,
            i,
            terms,
        ));
        for candidate in unique_position_candidates(
            problem,
            i,
            rotations[i],
            half[i],
            copper_bbox[i],
            old,
            candidates,
        ) {
            pos[i] = candidate;
            if !is_legal(problem, half, copper_bbox, margin, pos) {
                continue;
            }
            let next_cost =
                place_cost(problem, nets, half, margin, rotations, terms, pos);
            if next_cost + 1e-9 < best_cost {
                best_cost = next_cost;
                best = candidate;
            }
        }
        pos[i] = best;
        if best_cost + 1e-9 < *cost {
            *cost = best_cost;
            improved = true;
        }
    }
    improved
}

pub(crate) fn position_polish_part_order(
    problem: &PlacementView,
    nets: &[crate::LogicalNet],
    rotations: &[f64],
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
) -> Vec<usize> {
    let mut metrics = vec![PositionPolishMetric::default(); problem.parts.len()];
    for net in nets {
        for i in 0..net.pins.len() {
            for j in i + 1..net.pins.len() {
                metrics[net.pins[i].part].connected_degree += 1;
                metrics[net.pins[j].part].connected_degree += 1;
            }
        }
    }

    let edges = ratline_tree_edge_list(problem, nets, rotations, pos);
    for i in 0..edges.len() {
        let a = &edges[i];
        for b in &edges[i + 1..] {
            if a.net_idx == b.net_idx {
                continue;
            }
            if a.a.part == b.a.part
                || a.a.part == b.b.part
                || a.b.part == b.a.part
                || a.b.part == b.b.part
            {
                continue;
            }
            if ratline_edge_layers_overlap(a, b)
                && geom::Segment::new(a.a_pos, a.b_pos)
                    .intersects(geom::Segment::new(b.a_pos, b.b_pos))
            {
                for part in [a.a.part, a.b.part, b.a.part, b.b.part] {
                    metrics[part].crossing_pressure += 1;
                }
            }
        }
    }

    for edge in &edges {
        let segment = geom::Segment::new(edge.a_pos, edge.b_pos);
        for part_idx in 0..problem.parts.len() {
            if problem.parts[part_idx].locked.is_some()
                || part_idx == edge.a.part
                || part_idx == edge.b.part
            {
                continue;
            }
            let obstacle =
                Rect::from_center_half(pos[part_idx], half[part_idx]).inflate(margin / 2.0);
            if obstacle.dist_to_segment(segment) <= geom::EPS {
                metrics[part_idx].obstruction_pressure += 1;
            }
        }
        for keepout in &problem.keepouts {
            if keepout.dist_to_segment(segment) <= geom::EPS {
                metrics[edge.a.part].obstruction_pressure += 1;
                metrics[edge.b.part].obstruction_pressure += 1;
            }
        }
    }

    let mut order: Vec<usize> = (0..problem.parts.len())
        .filter(|&idx| problem.parts[idx].locked.is_none())
        .collect();
    order.sort_by_key(|&idx| {
        let m = metrics[idx];
        (
            std::cmp::Reverse(m.crossing_pressure),
            std::cmp::Reverse(m.obstruction_pressure),
            std::cmp::Reverse(m.connected_degree),
            idx,
        )
    });
    order
}

#[derive(Clone, Copy, Default)]
struct PositionPolishMetric {
    crossing_pressure: usize,
    obstruction_pressure: usize,
    connected_degree: usize,
}

pub(crate) fn unique_position_candidates(
    problem: &PlacementView,
    part_idx: usize,
    rotation: f64,
    half: (f64, f64),
    copper_bbox: Rect,
    old: Point2,
    targets: Vec<Point2>,
) -> Vec<Point2> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for target in targets {
        let part = &problem.parts[part_idx];
        let mut snapped = Point2 {
            x: PLACEMENT_GRID.snap(target.x),
            y: PLACEMENT_GRID.snap(target.y),
        };
        // Edge datums commonly sit at a fractional library coordinate. Keep an
        // exact datum-normal target exact instead of moving it to the 0.5 mm
        // component grid (UTC16-G uses y=4.34).
        for edge in [Edge::N, Edge::S, Edge::W, Edge::E] {
            let Some(normal) = datum_edge_target(part, rotation, edge, &problem.bounds) else {
                continue;
            };
            match edge {
                Edge::N | Edge::S if (target.y - normal).abs() <= geom::EPS => {
                    snapped.y = normal;
                }
                Edge::W | Edge::E if (target.x - normal).abs() <= geom::EPS => {
                    snapped.x = normal;
                }
                _ => {}
            }
        }
        let candidate = if part.edge_datum.is_some() {
            let envelope = part_placement_bounds_envelope(part, half, copper_bbox);
            let mut clamped = clamp_center_for_envelope(&problem.bounds, snapped, envelope);
            // Preserve an exact physical edge target after tangential/copper
            // clamping. An impossible copper envelope is rejected by is_legal.
            for edge in [Edge::N, Edge::S, Edge::W, Edge::E] {
                let Some(normal) = datum_edge_target(part, rotation, edge, &problem.bounds) else {
                    continue;
                };
                match edge {
                    Edge::N | Edge::S if (target.y - normal).abs() <= geom::EPS => {
                        clamped.y = normal;
                    }
                    Edge::W | Edge::E if (target.x - normal).abs() <= geom::EPS => {
                        clamped.x = normal;
                    }
                    _ => {}
                }
            }
            clamped
        } else {
            problem.bounds.clamp_center_for_half(snapped, half)
        };
        if candidate.dist(old) < geom::EPS {
            continue;
        }
        let key = (
            (candidate.x * 1000.0).round() as i64,
            (candidate.y * 1000.0).round() as i64,
        );
        if seen.insert(key) {
            out.push(candidate);
        }
    }
    out
}

/// Candidate seats on a board edge for a part the hints steer there: all four
/// edges for a nearest-edge seeker, and only the named one for an authored
/// `edge` intent — plus, for the named edge, the FLUSH seat, since the intent is
/// that the part's courtyard reach the edge, not merely the edge band.
pub(crate) fn edge_seek_position_candidates(
    problem: &PlacementView,
    half: &[(f64, f64)],
    rotation: f64,
    pos: &[Point2],
    part_idx: usize,
    terms: &CostTerms,
) -> Vec<Point2> {
    let mut edges: Vec<Edge> = Vec::new();
    if terms.edge_seek.contains(&part_idx) {
        edges.extend([Edge::N, Edge::S, Edge::W, Edge::E]);
    }
    let named: Vec<Edge> = terms
        .edge_of
        .iter()
        .filter(|(part, _)| *part == part_idx)
        .map(|&(_, edge)| edge)
        .collect();
    edges.extend(named.iter().copied());
    if edges.is_empty() {
        return Vec::new();
    }
    let current = pos[part_idx];
    let h = half[part_idx];
    let mut out: Vec<Point2> = edges
        .into_iter()
        .filter_map(|edge| {
            if problem.parts[part_idx].edge_datum.is_some()
                && datum_edge_target(&problem.parts[part_idx], rotation, edge, &problem.bounds)
                    .is_none()
            {
                return None;
            }
            let normal =
                part_edge_target(&problem.parts[part_idx], rotation, edge, &problem.bounds, h);
            Some(match edge {
                Edge::N | Edge::S => Point2 {
                    x: current.x,
                    y: normal,
                },
                Edge::W | Edge::E => Point2 {
                    x: normal,
                    y: current.y,
                },
            })
        })
        .collect();
    let flush = |edge: Edge| {
        let part = &problem.parts[part_idx];
        let envelope = part_placement_bounds_envelope(part, h, rotated_copper_bbox(part, rotation));
        let b = &problem.bounds;
        match edge {
            Edge::N => Point2 { x: current.x, y: b.min_y - envelope.min_y },
            Edge::S => Point2 { x: current.x, y: b.max_y - envelope.max_y },
            Edge::W => Point2 { x: b.min_x - envelope.min_x, y: current.y },
            Edge::E => Point2 { x: b.max_x - envelope.max_x, y: current.y },
        }
    };
    out.extend(named.into_iter().map(flush));
    out
}

pub(crate) fn net_centroid_position_candidates(
    problem: &PlacementView,
    nets: &[crate::LogicalNet],
    rotations: &[f64],
    pos: &[Point2],
    part_idx: usize,
) -> Vec<Point2> {
    let mut candidates = Vec::new();
    let mut all_own_x = 0.0;
    let mut all_own_y = 0.0;
    let mut all_own_n = 0usize;
    let mut all_other_x = 0.0;
    let mut all_other_y = 0.0;
    let mut all_other_n = 0usize;
    let mut all_own_offsets = Vec::new();
    let mut all_other_positions = Vec::new();
    let current_center = pos[part_idx];

    for net in nets {
        let mut own_x = 0.0;
        let mut own_y = 0.0;
        let mut own_n = 0usize;
        let mut other_x = 0.0;
        let mut other_y = 0.0;
        let mut other_n = 0usize;
        let mut own_pins = Vec::new();
        let mut other_positions = Vec::new();

        for pin in &net.pins {
            let part = &problem.parts[pin.part];
            let pad = &part.pads[pin.pad];
            let off = pad.offset.rotate(rotations[pin.part]);
            if pin.part == part_idx {
                own_x += off.x;
                own_y += off.y;
                own_n += 1;
                all_own_offsets.push(off);
                own_pins.push((
                    off,
                    Point2 {
                        x: current_center.x + off.x,
                        y: current_center.y + off.y,
                    },
                ));
            } else {
                let other = Point2 {
                    x: pos[pin.part].x + off.x,
                    y: pos[pin.part].y + off.y,
                };
                other_x += other.x;
                other_y += other.y;
                other_n += 1;
                all_other_positions.push(other);
                other_positions.push(other);
            }
        }

        if own_n > 0 && other_n > 0 {
            all_own_x += own_x;
            all_own_y += own_y;
            all_own_n += own_n;
            all_other_x += other_x;
            all_other_y += other_y;
            all_other_n += other_n;
            candidates.push(Point2 {
                x: other_x / other_n as f64 - own_x / own_n as f64,
                y: other_y / other_n as f64 - own_y / own_n as f64,
            });
            add_nearest_pad_position_candidates(&mut candidates, &own_pins, &other_positions);
        }
    }

    if all_own_n > 0 && all_other_n > 0 {
        let mean_target = Point2 {
            x: all_other_x / all_other_n as f64 - all_own_x / all_own_n as f64,
            y: all_other_y / all_other_n as f64 - all_own_y / all_own_n as f64,
        };
        candidates.push(mean_target);
        candidates.push(Point2 {
            x: mean_target.x,
            y: current_center.y,
        });
        candidates.push(Point2 {
            x: current_center.x,
            y: mean_target.y,
        });
        let median_target = Point2 {
            x: median_coord(all_other_positions.iter().map(|p| p.x))
                - median_coord(all_own_offsets.iter().map(|p| p.x)),
            y: median_coord(all_other_positions.iter().map(|p| p.y))
                - median_coord(all_own_offsets.iter().map(|p| p.y)),
        };
        candidates.push(median_target);
        candidates.push(Point2 {
            x: median_target.x,
            y: current_center.y,
        });
        candidates.push(Point2 {
            x: current_center.x,
            y: median_target.y,
        });
    }

    candidates
}

fn add_nearest_pad_position_candidates(
    candidates: &mut Vec<Point2>,
    own_pins: &[(Point2, Point2)],
    other_positions: &[Point2],
) {
    for &(own_offset, own_world) in own_pins {
        let Some(nearest) = other_positions.iter().min_by(|a, b| {
            squared_distance(own_world, **a)
                .total_cmp(&squared_distance(own_world, **b))
                .then_with(|| a.x.total_cmp(&b.x))
                .then_with(|| a.y.total_cmp(&b.y))
        }) else {
            continue;
        };
        let full_align = Point2 {
            x: nearest.x - own_offset.x,
            y: nearest.y - own_offset.y,
        };
        candidates.push(full_align);
        candidates.push(Point2 {
            x: full_align.x,
            y: own_world.y - own_offset.y,
        });
        candidates.push(Point2 {
            x: own_world.x - own_offset.x,
            y: full_align.y,
        });
    }
}

fn squared_distance(a: Point2, b: Point2) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx * dx + dy * dy
}

fn median_coord(values: impl Iterator<Item = f64>) -> f64 {
    let mut values: Vec<i64> = values.map(|v| (v * 1000.0).round() as i64).collect();
    values.sort_unstable();
    values
        .get(values.len() / 2)
        .map(|v| *v as f64 / 1000.0)
        .unwrap_or(0.0)
}

#[cfg(test)]
pub(crate) fn ratline_crossing_position_candidates(
    problem: &PlacementView,
    nets: &[crate::LogicalNet],
    rotations: &[f64],
    pos: &[Point2],
    part_idx: usize,
) -> Vec<Point2> {
    let edges = ratline_tree_edge_list(problem, nets, rotations, pos);
    ratline_crossing_position_candidates_from_edges(problem, rotations, part_idx, &edges)
}

pub(crate) fn ratline_crossing_position_candidates_from_edges(
    problem: &PlacementView,
    rotations: &[f64],
    part_idx: usize,
    edges: &[RatlineEdge<'_>],
) -> Vec<Point2> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for own_edge in edges {
        let Some((own_pin, partner_pin, own_pad, partner_pad)) = edge_for_part(own_edge, part_idx)
        else {
            continue;
        };
        let own_offset = problem.parts[own_pin.part].pads[own_pin.pad]
            .offset
            .rotate(rotations[own_pin.part]);

        for other_edge in edges {
            if own_edge.net_idx == other_edge.net_idx {
                continue;
            }
            if other_edge.a.part == part_idx
                || other_edge.b.part == part_idx
                || other_edge.a.part == partner_pin.part
                || other_edge.b.part == partner_pin.part
            {
                continue;
            }
            if !ratline_edge_layers_overlap(own_edge, other_edge) {
                continue;
            }

            if !geom::Segment::new(partner_pad, own_pad)
                .intersects(geom::Segment::new(other_edge.a_pos, other_edge.b_pos))
            {
                continue;
            }
            let Some(new_pad) = move_point_just_past_line(
                own_pad,
                partner_pad,
                other_edge.a_pos,
                other_edge.b_pos,
                problem,
            ) else {
                continue;
            };
            let center = Point2 {
                x: new_pad.x - own_offset.x,
                y: new_pad.y - own_offset.y,
            };
            let key = (
                (center.x * 1000.0).round() as i64,
                (center.y * 1000.0).round() as i64,
            );
            if seen.insert(key) {
                candidates.push(center);
            }
        }
    }

    candidates
}

#[cfg(test)]
pub(crate) fn ratline_obstruction_position_candidates(
    problem: &PlacementView,
    nets: &[crate::LogicalNet],
    rotations: &[f64],
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
    part_idx: usize,
) -> Vec<Point2> {
    let edges = ratline_tree_edge_list(problem, nets, rotations, pos);
    ratline_obstruction_position_candidates_from_edges(
        problem, rotations, half, margin, pos, part_idx, &edges,
    )
}

pub(crate) fn ratline_obstruction_position_candidates_from_edges(
    problem: &PlacementView,
    rotations: &[f64],
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
    part_idx: usize,
    edges: &[RatlineEdge<'_>],
) -> Vec<Point2> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for edge in edges {
        let Some((own_pin, partner_pin, own_pad, partner_pad)) = edge_for_part(edge, part_idx)
        else {
            continue;
        };
        let own_offset = problem.parts[own_pin.part].pads[own_pin.pad]
            .offset
            .rotate(rotations[own_pin.part]);
        let ratline = geom::Segment::new(partner_pad, own_pad);

        for obstacle in
            ratline_obstacles(problem, half, margin, pos, own_pin.part, partner_pin.part)
        {
            if obstacle.dist_to_segment(ratline) > geom::EPS {
                continue;
            }
            for new_pad in obstruction_relief_pad_targets(problem, obstacle, own_pad, partner_pad) {
                let center = Point2 {
                    x: new_pad.x - own_offset.x,
                    y: new_pad.y - own_offset.y,
                };
                let key = (
                    (center.x * 1000.0).round() as i64,
                    (center.y * 1000.0).round() as i64,
                );
                if seen.insert(key) {
                    candidates.push(center);
                }
            }
        }
    }

    candidates
}

#[cfg(test)]
pub(crate) fn obstructing_part_position_candidates(
    problem: &PlacementView,
    nets: &[crate::LogicalNet],
    rotations: &[f64],
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
    part_idx: usize,
) -> Vec<Point2> {
    let edges = ratline_tree_edge_list(problem, nets, rotations, pos);
    obstructing_part_position_candidates_from_edges(problem, half, margin, pos, part_idx, &edges)
}

pub(crate) fn obstructing_part_position_candidates_from_edges(
    problem: &PlacementView,
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
    part_idx: usize,
    edges: &[RatlineEdge<'_>],
) -> Vec<Point2> {
    let obstacle = Rect::from_center_half(pos[part_idx], half[part_idx]).inflate(margin / 2.0);
    let mut candidates = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for edge in edges {
        if edge.a.part == part_idx || edge.b.part == part_idx {
            continue;
        }
        let segment = geom::Segment::new(edge.a_pos, edge.b_pos);
        if obstacle.dist_to_segment(segment) > geom::EPS {
            continue;
        }
        for center in
            move_center_away_from_segment(problem, half[part_idx], margin, pos[part_idx], segment)
        {
            let key = (
                (center.x * 1000.0).round() as i64,
                (center.y * 1000.0).round() as i64,
            );
            if seen.insert(key) {
                candidates.push(center);
            }
        }
    }

    candidates
}

fn move_center_away_from_segment(
    problem: &PlacementView,
    half: (f64, f64),
    margin: f64,
    center: Point2,
    segment: geom::Segment,
) -> Vec<Point2> {
    let dx = segment.b.x - segment.a.x;
    let dy = segment.b.y - segment.a.y;
    let len = (dx * dx + dy * dy).sqrt();
    if len < geom::EPS {
        return Vec::new();
    }
    let nx = -dy / len;
    let ny = dx / len;
    let signed = (center.x - segment.a.x) * nx + (center.y - segment.a.y) * ny;
    let relief = half.0.hypot(half.1)
        + margin / 2.0
        + problem.clearance
        + problem.min_trace_width
        + PLACEMENT_GRID.pitch();

    [-relief, relief]
        .into_iter()
        .map(|target| Point2 {
            x: center.x + nx * (target - signed),
            y: center.y + ny * (target - signed),
        })
        .collect()
}

fn ratline_obstacles(
    problem: &PlacementView,
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
    own_part: usize,
    partner_part: usize,
) -> Vec<Rect> {
    let mut obstacles = Vec::new();
    for part_idx in 0..problem.parts.len() {
        if part_idx == own_part || part_idx == partner_part {
            continue;
        }
        obstacles.push(Rect::from_center_half(pos[part_idx], half[part_idx]).inflate(margin / 2.0));
    }
    obstacles.extend(problem.keepouts.iter().copied());
    obstacles
}

fn obstruction_relief_pad_targets(
    problem: &PlacementView,
    obstacle: Rect,
    own_pad: Point2,
    partner_pad: Point2,
) -> Vec<Point2> {
    let relief = (problem.clearance + problem.min_trace_width + PLACEMENT_GRID.pitch()).max(0.5);
    let mut out = Vec::with_capacity(4);
    if partner_pad.x < obstacle.min_x {
        out.push(Point2 {
            x: obstacle.min_x - relief,
            y: own_pad.y,
        });
    } else if partner_pad.x > obstacle.max_x {
        out.push(Point2 {
            x: obstacle.max_x + relief,
            y: own_pad.y,
        });
    }
    if partner_pad.y < obstacle.min_y {
        out.push(Point2 {
            x: own_pad.x,
            y: obstacle.min_y - relief,
        });
    } else if partner_pad.y > obstacle.max_y {
        out.push(Point2 {
            x: own_pad.x,
            y: obstacle.max_y + relief,
        });
    }

    out.extend([
        Point2 {
            x: obstacle.min_x - relief,
            y: own_pad.y,
        },
        Point2 {
            x: obstacle.max_x + relief,
            y: own_pad.y,
        },
        Point2 {
            x: own_pad.x,
            y: obstacle.min_y - relief,
        },
        Point2 {
            x: own_pad.x,
            y: obstacle.max_y + relief,
        },
    ]);
    out
}

#[derive(Clone)]
pub(crate) struct RatlineEdge<'a> {
    net_idx: usize,
    a: &'a Pin,
    b: &'a Pin,
    layers: Vec<LayerRef>,
    a_pos: Point2,
    b_pos: Point2,
}

fn edge_for_part<'a>(
    edge: &RatlineEdge<'a>,
    part_idx: usize,
) -> Option<(&'a Pin, &'a Pin, Point2, Point2)> {
    if edge.a.part == part_idx {
        Some((edge.a, edge.b, edge.a_pos, edge.b_pos))
    } else if edge.b.part == part_idx {
        Some((edge.b, edge.a, edge.b_pos, edge.a_pos))
    } else {
        None
    }
}

pub(crate) fn ratline_tree_edge_list<'a>(
    problem: &PlacementView,
    nets: &'a [crate::LogicalNet],
    rotations: &[f64],
    pos: &[Point2],
) -> Vec<RatlineEdge<'a>> {
    nets.iter()
        .enumerate()
        .flat_map(|(net_idx, net)| ratline_tree_edges(problem, rotations, pos, net_idx, net))
        .collect()
}

fn ratline_tree_edges<'a>(
    problem: &PlacementView,
    rotations: &[f64],
    pos: &[Point2],
    net_idx: usize,
    net: &'a crate::LogicalNet,
) -> Vec<RatlineEdge<'a>> {
    match net.pins.as_slice() {
        [] | [_] => Vec::new(),
        [a, b] => vec![RatlineEdge {
            net_idx,
            a,
            b,
            layers: ratline_edge_layers(problem, a, b),
            a_pos: pin_world_pos(problem, rotations, pos, a),
            b_pos: pin_world_pos(problem, rotations, pos, b),
        }],
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
                edges.push(RatlineEdge {
                    net_idx,
                    a: &pins[ai],
                    b: &pins[bi],
                    layers: ratline_edge_layers(problem, &pins[ai], &pins[bi]),
                    a_pos: pin_positions[ai],
                    b_pos: pin_positions[bi],
                });
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

fn ratline_edge_requires_layer_change(problem: &PlacementView, a: &Pin, b: &Pin) -> bool {
    let a_layers = &problem.parts[a.part].pads[a.pad].layers;
    let b_layers = &problem.parts[b.part].pads[b.pad].layers;
    !a_layers
        .iter()
        .any(|layer| b_layers.iter().any(|other| other == layer))
}

fn ratline_edge_layers_overlap(a: &RatlineEdge<'_>, b: &RatlineEdge<'_>) -> bool {
    a.layers
        .iter()
        .any(|layer| b.layers.iter().any(|other| other == layer))
}

fn pin_world_pos(
    problem: &PlacementView,
    rotations: &[f64],
    pos: &[Point2],
    pin: &crate::Pin,
) -> Point2 {
    let off = problem.parts[pin.part].pads[pin.pad]
        .offset
        .rotate(rotations[pin.part]);
    Point2 {
        x: pos[pin.part].x + off.x,
        y: pos[pin.part].y + off.y,
    }
}

fn move_point_just_past_line(
    point: Point2,
    same_side_as: Point2,
    line_a: Point2,
    line_b: Point2,
    problem: &PlacementView,
) -> Option<Point2> {
    let dx = line_b.x - line_a.x;
    let dy = line_b.y - line_a.y;
    let len = (dx * dx + dy * dy).sqrt();
    if len < geom::EPS {
        return None;
    }
    let nx = -dy / len;
    let ny = dx / len;
    let signed_dist = |p: Point2| (p.x - line_a.x) * nx + (p.y - line_a.y) * ny;
    let point_dist = signed_dist(point);
    let target_side = signed_dist(same_side_as).signum();
    if target_side.abs() < f64::EPSILON || point_dist.signum() == target_side {
        return None;
    }
    let relief = (problem.clearance + problem.min_trace_width + PLACEMENT_GRID.pitch()).max(0.5);
    let target_dist = target_side * relief;
    Some(Point2 {
        x: point.x + nx * (target_dist - point_dist),
        y: point.y + ny * (target_dist - point_dist),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn polish_swaps(
    problem: &PlacementView,
    nets: &[crate::LogicalNet],
    margin: f64,
    terms: &CostTerms,
    rotations: &[f64],
    half: &[(f64, f64)],
    copper_bbox: &[Rect],
    pos: &mut [Point2],
) {
    let mut cost = place_cost(problem, nets, half, margin, rotations, terms, pos);
    for _ in 0..2 {
        let mut improved = false;
        for (a, b) in swap_pair_order(problem, nets, rotations, pos) {
            pos.swap(a, b);
            if !is_legal(problem, half, copper_bbox, margin, pos) {
                pos.swap(a, b);
                continue;
            }
            let next_cost =
                place_cost(problem, nets, half, margin, rotations, terms, pos);
            if next_cost + 1e-9 < cost {
                cost = next_cost;
                improved = true;
            } else {
                pos.swap(a, b);
            }
        }
        if !improved {
            break;
        }
    }
}

pub(crate) fn swap_pair_order(
    problem: &PlacementView,
    nets: &[crate::LogicalNet],
    rotations: &[f64],
    pos: &[Point2],
) -> Vec<(usize, usize)> {
    let mut connected = std::collections::BTreeSet::new();
    for net in nets {
        for i in 0..net.pins.len() {
            for j in i + 1..net.pins.len() {
                push_pair(&mut connected, net.pins[i].part, net.pins[j].part);
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
            if a.a.part == b.a.part
                || a.a.part == b.b.part
                || a.b.part == b.a.part
                || a.b.part == b.b.part
            {
                continue;
            }
            if ratline_edge_layers_overlap(a, b)
                && geom::Segment::new(a.a_pos, a.b_pos)
                    .intersects(geom::Segment::new(b.a_pos, b.b_pos))
            {
                for pa in [a.a.part, a.b.part] {
                    for pb in [b.a.part, b.b.part] {
                        push_pair(&mut crossing_related, pa, pb);
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
        for part_idx in 0..problem.parts.len() {
            if problem.parts[part_idx].locked.is_some()
                || part_idx == edge.a.part
                || part_idx == edge.b.part
            {
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
    for a in 0..problem.parts.len() {
        if problem.parts[a].locked.is_some() {
            continue;
        }
        for b in a + 1..problem.parts.len() {
            if problem.parts[b].locked.is_some() {
                continue;
            }
            let key = (a, b);
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

fn push_pair(set: &mut std::collections::BTreeSet<(usize, usize)>, a: usize, b: usize) {
    if a == b {
        return;
    }
    set.insert(if a < b { (a, b) } else { (b, a) });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn polish_rotations(
    problem: &PlacementView,
    nets: &[crate::LogicalNet],
    margin: f64,
    terms: &CostTerms,
    pos: &[Point2],
    rotations: &mut [f64],
    half: &mut [(f64, f64)],
    copper_bbox: &mut [Rect],
) {
    let mut cost = place_cost(problem, nets, half, margin, rotations, terms, pos);

    for _ in 0..4 {
        let mut improved = false;
        for i in 0..problem.parts.len() {
            if problem.parts[i].locked.is_some() {
                continue;
            }
            let old_rot = rotations[i];
            let old_half = half[i];
            let old_copper = copper_bbox[i];
            let mut best_rot = old_rot;
            let mut best_half = old_half;
            let mut best_copper = old_copper;
            let mut best_cost = cost;

            for candidate in [0.0, 90.0, 180.0, 270.0] {
                let candidate = geom::snap_quadrant(candidate);
                if candidate == old_rot {
                    continue;
                }
                rotations[i] = candidate;
                half[i] = rotated_courtyard_half(&problem.parts[i], candidate);
                copper_bbox[i] = rotated_copper_bbox(&problem.parts[i], candidate);
                if !is_legal(problem, half, copper_bbox, margin, pos) {
                    continue;
                }
                let next_cost =
                    place_cost(problem, nets, half, margin, rotations, terms, pos);
                if next_cost + 1e-9 < best_cost {
                    best_cost = next_cost;
                    best_rot = candidate;
                    best_half = half[i];
                    best_copper = copper_bbox[i];
                }
            }

            rotations[i] = best_rot;
            half[i] = best_half;
            copper_bbox[i] = best_copper;
            if best_cost + 1e-9 < cost {
                improved = true;
                cost = best_cost;
            }
        }
        if !improved {
            break;
        }
    }
}
