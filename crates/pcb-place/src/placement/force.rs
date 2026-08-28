//! Force-directed seed: parts relax under net-centroid springs, group cohesion,
//! region/edge pulls, decoupling co-placement, and short-range courtyard repulsion.
//! Plus [`snap_caps_to_anchor_ring`], a cheap seed fix that pulls a stranded bypass
//! cap onto the nearest free ring slot around its anchor before the annealer runs.

use super::geometry::{
    PLACE_GRID, PLACEMENT_GRID, SPIRAL_MAX_RING, aspect_edge, clamp_center_for_envelope,
    edge_delta, nearest_edge, pad_world, part_edge_target, part_placement_bounds_envelope,
    rotated_copper_bbox, sign_nonzero,
};
use super::legalize::collides;
use super::route::PlaceOpts;
use pcb_model::{Point2, Rect};
use place_model::decoupling_pairs;
use place_model::{LogicalNet, PlaceProblem, PlacementHints};

/// Force-directed iteration count.
const FORCE_ITERS: usize = 200;

/// Cooling: multiply the step scale by this every [`COOL_EVERY`] iterations.
const COOL_FACTOR: f64 = 0.9;

/// Apply [`COOL_FACTOR`] every this many iterations.
const COOL_EVERY: usize = 20;

/// Base spring constant for net centroid attraction (normalized by pin count).
const NET_SPRING_K: f64 = 0.08;

/// Spring constant for group-cohesion (members pulled to group centroid).
const GROUP_SPRING_K: f64 = 0.05;

/// Pull strength toward a region centroid / edge band when a hint applies.
const REGION_PULL_K: f64 = 0.10;
const EDGE_PULL_K: f64 = 0.10;

/// Pull strength for auto edge-affinity (connectors → nearest edge). Stronger
/// than [`EDGE_PULL_K`] so it overcomes the inward net springs of a connector
/// wired to several nets, which would otherwise strand it in the interior.
const EDGE_SEEK_K: f64 = 0.30;

/// Direct pull of a decoupling cap toward its IC ([`decoupling_pairs`]), in the
/// decoupling placement variant only. Strong enough that the cap hugs the IC
/// (shortening the supply loop); `place_best` keeps the variant only when it
/// routes at least as cleanly, so this never regresses a board it does not help.
const DECOUPLE_K: f64 = 0.35;

/// Short-range repulsion gain on margin-inflated courtyard overlap.
const REPULSION_K: f64 = 0.5;

