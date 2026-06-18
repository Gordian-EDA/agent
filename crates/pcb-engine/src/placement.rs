//! Deterministic placement: force-directed seed + legalizer (slice 4, Task 1).
//!
//! Turns a bag of footprint-shaped [`Part`]s carrying per-pad net names into
//! legal board positions, optionally steered by LLM-authored [`PlacementHints`]
//! (group cohesion, region containment, edge affinity). The engine is **pure
//! and deterministic** — no RNG, no I/O beyond serde — and the LLM never emits
//! coordinates: it emits hints (data), and [`place`] does the geometry.
//!
//! ## Canonical net source
//!
//! **Pads carry net names; logical nets are derived.** A [`PartPad`] optionally
//! names the net it belongs to ([`PartPad::net`]); the set of (part, pad) sites
//! sharing a net name *is* the net. There is no separate authoritative net list
//! to keep in sync — [`derive_nets`] groups pads by name on demand. This is the
//! single source of truth (the plan's delegated design choice): a net with one
//! pin is ignored (nothing to connect / pull toward), a net with ≥ 2 pins
//! drives a centroid spring in [`place`] and becomes a [`crate::problem::Connection`]
//! in [`to_route_problem`].
//!
//! ## The two stages
//!
//! 1. **Force-directed seed** ([`force_layout`]): parts start on a deterministic
//!    grid sorted by reference, then relax under net centroid springs, group
//!    cohesion springs, region/edge pulls, short-range courtyard repulsion
//!    (only on margin-inflated overlap — not a global n-body), and a bounds
//!    clamp. Fixed iteration count with cooling; locked parts never move.
//! 2. **Legalizer** ([`legalize`]): snap every movable part to the placement
//!    grid, then — processing parts area-descending — resolve any residual
//!    courtyard overlap by a deterministic spiral search for the nearest free
//!    grid cell, clamping in bounds. Locked parts are immovable obstacles.
//!
//! `legal` is then **verified by exact geometry** ([`is_legal`]) — never trusted
//! from the algorithm. If the legalizer cannot seat every part without overlap
//! in bounds, the result is returned with `legal: false` and a report; the
//! engine never panics and never silently overlaps.

use crate::problem::{
    Bounds, Connection, LayerRef, Obstacle, Point2, RoutePoint, RouteProblem,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── design constants (defined once) ──────────────────────────────────────────

/// Legalizer snap grid (mm). Placed positions land on multiples of this.
const PLACE_GRID: f64 = 0.5;

/// Minimum courtyard-to-courtyard gap (mm). The effective margin is
/// `max(clearance, COURTYARD_MARGIN_MIN)`.
const COURTYARD_MARGIN_MIN: f64 = 0.25;

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
/// (shortening the supply loop); [`place_best`] keeps the variant only when it
/// routes at least as cleanly, so this never regresses a board it does not help.
const DECOUPLE_K: f64 = 0.35;

/// Short-range repulsion gain on margin-inflated courtyard overlap.
const REPULSION_K: f64 = 0.5;

/// How deep the edge "band" extends from the board edge (mm) for edge affinity:
/// a part whose courtyard half-extent fits within this of the edge counts as
/// "on the edge". Also the target inset the edge pull aims for.
const EDGE_BAND: f64 = 2.0;

/// Spiral search cap: how many grid rings the legalizer probes before giving up
/// on a part (→ `legal: false`). Generous; a real board seats in a few rings.
const SPIRAL_MAX_RING: i64 = 400;

// ── model ────────────────────────────────────────────────────────────────────

/// A placement problem: board bounds, design clearance, and the parts to place.
///
/// Logical nets are **not** a field — they are derived from per-pad net names
/// ([`derive_nets`]); pads are the single canonical net source. Unknown JSON
/// fields are rejected (schema drift fails loudly), like `RouteProblem`'s
/// solution types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceProblem {
    /// Board outline (mm, y-down).
    pub bounds: Bounds,
    /// Copper-to-copper clearance (mm); also floors the courtyard margin.
    #[serde(default = "default_clearance")]
    pub clearance: f64,
    /// Number of copper layers (carried into the emitted [`RouteProblem`]).
    #[serde(default = "default_layer_count")]
    pub layer_count: u32,
    /// Minimum trace width (mm), carried into the emitted [`RouteProblem`].
    #[serde(default = "default_min_trace_width")]
    pub min_trace_width: f64,
    /// The parts to place.
    pub parts: Vec<Part>,
}

fn default_clearance() -> f64 {
    0.2
}
fn default_layer_count() -> u32 {
    2
}
fn default_min_trace_width() -> f64 {
    0.2
}

/// A footprint-shaped part: a reference, a courtyard rectangle (centered on the
/// part origin), its pads, and an optional locked position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Part {
    /// Schematic reference designator ("R1", "U2", "J1"). Must be unique;
    /// determinism sorts parts by this.
    pub reference: String,
    /// Courtyard width (x extent, mm), centered on the part origin at rotation 0.
    pub courtyard_w: f64,
    /// Courtyard height (y extent, mm), centered on the part origin at rotation 0.
    pub courtyard_h: f64,
    /// The part's pads (offsets are relative to the part origin at rotation 0).
    pub pads: Vec<PartPad>,
    /// If present, the part is pinned at this position/rotation and never moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<LockedAt>,
}

/// One pad: a number, an offset from the part origin (rotation 0), a size, the
/// copper layers it sits on, and the net name it belongs to (the canonical net
/// source).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PartPad {
    /// Pad number/name ("1", "2", "A1").
    pub number: String,
    /// Offset from the part origin at rotation 0 (mm).
    pub offset: Point2,
    /// Pad width (x, mm at rotation 0).
    pub width: f64,
    /// Pad height (y, mm at rotation 0).
    pub height: f64,
    /// Copper layers the pad sits on ("top", "bottom", … — thru-hole lists all).
    pub layers: Vec<LayerRef>,
    /// The net this pad belongs to, if any. `None` = unconnected pad.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
}

