//! The placement pipeline entry points + the routability oracle that picks among
//! variants, and [`to_route_problem`] (the bridge from a placement to the router).

use super::anneal::anneal_placement;
use super::cost::{compute_hpwl, place_cost};
use super::force::{force_layout, snap_caps_to_anchor_ring};
use super::geometry::{
    courtyard_margin, rotate_offset, rotated_copper_bbox, rotated_courtyard_half, snap_rotation,
};
use super::hints::{apply_grid_hints, unified_fanout_place};
use super::legalize::{initial_grid, is_legal, legalize};
use super::model::{
    derive_nets, Placement, PlaceProblem, PlaceReport, PlaceResult, PlacementHints,
};
use super::pairs::decoupling_pairs;
use crate::problem::{
    Connection, LayerRef, Obstacle, Point2, RoutePoint, RouteProblem,
};
use std::collections::BTreeMap;

/// Per-variant placement toggles, tried and selected by [`place_best`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PlaceOpts {
    /// Pull each decoupling cap to hug its IC ([`super::pairs::decoupling_pairs`]) via
    /// the force spring, AND snap each cap to the nearest free ring slot around its
    /// anchor in the seed ([`snap_caps_to_anchor_ring`]). Off in the baseline, so
    /// `place_best` keeps an unsnapped candidate to fall back to when snapping hurts
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

/// Place `problem` and return the variant that ROUTES cleanest — the placement
/// analog of [`crate::pipeline::route_auto`]. It runs the baseline placement plus
/// idiom variants (decoupling co-placement, aspect-aware connector edges, both),
/// routes each, and keeps whichever yields fewer routing faults (unrouted nets +
/// geometry DRC violations), breaking ties by lower routed wirelength then HPWL.
/// The baseline is always a candidate, so an idiom variant that does not actually
/// help (e.g. one that scatters a board's power net) is automatically discarded —
/// the oracle decides per board, so aggressive idioms can never regress a board
/// they do not improve. The decouple idiom is where the cap-ring snap
/// ([`snap_caps_to_anchor_ring`]) lives: the baseline runs UNSNAPPED and the
/// `decouple` variant runs snapped, so the oracle picks snapped-vs-unsnapped per
/// board (the snap tightens supply loops on some boards but hurts routability on
/// others — having both as candidates lets the fault count decide).
pub fn place_best(problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
    let has_decouple = !decoupling_pairs(problem).is_empty();
    let has_edge = !hints.edge_seek.is_empty();

    // The variants worth trying for THIS board (always include the baseline).
    // The SA refinement subsumes the decouple/edge springs (its cost does
    // cohesion + edge-seek directly), so the annealed variant is the main
    // alternative; the spring variants stay as cheap extra candidates.
    let mut opts = vec![PlaceOpts::default()];
    opts.push(PlaceOpts { anneal: true, aspect_edge: has_edge, decouple: false });
    if has_decouple {
        opts.push(PlaceOpts { decouple: true, aspect_edge: false, anneal: false });
    }
    if has_edge {
        opts.push(PlaceOpts { decouple: false, aspect_edge: true, anneal: false });
    }

    let cost = |r: &PlaceResult| -> (usize, u64, u64) {
        if !r.legal {
            return (usize::MAX, u64::MAX, u64::MAX);
        }
        let rp = to_route_problem(problem, &r.placements);
        // Rank variants with the FAST naive router — only relative routability
        // matters here, and the slow capacity-mesh router on every variant of a
        // 70-part board is needlessly expensive (export re-routes with route_auto).
        let routed = crate::router::route(&rp);
        let geom = crate::lint::lint(&rp, &routed.solution)
            .iter()
            .filter(|v| !matches!(v, crate::lint::DrcViolation::Connectivity { .. }))
            .count();
        // PRIMARY: routing faults (an honest unrouted net + geometry violations) —
        // a worse-routed layout is never chosen. SECONDARY: the layout cost (so the
        // annealer's compaction / cohesion / silk-gap gains decide among equally-
        // routable layouts). hpwl breaks final ties.
        (
            routed.failed.len() + geom,
            (r.report.layout_cost * 1000.0) as u64,
            (r.report.hpwl * 1000.0) as u64,
        )
    };

    // Evaluate every variant IN PARALLEL — each is an independent, pure place+route
    // (the SA seed is fixed, so a variant's result is deterministic regardless of
    // thread/order). We then pick the lowest-cost; opts[0] is the baseline and wins
    // exact ties via the index tie-break, preserving the previous baseline-first
    // selection bit-for-bit. The slow part of a big board is these N variant
    // place+rank-route passes, so fanning them across cores is the main speed lever.
    use rayon::prelude::*;
    let mut scored: Vec<(usize, (usize, u64, u64), PlaceResult)> = opts
        .par_iter()
        .enumerate()
        .map(|(i, &o)| {
            let r = place_variant(problem, hints, o);
            let c = cost(&r);
            (i, c, r)
        })
        .collect();
    scored.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let mut best = scored.swap_remove(0).2;
    // Post-pass: seat mounting holes (corner_seek) at the board corners on the
    // WINNING placement. They carry no signal nets (GND-plane only), so moving
    // them never changes routing — which is why this must run AFTER the faults-
    // ranked variant selection rather than inside a routing-affecting variant.
    seat_corner_seek_parts(problem, hints, &mut best);
    best
}

