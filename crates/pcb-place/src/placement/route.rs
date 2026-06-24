//! The placement pipeline entry points + the built-in [`Placer`]s and the
//! [`RouteRanker`] the routability oracle is wired from.
//!
//! [`to_route_problem`] (the bridge from a placement to the router) and the
//! [`Placer`]/[`RouteRanker`]/[`RoutabilityOracle`] trait seam all live in the
//! kernel ([`pcb_model::place`]); this module supplies the BUILT-IN implementations:
//! [`LegalizingPlacer`] (force + legalize, the baseline + spring idioms),
//! [`AnnealingPlacer`] (SA refine), [`FanoutPlacer`] (the structured radial
//! fast-path), and [`GridAstarRanker`] (a grid-astar-backed default [`RouteRanker`]
//! so the router stays injectable, not hardwired).

use super::anneal::anneal_placement;
use super::cost::{compute_hpwl, place_cost};
use super::force::{force_layout, snap_caps_to_anchor_ring};
use super::geometry::{
    courtyard_margin, rotated_copper_bbox, rotated_courtyard_half, snap_rotation,
};
use super::hints::{apply_grid_hints, unified_fanout_place};
use super::legalize::{initial_grid, is_legal, legalize};
use super::model::{
    derive_nets, Placement, PlaceProblem, PlaceReport, PlaceResult, PlacementHints,
};
use super::pairs::decoupling_pairs;
use crate::problem::place::{Placer, RoutabilityOracle, RouteRanker};
use crate::problem::{Point2, RouteProblem};

/// [`to_route_problem`] now lives in the kernel ([`pcb_model::place`]) so a
/// third-party placer can build a [`RouteProblem`] from its own placement without
/// depending on `pcb-place`. Re-exported so callers are unchanged.
pub use crate::problem::place::to_route_problem;

/// Per-variant placement toggles a [`LegalizingPlacer`]/[`AnnealingPlacer`] carries.
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

/// The force-directed-seed + spiral-legalize [`Placer`] — the always-legal baseline.
/// With `PlaceOpts::default` it is the pure seed ([`LegalizingPlacer::baseline`]); the
/// `decouple`/`aspect_edge` opts turn on the spring idioms (cap co-placement,
/// aspect-aware connector edges) as extra oracle candidates. Never anneals.
pub struct LegalizingPlacer {
    pub(crate) opts: PlaceOpts,
}

impl LegalizingPlacer {
    /// The baseline placer (pure force seed + legalize, no idioms).
    pub fn baseline() -> Self {
        Self { opts: PlaceOpts::default() }
    }
}

impl Placer for LegalizingPlacer {
    fn name(&self) -> &'static str {
        "legalizing"
    }
    fn place(&self, problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
        place_variant(problem, hints, self.opts)
    }
}

/// The simulated-annealing-refine [`Placer`]: the force seed then [`anneal_placement`]
/// (escape the springs' local minima, optimize the explicit layout cost). The
/// oracle's main alternative to the baseline.
pub struct AnnealingPlacer {
    pub(crate) opts: PlaceOpts,
}

impl AnnealingPlacer {
    /// The SA-refine placer, seeded from the baseline force layout.
    pub fn new() -> Self {
        Self { opts: PlaceOpts { anneal: true, aspect_edge: false, decouple: false } }
    }
}

impl Default for AnnealingPlacer {
    fn default() -> Self {
        Self::new()
    }
}

impl Placer for AnnealingPlacer {
    fn name(&self) -> &'static str {
        "anneal"
    }
    fn place(&self, problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
        place_variant(problem, hints, self.opts)
    }
}

/// A [`RouteRanker`] backed by the FAST grid-astar router + the shared DRC lint —
/// the built-in routability scorer the oracle uses by default. Lives here (not in
/// the kernel) so the router stays INJECTABLE: a third party supplies its own
/// `RouteRanker` to rank with its own router instead.
pub struct GridAstarRanker;

impl RouteRanker for GridAstarRanker {
    /// `(faults, routed_wirelength)`: route with the fast naive router, count
    /// unrouted nets + geometry DRC violations (Connectivity excluded — it is the
    /// router's own unrouted signal, already counted in `failed`). Only relative
    /// routability matters for variant selection, so the slow capacity-mesh router
    /// on every candidate of a 70-part board is needlessly expensive (export
    /// re-routes with route_auto).
    ///
    /// Uses the ORTHOGONAL route deliberately: the ranker only needs a stable relative
    /// routability proxy, and the orthogonal estimate keeps the placement choice
    /// invariant to the router's 8-way diagonal default (the diagonal pass is a routing
    /// improvement, not a placement signal — coupling it in would re-rank every board's
    /// placement and drift the layout). Export's `route_auto` still routes 8-way.
    fn faults(&self, rp: &RouteProblem) -> usize {
        let routed = crate::router::route_orthogonal(rp);
        let geom = crate::lint::lint(rp, &routed.solution)
            .iter()
            .filter(|v| !matches!(v, crate::lint::DrcViolation::Connectivity { .. }))
            .count();
        routed.failed.len() + geom
    }
}