/// A locked (pinned) placement: an absolute position and a rotation in degrees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockedAt {
    /// Absolute part-origin position (mm).
    pub at: Point2,
    /// Rotation in degrees (0/90/180/270; other values are snapped).
    #[serde(default)]
    pub rotation: i32,
}

/// LLM-authored placement hints. **Empty hints are valid** and must produce a
/// legal placement (hints improve, never gate). Data only — slice 5's LLM
/// integration is a serde/prompt problem, not an engine change.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlacementHints {
    /// Grouping/region/edge hints.
    #[serde(default)]
    pub groups: Vec<GroupHint>,
    /// References that should be pulled to their NEAREST board edge (connectors,
    /// headers, mounting holes — parts a cable or the enclosure reaches from
    /// outside). Unlike a group `edge` hint, the engine picks each part's nearest
    /// edge automatically, so the caller need not know the final layout. A
    /// professional board puts these at the perimeter, not stranded in the
    /// interior with copper wrapping around them.
    #[serde(default)]
    pub edge_seek: Vec<String>,
}

/// A group of parts that should cohere, optionally pulled into a region and/or
/// toward a board edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupHint {
    /// Human label (for provenance/debug; not load-bearing).
    pub name: String,
    /// References of the parts in this group.
    pub members: Vec<String>,
    /// Optional rectangle the members should land inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Rect>,
    /// Optional board edge the group should hug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<Edge>,
}

/// An axis-aligned region rectangle (mm).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rect {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

impl Rect {
    fn center(&self) -> Point2 {
        Point2 {
            x: (self.min_x + self.max_x) / 2.0,
            y: (self.min_y + self.max_y) / 2.0,
        }
    }
    fn contains(&self, p: &Point2) -> bool {
        p.x >= self.min_x && p.x <= self.max_x && p.y >= self.min_y && p.y <= self.max_y
    }
}

/// A board edge for edge-affinity hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Edge {
    N,
    S,
    E,
    W,
}

/// The result of [`place`]: per-part placements, a legality verdict (verified by
/// exact geometry), and a quality/diagnostic report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceResult {
    /// Placed parts (one per input part, in input order).
    pub placements: Vec<Placement>,
    /// True iff no courtyard overlap (with margin) and all parts in bounds —
    /// verified by exact geometry at the end, not trusted from the algorithm.
    pub legal: bool,
    /// Diagnostics + the HPWL quality number.
    pub report: PlaceReport,
}

/// One part's final placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Placement {
    /// The part this places.
    pub reference: String,
    /// Final part-origin position (mm).
    pub at: Point2,
    /// Final rotation (degrees; 0/90/180/270).
    pub rotation: i32,
}

/// Placement diagnostics: how much legalization happened and the HPWL metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceReport {
    /// How many parts the spiral legalizer had to move off their snapped cell.
    pub overlaps_resolved: usize,
    /// How many parts were clamped because the force layout pushed them out of
    /// bounds.
    pub out_of_bounds_clamps: usize,
    /// Half-perimeter wirelength over net bounding boxes (mm) — the cheap
    /// placement-quality number (lower is tighter).
    pub hpwl: f64,
}

// ── derived nets ─────────────────────────────────────────────────────────────

/// A pin site: which part, which pad.
#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    /// Index into `problem.parts`.
    pub part: usize,
    /// Index into that part's `pads`.
    pub pad: usize,
}

/// A derived logical net: a name and the pin sites that share it.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalNet {
    pub name: String,
    pub pins: Vec<Pin>,
}

/// Derive logical nets from per-pad net names — the canonical source. Pins are
/// grouped by net name in deterministic (name, part, pad) order. Single-pin
/// nets are kept (callers filter: a 1-pin net has nothing to connect).
pub fn derive_nets(problem: &PlaceProblem) -> Vec<LogicalNet> {
    let mut by_name: BTreeMap<String, Vec<Pin>> = BTreeMap::new();
    for (pi, part) in problem.parts.iter().enumerate() {
        for (di, pad) in part.pads.iter().enumerate() {
            if let Some(net) = &pad.net {
                by_name
                    .entry(net.clone())
                    .or_default()
                    .push(Pin { part: pi, pad: di });
            }
        }
    }
    by_name
        .into_iter()
        .map(|(name, pins)| LogicalNet { name, pins })
        .collect()
}

// ── public entry: place ──────────────────────────────────────────────────────

/// Detect decoupling co-placement pairs `(cap_idx, ic_idx)`: a 2-pad part whose
/// BOTH pad nets also appear on a larger (≥3-pad) part is its decoupling cap and
/// should hug that IC/regulator. The smaller-index qualifying anchor wins
/// (deterministic). A part wired to two unrelated nets (e.g. a divider resistor)
/// finds no single anchor with both nets, so this fires only for real bypass caps.
fn decoupling_pairs(problem: &PlaceProblem) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    for (si, small) in problem.parts.iter().enumerate() {
        if small.pads.len() != 2 {
            continue;
        }
        let nets: Vec<&str> = small.pads.iter().filter_map(|p| p.net.as_deref()).collect();
        if nets.len() != 2 || nets[0] == nets[1] {
            continue;
        }
        for (ai, anc) in problem.parts.iter().enumerate() {
            if ai == si || anc.pads.len() < 3 {
                continue;
            }
            let anc_nets: std::collections::BTreeSet<&str> =
                anc.pads.iter().filter_map(|p| p.net.as_deref()).collect();
            if anc_nets.contains(nets[0]) && anc_nets.contains(nets[1]) {
                pairs.push((si, ai));
                break;
            }
        }
    }
    pairs
}

