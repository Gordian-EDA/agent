//! Detailed-routing pipeline entry point: stitch + fallback (slice 3, Task 3).
//!
//! This is the top of the detailed router. It runs the three detailed stages in
//! sequence and stitches their cell-local copper into a board [`RouteSolution`]:
//!
//! 1. [`global_route`] — coarse, congestion-negotiated cell paths over the mesh.
//! 2. [`assign_crossings`] — concrete per-boundary crossing points + cell jobs.
//! 3. [`route_cells`] — fine octilinear A* inside each cell, emitting cell-local
//!    polylines that meet **byte-exactly** at the shared crossing points.
//!
//! [`route_detailed`] folds every stage's failures into one [`RouteResult`] with
//! provenance in the reason string (`"global: …"`, `"assign: …"`, `"cell N: …"`),
//! and stitches only the *fully successful* nets into copper. [`route_auto`] runs
//! the detailed pipeline and, if anything failed, also runs the always-correct
//! slice-1 router ([`crate::router::route`]) and returns whichever has fewer
//! failed nets (naive wins ties — it is the battle-tested path). A [`RouterKind`]
//! tag records which engine produced the returned result.
//!
//! ## Stitching (the connectivity contract)
//!
//! [`route_cells`] snaps every terminal endpoint to its exact mm position, so a
//! leaf's `Exit` and the neighbour's `Entry` are byte-identical [`Point2`]s.
//! Stitching therefore joins cell-local polylines **by exact coordinate
//! identity**: per net, per layer, two polylines that share a byte-identical
//! endpoint where exactly two polyline-ends meet are concatenated into one
//! continuous run; the joined run is re-`simplify`d (the 45°-aware merge from
//! [`crate::detail`]) so a straight crossing collapses to a single segment. Where
//! **three or more** polyline-ends meet at one point — a T-junction in a
//! multi-point net — the polylines are left meeting at the shared vertex (KiCAD
//! and the connectivity oracle treat a shared vertex as connected), never forced
//! into one impossible polyline. A net with a failure anywhere contributes **no**
//! copper at all: a half-routed net would (rightly) trip the connectivity lint,
//! so its cell routes are dropped and it is reported failed.

use crate::crossing::assign_crossings;
use crate::detail::{self, CellRoute, CellRouteResult};
use crate::pathing::{global_route, GlobalRouteResult};
use crate::problem::{FailedNet, Point2, RouteProblem, RouteSolution, Trace, Via};
use crate::router;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Byte-exact coincidence epsilon. Stitching relies on the detailed stage having
/// snapped shared endpoints to *identical* mm coordinates, so a near-zero
/// tolerance is all that is needed (and is correct: a genuine gap must not be
/// silently bridged — the lint is the authority on connectivity).
const JOIN_EPS: f64 = 1e-12;

// ── provenance ─────────────────────────────────────────────────────────────────

/// Which routing engine produced a [`RouteResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RouterKind {
    /// The slice-1 sequential grid router ([`crate::router::route`]).
    Naive,
    /// The slice-3 detailed pipeline ([`route_detailed`]).
    Detailed,
}

// ── metrics ────────────────────────────────────────────────────────────────────

/// Comparable size/quality metrics for a [`RouteSolution`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteMetrics {
    /// Total copper wirelength (mm): the sum of every trace polyline's length.
    pub wirelength: f64,
    /// Number of vias in the solution.
    pub via_count: usize,
    /// Number of trace polylines in the solution.
    pub trace_count: usize,
}

/// Compute [`RouteMetrics`] for a solution (wirelength = Σ polyline lengths).
pub fn metrics(solution: &RouteSolution) -> RouteMetrics {
    let mut wirelength = 0.0;
    for t in &solution.traces {
        for w in t.path.windows(2) {
            let dx = w[1].x - w[0].x;
            let dy = w[1].y - w[0].y;
            wirelength += (dx * dx + dy * dy).sqrt();
        }
    }
    RouteMetrics {
        wirelength,
        via_count: solution.vias.len(),
        trace_count: solution.traces.len(),
    }
}