/// Relax `pos` under the force model. Locked parts are anchors (never moved) but
/// still attract movable parts through shared nets/groups. Deterministic: fixed
/// iteration count, no RNG, forces summed in a fixed order.
pub(crate) fn force_layout(
    problem: &PlaceProblem,
    hints: &PlacementHints,
    nets: &[LogicalNet],
    half: &[(f64, f64)],
    margin: f64,
    opts: PlaceOpts,
    pos: &mut [Point2],
) {
    let n = problem.parts.len();
    let locked: Vec<bool> = problem.parts.iter().map(|p| p.locked.is_some()).collect();
    // Decoupling co-placement pairs (cap → IC), only when this variant enables it.
    let decoupling: Vec<(usize, usize)> = if opts.decouple {
        let grouped: std::collections::BTreeSet<usize> = hints
            .groups
            .iter()
            .flat_map(|g| {
                g.members
                    .iter()
                    .filter_map(|m| problem.parts.iter().position(|p| &p.reference == m))
            })
            .collect();
        decoupling_pairs(problem)
            .into_iter()
            .filter(|(cap, _)| !grouped.contains(cap))
            .collect()
    } else {
        Vec::new()
    };

    // Per-part group hints (a part may be in several groups).
    // We precompute, for each group, the member indices that exist.
    let groups: Vec<Vec<usize>> = hints
        .groups
        .iter()
        .map(|g| {
            g.members
                .iter()
                .filter_map(|m| problem.parts.iter().position(|p| &p.reference == m))
                .collect()
        })
        .collect();

    // Parts that should hug their nearest board edge (connectors/headers).
    let edge_seek: Vec<usize> = hints
        .edge_seek
        .iter()
        .filter_map(|m| problem.parts.iter().position(|p| &p.reference == m))
        .collect();

    let mut scale = 1.0_f64;

    for iter in 0..FORCE_ITERS {
        if iter > 0 && iter % COOL_EVERY == 0 {
            scale *= COOL_FACTOR;
        }
        let mut force = vec![(0.0_f64, 0.0_f64); n];

        // (a) Net centroid springs: each multi-pin net pulls its parts toward
        //     the net's pin centroid. Strength normalized by pin count so a big
        //     net does not dominate.
        for net in nets {
            if net.pins.len() < 2 {
                continue;
            }
            // Centroid of pad world positions.
            let mut cx = 0.0;
            let mut cy = 0.0;
            for pin in &net.pins {
                let w = pad_world(problem, pos, pin);
                cx += w.x;
                cy += w.y;
            }
            let inv = 1.0 / net.pins.len() as f64;
            cx *= inv;
            cy *= inv;
            let k = NET_SPRING_K * inv;
            for pin in &net.pins {
                let w = pad_world(problem, pos, pin);
                force[pin.part].0 += k * (cx - w.x);
                force[pin.part].1 += k * (cy - w.y);
            }
        }

        // (b) Group cohesion springs: members pulled toward the group centroid.
        for members in &groups {
            if members.len() < 2 {
                continue;
            }
            let mut cx = 0.0;
            let mut cy = 0.0;
            for &m in members {
                cx += pos[m].x;
                cy += pos[m].y;
            }
            let inv = 1.0 / members.len() as f64;
            cx *= inv;
            cy *= inv;
            for &m in members {
                force[m].0 += GROUP_SPRING_K * (cx - pos[m].x);
                force[m].1 += GROUP_SPRING_K * (cy - pos[m].y);
            }
        }

        // (c) Region containment + (d) edge affinity pulls.
        for (g, members) in hints.groups.iter().zip(&groups) {
            if let Some(region) = &g.region {
                let c = region.center();
                for &m in members {
                    // Only pull when outside the region (containment, not a
                    // constant inward bias that fights net springs).
                    if !region.contains(pos[m]) {
                        force[m].0 += REGION_PULL_K * (c.x - pos[m].x);
                        force[m].1 += REGION_PULL_K * (c.y - pos[m].y);
                    }
                }
            }
            if let Some(edge) = &g.edge {
                for &m in members {
                    let target =
                        part_edge_target(&problem.parts[m], 0.0, *edge, &problem.bounds, half[m]);
                    let (dx, dy) = edge_delta(*edge, &pos[m], target);
                    force[m].0 += EDGE_PULL_K * dx;
                    force[m].1 += EDGE_PULL_K * dy;
                }
            }
        }

        // (d2) Auto edge-affinity: pull each edge-seeking part (connector/header)
        //      toward its NEAREST board edge, recomputed each iteration so it
        //      tracks the part as the net springs move it. Connectors belong at
        //      the perimeter; this stops the router from having to wrap copper
        //      around a centrally-stranded header.
        for &m in &edge_seek {
            // Aspect-aware variant: a tall part (a vertical multi-pin header) is
            // pulled to the nearest SIDE edge so its pad column lies ALONG that
            // edge, instead of the nearest edge overall (often the top) where the
            // column pokes into the interior. Wide parts prefer a top/bottom edge.
            let edge = if opts.aspect_edge {
                aspect_edge(
                    &pos[m],
                    &problem.bounds,
                    problem.parts[m].courtyard_w,
                    problem.parts[m].courtyard_h,
                )
            } else {
                nearest_edge(&pos[m], &problem.bounds)
            };
            let target = part_edge_target(&problem.parts[m], 0.0, edge, &problem.bounds, half[m]);
            let (dx, dy) = edge_delta(edge, &pos[m], target);
            force[m].0 += EDGE_SEEK_K * dx;
            force[m].1 += EDGE_SEEK_K * dy;
        }

        // (d3) Decoupling co-placement (variant-gated): pull each bypass cap
        //      toward its IC so it seats beside it — one-directional (the IC is
        //      not dragged around by its caps). Only active in the decoupling
        //      variant; place_best keeps it only when it routes at least as clean.
        for &(cap, ic) in &decoupling {
            force[cap].0 += DECOUPLE_K * (pos[ic].x - pos[cap].x);
            force[cap].1 += DECOUPLE_K * (pos[ic].y - pos[cap].y);
        }

        // (e) Short-range courtyard repulsion: only on margin-inflated overlap.
        //     O(n^2) but n is tiny and this is short-range (zero outside overlap).
        for i in 0..n {
            let courtyard_i = Rect::from_center_half(pos[i], half[i]).inflate(margin / 2.0);
            for j in (i + 1)..n {
                let courtyard_j = Rect::from_center_half(pos[j], half[j]).inflate(margin / 2.0);
                let (ox, oy) = courtyard_i.axis_penetration(&courtyard_j);
                if ox > 0.0 && oy > 0.0 {
                    // Push apart along the axis of least penetration (the cheap
                    // separating move), proportional to penetration.
                    let dx = pos[i].x - pos[j].x;
                    let dy = pos[i].y - pos[j].y;
                    if ox <= oy {
                        let s = REPULSION_K * ox * sign_nonzero(dx);
                        force[i].0 += s;
                        force[j].0 -= s;
                    } else {
                        let s = REPULSION_K * oy * sign_nonzero(dy);
                        force[i].1 += s;
                        force[j].1 -= s;
                    }
                }
            }
        }

        // Integrate (locked parts pinned) and clamp the origin into bounds.
        for i in 0..n {
            if locked[i] {
                continue;
            }
            pos[i].x += force[i].0 * scale;
            pos[i].y += force[i].1 * scale;
            pos[i] = if problem.parts[i].edge_datum.is_some() {
                let copper = rotated_copper_bbox(&problem.parts[i], 0.0);
                let envelope = part_placement_bounds_envelope(&problem.parts[i], half[i], copper);
                clamp_center_for_envelope(&problem.bounds, pos[i], envelope)
            } else {
                problem.bounds.clamp_center_for_half(pos[i], half[i])
            };
        }
    }
}