/// Place `problem` and return the variant that ROUTES cleanest — the placement
/// analog of [`crate::pipeline::route_auto`]. It runs the baseline placement plus
/// idiom variants (decoupling co-placement, aspect-aware connector edges, both),
/// routes each, and keeps whichever yields fewer routing faults (unrouted nets +
/// geometry DRC violations), breaking ties by lower routed wirelength then HPWL.
/// The baseline is always a candidate, so an idiom variant that does not actually
/// help (e.g. one that scatters a board's power net) is automatically discarded —
/// the oracle decides per board, so aggressive idioms can never regress a board
/// they do not improve.
pub fn place_best(problem: &PlaceProblem, hints: &PlacementHints) -> PlaceResult {
    let has_decouple = !decoupling_pairs(problem).is_empty();
    let has_edge = !hints.edge_seek.is_empty();

    // The variants worth trying for THIS board (always include the baseline).
    let mut opts = vec![PlaceOpts::default()];
    if has_decouple {
        opts.push(PlaceOpts { decouple: true, aspect_edge: false });
    }
    if has_edge {
        opts.push(PlaceOpts { decouple: false, aspect_edge: true });
    }
    if has_decouple && has_edge {
        opts.push(PlaceOpts { decouple: true, aspect_edge: true });
    }

    let cost = |r: &PlaceResult| -> (usize, u64, u64) {
        if !r.legal {
            return (usize::MAX, u64::MAX, u64::MAX);
        }
        let rp = to_route_problem(problem, &r.placements);
        let routed = crate::pipeline::route_auto(&rp);
        let geom = crate::lint::lint(&rp, &routed.solution)
            .iter()
            .filter(|v| !matches!(v, crate::lint::DrcViolation::Connectivity { .. }))
            .count();
        let wl = crate::pipeline::metrics(&routed.solution).wirelength;
        // Scale f64 metrics to sortable u64 (all non-negative finite).
        (routed.failed.len() + geom, (wl * 1000.0) as u64, (r.report.hpwl * 1000.0) as u64)
    };

    // Baseline first so it wins exact ties (battle-tested), then keep the best.
    let mut best = place_variant(problem, hints, PlaceOpts::default());
    let mut best_cost = cost(&best);
    for &o in opts.iter().skip(1) {
        let cand = place_variant(problem, hints, o);
        let c = cost(&cand);
        if c < best_cost {
            best = cand;
            best_cost = c;
        }
    }
    best
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
fn place_variant(problem: &PlaceProblem, hints: &PlacementHints, opts: PlaceOpts) -> PlaceResult {
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
    let legal = is_legal(problem, &half, margin, &pos);

    let hpwl = compute_hpwl(problem, &nets, &pos, &rotations);

    PlaceResult {
        placements,
        legal,
        report: PlaceReport {
            overlaps_resolved: leg.overlaps_resolved,
            out_of_bounds_clamps: leg.out_of_bounds_clamps,
            hpwl,
        },
    }
}

// ── force-directed seed ──────────────────────────────────────────────────────

/// Relax `pos` under the force model. Locked parts are anchors (never moved) but
/// still attract movable parts through shared nets/groups. Deterministic: fixed
/// iteration count, no RNG, forces summed in a fixed order.
fn force_layout(
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
                    if !region.contains(&pos[m]) {
                        force[m].0 += REGION_PULL_K * (c.x - pos[m].x);
                        force[m].1 += REGION_PULL_K * (c.y - pos[m].y);
                    }
                }
            }
            if let Some(edge) = &g.edge {
                for &m in members {
                    let target = edge_target(*edge, &problem.bounds, half[m]);
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
                aspect_edge(&pos[m], &problem.bounds, problem.parts[m].courtyard_w, problem.parts[m].courtyard_h)
            } else {
                nearest_edge(&pos[m], &problem.bounds)
            };
            let target = edge_target(edge, &problem.bounds, half[m]);
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
            for j in (i + 1)..n {
                let (ox, oy) = courtyard_overlap(pos, half, margin, i, j);
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
            clamp_into_bounds(&mut pos[i], &problem.bounds, half[i]);
        }
    }
}

/// A deterministic non-zero sign: +1 for ≥ 0, -1 for < 0 (so coincident parts
/// still get a fixed separating direction).
fn sign_nonzero(v: f64) -> f64 {
    if v < 0.0 {
        -1.0
    } else {
        1.0
    }
}

// ── legalizer ────────────────────────────────────────────────────────────────

/// Outcome counters from [`legalize`].
struct LegalizeStats {
    overlaps_resolved: usize,
    out_of_bounds_clamps: usize,
}

/// Snap movable parts to the placement grid, then resolve residual courtyard
/// overlaps by a deterministic nearest-free-cell spiral, processing parts
/// area-descending (big parts seat first). Locked parts are fixed obstacles.
///
/// Records (and never trusts) — legality is re-checked by [`is_legal`] after.
fn legalize(
    problem: &PlaceProblem,
    half: &[(f64, f64)],
    margin: f64,
    pos: &mut [Point2],
) -> LegalizeStats {
    let n = problem.parts.len();
    let locked: Vec<bool> = problem.parts.iter().map(|p| p.locked.is_some()).collect();

    let mut out_of_bounds_clamps = 0;
    // Snap + clamp movable parts.
    for i in 0..n {
        if locked[i] {
            continue;
        }
        let before = pos[i].clone();
        pos[i].x = snap(pos[i].x);
        pos[i].y = snap(pos[i].y);
        clamp_into_bounds(&mut pos[i], &problem.bounds, half[i]);
        if (pos[i].x - before.x).abs() > PLACE_GRID || (pos[i].y - before.y).abs() > PLACE_GRID {
            // A real bounds clamp (more than a snap's worth of motion).
            out_of_bounds_clamps += 1;
        }
    }

    // Process order: area-descending, ties by reference (deterministic). Locked
    // parts are placed (immovable) first as obstacles by seeding `placed`.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        let aa = half[a].0 * half[a].1;
        let ab = half[b].0 * half[b].1;
        ab.partial_cmp(&aa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| problem.parts[a].reference.cmp(&problem.parts[b].reference))
    });

    let mut placed: Vec<usize> = Vec::new();
    // Seat locked parts first (they are immovable obstacles for everyone).
    for &i in &order {
        if locked[i] {
            placed.push(i);
        }
    }

    let mut overlaps_resolved = 0;
    for &i in &order {
        if locked[i] {
            continue;
        }
        // Does the snapped cell collide with anything already placed?
        if !collides(&pos[i], half[i], pos, half, margin, &placed) {
            placed.push(i);
            continue;
        }
        // Spiral out from the snapped cell for the nearest free grid cell.
        let origin = pos[i].clone();
        if let Some(found) = spiral_free_cell(problem, half, margin, i, &placed, &origin, pos) {
            pos[i] = found;
            overlaps_resolved += 1;
            placed.push(i);
        } else {
            // No legal cell within the cap: leave it (is_legal will flag the
            // board) and still mark it placed so others route around its spot.
            placed.push(i);
        }
    }

    LegalizeStats {
        overlaps_resolved,
        out_of_bounds_clamps,
    }
}