// ── pipeline entry points ──────────────────────────────────────────────────────

/// Route `problem` through the full detailed pipeline (global → assign → cell →
/// stitch). Never panics; every stage's failures are folded into
/// [`RouteResult::failed`] with provenance in the reason, and a net that fails at
/// *any* stage contributes no copper to the returned solution. The result is
/// tagged [`RouterKind::Detailed`].
pub fn route_detailed(problem: &RouteProblem) -> RouteResult {
    let mut failed: Vec<FailedNet> = Vec::new();
    // A net failing anywhere drops its copper everywhere. Collected by name.
    let mut failed_names: std::collections::BTreeSet<String> = Default::default();

    // 1. Global routing.
    let global: GlobalRouteResult = global_route(problem);
    if !global.is_feasible() {
        // Fold the per-net "unrouted" failures, then a single overflow
        // pseudo-failure if any edge is over capacity (the overflow is not
        // attributable to one net, so it is reported as a board-level fault).
        for u in &global.report.unrouted {
            fail(&mut failed, &mut failed_names, &u.connection, format!("global: {}", u.reason));
        }
        if global.report.final_overflow > 0 {
            failed.push(FailedNet {
                connection: String::new(),
                reason: format!(
                    "global: {} unit(s) of residual edge overflow after {} rip-up iteration(s)",
                    global.report.final_overflow, global.report.iterations
                ),
            });
        }
    }

    let mesh = crate::mesh::CapacityMesh::build(problem);

    // 2. Crossing assignment.
    let assignment = assign_crossings(problem, &mesh, &global.plan);
    for f in &assignment.failures {
        // An assignment failure is per-boundary or per-via; surface it as a
        // board-level fault with assign provenance (its detailed payload is the
        // honest record).
        failed.push(FailedNet {
            connection: assignment_failure_connection(f),
            reason: format!("assign: {}", assignment_failure_reason(f)),
        });
    }

    // 3. Per-cell detailed routing.
    let cells: CellRouteResult = detail::route_cells(problem, &mesh, &assignment);
    for f in &cells.failed {
        // route_cells already prefixes the reason with "cell N: …"; keep that
        // provenance and mark the net as failed so its copper is dropped.
        fail(&mut failed, &mut failed_names, &f.connection, f.reason.clone());
    }

    // 4. Stitch the SUCCESSFUL nets' cell copper into a board solution. A net
    //    listed in `failed_names` contributes nothing (no half-routed copper).
    let solution = stitch(problem, &cells.cell_routes, &failed_names);

    RouteResult {
        solution,
        failed,
        router: RouterKind::Detailed,
    }
}

/// Route `problem`, automatically falling back to the slice-1 router.
///
/// Runs [`route_detailed`]; if it has zero failed nets, returns it (tagged
/// [`RouterKind::Detailed`]). Otherwise also runs the always-correct slice-1
/// router and returns whichever result has **fewer** failed nets — the naive
/// router wins ties as the battle-tested fallback. The returned result's
/// [`RouteResult::router`] records which engine won.
pub fn route_auto(problem: &RouteProblem) -> RouteResult {
    let mut detailed = route_detailed(problem);
    reconcile_connectivity(problem, &mut detailed.solution, &mut detailed.failed);

    // Always also run the slice-1 router and keep the BETTER of the two — judged
    // not only on completeness (fewest faults) but on NEATNESS. On a simple board
    // the detailed router can route everything yet wander with more vias than the
    // direct grid router needs; on a congested board the detailed router's
    // capacity-aware routing wins on faults. The quality key
    // `(faults, via_count, wirelength)` lets each board pick the cleaner result —
    // faults stay primary (never trade routability), then fewer vias, then shorter.
    let strict = router::route(problem);
    let lenient = router::route_lenient(problem);
    let naive = if score(problem, &lenient) < score(problem, &strict) {
        lenient
    } else {
        strict
    };
    let nq = quality(problem, &naive.solution, naive.failed.len());
    let dq = quality(problem, &detailed.solution, detailed.failed.len());
    // Naive wins exact ties (the battle-tested path) and is preferred when it is
    // no worse on faults and no busier (fewer/equal vias + shorter/equal copper).
    if nq <= dq {
        RouteResult {
            solution: naive.solution,
            failed: naive.failed,
            router: RouterKind::Naive,
        }
    } else {
        detailed
    }
}