/// THE PLACEMENT PIPELINE — the single visible entry the tool layer calls.
///
/// All of force / anneal / fan-out are placement; this is the order they run in:
///
/// 1. **STRUCTURED fast-path** — [`unified_fanout_place`]: a board with a dominant
///    fine-pitch IC gets the textbook radial layout (IC centred, decoupling caps +
///    series resistors ringed in IC-pad order, connectors on the edges), overlap-free
///    by construction.
/// 2. **OPTIMIZE fallback** — [`place_best`]: when the fan-out doesn't apply (no
///    dominant IC) or can't seat legally, run the cost-optimized search — a
///    force-directed seed, optionally simulated-annealing-refined, plus idiom
///    variants, each routed and the most routable kept.
///
/// `$NO_UNIFIED` forces stage 2 (pure `place_best`). Never worse than the legalizing
/// baseline: a fan-out that can't seat legally is discarded in favour of `place_best`.
pub fn place_board(problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
    // Stage 1 — structured fan-out fast-path.
    let mut p = problem.clone();
    let fanned = std::env::var("NO_UNIFIED").is_err() && unified_fanout_place(&mut p);
    if !fanned {
        apply_grid_hints(&mut p, hints);
    }
    let mut result = place_best(&p, hints);
    // Stage 2 — optimize fallback when the fan-out couldn't seat legally.
    if fanned && !result.legal {
        let mut base = problem.clone();
        apply_grid_hints(&mut base, hints);
        result = place_best(&base, hints);
    }
    result
}