/// Spiral outward from `origin` (on the placement grid) for the nearest cell
/// where part `i` collides with none of `placed` and stays in bounds. Rings are
/// probed in increasing Chebyshev radius; within a ring, cells are visited in a
/// fixed (sorted) order for determinism. `None` if nothing is found within
/// [`SPIRAL_MAX_RING`] rings.
fn spiral_free_cell(
    problem: &PlaceProblem,
    half: &[(f64, f64)],
    margin: f64,
    i: usize,
    placed: &[usize],
    origin: &Point2,
    pos: &[Point2],
) -> Option<Point2> {
    for ring in 1..=SPIRAL_MAX_RING {
        // Collect this ring's offsets, sorted deterministically: by squared
        // distance, then dy, then dx — so the nearest cell (fixed tiebreak) wins.
        let mut cells: Vec<(i64, i64)> = Vec::new();
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs() == ring || dy.abs() == ring {
                    cells.push((dx, dy));
                }
            }
        }
        cells.sort_by_key(|&(dx, dy)| (dx * dx + dy * dy, dy, dx));
        for (dx, dy) in cells {
            let cand = Point2 {
                x: snap(origin.x + dx as f64 * PLACE_GRID),
                y: snap(origin.y + dy as f64 * PLACE_GRID),
            };
            // Must fit in bounds without clamping (clamping would move it off
            // the probed cell and could re-collide).
            if !fits_in_bounds(&cand, &problem.bounds, half[i]) {
                continue;
            }
            if !collides(&cand, half[i], pos, half, margin, placed) {
                return Some(cand);
            }
        }
    }
    None
}

// ── collision predicate ──────────────────────────────────────────────────────

/// Does a part at `cand` with half-extent `cand_half` margin-overlap any part in
/// `placed` (whose positions are `pos[j]`, half-extents `half[j]`)?
fn collides(
    cand: &Point2,
    cand_half: (f64, f64),
    pos: &[Point2],
    half: &[(f64, f64)],
    margin: f64,
    placed: &[usize],
) -> bool {
    placed.iter().any(|&j| {
        let (ox, oy) = rect_overlap(cand, cand_half, &pos[j], half[j], margin);
        ox > 0.0 && oy > 0.0
    })
}

// ── geometry helpers ─────────────────────────────────────────────────────────

/// Margin-inflated overlap of two parts' courtyards, per axis (mm; >0 on both
/// axes ⇒ overlapping). Each courtyard is inflated by `margin/2` per side so the
/// required *gap* between courtyards is `margin`.
fn courtyard_overlap(
    pos: &[Point2],
    half: &[(f64, f64)],
    margin: f64,
    i: usize,
    j: usize,
) -> (f64, f64) {
    rect_overlap(&pos[i], half[i], &pos[j], half[j], margin)
}

/// Margin-inflated axis overlaps of two centered rects.
fn rect_overlap(
    ci: &Point2,
    hi: (f64, f64),
    cj: &Point2,
    hj: (f64, f64),
    margin: f64,
) -> (f64, f64) {
    let m = margin / 2.0;
    let ox = (hi.0 + m + hj.0 + m) - (ci.x - cj.x).abs();
    let oy = (hi.1 + m + hj.1 + m) - (ci.y - cj.y).abs();
    (ox, oy)
}

/// Does a part's courtyard fit fully within `bounds`?
fn fits_in_bounds(p: &Point2, b: &Bounds, h: (f64, f64)) -> bool {
    p.x - h.0 >= b.min_x - 1e-9
        && p.x + h.0 <= b.max_x + 1e-9
        && p.y - h.1 >= b.min_y - 1e-9
        && p.y + h.1 <= b.max_y + 1e-9
}

/// Clamp a part origin so its courtyard fits in bounds (best effort: if the part
/// is wider than the board, it is centered on that axis).
fn clamp_into_bounds(p: &mut Point2, b: &Bounds, h: (f64, f64)) {
    let (lo_x, hi_x) = (b.min_x + h.0, b.max_x - h.0);
    let (lo_y, hi_y) = (b.min_y + h.1, b.max_y - h.1);
    p.x = if lo_x <= hi_x {
        p.x.clamp(lo_x, hi_x)
    } else {
        (b.min_x + b.max_x) / 2.0
    };
    p.y = if lo_y <= hi_y {
        p.y.clamp(lo_y, hi_y)
    } else {
        (b.min_y + b.max_y) / 2.0
    };
}

/// Snap a coordinate to the placement grid.
fn snap(v: f64) -> f64 {
    (v / PLACE_GRID).round() * PLACE_GRID
}

/// The effective courtyard margin: `max(clearance, COURTYARD_MARGIN_MIN)`.
fn courtyard_margin(clearance: f64) -> f64 {
    clearance.max(COURTYARD_MARGIN_MIN)
}