/// Snap each unlocked decoupling cap to the nearest free ring slot around its anchor IC.
///
/// For every detected `(cap, ic)` pair ([`decoupling_pairs`]) whose cap is movable and
/// not under an explicit group/surround hint, search outward (increasing radius) for the
/// nearest grid-snapped position that (a) clears the anchor courtyard by the margin, (b)
/// collides with no already-seated part, and (c) fits in bounds; move the cap there. Caps
/// sharing one anchor are seated one at a time and become obstacles for the next, so they
/// ring the IC instead of stacking. A no-op when there are no decoupling pairs.
///
/// This only relocates caps the force seed stranded — a cap already hugging its IC finds a
/// free slot at the smallest radius (often where it already is), so a good seed is left
/// essentially untouched; the win is on multi-IC boards where the seed splits a cap between
/// its IC and a far power net.
pub(crate) fn snap_caps_to_anchor_ring(
    problem: &PlaceProblem,
    hints: &PlacementHints,
    half: &[(f64, f64)],
    margin: f64,
    pos: &mut [Point2],
) {
    // Caps the agent explicitly grouped/surrounded are placed deliberately — leave them.
    let hinted: std::collections::BTreeSet<usize> = hints
        .groups
        .iter()
        .flat_map(|g| {
            g.members
                .iter()
                .filter_map(|m| problem.parts.iter().position(|p| &p.reference == m))
        })
        .collect();
    let pairs: Vec<(usize, usize)> = decoupling_pairs(problem)
        .into_iter()
        .filter(|(cap, _)| problem.parts[*cap].locked.is_none() && !hinted.contains(cap))
        .collect();
    if pairs.is_empty() {
        return;
    }
    // Everything except the caps being moved is a fixed obstacle for the snap.
    let moving: std::collections::BTreeSet<usize> = pairs.iter().map(|(c, _)| *c).collect();
    let mut seated: Vec<usize> = (0..problem.parts.len())
        .filter(|i| !moving.contains(i))
        .collect();
    for (cap, ic) in pairs {
        let anchor = pos[ic];
        // Ring radius starts just past both courtyards touching with margin and grows by
        // the grid; angular probes are evenly spaced and tried nearest-the-IC first.
        let base = (half[cap].0 + half[cap].1) / 2.0 + (half[ic].0 + half[ic].1) / 2.0 + margin;
        let mut best: Option<Point2> = None;
        'search: for ring in 0..SPIRAL_MAX_RING {
            let radius = base + ring as f64 * PLACE_GRID;
            let steps = ((2.0 * std::f64::consts::PI * radius / PLACE_GRID).ceil() as usize).max(8);
            for s in 0..steps {
                let theta = s as f64 / steps as f64 * 2.0 * std::f64::consts::PI;
                let cand = Point2 {
                    x: PLACEMENT_GRID.snap(anchor.x + radius * theta.cos()),
                    y: PLACEMENT_GRID.snap(anchor.y + radius * theta.sin()),
                };
                if !problem
                    .bounds
                    .contains_rect_eps(&Rect::from_center_half(cand, half[cap]), 1e-9)
                {
                    continue;
                }
                if !collides(&cand, half[cap], pos, half, margin, &seated) {
                    best = Some(cand);
                    break 'search;
                }
            }
        }
        if let Some(p) = best {
            pos[cap] = p;
        }
        // The cap is now a fixed obstacle for the remaining caps around any anchor.
        seated.push(cap);
    }
}