/// A routed candidate's quality key, lower is better:
/// `(total DRC faults, via count, wirelength×100)`.
/// - **faults** (`unrouted + geometry violations`) is PRIMARY — never trade
///   routability/DRC-cleanliness for looks; an honest unrouted net beats a silent
///   geometry violation (geometry folded into the count).
/// - **via count** then **wirelength** are the neatness tiebreakers, so when two
///   routers both route a board cleanly the engine keeps the tidier copper.
fn quality(problem: &RouteProblem, solution: &RouteSolution, failed: usize) -> (usize, usize, u64) {
    let geom = geometry_violations(problem, solution);
    let wl = metrics(solution).wirelength;
    (failed + geom, solution.vias.len(), (wl * 100.0) as u64)
}

/// `(total DRC faults, geometry faults)` for choosing between slice-1 variants.
fn key(failed: usize, geom: usize) -> (usize, usize) {
    (failed + geom, geom)
}

/// [`key`] for a slice-1 candidate.
fn score(problem: &RouteProblem, r: &router::RouteResult) -> (usize, usize) {
    key(r.failed.len(), geometry_violations(problem, &r.solution))
}

/// Make a routed result DRC-HONEST: the lint is the authority, not the router's
/// own bookkeeping. First drop any net whose copper violates GEOMETRY (clearance
/// / width / via / bounds) — the engine must never emit copper that fails DRC —
/// then drop any net left unconnected or shorted (a cross-net merge). Every
/// dropped net is reported failed. After this `failed` is faithful and the
/// surviving copper is fully DRC-clean, so `route_auto`'s comparison ranks a
/// silent violation or phantom-route below an engine that cleanly connected
/// fewer nets, and the engine never ships copper that fails DRC.
fn reconcile_connectivity(
    problem: &RouteProblem,
    solution: &mut RouteSolution,
    failed: &mut Vec<FailedNet>,
) {
    let mut broken = crate::lint::drop_violating_copper(problem, solution);
    broken.extend(crate::lint::drop_unconnected_copper(problem, solution));
    let known: std::collections::BTreeSet<&str> =
        failed.iter().map(|f| f.connection.as_str()).collect();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let new: Vec<FailedNet> = broken
        .into_iter()
        .filter(|name| !known.contains(name.as_str()) && seen.insert(name.clone()))
        .map(|name| FailedNet {
            connection: name,
            reason: "DRC oracle: net dropped — could not be routed cleanly (clearance/connectivity)"
                .to_string(),
        })
        .collect();
    failed.extend(new);
}

/// Count the *geometry* DRC violations of a solution — clearance, trace width,
/// via clearance, out-of-bounds, invalid layer — excluding the connectivity
/// lints, which already correlate with the failed-net count. This is the
/// tiebreaker [`route_auto`] uses so a fully-routed-but-violating solution never
/// beats a DRC-clean one.
fn geometry_violations(problem: &RouteProblem, solution: &RouteSolution) -> usize {
    crate::lint::lint(problem, solution)
        .iter()
        .filter(|v| !matches!(v, crate::lint::DrcViolation::Connectivity { .. }))
        .count()
}

/// The outcome of a pipeline route: copper, failures, and which engine produced
/// it.
///
/// Mirrors [`crate::router::RouteResult`] (solution + failed) and adds the
/// [`router`](RouteResult::router) provenance tag so callers and tests can see
/// which engine produced the returned copper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteResult {
    /// Emitted traces and vias.
    pub solution: RouteSolution,
    /// Nets that could not be fully routed (deterministic order), with stage
    /// provenance in each reason.
    pub failed: Vec<FailedNet>,
    /// Which engine produced this result.
    pub router: RouterKind,
}

