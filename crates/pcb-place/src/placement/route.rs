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
use super::cost::{compute_hpwl_with_rotations, place_cost};
use super::force::{force_layout, snap_caps_to_anchor_ring};
use super::geometry::{
    PLACEMENT_GRID, courtyard_margin, rotated_copper_bbox, rotated_courtyard_half,
};
use super::hints::{apply_edge_lock, apply_grid_hints, unified_fanout_place};
use super::legalize::{initial_grid, is_legal, legalize};
use super::model::{
    Pin, PlaceProblem, PlaceReport, PlaceResult, Placement, PlacementHints, derive_nets,
};
use super::pairs::decoupling_pairs;
use crate::problem::place::{PlacementRankKey, Placer, RoutabilityOracle, RouteRanker};
use crate::problem::{Point2, Rect, RouteProblem, Router, failed_pad_weight};

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
        Self {
            opts: PlaceOpts::default(),
        }
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
        Self {
            opts: PlaceOpts {
                anneal: true,
                aspect_edge: false,
                decouple: false,
            },
        }
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

/// A deterministic edge-locked placer for explicit connector/header edge hints.
/// It is offered as an oracle candidate, not a replacement for the soft edge
/// spring: crowded boards often route better when connectors are pinned to the
/// frame before relaxation, while quieter boards can still keep the baseline or
/// annealed candidate if hard locking is worse.
pub(crate) struct EdgeLockedPlacer;

impl Placer for EdgeLockedPlacer {
    fn name(&self) -> &'static str {
        "edge-lock"
    }

    fn place(&self, problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
        let mut edge_locked = problem.clone();
        apply_edge_lock(&mut edge_locked, &hints.edge_seek);
        place_variant(
            &edge_locked,
            hints,
            PlaceOpts {
                anneal: true,
                aspect_edge: false,
                decouple: false,
            },
        )
    }
}

/// A [`RouteRanker`] backed by the FAST grid-astar router + the shared DRC lint —
/// the built-in routability scorer the oracle uses by default. Lives here (not in
/// the kernel) so the router stays INJECTABLE: a third party supplies its own
/// `RouteRanker` to rank with its own router instead.
pub struct GridAstarRanker;

const FULL_GRID_RANKER_FALLBACK_MAX_CONNECTIONS: usize = 4;
const FULL_GRID_RANKER_FALLBACK_MAX_TERMINALS: usize = 12;

impl RouteRanker for GridAstarRanker {
    /// Route with the fast naive router, count failed-pad weight + geometry DRC
    /// violations (Connectivity excluded — it is the router's own unrouted
    /// signal, already counted in `failed`). Weighting by pins mirrors the PCB
    /// router selector: failing one high-pin-count bus/power net is worse than
    /// failing a tiny two-pin signal. Only relative routability matters for
    /// variant selection, so the slow capacity-mesh router on every candidate of
    /// a 70-part board is needlessly expensive (export re-routes with route_auto).
    ///
    /// Uses the ORTHOGONAL route as the first-pass stable proxy: it keeps the common
    /// clean-board placement choice invariant to the router's 8-way diagonal default.
    /// Within that orthogonal family it mirrors the grid router's strict/lenient
    /// fallback. If the orthogonal proxy still leaves faults on a small local route
    /// problem, the ranker pays for the full grid portfolio and keeps it when it
    /// proves strictly better; full-board placement-oracle candidates stay on the
    /// fast proxy.
    fn faults(&self, rp: &RouteProblem) -> usize {
        self.rank_key(rp).0
    }

    fn rank_key(&self, rp: &RouteProblem) -> (usize, usize, usize, u64) {
        let routed = crate::router::route_orthogonal(rp);
        let strict_key = route_rank_key(rp, &routed);
        if strict_key.0 == 0 {
            return strict_key;
        }

        let lenient = crate::router::route_orthogonal_lenient(rp);
        let lenient_key = route_rank_key(rp, &lenient);
        let orthogonal_key = if route_rank_key_better(lenient_key, strict_key) {
            lenient_key
        } else {
            strict_key
        };
        if orthogonal_key.0 == 0 {
            return orthogonal_key;
        }

        rank_key_with_full_grid_fallback(
            orthogonal_key,
            should_try_full_grid_ranker_fallback(rp, orthogonal_key),
            || route_rank_key(rp, &crate::router::GridAStarRouter.route(rp)),
        )
    }
}