/// Snap an arbitrary rotation (degrees) to the nearest quadrant in 0/90/180/270.
fn snap_rotation(deg: i32) -> i32 {
    let r = deg.rem_euclid(360);
    (((r + 45) / 90) * 90) % 360
}

/// Courtyard half-extents after a quadrant rotation (90/270 swap w/h).
fn rotated_courtyard_half(part: &Part, rot: i32) -> (f64, f64) {
    let (w, h) = (part.courtyard_w / 2.0, part.courtyard_h / 2.0);
    match rot {
        90 | 270 => (h, w),
        _ => (w, h),
    }
}

/// A pad offset rotated by a quadrant (degrees), y-down.
fn rotate_offset(off: &Point2, rot: i32) -> Point2 {
    // KiCAD footprint-rotation convention (y-down board coords): a pad's local
    // offset under a footprint rotated by `rot` lands at these world offsets.
    // Verified against kicad-cli: a 270° footprint maps local (x,y) → (-y, x).
    // (The 90 and 270 cases were previously swapped, which placed the engine's
    // routing targets on the WRONG physical pad for any rotated part → shorts.)
    match rot.rem_euclid(360) {
        90 => Point2 { x: off.y, y: -off.x },
        180 => Point2 { x: -off.x, y: -off.y },
        270 => Point2 { x: -off.y, y: off.x },
        _ => off.clone(),
    }
}

/// World position of a pin's pad center given current part positions.
fn pad_world(problem: &PlaceProblem, pos: &[Point2], pin: &Pin) -> Point2 {
    let part = &problem.parts[pin.part];
    let rot = part.locked.as_ref().map(|l| snap_rotation(l.rotation)).unwrap_or(0);
    let off = rotate_offset(&part.pads[pin.pad].offset, rot);
    Point2 {
        x: pos[pin.part].x + off.x,
        y: pos[pin.part].y + off.y,
    }
}

/// The pull target for an edge hint: a point on the edge band line, keeping the
/// part's other coordinate where it is (only the edge-normal coordinate matters).
/// Per-variant placement toggles, tried and selected by [`place_best`].
#[derive(Debug, Clone, Copy, Default)]
struct PlaceOpts {
    /// Pull each decoupling cap to hug its IC ([`decoupling_pairs`]).
    decouple: bool,
    /// Bias edge-seeking by part aspect: a tall connector goes to a side edge so
    /// its pad column lies along it, not the top where it pokes inward.
    aspect_edge: bool,
}

/// The board edge a part should hug given its aspect: a part taller than wide
/// prefers the nearer SIDE edge (E/W) — its long axis then runs along the edge;
/// a wider part prefers the nearer top/bottom (N/S). Square parts fall back to
/// the overall nearest edge.
fn aspect_edge(p: &Point2, b: &Bounds, w: f64, h: f64) -> Edge {
    if h > w {
        if p.x - b.min_x <= b.max_x - p.x { Edge::W } else { Edge::E }
    } else if w > h {
        if p.y - b.min_y <= b.max_y - p.y { Edge::N } else { Edge::S }
    } else {
        nearest_edge(p, b)
    }
}

/// The board edge nearest to `p`. Ties break in N, S, W, E order (deterministic).
fn nearest_edge(p: &Point2, b: &Bounds) -> Edge {
    let d_n = p.y - b.min_y;
    let d_s = b.max_y - p.y;
    let d_w = p.x - b.min_x;
    let d_e = b.max_x - p.x;
    let mut best = Edge::N;
    let mut best_d = d_n;
    for (d, e) in [(d_s, Edge::S), (d_w, Edge::W), (d_e, Edge::E)] {
        if d < best_d {
            best_d = d;
            best = e;
        }
    }
    best
}

fn edge_target(edge: Edge, b: &Bounds, h: (f64, f64)) -> f64 {
    match edge {
        Edge::N => b.min_y + h.1 + EDGE_BAND.min((b.max_y - b.min_y) / 2.0),
        Edge::S => b.max_y - h.1 - EDGE_BAND.min((b.max_y - b.min_y) / 2.0),
        Edge::W => b.min_x + h.0 + EDGE_BAND.min((b.max_x - b.min_x) / 2.0),
        Edge::E => b.max_x - h.0 - EDGE_BAND.min((b.max_x - b.min_x) / 2.0),
    }
}

/// The edge pull delta (only the edge-normal axis is driven; the tangential axis
/// is left to nets/groups).
fn edge_delta(edge: Edge, p: &Point2, target: f64) -> (f64, f64) {
    match edge {
        Edge::N | Edge::S => (0.0, target - p.y),
        Edge::E | Edge::W => (target - p.x, 0.0),
    }
}

// ── exact-geometry legality check ────────────────────────────────────────────

/// The placement analog of the lint: re-verify in exact geometry that no two
/// courtyards overlap (with margin) and every part is in bounds. Never trusts
/// the legalizer.
fn is_legal(problem: &PlaceProblem, half: &[(f64, f64)], margin: f64, pos: &[Point2]) -> bool {
    let n = problem.parts.len();
    for i in 0..n {
        if !fits_in_bounds(&pos[i], &problem.bounds, half[i]) {
            return false;
        }
        for j in (i + 1)..n {
            let (ox, oy) = courtyard_overlap(pos, half, margin, i, j);
            // Strictly-positive overlap on BOTH axes is a real courtyard
            // collision. Touching exactly at the margin (overlap == 0) is legal.
            if ox > 1e-9 && oy > 1e-9 {
                return false;
            }
        }
    }
    true
}

// ── HPWL ─────────────────────────────────────────────────────────────────────