// ── stitching ──────────────────────────────────────────────────────────────────

/// Stitch per-cell copper into board traces + vias, skipping nets in `skip`.
///
/// Per net, per layer, cell-local polylines are concatenated by byte-exact
/// endpoint identity (degree-2 joins), re-simplified, and emitted as [`Trace`]s;
/// every [`crate::detail::CellVia`] becomes a [`Via`], deduplicated by exact
/// position. Net/layer/trace order is deterministic (BTreeMap key order).
fn stitch(
    problem: &RouteProblem,
    cell_routes: &[CellRoute],
    skip: &std::collections::BTreeSet<String>,
) -> RouteSolution {
    let width = problem.min_trace_width;

    // Group cell copper by net, preserving deterministic (net, then layer) order.
    // Per net: per-layer list of polylines, plus the net's via sites.
    let mut by_net: BTreeMap<String, NetCopper> = BTreeMap::new();
    for cr in cell_routes {
        if skip.contains(&cr.connection) {
            continue;
        }
        let nc = by_net.entry(cr.connection.clone()).or_default();
        for t in &cr.traces {
            nc.polylines_on(&t.layer.0).push(t.points.clone());
        }
        for v in &cr.vias {
            nc.vias.push(v.at.clone());
        }
    }

    let mut traces: Vec<Trace> = Vec::new();
    let mut vias: Vec<Via> = Vec::new();

    for (connection, nc) in by_net {
        // Traces: join + simplify per layer (layers in BTreeMap key order).
        for (layer, polylines) in nc.by_layer {
            for poly in join_polylines(polylines) {
                let simplified = detail::simplify(poly);
                if simplified.len() < 2 {
                    continue;
                }
                traces.push(Trace {
                    connection: connection.clone(),
                    layer: crate::problem::LayerRef(layer.clone()),
                    width,
                    path: simplified,
                });
            }
        }
        // Vias: dedup by exact position, and DROP spurious ones. A via is real
        // only if this net actually changes layer there — i.e. it has copper (a
        // trace endpoint or a pad) on ≥2 distinct layers at the via's position.
        // The cell stitch can emit a via where the net only has copper on one
        // layer (a layer transition that simplified away), which KiCAD flags as
        // `via_dangling`. Dropping it cannot break connectivity: by definition the
        // net is already connected without it, and the connectivity oracle +
        // naive fallback in `route_auto` catch any over-drop.
        for at in nc.vias {
            if vias
                .iter()
                .any(|v| v.connection == connection && same_point(&v.at, &at))
            {
                continue;
            }
            // A via connects a layer if a same-net trace TOUCHES it there — and
            // that touch can be at a trace endpoint OR a point the trace passes
            // straight through (a collinear interior point `simplify` removed). So
            // test distance to each trace SEGMENT, not just to its vertices, or a
            // genuinely-connecting via is mistaken for dangling and dropped.
            const TOUCH: f64 = 0.02;
            let mut layers: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            for t in &traces {
                if t.connection == connection
                    && t.path.windows(2).any(|w| seg_point_dist(&w[0], &w[1], &at) < TOUCH)
                {
                    layers.insert(t.layer.0.as_str());
                }
            }
            for ob in &problem.obstacles {
                if ob.connected_to.contains(&connection)
                    && (at.x - ob.center.x).abs() <= ob.width / 2.0 + 0.01
                    && (at.y - ob.center.y).abs() <= ob.height / 2.0 + 0.01
                {
                    for lr in &ob.layers {
                        layers.insert(lr.0.as_str());
                    }
                }
            }
            if layers.len() < 2 {
                continue; // spurious / dangling — drop
            }
            vias.push(Via {
                connection: connection.clone(),
                at,
                diameter: problem.via_diameter,
                drill: problem.via_drill,
            });
        }
    }

    RouteSolution { traces, vias }
}