/// Build the routability oracle for THIS board: the baseline [`LegalizingPlacer`]
/// (`placers[0]`, the exact-tie winner) plus the SA refine and, when they apply, the
/// spring-idiom variants. The candidate set + order match the legacy `place_best`
/// portfolio exactly, so the oracle's selection is byte-identical.
fn board_oracle(problem: &PlaceProblem, hints: &PlacementHints) -> RoutabilityOracle {
    let has_decouple = !decoupling_pairs(problem).is_empty();
    let has_edge = !hints.edge_seek.is_empty();

    // The variants worth trying for THIS board (always include the baseline).
    // The SA refinement subsumes the decouple/edge springs (its cost does
    // cohesion + edge-seek directly), so the annealed variant is the main
    // alternative; the spring variants stay as cheap extra candidates.
    let mut placers: Vec<Box<dyn Placer + Send + Sync>> =
        vec![Box::new(LegalizingPlacer { opts: PlaceOpts::default() })];
    placers.push(Box::new(AnnealingPlacer {
        opts: PlaceOpts { anneal: true, aspect_edge: has_edge, decouple: false },
    }));
    if has_decouple {
        placers.push(Box::new(LegalizingPlacer {
            opts: PlaceOpts { decouple: true, aspect_edge: false, anneal: false },
        }));
    }
    if has_edge {
        placers.push(Box::new(LegalizingPlacer {
            opts: PlaceOpts { decouple: false, aspect_edge: true, anneal: false },
        }));
    }
    RoutabilityOracle::new(placers, Box::new(GridAstarRanker))
}

/// Place `problem` and return the variant that ROUTES cleanest — the placement
/// analog of `route_auto`. It runs the baseline placement plus idiom variants
/// (decoupling co-placement, aspect-aware connector edges), routes each via the
/// injected [`GridAstarRanker`], and keeps whichever yields fewer routing faults
/// (unrouted nets + geometry DRC violations), breaking ties by lower layout cost
/// then HPWL. The baseline is always a candidate, so an idiom variant that does not
/// actually help (e.g. one that scatters a board's power net) is automatically
/// discarded — the [`RoutabilityOracle`] decides per board, so aggressive idioms can
/// never regress a board they do not improve. The decouple idiom is where the
/// cap-ring snap ([`snap_caps_to_anchor_ring`]) lives: the baseline runs UNSNAPPED
/// and the `decouple` variant runs snapped, so the oracle picks snapped-vs-unsnapped
/// per board.
pub fn place_best(problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
    let mut best = board_oracle(problem, hints).place(problem, hints);
    // Post-pass: seat mounting holes (corner_seek) at the board corners on the
    // WINNING placement. They carry no signal nets (GND-plane only), so moving
    // them never changes routing — which is why this must run AFTER the faults-
    // ranked variant selection rather than inside a routing-affecting variant.
    seat_corner_seek_parts(problem, hints, &mut best);
    best
}

/// The structured radial fan-out [`Placer`]: a board with a dominant fine-pitch IC
/// gets the textbook layout (IC centred, decoupling caps + series resistors ringed
/// in IC-pad order, connectors on the edges) via [`unified_fanout_place`],
/// overlap-free by construction, then the oracle ([`place_best`]) seats the rest.
/// When the fan-out doesn't apply (no dominant IC) or its result is illegal, it
/// falls back to the oracle on the un-fanned board — so it is NEVER worse than the
/// legalizing baseline. This is `place_board`'s built-in placer.
pub struct FanoutPlacer;

impl Placer for FanoutPlacer {
    fn name(&self) -> &'static str {
        "fanout"
    }
    fn place(&self, problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
        // Stage 1 — structured fan-out fast-path.
        let mut p = problem.clone();
        let fanned = std::env::var("NO_UNIFIED").is_err() && unified_fanout_place(&mut p);
        if !fanned {
            apply_grid_hints(&mut p, hints);
        }
        let result = place_best(&p, hints);
        // Stage 2 — optimize fallback when the fan-out couldn't seat legally.
        if fanned && !result.legal {
            let mut base = problem.clone();
            apply_grid_hints(&mut base, hints);
            return place_best(&base, hints);
        }
        result
    }
}

/// THE PLACEMENT PIPELINE — the single visible entry the tool layer calls.
///
/// All of force / anneal / fan-out are placement; this runs the [`FanoutPlacer`]:
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
    FanoutPlacer.place(problem, hints)
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

    let hpwl = compute_hpwl(problem, &nets, &pos);
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