/// Half-perimeter wirelength over net bounding boxes (mm): for each multi-pin
/// net, `(maxX-minX) + (maxY-minY)` of its pad world positions, summed.
fn compute_hpwl(
    problem: &PlaceProblem,
    nets: &[LogicalNet],
    pos: &[Point2],
    _rot: &[i32],
) -> f64 {
    let mut total = 0.0;
    for net in nets {
        if net.pins.len() < 2 {
            continue;
        }
        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for pin in &net.pins {
            let w = pad_world(problem, pos, pin);
            min_x = min_x.min(w.x);
            max_x = max_x.max(w.x);
            min_y = min_y.min(w.y);
            max_y = max_y.max(w.y);
        }
        total += (max_x - min_x) + (max_y - min_y);
    }
    total
}

// ── initial grid ─────────────────────────────────────────────────────────────

/// Deterministic initial layout: parts (sorted by reference) on a near-square
/// grid sized to the largest courtyard, anchored at the board's top-left inset.
/// No RNG — the grid is a pure function of the parts.
fn initial_grid(problem: &PlaceProblem, half: &[(f64, f64)]) -> Vec<Point2> {
    let n = problem.parts.len();
    let mut pos = vec![Point2 { x: 0.0, y: 0.0 }; n];
    if n == 0 {
        return pos;
    }

    // Sort indices by reference for a deterministic cell assignment.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| problem.parts[a].reference.cmp(&problem.parts[b].reference));

    // Cell pitch = largest courtyard extent + a margin, snapped to the grid.
    let max_half = half
        .iter()
        .map(|(w, h)| w.max(*h))
        .fold(0.0_f64, f64::max);
    let pitch = snap((max_half * 2.0 + courtyard_margin(problem.clearance)).max(PLACE_GRID)) + PLACE_GRID;

    let cols = (n as f64).sqrt().ceil().max(1.0) as usize;
    let b = &problem.bounds;
    let x0 = b.min_x + max_half + PLACE_GRID;
    let y0 = b.min_y + max_half + PLACE_GRID;

    for (rank, &i) in order.iter().enumerate() {
        let r = rank / cols;
        let c = rank % cols;
        let mut p = Point2 {
            x: x0 + c as f64 * pitch,
            y: y0 + r as f64 * pitch,
        };
        clamp_into_bounds(&mut p, b, half[i]);
        pos[i] = p;
    }
    pos
}

// ── to_route_problem ─────────────────────────────────────────────────────────

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
    }
}