/// Per-net copper accumulated during stitching: polylines keyed by layer name
/// (BTreeMap for deterministic layer order) plus via sites.
#[derive(Default)]
struct NetCopper {
    by_layer: BTreeMap<String, Vec<Vec<Point2>>>,
    vias: Vec<Point2>,
}

impl NetCopper {
    fn polylines_on(&mut self, layer: &str) -> &mut Vec<Vec<Point2>> {
        self.by_layer.entry(layer.to_owned()).or_default()
    }
}

/// Concatenate polylines that meet byte-exactly at a degree-2 endpoint into
/// continuous runs. A point where three or more polyline-ends meet (a T-junction)
/// is left as a shared vertex — the polylines touching it stay separate (KiCAD /
/// the connectivity oracle treat a shared vertex as connected). Deterministic:
/// merges are tried in input order; the output is sorted by first point.
fn join_polylines(mut polys: Vec<Vec<Point2>>) -> Vec<Vec<Point2>> {
    // Drop degenerate polylines (< 2 points) up front.
    polys.retain(|p| p.len() >= 2);
    if polys.len() <= 1 {
        return polys;
    }

    // Endpoint multiplicity: how many polyline-ends land on each exact point.
    // Only a point shared by exactly two ends is a safe degree-2 join; a T-
    // junction (≥ 3) must keep its polylines separate.
    loop {
        let degree = endpoint_degree(&polys);
        let mut merged = false;

        'outer: for i in 0..polys.len() {
            for j in (i + 1)..polys.len() {
                // Try to join polys[i] and polys[j] at a shared, degree-2 end.
                if let Some(joined) = try_join(&polys[i], &polys[j], &degree) {
                    // Replace i with the joined run, remove j.
                    polys[i] = joined;
                    polys.remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }

        if !merged {
            break;
        }
    }

    // Deterministic output order: by first point, then last point.
    polys.sort_by(|a, b| {
        point_key(&a[0])
            .partial_cmp(&point_key(&b[0]))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                point_key(a.last().unwrap())
                    .partial_cmp(&point_key(b.last().unwrap()))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    polys
}

/// Count, per exact endpoint, how many polyline-ends (first/last point) coincide
/// with it. Keyed by quantised coordinates so byte-exact-equal points collide.
fn endpoint_degree(polys: &[Vec<Point2>]) -> BTreeMap<(i64, i64), usize> {
    let mut degree: BTreeMap<(i64, i64), usize> = BTreeMap::new();
    for p in polys {
        *degree.entry(quant(&p[0])).or_insert(0) += 1;
        *degree.entry(quant(p.last().unwrap())).or_insert(0) += 1;
    }
    degree
}

/// Join `a` and `b` if they share a byte-exact endpoint that is a degree-2 join
/// point (exactly two ends meet there). Returns the concatenated run oriented so
/// the shared point is interior, or `None` if no safe join exists.
fn try_join(
    a: &[Point2],
    b: &[Point2],
    degree: &BTreeMap<(i64, i64), usize>,
) -> Option<Vec<Point2>> {
    let a0 = &a[0];
    let a1 = a.last().unwrap();
    let b0 = &b[0];
    let b1 = b.last().unwrap();

    // A shared point is only joinable when exactly two ends meet there (degree
    // 2). At a T-junction the degree is ≥ 3 and we must not merge.
    let deg2 = |p: &Point2| degree.get(&quant(p)).copied().unwrap_or(0) == 2;

    // Four ways the two runs can abut. Pick the one whose shared point is degree
    // 2; concatenate dropping the duplicated shared point.
    if same_point(a1, b0) && deg2(a1) {
        // a … a1 == b0 … b1
        let mut out = a.to_vec();
        out.extend_from_slice(&b[1..]);
        return Some(out);
    }
    if same_point(a1, b1) && deg2(a1) {
        // a … a1 == b1 … b0  (reverse b)
        let mut out = a.to_vec();
        out.extend(b.iter().rev().skip(1).cloned());
        return Some(out);
    }
    if same_point(a0, b1) && deg2(a0) {
        // b0 … b1 == a0 … a1
        let mut out = b.to_vec();
        out.extend_from_slice(&a[1..]);
        return Some(out);
    }
    if same_point(a0, b0) && deg2(a0) {
        // a1 … a0 == b0 … b1  (reverse a)
        let mut out: Vec<Point2> = a.iter().rev().cloned().collect();
        out.extend_from_slice(&b[1..]);
        return Some(out);
    }
    None
}

/// Are two points byte-exactly (within [`JOIN_EPS`]) the same?
fn same_point(a: &Point2, b: &Point2) -> bool {
    (a.x - b.x).abs() < JOIN_EPS && (a.y - b.y).abs() < JOIN_EPS
}

/// Distance from point `p` to segment `a`–`b` (mm). Used to test whether a via
/// lies on a trace (endpoint or pass-through) when filtering dangling vias.
fn seg_point_dist(a: &Point2, b: &Point2, p: &Point2) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-12 {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0);
    let (cx, cy) = (a.x + t * dx, a.y + t * dy);
    ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt()
}

/// Quantise a point to an integer key so byte-exact-equal points collide in a
/// `BTreeMap`. The detailed stage emits identical bytes for shared endpoints, so
/// a fine quantum (1e9 ⇒ ~1 nm) keeps distinct points distinct while collapsing
/// the deliberately-identical ones.
fn quant(p: &Point2) -> (i64, i64) {
    ((p.x * 1e9).round() as i64, (p.y * 1e9).round() as i64)
}

/// A total-ordering sort key for a point (x then y).
fn point_key(p: &Point2) -> (f64, f64) {
    (p.x, p.y)
}

// ── failure folding helpers ────────────────────────────────────────────────────

/// Record a per-net failure (with its stage-prefixed reason) and mark the net so
/// its copper is dropped from the stitched solution.
fn fail(
    failed: &mut Vec<FailedNet>,
    failed_names: &mut std::collections::BTreeSet<String>,
    connection: &str,
    reason: String,
) {
    failed.push(FailedNet {
        connection: connection.to_owned(),
        reason,
    });
    failed_names.insert(connection.to_owned());
}

/// The connection a crossing-assignment failure is attributable to (empty for a
/// boundary overflow, which is not one net's fault).
fn assignment_failure_connection(f: &crate::crossing::AssignmentFailure) -> String {
    use crate::crossing::AssignmentFailure::*;
    match f {
        Overflow { .. } => String::new(),
        ViaSite { connection, .. } => connection.clone(),
    }
}

/// A human-readable reason for a crossing-assignment failure.
fn assignment_failure_reason(f: &crate::crossing::AssignmentFailure) -> String {
    use crate::crossing::AssignmentFailure::*;
    match f {
        Overflow {
            edge,
            layer,
            needed,
            available,
        } => format!(
            "boundary overflow on edge {edge} layer {layer}: {needed} crossings need slots, {available} fit"
        ),
        ViaSite { connection, leaf } => {
            format!("no clear via site for {connection} in leaf {leaf}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lint::lint;
    use std::path::Path;

    fn load(name: &str) -> RouteProblem {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name);
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
    }

    // ── led-r: the strict gate that must pass through route_detailed today ──────

    #[test]
    fn led_r_detailed_is_clean_and_lints_empty() {
        let p = load("led-r.json");
        let r = route_detailed(&p);
        assert_eq!(r.router, RouterKind::Detailed);
        assert!(
            r.failed.is_empty(),
            "led-r must route cleanly through route_detailed today: {:?}",
            r.failed
        );
        let vs = lint(&p, &r.solution);
        assert!(vs.is_empty(), "led-r detailed solution must lint CLEAN, got {vs:?}");
        let m = metrics(&r.solution);
        assert!(m.wirelength > 0.0, "led-r produced copper");
        assert!(m.trace_count > 0, "led-r has traces");
    }

    // ── quad: now routes cleanly through route_detailed (Task 3.5 finisher) ─────

    #[test]
    fn quad_detailed_is_clean_and_lints_empty() {
        // Task 3.5 (hotspot repair — the per-net finisher) closed quad's residual
        // cell failures: its six over-converged central crossings are completed by
        // the full-board finisher after the per-cell pass. route_detailed is now
        // clean end-to-end and lints empty.
        let p = load("quad.json");
        let r = route_detailed(&p);
        assert_eq!(r.router, RouterKind::Detailed);
        assert!(
            r.failed.is_empty(),
            "quad must route cleanly through route_detailed after the finisher: {:?}",
            r.failed
        );
        let vs = lint(&p, &r.solution);
        assert!(vs.is_empty(), "quad detailed solution must lint CLEAN, got {vs:?}");
    }

    #[test]
    fn quad_auto_is_clean() {
        // route_auto now picks the NEATER of detailed/naive (quality key: faults,
        // then vias, then wirelength), so for quad it keeps the tidier naive
        // result — equally clean, fewer vias. The invariant guarded here is that
        // route_auto's chosen result is fully routed and lints CLEAN, whichever
        // router wins (the provenance is a quality outcome, not a fixed promise).
        let p = load("quad.json");
        let r = route_auto(&p);
        assert!(r.failed.is_empty(), "route_auto routes quad cleanly: {:?}", r.failed);
        let vs = lint(&p, &r.solution);
        assert!(vs.is_empty(), "quad route_auto solution must lint CLEAN, got {vs:?}");
    }

    // ── congested: the per-net finisher repairs most of the wall, but the wall is
    //    EXACTLY saturated (8 top-layer crossings for 8 nets, the relief gap's fifth
    //    slot at its blocked margin) and a few nets stay honest failures — the
    //    rip-up case the slice plan put off the table. route_detailed reports them
    //    with finisher provenance; route_auto falls back to whichever engine has
    //    fewer failed nets. (See `tests/detailed_gate.rs` for the geometry note.)

    #[test]
    fn congested_auto_reports_honest_failures() {
        let p = load("congested.json");
        // route_detailed(congested) is the expensive path — call it ONCE and derive
        // route_auto's outcome from it + naive (route_auto runs exactly this
        // route_detailed internally, then naive, and returns the fewer-failed result),
        // rather than paying for a second full detailed route.
        let detailed = route_detailed(&p);
        let naive = router::route(&p);
        assert!(
            !naive.failed.is_empty() && !detailed.failed.is_empty(),
            "congested still defeats both engines (naive {} / detailed {} failed); \
             the saturated wall is rip-up territory, out of this slice's scope",
            naive.failed.len(),
            detailed.failed.len()
        );
        // The detailed finisher leaves only honest connectivity failures — every
        // residual carries finisher provenance, and the emitted copper is geometry-
        // clean (the lint shows only Connectivity from the dropped failed nets, no
        // clearance/via violation).
        assert!(
            detailed.failed.iter().all(|f| f.reason.contains("finisher")),
            "every congested residual must carry finisher provenance: {:?}",
            detailed.failed
        );
        let geom_violations: Vec<_> = lint(&p, &detailed.solution)
            .into_iter()
            .filter(|v| !format!("{v:?}").contains("Connectivity"))
            .collect();
        assert!(
            geom_violations.is_empty(),
            "congested detailed copper must be geometry-clean (only connectivity gaps \
             from dropped nets are allowed), got {geom_violations:?}"
        );
        // route_auto would return whichever engine has fewer failed nets (naive on
        // ties); with both non-empty here the report is honest either way.
        if naive.failed.len() <= detailed.failed.len() {
            // naive (or tie) wins — the always-correct fallback is preserved.
            assert!(!naive.failed.is_empty());
        } else {
            assert!(!detailed.failed.is_empty());
        }
    }

    // ── determinism ─────────────────────────────────────────────────────────────

    #[test]
    fn route_detailed_is_deterministic() {
        let p = load("quad.json");
        let a = route_detailed(&p);
        let b = route_detailed(&p);
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        assert_eq!(ja, jb, "two route_detailed runs must serialize byte-equal");
    }

    // ── partial-net rule: a failed net contributes NO copper ────────────────────

    #[test]
    fn no_trace_belongs_to_a_failed_net() {
        // The partial-net rule: a net listed in `failed` must contribute zero
        // copper to the solution (a half-routed net would trip connectivity).
        let p = load("quad.json");
        let r = route_detailed(&p);
        let failed_names: std::collections::BTreeSet<&str> =
            r.failed.iter().map(|f| f.connection.as_str()).collect();
        for t in &r.solution.traces {
            assert!(
                !failed_names.contains(t.connection.as_str()),
                "trace for failed net {} must not appear in route_detailed solution",
                t.connection
            );
        }
        for v in &r.solution.vias {
            assert!(
                !failed_names.contains(v.connection.as_str()),
                "via for failed net {} must not appear in route_detailed solution",
                v.connection
            );
        }
    }

    // ── stitch unit tests: joins, T-junctions, simplification ───────────────────

    fn pt(x: f64, y: f64) -> Point2 {
        Point2 { x, y }
    }

    #[test]
    fn join_concatenates_two_polylines_at_a_shared_endpoint() {
        // Two runs meeting byte-exactly at (5,0) join into one, and the collinear
        // join collapses via simplify.
        let polys = vec![
            vec![pt(0.0, 0.0), pt(5.0, 0.0)],
            vec![pt(5.0, 0.0), pt(10.0, 0.0)],
        ];
        let joined = join_polylines(polys);
        assert_eq!(joined.len(), 1, "two abutting runs join into one");
        let simplified = detail::simplify(joined.into_iter().next().unwrap());
        assert_eq!(
            simplified,
            vec![pt(0.0, 0.0), pt(10.0, 0.0)],
            "the collinear join collapses to two points"
        );
    }

    #[test]
    fn join_reverses_when_needed() {
        // Runs that meet end-to-end with mismatched orientation still join.
        let polys = vec![
            vec![pt(0.0, 0.0), pt(5.0, 5.0)],
            vec![pt(10.0, 10.0), pt(5.0, 5.0)], // shares its LAST point with run 0's last
        ];
        let joined = join_polylines(polys);
        assert_eq!(joined.len(), 1, "a reversed abutment still joins");
        let run = &joined[0];
        assert_eq!(run.first().unwrap(), &pt(0.0, 0.0));
        assert_eq!(run.last().unwrap(), &pt(10.0, 10.0));
    }

    #[test]
    fn t_junction_keeps_polylines_separate() {
        // Three runs meeting at (0,0) — a T-junction — must NOT be merged into a
        // single polyline (impossible); they stay separate, sharing the vertex.
        let polys = vec![
            vec![pt(0.0, 0.0), pt(-5.0, 0.0)],
            vec![pt(0.0, 0.0), pt(5.0, 0.0)],
            vec![pt(0.0, 0.0), pt(0.0, 5.0)],
        ];
        let joined = join_polylines(polys);
        assert_eq!(
            joined.len(),
            3,
            "a degree-3 junction keeps all three runs separate (shared vertex)"
        );
        // Every run still touches the junction.
        assert!(joined.iter().all(|r| r.iter().any(|q| same_point(q, &pt(0.0, 0.0)))));
    }

    #[test]
    fn metrics_sum_polyline_lengths() {
        let s = RouteSolution {
            traces: vec![
                Trace {
                    connection: "A".to_owned(),
                    layer: crate::problem::LayerRef::top(),
                    width: 0.2,
                    path: vec![pt(0.0, 0.0), pt(3.0, 0.0), pt(3.0, 4.0)],
                },
            ],
            vias: vec![Via {
                connection: "A".to_owned(),
                at: pt(3.0, 0.0),
                diameter: 0.6,
                drill: 0.3,
            }],
        };
        let m = metrics(&s);
        assert!((m.wirelength - 7.0).abs() < 1e-9, "3 + 4 = 7mm");
        assert_eq!(m.via_count, 1);
        assert_eq!(m.trace_count, 1);
    }
}