pub(crate) fn route_rank_key(
    rp: &RouteProblem,
    routed: &crate::problem::RouteResult,
) -> (usize, usize, usize, u64) {
    let geom = crate::lint::lint(rp, &routed.solution)
        .iter()
        .filter(|v| !matches!(v, crate::lint::DrcViolation::Connectivity { .. }))
        .count();
    let metrics = routed.solution.metrics();
    (
        failed_pad_weight(rp, &routed.failed) + geom,
        routed.failed.len(),
        metrics.via_count,
        (metrics.wirelength * 1000.0).round() as u64,
    )
}

pub(crate) fn route_rank_key_better(
    candidate: (usize, usize, usize, u64),
    incumbent: (usize, usize, usize, u64),
) -> bool {
    candidate < incumbent
}

pub(crate) fn rank_key_with_full_grid_fallback<F>(
    orthogonal_key: (usize, usize, usize, u64),
    should_try_full_grid: bool,
    full_grid_key: F,
) -> (usize, usize, usize, u64)
where
    F: FnOnce() -> (usize, usize, usize, u64),
{
    if orthogonal_key.0 == 0 || !should_try_full_grid {
        return orthogonal_key;
    }
    let full_grid_key = full_grid_key();
    if route_rank_key_better(full_grid_key, orthogonal_key) {
        full_grid_key
    } else {
        orthogonal_key
    }
}