/// Via geometry carried into the emitted [`RouteProblem`] (mirrors `problem.rs`
/// defaults — the value the existing fixtures and oracle expect).
const DEFAULT_VIA_DIAMETER: f64 = 0.6;
const DEFAULT_VIA_DRILL: f64 = 0.3;

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectivity;
    use crate::lint::lint;
    use crate::pipeline::route_auto;

    fn board(w: f64, h: f64) -> Bounds {
        Bounds {
            min_x: 0.0,
            max_x: w,
            min_y: 0.0,
            max_y: h,
        }
    }

    fn top() -> Vec<LayerRef> {
        vec![LayerRef::top()]
    }

    /// An R_0603-ish 2-pad part (crib numbers from the vendored footprint:
    /// pads at ±0.825, 0.8×0.95). The courtyard here is the pad-enclosing one
    /// (2.8×1.4): a courtyard MUST enclose its pads for courtyard-only
    /// legalization to imply pad clearance — see the to_route_problem finding.
    /// (The vendored R_0603 ships a tight body-hugging F.CrtYd of 1.6×0.825 that
    /// does NOT enclose the ±1.225 pad span; using that here would let two
    /// gap-legal courtyards still short foreign pads.)
    fn r0603(reference: &str, pad1_net: Option<&str>, pad2_net: Option<&str>) -> Part {
        Part {
            reference: reference.to_owned(),
            courtyard_w: 2.8,
            courtyard_h: 1.4,
            pads: vec![
                PartPad {
                    number: "1".to_owned(),
                    offset: Point2 { x: -0.825, y: 0.0 },
                    width: 0.8,
                    height: 0.95,
                    layers: top(),
                    net: pad1_net.map(str::to_owned),
                },
                PartPad {
                    number: "2".to_owned(),
                    offset: Point2 { x: 0.825, y: 0.0 },
                    width: 0.8,
                    height: 0.95,
                    layers: top(),
                    net: pad2_net.map(str::to_owned),
                },
            ],
            locked: None,
        }
    }

    fn place_at(p: &mut Part, x: f64, y: f64, rotation: i32) {
        p.locked = Some(LockedAt {
            at: Point2 { x, y },
            rotation,
        });
    }

    // ── empty hints: legal + deterministic ──────────────────────────────────

    #[test]
    fn empty_hints_small_board_is_legal_and_deterministic() {
        let problem = PlaceProblem {
            bounds: board(30.0, 20.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![
                r0603("R1", Some("A"), Some("B")),
                r0603("R2", Some("B"), Some("C")),
                r0603("R3", Some("C"), Some("A")),
            ],
        };
        let hints = PlacementHints::default();
        let a = place(&problem, &hints);
        assert!(a.legal, "empty-hints placement must be legal: {a:?}");

        // Determinism: serialize twice, byte-equal (no RNG).
        let b = place(&problem, &hints);
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        assert_eq!(ja, jb, "two place() runs must serialize byte-equal");
    }

    // ── locked parts never move ─────────────────────────────────────────────

    #[test]
    fn locked_part_does_not_move() {
        let mut locked = r0603("R1", Some("A"), Some("B"));
        place_at(&mut locked, 7.5, 12.0, 90);
        let problem = PlaceProblem {
            bounds: board(30.0, 20.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![
                locked,
                r0603("R2", Some("B"), Some("C")),
                r0603("R3", Some("C"), Some("A")),
            ],
        };
        let res = place(&problem, &PlacementHints::default());
        let r1 = res.placements.iter().find(|p| p.reference == "R1").unwrap();
        assert_eq!(r1.at, Point2 { x: 7.5, y: 12.0 }, "locked R1 must stay put");
        assert_eq!(r1.rotation, 90, "locked rotation preserved");
        assert!(res.legal, "board with a locked part still legal: {res:?}");
    }

    // ── connected parts end closer than unconnected ─────────────────────────

    #[test]
    fn connected_parts_end_closer_than_unconnected() {
        // R1 and R9 share net "L" but sort to opposite ends of the deterministic
        // initial grid (refs are placed in sorted order across a near-square
        // grid). The net spring must pull them together so the connected pair
        // ends MUCH closer than the unconnected pair (R3, R7) that the grid keeps
        // apart. This proves the spring overcomes the seed, not that any two
        // adjacent grid cells differ.
        let problem = PlaceProblem {
            bounds: board(60.0, 50.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![
                r0603("R1", Some("L"), Some("P1")),
                r0603("R2", None, None),
                r0603("R3", None, None),
                r0603("R4", None, None),
                r0603("R5", None, None),
                r0603("R6", None, None),
                r0603("R7", None, None),
                r0603("R8", None, None),
                r0603("R9", Some("L"), Some("P2")),
            ],
        };
        let res = place(&problem, &PlacementHints::default());
        assert!(res.legal, "{res:?}");
        let at = |r: &str| {
            res.placements
                .iter()
                .find(|p| p.reference == r)
                .map(|p| p.at.clone())
                .unwrap()
        };
        let d = |a: Point2, b: Point2| ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
        let connected = d(at("R1"), at("R9"));
        // Two parts the grid seeds at opposite ends and that no net pulls together.
        let unconnected = d(at("R3"), at("R7"));
        assert!(
            connected < unconnected,
            "connected R1-R9 ({connected:.2}) must be closer than unconnected R3-R7 ({unconnected:.2})"
        );
    }

    // ── region containment ──────────────────────────────────────────────────

    #[test]
    fn group_with_region_lands_members_inside() {
        let region = Rect {
            min_x: 40.0,
            max_x: 58.0,
            min_y: 22.0,
            max_y: 38.0,
        };
        let problem = PlaceProblem {
            bounds: board(60.0, 40.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![
                r0603("R1", Some("A"), Some("B")),
                r0603("R2", Some("B"), Some("C")),
                r0603("R3", None, None),
            ],
        };
        let hints = PlacementHints {
            groups: vec![GroupHint {
                name: "corner".to_owned(),
                members: vec!["R1".to_owned(), "R2".to_owned()],
                region: Some(region.clone()),
                edge: None,
            }],
            ..Default::default()
        };
        let res = place(&problem, &hints);
        assert!(res.legal, "{res:?}");
        for r in ["R1", "R2"] {
            let p = res.placements.iter().find(|p| p.reference == r).unwrap();
            assert!(
                region.contains(&p.at),
                "{r} at {:?} must land inside region {region:?}",
                p.at
            );
        }
    }

    // ── edge affinity ───────────────────────────────────────────────────────

    #[test]
    fn edge_affinity_part_touches_edge_band() {
        let problem = PlaceProblem {
            bounds: board(60.0, 40.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![
                // A connector-ish 2-pin part.
                Part {
                    reference: "J1".to_owned(),
                    courtyard_w: 2.54,
                    courtyard_h: 3.81,
                    pads: vec![
                        PartPad {
                            number: "1".to_owned(),
                            offset: Point2 { x: 0.0, y: -1.27 },
                            width: 1.7,
                            height: 1.7,
                            layers: vec![LayerRef::top(), LayerRef::bottom()],
                            net: Some("NET1".to_owned()),
                        },
                        PartPad {
                            number: "2".to_owned(),
                            offset: Point2 { x: 0.0, y: 1.27 },
                            width: 1.7,
                            height: 1.7,
                            layers: vec![LayerRef::top(), LayerRef::bottom()],
                            net: Some("NET2".to_owned()),
                        },
                    ],
                    locked: None,
                },
                r0603("R1", Some("NET1"), Some("X")),
                r0603("R2", Some("NET2"), Some("Y")),
            ],
        };
        let hints = PlacementHints {
            groups: vec![GroupHint {
                name: "connector".to_owned(),
                members: vec!["J1".to_owned()],
                region: None,
                edge: Some(Edge::W),
            }],
            ..Default::default()
        };
        let res = place(&problem, &hints);
        assert!(res.legal, "{res:?}");
        let j1 = res.placements.iter().find(|p| p.reference == "J1").unwrap();
        // West edge band: the courtyard's left edge within EDGE_BAND of min_x.
        let left_edge = j1.at.x - 2.54 / 2.0;
        assert!(
            left_edge <= problem.bounds.min_x + EDGE_BAND + PLACE_GRID,
            "J1 left edge {left_edge:.2} must sit in the west band (<= {:.2})",
            problem.bounds.min_x + EDGE_BAND + PLACE_GRID
        );
    }

    // ── overlap resolution: everything starts at one point ──────────────────

    #[test]
    fn all_at_one_point_resolves_to_no_overlap() {
        // Six parts all LOCKED-free but seeded by the engine; then we additionally
        // stress the legalizer by forcing a degenerate seed via tiny board cell:
        // simplest expression — many parts, small-ish board, no nets (pure repulsion
        // + legalizer must still separate them).
        let parts: Vec<Part> = (0..8)
            .map(|i| r0603(&format!("R{i}"), None, None))
            .collect();
        let problem = PlaceProblem {
            bounds: board(40.0, 40.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts,
        };
        let res = place(&problem, &PlacementHints::default());
        assert!(
            res.legal,
            "8 parts must legalize to zero overlap on a 40x40 board: {res:?}"
        );

        // Stronger: directly verify exact geometry has no courtyard overlap.
        let half: Vec<(f64, f64)> = problem
            .parts
            .iter()
            .map(|p| (p.courtyard_w / 2.0, p.courtyard_h / 2.0))
            .collect();
        let pos: Vec<Point2> = res.placements.iter().map(|p| p.at.clone()).collect();
        assert!(is_legal(&problem, &half, courtyard_margin(0.2), &pos));
    }

    // ── to_route_problem: parseable + connectivity oracle accepts pads/points ─

    #[test]
    fn to_route_problem_round_trips_and_oracle_accepts_geometry() {
        let problem = PlaceProblem {
            bounds: board(30.0, 20.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.25,
            parts: vec![
                r0603("R1", Some("SIG"), Some("GND")),
                r0603("R2", Some("SIG"), Some("GND")),
            ],
        };
        let res = place(&problem, &PlacementHints::default());
        assert!(res.legal);
        let rp = to_route_problem(&problem, &res.placements);

        // Round-trips serde.
        let json = serde_json::to_string(&rp).unwrap();
        let rp2: RouteProblem = serde_json::from_str(&json).unwrap();
        assert_eq!(rp, rp2, "emitted RouteProblem must round-trip serde");

        // Multi-pin nets became connections (SIG and GND each have 2 pins).
        let names: Vec<&str> = rp.connections.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"SIG") && names.contains(&"GND"), "{names:?}");

        // Every connection point must sit on a pad of its net: feed an EMPTY
        // solution to the connectivity oracle. With no copper, multi-pin nets are
        // reported Unconnected (their points are not yet joined) but there must be
        // NO CrossNetMerge — the points-on-pads geometry is sound. (A clean route
        // below proves the points are actually reachable.)
        let empty = crate::problem::RouteSolution {
            traces: vec![],
            vias: vec![],
        };
        let v = connectivity::check(&rp, &empty);
        assert!(
            v.iter().all(|x| matches!(x, connectivity::Violation::Unconnected { .. })),
            "no copper: only Unconnected expected, got {v:?}"
        );
    }

    // ── integration smoke: place → to_route_problem → route_auto → lint clean ─

    #[test]
    fn integration_two_part_board_routes_clean() {
        // A trivial 2-resistor board sharing two nets. Place it, hand it to the
        // production router, and assert the copper lints clean (no failed nets,
        // empty lint) — the placement→routing handoff end to end.
        let problem = PlaceProblem {
            bounds: board(30.0, 20.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.25,
            parts: vec![
                r0603("R1", Some("SIG"), Some("GND")),
                r0603("R2", Some("SIG"), Some("GND")),
            ],
        };
        let res = place(&problem, &PlacementHints::default());
        assert!(res.legal, "placement legal: {res:?}");
        let rp = to_route_problem(&problem, &res.placements);
        let routed = route_auto(&rp);
        assert!(
            routed.failed.is_empty(),
            "the placed board must route with zero failed nets: {:?}",
            routed.failed
        );
        let violations = lint(&rp, &routed.solution);
        assert!(
            violations.is_empty(),
            "the placed+routed board must lint clean: {violations:?}"
        );
    }

    // ── HPWL is reported and sane ────────────────────────────────────────────

    #[test]
    fn hpwl_is_reported_and_nonnegative() {
        let problem = PlaceProblem {
            bounds: board(30.0, 20.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![
                r0603("R1", Some("A"), Some("B")),
                r0603("R2", Some("B"), Some("C")),
            ],
        };
        let res = place(&problem, &PlacementHints::default());
        assert!(res.report.hpwl >= 0.0, "HPWL must be non-negative");
        // Net "B" is the only 2-pin net; its HPWL is the pad-center bbox half-perim,
        // strictly positive once the two parts are apart.
        assert!(res.report.hpwl > 0.0, "two connected parts give positive HPWL");
    }

    // ── never panics on an impossible board ─────────────────────────────────

    #[test]
    fn impossible_board_returns_not_legal_without_panic() {
        // A board far too small for its parts: 3 R_0603 courtyards (1.6mm wide)
        // cannot fit with margin on a 1x1 board. The engine must return
        // legal:false, never panic.
        let problem = PlaceProblem {
            bounds: board(1.0, 1.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![
                r0603("R1", Some("A"), Some("B")),
                r0603("R2", Some("B"), Some("C")),
                r0603("R3", Some("C"), Some("A")),
            ],
        };
        let res = place(&problem, &PlacementHints::default());
        assert!(!res.legal, "an impossible board must report legal:false");
        assert_eq!(res.placements.len(), 3, "still returns a placement per part");
    }

    // ── empty problem is trivially legal ────────────────────────────────────

    #[test]
    fn rotate_offset_matches_kicad_convention() {
        // Verified against kicad-cli: a SOIC-8 pad at local (-2.475, 1.905) under a
        // footprint rotated 270° lands at world offset (-1.905, -2.475). The two
        // 90/270 directions must not be swapped, or routing targets the wrong pad.
        let p = rotate_offset(&Point2 { x: -2.475, y: 1.905 }, 270);
        assert!((p.x - -1.905).abs() < 1e-9 && (p.y - -2.475).abs() < 1e-9, "{p:?}");
        // 90 is the inverse; 180 negates; 0 is identity.
        let q = rotate_offset(&Point2 { x: -2.475, y: 1.905 }, 90);
        assert!((q.x - 1.905).abs() < 1e-9 && (q.y - 2.475).abs() < 1e-9, "{q:?}");
        let r = rotate_offset(&Point2 { x: 1.0, y: 2.0 }, 180);
        assert!((r.x - -1.0).abs() < 1e-9 && (r.y - -2.0).abs() < 1e-9, "{r:?}");
    }

    #[test]
    fn empty_problem_is_legal() {
        let problem = PlaceProblem {
            bounds: board(10.0, 10.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            parts: vec![],
        };
        let res = place(&problem, &PlacementHints::default());
        assert!(res.legal);
        assert!(res.placements.is_empty());
        assert_eq!(res.report.hpwl, 0.0);
    }
}