/// Move each `corner_seek` part to its nearest board CORNER that leaves the
/// placement legal (greedy, nearest-first; a corner already taken by another
/// such part or overlapping a component is skipped). A no-op when there are no
/// corner-seek parts. Safe on any placement: corner-seek parts (mounting holes)
/// have no nets, so this cannot change connectivity or routing.
fn seat_corner_seek_parts(problem: &PlaceProblem, hints: &PlacementHints, best: &mut PlaceResult) {
    if !best.legal {
        return;
    }
    let corner_idx: Vec<usize> = hints
        .corner_seek
        .iter()
        .filter_map(|r| problem.parts.iter().position(|p| &p.reference == r))
        .collect();
    if corner_idx.is_empty() {
        return;
    }
    let margin = courtyard_margin(problem.clearance);
    let rots: Vec<i32> = best.placements.iter().map(|p| p.rotation).collect();
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rots)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<(f64, f64, f64, f64)> = problem
        .parts
        .iter()
        .zip(&rots)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let mut pos: Vec<Point2> = best.placements.iter().map(|p| p.at.clone()).collect();
    let b = &problem.bounds;
    let corners = [
        (b.min_x, b.min_y),
        (b.max_x, b.min_y),
        (b.min_x, b.max_y),
        (b.max_x, b.max_y),
    ];
    let mut used = [false; 4];
    for &i in &corner_idx {
        let h = half[i];
        // Inset each corner by this part's half so it sits fully on-board.
        let inset = |c: (f64, f64)| Point2 {
            x: if c.0 == b.min_x { b.min_x + h.0 } else { b.max_x - h.0 },
            y: if c.1 == b.min_y { b.min_y + h.1 } else { b.max_y - h.1 },
        };
        let mut order: Vec<usize> = (0..4).collect();
        let d = |c: (f64, f64)| (pos[i].x - c.0).powi(2) + (pos[i].y - c.1).powi(2);
        order.sort_by(|&a, &c| d(corners[a]).partial_cmp(&d(corners[c])).unwrap());
        let saved = pos[i].clone();
        for &ci in &order {
            if used[ci] {
                continue;
            }
            pos[i] = inset(corners[ci]);
            if is_legal(problem, &half, &copper_bbox, margin, &pos) {
                used[ci] = true;
                break;
            }
            pos[i] = saved.clone();
        }
    }
    for (p, np) in best.placements.iter_mut().zip(&pos) {
        p.at = np.clone();
    }
}

/// Place `problem`'s parts under `hints`, deterministically.
///
/// Runs the force-directed seed then the legalizer; locked parts never move;
/// empty hints are fully supported. The returned `legal` flag is verified by
/// exact geometry. Never panics: an impossible board returns `legal: false`
/// with a report rather than overlapping silently or aborting. This is the
/// baseline (no idiom variants); [`place_best`] selects among variants.
pub fn place(problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
    place_variant(problem, hints, PlaceOpts::default())
}

/// [`place`] with a specific set of idiom variant toggles.
pub(crate) fn place_variant(problem: &PlaceProblem, hints: &PlacementHints, opts: PlaceOpts) -> PlaceResult {
    let n = problem.parts.len();
    let nets = derive_nets(problem);
    let margin = courtyard_margin(problem.clearance);

    // Rotation is fixed per part for v1: locked parts use their locked rotation
    // (snapped to a quadrant); everyone else stays at 0. The engine never
    // auto-rotates.
    let rotations: Vec<i32> = problem
        .parts
        .iter()
        .map(|p| p.locked.as_ref().map(|l| snap_rotation(l.rotation)).unwrap_or(0))
        .collect();

    // Rotated courtyard half-extents per part (rotation only swaps w/h here).
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &rot)| rotated_courtyard_half(p, rot))
        .collect();
    let copper_bbox: Vec<(f64, f64, f64, f64)> = problem
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
            pos[i] = l.at.clone();
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
    //     stays UNSNAPPED — `place_best`'s fault-first oracle then has both a snapped and
    //     an unsnapped candidate and picks whichever routes cleaner per board (the snap
    //     helps some boards' supply loops but hurts others' routability).
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
    let leg = legalize(problem, &half, margin, &mut pos);

    // 4. Build placements (input order) and the report.
    let placements: Vec<Placement> = (0..n)
        .map(|i| Placement {
            reference: problem.parts[i].reference.clone(),
            at: pos[i].clone(),
            rotation: rotations[i],
        })
        .collect();

    // 5. Verify legality by EXACT geometry — never trust the algorithm.
    let legal = is_legal(problem, &half, &copper_bbox, margin, &pos);

    let hpwl = compute_hpwl(problem, &nets, &pos, &rotations);
    let pairs = decoupling_pairs(problem);
    let edge_idx: Vec<usize> = hints
        .edge_seek
        .iter()
        .filter_map(|r| problem.parts.iter().position(|p| &p.reference == r))
        .collect();
    let layout_cost = place_cost(problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos);

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