fn should_try_full_grid_ranker_fallback(
    rp: &RouteProblem,
    orthogonal_key: (usize, usize, usize, u64),
) -> bool {
    orthogonal_key.0 > 0
        && rp.connections.len() <= FULL_GRID_RANKER_FALLBACK_MAX_CONNECTIONS
        && rp
            .connections
            .iter()
            .map(|conn| conn.points_to_connect.len())
            .sum::<usize>()
            <= FULL_GRID_RANKER_FALLBACK_MAX_TERMINALS
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
    let mut placers: Vec<Box<dyn Placer + Send + Sync>> = vec![Box::new(LegalizingPlacer {
        opts: PlaceOpts::default(),
    })];
    placers.push(Box::new(AnnealingPlacer {
        opts: PlaceOpts {
            anneal: true,
            aspect_edge: has_edge,
            decouple: false,
        },
    }));
    if has_decouple {
        placers.push(Box::new(LegalizingPlacer {
            opts: PlaceOpts {
                decouple: true,
                aspect_edge: false,
                anneal: false,
            },
        }));
    }
    if hints.edge_seek.len() >= 2 {
        placers.push(Box::new(EdgeLockedPlacer));
    }
    if has_edge {
        placers.push(Box::new(LegalizingPlacer {
            opts: PlaceOpts {
                decouple: false,
                aspect_edge: true,
                anneal: false,
            },
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
    place_best_with_rank_key(problem, hints).0
}

fn place_best_with_rank_key(
    problem: &PlaceProblem,
    hints: &PlacementHints,
) -> (PlaceResult, PlacementRankKey) {
    let (mut best, key) = board_oracle(problem, hints).place_with_rank_key(problem, hints);
    // Post-pass: seat mounting holes (corner_seek) at the board corners on the
    // WINNING placement. They carry no signal nets (GND-plane only), so moving
    // them never changes routing — which is why this must run AFTER the faults-
    // ranked variant selection rather than inside a routing-affecting variant.
    seat_corner_seek_parts(problem, hints, &mut best);
    (best, key)
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
        if std::env::var("FANOUT_DEBUG").is_ok() {
            eprintln!("[fanout] fanned={fanned}");
        }
        if !fanned {
            apply_grid_hints(&mut p, hints);
        }
        if fanned {
            let mut result = place(&p, hints);
            seat_corner_seek_parts(problem, hints, &mut result);
            if std::env::var("FANOUT_DEBUG").is_ok() {
                eprintln!(
                    "[fanout] result legal={} hpwl={:.2} overlaps={}",
                    result.legal, result.report.hpwl, result.report.overlaps_resolved
                );
            }
            if result.legal {
                let mut base = problem.clone();
                apply_grid_hints(&mut base, hints);
                let (fallback, fallback_key) = place_best_with_rank_key(&base, hints);
                let winner = if base == *problem {
                    let result_key = place_rank_key(problem, &result);
                    better_place_result_with_keys(result, result_key, fallback, fallback_key)
                } else {
                    better_place_result(problem, result, fallback)
                };
                if std::env::var("FANOUT_DEBUG").is_ok() {
                    eprintln!(
                        "[fanout] selected legal candidate hpwl={:.2} layout_cost={:.2}",
                        winner.report.hpwl, winner.report.layout_cost
                    );
                }
                return winner;
            }
            if std::env::var("FANOUT_DEBUG").is_ok() {
                debug_overlaps(&p, &result);
            }
            let optimized = place_best(&p, hints);
            if std::env::var("FANOUT_DEBUG").is_ok() {
                eprintln!(
                    "[fanout] optimized legal={} hpwl={:.2} overlaps={}",
                    optimized.legal, optimized.report.hpwl, optimized.report.overlaps_resolved
                );
                if !optimized.legal {
                    debug_overlaps(&p, &optimized);
                }
            }
            if optimized.legal {
                return optimized;
            }
            let mut base = problem.clone();
            apply_grid_hints(&mut base, hints);
            if std::env::var("FANOUT_DEBUG").is_ok() {
                eprintln!("[fanout] falling back to non-fanned placement");
            }
            return place_best(&base, hints);
        }
        place_best(&p, hints)
    }
}

fn place_rank_key(problem: &PlaceProblem, result: &PlaceResult) -> PlacementRankKey {
    if !result.legal {
        return (
            (usize::MAX, usize::MAX, usize::MAX, u64::MAX),
            u64::MAX,
            u64::MAX,
        );
    }
    let rp = to_route_problem(problem, &result.placements);
    (
        GridAstarRanker.rank_key(&rp),
        (result.report.layout_cost * 1000.0) as u64,
        (result.report.hpwl * 1000.0) as u64,
    )
}

pub(crate) fn better_place_result(
    problem: &PlaceProblem,
    incumbent: PlaceResult,
    challenger: PlaceResult,
) -> PlaceResult {
    let incumbent_key = place_rank_key(problem, &incumbent);
    let challenger_key = place_rank_key(problem, &challenger);
    better_place_result_with_keys(incumbent, incumbent_key, challenger, challenger_key)
}

fn better_place_result_with_keys(
    incumbent: PlaceResult,
    incumbent_key: PlacementRankKey,
    challenger: PlaceResult,
    challenger_key: PlacementRankKey,
) -> PlaceResult {
    if challenger_key < incumbent_key {
        challenger
    } else {
        incumbent
    }
}

fn debug_overlaps(problem: &PlaceProblem, result: &PlaceResult) {
    let margin = courtyard_margin(problem.clearance);
    let mut pos = std::collections::BTreeMap::new();
    let mut rot = std::collections::BTreeMap::new();
    for p in &result.placements {
        pos.insert(p.reference.as_str(), p.at.clone());
        rot.insert(p.reference.as_str(), p.rotation);
    }
    let mut printed = 0usize;
    for i in 0..problem.parts.len() {
        let Some(pi) = pos.get(problem.parts[i].reference.as_str()) else {
            continue;
        };
        let hi = rotated_courtyard_half(
            &problem.parts[i],
            *rot.get(problem.parts[i].reference.as_str()).unwrap_or(&0.0),
        );
        let ri = Rect::from_center_half(*pi, hi).inflate(margin / 2.0);
        for j in (i + 1)..problem.parts.len() {
            let Some(pj) = pos.get(problem.parts[j].reference.as_str()) else {
                continue;
            };
            let hj = rotated_courtyard_half(
                &problem.parts[j],
                *rot.get(problem.parts[j].reference.as_str()).unwrap_or(&0.0),
            );
            let rj = Rect::from_center_half(*pj, hj).inflate(margin / 2.0);
            let (ox, oy) = ri.axis_penetration(&rj);
            if ox > 1e-9 && oy > 1e-9 {
                eprintln!(
                    "[fanout] overlap {} {} ox={:.2} oy={:.2}",
                    problem.parts[i].reference, problem.parts[j].reference, ox, oy
                );
                printed += 1;
                if printed >= 12 {
                    return;
                }
            }
        }
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
            x: if c.0 == b.min_x {
                b.min_x + h.0
            } else {
                b.max_x - h.0
            },
            y: if c.1 == b.min_y {
                b.min_y + h.1
            } else {
                b.max_y - h.1
            },
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
pub(crate) fn place_variant(
    problem: &PlaceProblem,
    hints: &PlacementHints,
    opts: PlaceOpts,
) -> PlaceResult {
    let n = problem.parts.len();
    let nets = derive_nets(problem);
    let margin = courtyard_margin(problem.clearance);
    let pairs = decoupling_pairs(problem);
    let edge_idx: Vec<usize> = hints
        .edge_seek
        .iter()
        .filter_map(|r| problem.parts.iter().position(|p| &p.reference == r))
        .collect();

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

    polish_rotations(
        problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &pos,
        &mut rotations,
        &mut half,
        &mut copper_bbox,
    );
    polish_positions(
        problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );
    polish_swaps(
        problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );
    polish_rotations(
        problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &pos,
        &mut rotations,
        &mut half,
        &mut copper_bbox,
    );

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

    let hpwl = compute_hpwl_with_rotations(problem, &nets, &pos, &rotations);
    let layout_cost = place_cost(
        problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
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
    problem: &PlaceProblem,
    nets: &[super::model::LogicalNet],
    margin: f64,
    pairs: &[(usize, usize)],
    edge_idx: &[usize],
    rotations: &[f64],
    half: &[(f64, f64)],
    copper_bbox: &[Rect],
    pos: &mut [Point2],
) {
    let mut cost = place_cost(problem, nets, half, margin, rotations, pairs, edge_idx, pos);
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
        for i in 0..problem.parts.len() {
            if problem.parts[i].locked.is_some() {
                continue;
            }
            let old = pos[i];
            let mut best = old;
            let mut best_cost = cost;
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
            candidates.extend(ratline_crossing_position_candidates(
                problem, nets, rotations, pos, i,
            ));
            candidates.extend(ratline_obstruction_position_candidates(
                problem, nets, rotations, half, margin, pos, i,
            ));
            for candidate in unique_position_candidates(problem, half[i], old, candidates) {
                pos[i] = candidate;
                if !is_legal(problem, half, copper_bbox, margin, pos) {
                    continue;
                }
                let next_cost =
                    place_cost(problem, nets, half, margin, rotations, pairs, edge_idx, pos);
                if next_cost + 1e-9 < best_cost {
                    best_cost = next_cost;
                    best = candidate;
                }
            }
            pos[i] = best;
            if best_cost + 1e-9 < cost {
                cost = best_cost;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
}

pub(crate) fn unique_position_candidates(
    problem: &PlaceProblem,
    half: (f64, f64),
    old: Point2,
    targets: Vec<Point2>,
) -> Vec<Point2> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for target in targets {
        let candidate = problem.bounds.clamp_center_for_half(
            Point2 {
                x: PLACEMENT_GRID.snap(target.x),
                y: PLACEMENT_GRID.snap(target.y),
            },
            half,
        );
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

pub(crate) fn net_centroid_position_candidates(
    problem: &PlaceProblem,
    nets: &[super::model::LogicalNet],
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
        candidates.push(Point2 {
            x: all_other_x / all_other_n as f64 - all_own_x / all_own_n as f64,
            y: all_other_y / all_other_n as f64 - all_own_y / all_own_n as f64,
        });
        candidates.push(Point2 {
            x: median_coord(all_other_positions.iter().map(|p| p.x))
                - median_coord(all_own_offsets.iter().map(|p| p.x)),
            y: median_coord(all_other_positions.iter().map(|p| p.y))
                - median_coord(all_own_offsets.iter().map(|p| p.y)),
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
        candidates.push(Point2 {
            x: nearest.x - own_offset.x,
            y: nearest.y - own_offset.y,
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

pub(crate) fn ratline_crossing_position_candidates(
    problem: &PlaceProblem,
    nets: &[super::model::LogicalNet],
    rotations: &[f64],
    pos: &[Point2],
    part_idx: usize,
) -> Vec<Point2> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let edges: Vec<_> = nets
        .iter()
        .enumerate()
        .flat_map(|(net_idx, net)| ratline_tree_edges(problem, rotations, pos, net_idx, net))
        .collect();

    for own_edge in &edges {
        let Some((own_pin, partner_pin, own_pad, partner_pad)) = edge_for_part(own_edge, part_idx)
        else {
            continue;
        };
        let own_offset = problem.parts[own_pin.part].pads[own_pin.pad]
            .offset
            .rotate(rotations[own_pin.part]);

        for other_edge in &edges {
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

pub(crate) fn ratline_obstruction_position_candidates(
    problem: &PlaceProblem,
    nets: &[super::model::LogicalNet],
    rotations: &[f64],
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
    part_idx: usize,
) -> Vec<Point2> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let edges: Vec<_> = nets
        .iter()
        .enumerate()
        .flat_map(|(net_idx, net)| ratline_tree_edges(problem, rotations, pos, net_idx, net))
        .collect();

    for edge in &edges {
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

fn ratline_obstacles(
    problem: &PlaceProblem,
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
    problem: &PlaceProblem,
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

#[derive(Clone, Copy)]
struct RatlineEdge<'a> {
    net_idx: usize,
    a: &'a Pin,
    b: &'a Pin,
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

fn ratline_tree_edges<'a>(
    problem: &PlaceProblem,
    rotations: &[f64],
    pos: &[Point2],
    net_idx: usize,
    net: &'a super::model::LogicalNet,
) -> Vec<RatlineEdge<'a>> {
    match net.pins.as_slice() {
        [] | [_] => Vec::new(),
        [a, b] => vec![RatlineEdge {
            net_idx,
            a,
            b,
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
                    let pa = pin_positions[ai];
                    for (bi, _) in pins.iter().enumerate() {
                        if in_tree[bi] {
                            continue;
                        }
                        let dist = pa.dist(pin_positions[bi]);
                        let replace = best.is_none_or(|(old_a, old_b)| {
                            let old_dist = pin_positions[old_a].dist(pin_positions[old_b]);
                            dist < old_dist - 1e-9
                                || ((dist - old_dist).abs() <= 1e-9 && (ai, bi) < (old_a, old_b))
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
                    a_pos: pin_positions[ai],
                    b_pos: pin_positions[bi],
                });
            }
            edges
        }
    }
}

fn pin_world_pos(
    problem: &PlaceProblem,
    rotations: &[f64],
    pos: &[Point2],
    pin: &super::model::Pin,
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
    problem: &PlaceProblem,
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
    problem: &PlaceProblem,
    nets: &[super::model::LogicalNet],
    margin: f64,
    pairs: &[(usize, usize)],
    edge_idx: &[usize],
    rotations: &[f64],
    half: &[(f64, f64)],
    copper_bbox: &[Rect],
    pos: &mut [Point2],
) {
    let mut cost = place_cost(problem, nets, half, margin, rotations, pairs, edge_idx, pos);
    for _ in 0..2 {
        let mut improved = false;
        for (a, b) in swap_pair_order(problem, nets, rotations, pos) {
            pos.swap(a, b);
            if !is_legal(problem, half, copper_bbox, margin, pos) {
                pos.swap(a, b);
                continue;
            }
            let next_cost =
                place_cost(problem, nets, half, margin, rotations, pairs, edge_idx, pos);
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
    problem: &PlaceProblem,
    nets: &[super::model::LogicalNet],
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
        let a = edges[i];
        for &b in &edges[i + 1..] {
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
            if geom::Segment::new(a.a_pos, a.b_pos).intersects(geom::Segment::new(b.a_pos, b.b_pos))
            {
                for pa in [a.a.part, a.b.part] {
                    for pb in [b.a.part, b.b.part] {
                        push_pair(&mut crossing_related, pa, pb);
                    }
                }
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
            } else {
                2
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
    problem: &PlaceProblem,
    nets: &[super::model::LogicalNet],
    margin: f64,
    pairs: &[(usize, usize)],
    edge_idx: &[usize],
    pos: &[Point2],
    rotations: &mut [f64],
    half: &mut [(f64, f64)],
    copper_bbox: &mut [Rect],
) {
    let mut cost = place_cost(problem, nets, half, margin, rotations, pairs, edge_idx, pos);

    for _ in 0..4 {
        let mut improved = false;
        for i in 0..problem.parts.len() {
            if problem.parts[i].locked.is_some() {
                continue;
            }
            let old_rot = rotations[i];
            let old_half = half[i];
            let old_copper = copper_bbox[i].clone();
            let mut best_rot = old_rot;
            let mut best_half = old_half;
            let mut best_copper = old_copper.clone();
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
                    place_cost(problem, nets, half, margin, rotations, pairs, edge_idx, pos);
                if next_cost + 1e-9 < best_cost {
                    best_cost = next_cost;
                    best_rot = candidate;
                    best_half = half[i];
                    best_copper = copper_bbox[i].clone();
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