/// Via geometry carried into the emitted [`RouteProblem`] (mirrors `problem.rs`
/// defaults — the value the existing fixtures and oracle expect).
const DEFAULT_VIA_DIAMETER: f64 = 0.6;
const DEFAULT_VIA_DRILL: f64 = 0.3;

/// Build a [`RouteProblem`] from a placement: every pad becomes a net-attributed
/// obstacle (at its placed+rotated world position), and every multi-pin net
/// becomes a [`Connection`] whose `points_to_connect` are the pad centers on the
/// pad's layer. Board bounds and design rules are carried from the problem.
///
/// The emitted problem round-trips serde and is accepted by `route_auto` and the
/// connectivity oracle unchanged (pads on nets, points on pads).
pub fn to_route_problem(problem: &PlaceProblem, placements: &[Placement]) -> RouteProblem {
    // Index placements by reference so we tolerate any order.
    let place_by_ref: BTreeMap<&str, &Placement> =
        placements.iter().map(|p| (p.reference.as_str(), p)).collect();

    let mut obstacles: Vec<Obstacle> = Vec::new();
    // Net → its pad world positions + layer (for connections). Deterministic order.
    let mut net_points: BTreeMap<String, Vec<RoutePoint>> = BTreeMap::new();

    for part in &problem.parts {
        let Some(pl) = place_by_ref.get(part.reference.as_str()) else {
            continue;
        };
        let rot = snap_rotation(pl.rotation);
        for pad in &part.pads {
            let off = rotate_offset(&pad.offset, rot);
            let center = Point2 {
                x: pl.at.x + off.x,
                y: pl.at.y + off.y,
            };
            // Rotation swaps pad w/h for the quadrant cases.
            let (w, h) = match rot {
                90 | 270 => (pad.height, pad.width),
                _ => (pad.width, pad.height),
            };
            let connected_to = pad.net.clone().into_iter().collect::<Vec<_>>();
            obstacles.push(Obstacle {
                kind: "rect".to_owned(),
                layers: pad.layers.clone(),
                center: center.clone(),
                width: w,
                height: h,
                connected_to,
            });
            if let Some(net) = &pad.net {
                // The connection point sits at the pad center on the pad's first
                // copper layer (a thru-hole pad lists several; the route point
                // anchors one — the via/oracle stitch the rest).
                let layer = pad.layers.first().cloned().unwrap_or_else(LayerRef::top);
                net_points
                    .entry(net.clone())
                    .or_default()
                    .push(RoutePoint {
                        x: center.x,
                        y: center.y,
                        layer,
                    });
            }
        }
    }

    // Multi-pin nets → connections (single-pin nets have nothing to connect).
    let connections: Vec<Connection> = net_points
        .into_iter()
        .filter(|(_, pts)| pts.len() >= 2)
        .map(|(name, points_to_connect)| Connection {
            name,
            points_to_connect,
        })
        .collect();

    RouteProblem {
        layer_count: problem.layer_count,
        min_trace_width: problem.min_trace_width,
        obstacles,
        connections,
        bounds: problem.bounds.clone(),
        clearance: problem.clearance,
        // Via geometry: the defaults the existing fixtures use (problem.rs
        // `default_via_diameter`/`default_via_drill`). v1 placement does not
        // model via sizing, so it carries these constants.
        via_diameter: DEFAULT_VIA_DIAMETER,
        via_drill: DEFAULT_VIA_DRILL,
        // Per-net widths are applied by the agent layer (route_board) after this, from
        // the board's design rules; placement itself is width-agnostic.
        net_widths: std::collections::BTreeMap::new(),
        // Carry the custom outline so the router keeps copper inside the true shape.
        outline: problem.outline.clone(),
        escape_layers: Default::default(),
    }
}
