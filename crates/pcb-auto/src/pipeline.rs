//! One-call deterministic auto-layout: outline, mounting holes, connector edge seating,
//! connectivity-aware placement, a ground pour, Freerouting, and KiCad DRC as the oracle.
//!
//! A failed step is recorded in `notes`, not raised; only the final check is authoritative.
//! The pipeline is budgeted: every stage checks the wall clock before it starts, and the router
//! gets what is left, so a call keeps its `timeout_s` promise instead of finishing at any cost.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use kicad::KicadInstallation;
use kicad_footprint::{FootprintCatalog, FootprintId};

use crate::checks::{self, CheckReport};
use crate::freerouting::{self, RouteOptions};
use crate::geom::{inset_polygon, BBox};
use crate::model::{parse_footprint_module, Board};
use crate::place::{self, PlanOptions};
use crate::project;
use crate::rules;

/// How far inside the outline the ground pour sits.
const ZONE_INSET: f64 = 0.5;
/// Courtyard area / board area the outline is sized for. The human median is 0.6; the router
/// needs a little more room than a hand layout does, and the difference between 0.6 and 0.5 on a
/// Blue Pill is the difference between a board that routes and one that does not.
const LAYOUT_DENSITY: f64 = 0.6;
/// Routed a hair wider than the rule so router rounding stays legal.
const CLEARANCE_MARGIN_MM: f64 = 0.02;
/// Router effort. Measured on a placed Blue Pill: the auto-routing stage stops improving around
/// pass 15, and every pass past that is a no-op that still costs two seconds of wall clock.
const ROUTER_PASSES: u32 = 20;
/// Seconds held back from the router for the DRC run and the renders that follow it. A router
/// that overruns is killed and its whole session is lost, so the budget it is given must be one
/// it can actually finish inside.
const ROUTE_RESERVE_S: f64 = 26.0;
/// A repair only has to finish the handful of nets the whole-board route left; it gets a short
/// ladder so it fits in what is left of the budget instead of being killed mid-session.
const REPAIR_PASSES: u32 = 8;
/// A stitching pass changes the fill it just measured, so it is worth repeating — but a board
/// that still fragments after this many rounds has a placement problem, not a stitching one.
const STITCH_ROUNDS: usize = 4;

/// What outline the board should end up with.
#[derive(Debug, Clone)]
pub enum Outline {
    /// Size one from the parts at human density.
    Suggest,
    /// Leave the outline the board arrived with.
    Keep,
    Rect { w: f64, h: f64, radius: f64 },
}

#[derive(Debug, Clone)]
pub struct AutoOptions {
    pub outline: Outline,
    pub holes: u32,
    pub layers: u32,
    /// ref -> `left` | `right` | `top` | `bottom` | `any`.
    pub edge_for: BTreeMap<String, String>,
    pub gnd_zone: bool,
    /// Wall-clock budget for the whole call.
    pub timeout_s: u64,
}

impl Default for AutoOptions {
    fn default() -> Self {
        Self {
            outline: Outline::Suggest,
            holes: 0,
            layers: 2,
            edge_for: BTreeMap::new(),
            gnd_zone: true,
            timeout_s: 90,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AutoReport {
    pub ok: bool,
    pub completion: f64,
    pub unrouted: usize,
    pub drc_errors: usize,
    pub drc_warnings: usize,
    pub outline_mm: (f64, f64),
    pub notes: Vec<String>,
    pub seconds: f64,
}

struct Run<'a> {
    kicad: &'a KicadInstallation,
    path: PathBuf,
    opts: &'a AutoOptions,
    notes: Vec<String>,
    started: Instant,
}

impl Run<'_> {
    fn left_s(&self) -> f64 {
        self.opts.timeout_s as f64 - self.started.elapsed().as_secs_f64()
    }
    fn note(&mut self, msg: impl Into<String>) {
        self.notes.push(msg.into());
    }
}

/// Mounting-hole positions inside the outline bbox: corners first, extras along the long edges.
pub fn hole_positions(bb: &BBox, n: u32, inset: f64) -> Vec<(f64, f64)> {
    let (x0, y0, x1, y1) = (bb.x0 + inset, bb.y0 + inset, bb.x1 - inset, bb.y1 - inset);
    if x1 < x0 || y1 < y0 || n == 0 {
        return vec![];
    }
    let c = bb.center();
    let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
    match n {
        1 => return vec![(c.0, y0)],
        2 => {
            return if bb.w() >= bb.h() {
                vec![(x0, c.1), (x1, c.1)]
            } else {
                vec![(c.0, y0), (c.0, y1)]
            }
        }
        3 | 4 => return corners[..n as usize].to_vec(),
        _ => {}
    }
    let mut pts = corners.to_vec();
    let extra = n as usize - 4;
    let horizontal = bb.w() >= bb.h();
    let rows = extra.div_ceil(2) + 1;
    for i in 0..extra {
        let f = ((i / 2) + 1) as f64 / rows as f64;
        pts.push(if horizontal {
            (x0 + f * (x1 - x0), if i % 2 == 0 { y0 } else { y1 })
        } else {
            (if i % 2 == 0 { x0 } else { x1 }, y0 + f * (y1 - y0))
        });
    }
    pts.truncate(n as usize);
    pts
}

const HOLE_SIZES: &[(f64, &str)] = &[
    (2.2, "M2"),
    (2.7, "M2.5"),
    (3.2, "M3"),
    (4.3, "M4"),
    (5.3, "M5"),
];

fn hole_lib_id(catalog: &FootprintCatalog, diameter: f64) -> Option<FootprintId> {
    let (d, name) = HOLE_SIZES
        .iter()
        .min_by(|a, b| {
            (a.0 - diameter)
                .abs()
                .partial_cmp(&(b.0 - diameter).abs())
                .unwrap()
        })
        .copied()?;
    let want = format!("MountingHole:MountingHole_{d}mm_{name}");
    let id = FootprintId::parse(&want).ok()?;
    catalog.contains(&id).then_some(id)
}

fn is_mounting_hole(f: &crate::model::Footprint) -> bool {
    f.lib_id.to_lowercase().contains("mountinghole") || f.ref_.starts_with('H')
}

/// The board's ground net, under whatever name the schematic gave it.
fn ground_net(board: &Board) -> Option<crate::model::Net> {
    board
        .nets()
        .into_iter()
        .find(|n| n.id != 0 && rules::is_ground_name(&n.name))
}

/// How much of a board side a part eats when it is seated on one, courtyard plus a hair.
fn side_extent(f: &crate::model::Footprint) -> f64 {
    let b = f.courtyard_bbox();
    if b.valid() { b.w().max(b.h()) } else { 0.0 }
}

/// The outline to size a board at, given who is going on which edge.
///
/// The density rule alone answers "how much copper is there"; it cannot answer "does a 1x20
/// header fit on the edge you put it on". A part seated on the left or right edge eats board
/// HEIGHT, one on the top or bottom eats WIDTH, so each side's parts set a floor for the
/// dimension they run along — that floor is what makes a Blue Pill come out long and narrow
/// instead of square. Area is then given back on the dimension no connector constrains, so a
/// board grown to fit a header does not also double in area.
fn suggest_outline_for_edges(
    board: &Board,
    edge_for: &BTreeMap<String, String>,
    density: f64,
    margin: f64,
    aspect: f64,
) -> (f64, f64) {
    let fps = board.footprints();
    let seated: BTreeMap<&str, &str> = edge_for
        .iter()
        .map(|(r, e)| (r.as_str(), e.as_str()))
        .collect();
    let auto_seated = |f: &crate::model::Footprint| {
        place::is_connector(f) && !f.locked && !seated.contains_key(f.ref_.as_str())
    };
    let mut per_side: BTreeMap<&str, f64> = BTreeMap::new();
    let mut need_any = 0.0f64;
    let mut big_free = 0.0f64;
    let mut courtyard_area = 0.0f64;
    for f in &fps {
        let b = f.courtyard_bbox();
        if b.valid() {
            courtyard_area += b.w().max(0.0) * b.h().max(0.0);
        }
        match seated.get(f.ref_.as_str()) {
            Some(&edge) if ["left", "right", "top", "bottom"].contains(&edge) => {
                *per_side.entry(edge).or_default() += side_extent(f) + margin;
            }
            // A connector nobody assigned an edge to will be seated on whichever side has room.
            // It pins ONE dimension, not both: charging it to width and height alike is what
            // turns a board with two long headers into a square instead of a strip.
            _ if auto_seated(f) || seated.get(f.ref_.as_str()) == Some(&"any") => {
                need_any = need_any.max(side_extent(f) + 2.0 * margin);
            }
            _ => big_free = big_free.max(b.w().max(b.h())),
        }
    }
    let need_h = per_side
        .get("left")
        .copied()
        .unwrap_or(0.0)
        .max(per_side.get("right").copied().unwrap_or(0.0));
    let need_w = per_side
        .get("top")
        .copied()
        .unwrap_or(0.0)
        .max(per_side.get("bottom").copied().unwrap_or(0.0));
    let inner = courtyard_area / density.max(0.05);
    let h0 = (inner / aspect).sqrt();
    let w0 = inner / h0;
    let floor_w = need_w.max(big_free);
    let floor_h = need_h.max(big_free);
    let mut w = w0.max(floor_w);
    let mut h = h0.max(floor_h);
    // an unassigned connector needs a side long enough for it, on the long dimension
    if need_any > w.max(h) {
        if h >= w { h = need_any } else { w = need_any }
    }
    // give the area back on whichever dimension a connector did not pin
    if h > h0 && w > floor_w {
        w = floor_w.max(inner / h).min(w);
    } else if w > w0 && h > floor_h {
        h = floor_h.max(inner / w).min(h);
    }
    (
        ((w + 2.0 * margin) * 10.0).round() / 10.0,
        ((h + 2.0 * margin) * 10.0).round() / 10.0,
    )
}

fn step_outline(run: &mut Run, board: &mut Board) {
    let (w, h) = match &run.opts.outline {
        // an outline the board arrived with is fixed geometry; one it does not have has to be sized
        Outline::Keep if board.outline_polygon().is_some() => return,
        Outline::Keep => {
            run.note("outline='keep' but the board has no closed outline; sized one");
            suggest_outline_for_edges(board, &run.opts.edge_for, LAYOUT_DENSITY, 1.0, 1.5)
        }
        Outline::Rect { w, h, .. } => (*w, *h),
        Outline::Suggest => {
            suggest_outline_for_edges(board, &run.opts.edge_for, LAYOUT_DENSITY, 1.0, 1.5)
        }
    };
    let radius = match &run.opts.outline {
        Outline::Rect { radius, .. } => *radius,
        _ => 0.0,
    };
    let mut parts = BBox::empty();
    for f in board.footprints() {
        parts.add_bbox(&f.courtyard_bbox());
    }
    let (cx, cy) = if parts.valid() {
        parts.center()
    } else if let Some(old) = board.outline_bbox() {
        old.center()
    } else {
        (w / 2.0, h / 2.0)
    };
    let x0 = ((cx - w / 2.0) * 10.0).round() / 10.0;
    let y0 = ((cy - h / 2.0) * 10.0).round() / 10.0;
    board.set_outline_rect(x0, y0, w, h, radius);
}

fn step_holes(run: &mut Run, board: &mut Board) {
    if run.opts.holes == 0 {
        return;
    }
    let Some(bb) = board.outline_bbox() else { return };
    let catalog = match FootprintCatalog::from_root(run.kicad.footprint_dir()) {
        Ok(c) => c,
        Err(e) => {
            run.note(format!("mounting holes skipped: {e}"));
            return;
        }
    };
    let Some(id) = hole_lib_id(&catalog, 3.2) else {
        run.note("mounting holes skipped: no MountingHole footprint in the library");
        return;
    };
    let Ok(source) = catalog.source(&id) else {
        run.note("mounting holes skipped: MountingHole footprint unreadable");
        return;
    };
    let Ok(module) = parse_footprint_module(&source) else {
        run.note("mounting holes skipped: MountingHole footprint does not parse");
        return;
    };
    // A hole's courtyard is wider than its drill, so the inset has to clear the courtyard, not
    // the hole: too small an inset hangs it over the edge.
    let inset = 4.0f64;
    let pos = hole_positions(&bb, run.opts.holes, inset);
    if pos.is_empty() {
        run.note(format!(
            "outline {:.1}x{:.1} mm too small for a {inset} mm hole inset",
            bb.w(),
            bb.h()
        ));
        return;
    }
    let mut taken: BTreeSet<String> = board.footprints().into_iter().map(|f| f.ref_).collect();
    let mut n = 1;
    for (px, py) in pos {
        let ref_ = loop {
            let candidate = format!("H{n}");
            n += 1;
            if !taken.contains(&candidate) {
                break candidate;
            }
        };
        taken.insert(ref_.clone());
        board.add_footprint(&module, &ref_, "MountingHole", (px, py), 0.0, "front", &id.as_lib_id());
        board.set_locked(&ref_, true);
    }
}

fn step_placement(run: &mut Run, board: &mut Board, seed: u64) -> place::PlacementPlan {
    let mut opts = PlanOptions {
        edge_for: run.opts.edge_for.clone(),
        seat_connectors: true,
        seed,
        ..Default::default()
    };
    // Locked mounting holes are fixed geometry the placer routes around.
    opts.fixed = board
        .footprints()
        .into_iter()
        .filter(|f| f.locked || is_mounting_hole(f))
        .map(|f| f.ref_)
        .collect();
    let plan = place::plan_placement(board, &opts);
    place::apply(board, &plan);
    plan
}

/// Re-fit a suggested outline to the placed parts plus a margin when that is more than 10% smaller
/// by area. Skipped when the board carries mounting holes, whose seats the outline defines.
fn step_shrink(run: &mut Run, board: &mut Board) {
    if !matches!(run.opts.outline, Outline::Suggest) || run.opts.holes > 0 {
        return;
    }
    if board.footprints().iter().any(is_mounting_hole) {
        return;
    }
    let (Some(outline), true) = (board.outline_bbox(), true) else {
        return;
    };
    let mut parts = BBox::empty();
    for f in board.footprints() {
        parts.add_bbox(&f.courtyard_bbox());
    }
    if !parts.valid() {
        return;
    }
    let margin = 1.5;
    let (w, h) = (parts.w() + 2.0 * margin, parts.h() + 2.0 * margin);
    if w * h >= outline.w() * outline.h() * 0.9 {
        return;
    }
    let (cx, cy) = parts.center();
    board.set_outline_rect(
        ((cx - w / 2.0) * 10.0).round() / 10.0,
        ((cy - h / 2.0) * 10.0).round() / 10.0,
        (w * 100.0).round() / 100.0,
        (h * 100.0).round() / 100.0,
        0.0,
    );
    run.note(format!(
        "outline refitted to the placement: {:.0} -> {:.0} mm2",
        outline.w() * outline.h(),
        w * h
    ));
}

/// Pour ground on the back layer, before routing: the DSN writer holds a pin sitting in its own
/// net's fill out of the network, so the pour carries ground instead of the router tracing it.
fn step_gnd_zone(run: &mut Run, board: &mut Board) {
    if !run.opts.gnd_zone {
        return;
    }
    let Some(gnd) = ground_net(board) else {
        run.note("no GND-like net on this board; no pour");
        return;
    };
    let Some(poly) = board.outline_polygon() else {
        return;
    };
    let stack = board.copper_layers();
    // Both outer layers: on a two-layer board a back-only pour leaves every front ground pad to be
    // traced and via'd to it, which is most of the router's work and none of the human's.
    let want: Vec<String> = match (stack.first(), stack.last()) {
        (Some(a), Some(b)) if a != b => vec![a.clone(), b.clone()],
        (Some(a), _) => vec![a.clone()],
        _ => return,
    };
    let shape = inset_polygon(&poly, ZONE_INSET);
    let have = board.zones();
    for layer in want {
        if have
            .iter()
            .any(|z| z.keepout.is_none() && z.net_name == gnd.name && z.layers.contains(&layer))
        {
            continue;
        }
        // Solid pad connections, not thermal spokes: a spoke that cannot fit trips
        // `starved_thermal` and under-connects the pin.
        board.add_zone(&gnd.name, &layer, &shape, 0.0, true);
    }
}

/// Lay a board out end to end: outline, holes, placement, ground pour, routing, final DRC.
pub fn auto_layout(
    kicad: &KicadInstallation,
    pcb: &Path,
    opts: &AutoOptions,
) -> anyhow::Result<AutoReport> {
    let started = Instant::now();
    let mut run = Run {
        kicad,
        path: pcb.to_path_buf(),
        opts,
        notes: Vec::new(),
        started,
    };
    let mut board = Board::load(pcb)?;
    board.with_copper_layers(opts.layers.max(2) as usize);

    step_outline(&mut run, &mut board);
    step_holes(&mut run, &mut board);
    let plan = step_placement(&mut run, &mut board, 1);
    for u in &plan.unplaced {
        run.note(format!("unplaced {}: {}", u.0, u.1));
    }
    if plan.overlaps_after > 0 {
        run.note(format!(
            "{} courtyard overlap(s) left after placement",
            plan.overlaps_after
        ));
    }
    step_shrink(&mut run, &mut board);
    step_gnd_zone(&mut run, &mut board);
    board.save(Some(pcb))?;

    // The rules DRC will check the board against, written before anything is routed so the
    // router and the oracle agree on what legal means.
    let mut rules = rules::infer_rules(&board);
    if let Some(pad) = rules.pad_clearance {
        // A pad's own clearance override beats the net class in KiCad DRC.
        rules.clearance = rules.clearance.max(pad.min(0.5));
    }
    project::write_project_rules(pcb, &rules, &board)?;
    // The router rounds in its own units, so route a hair wider than the rule to stay legal.
    let mut route_rules = rules.clone();
    route_rules.clearance = ((route_rules.clearance + CLEARANCE_MARGIN_MM).min(0.5) * 1000.0).round() / 1000.0;
    let net_widths = rules::router_net_widths(&board, &rules);

    // ---- route -------------------------------------------------------------------
    let budget = (run.left_s() - ROUTE_RESERVE_S).max(10.0) as u64;
    let route = freerouting::route(
        &mut board,
        &RouteOptions {
            passes: ROUTER_PASSES,
            timeout_s: budget,
            rules: route_rules.clone(),
            net_widths: net_widths.clone(),
            ..Default::default()
        },
        kicad,
    );
    match route {
        Ok(r) => run.note(format!(
            "freerouting: {}/{} nets, {} tracks, {} vias in {:.0}s",
            r.routed_nets, r.nets_requested, r.tracks_added, r.vias_added, r.seconds
        )),
        Err(e) => run.note(format!("freerouting failed: {}", first_line(&e.to_string()))),
    }
    board.strip_zone_fills();
    board.save(Some(pcb))?;

    // ---- tie the pour back together --------------------------------------------
    // Tracks cut the plane into islands; every pair of them is a missing connection until a via
    // bridges them, and on a two-layer board that is most of what a clean route leaves open.
    stitch(&mut run, &mut board, pcb, rules.clearance)?;

    // ---- retry whatever is left, while there is budget for it ---------------------
    let mut report = checks::check(kicad, pcb)?;
    if report.unconnected > 0 && run.left_s() > 20.0 {
        let left: Vec<String> = report.unrouted_nets.clone();
        run.note(format!(
            "{} connection(s) left on {:?}; retrying those nets",
            report.unconnected,
            &left[..left.len().min(6)]
        ));
        let before = std::fs::read(pcb)?;
        let mut retry = Board::load(pcb)?;
        let budget = (run.left_s() - 6.0).max(5.0) as u64;
        // A wide rail that cannot fit is retried at plain signal width.
        let r = freerouting::route(
            &mut retry,
            &RouteOptions {
                passes: REPAIR_PASSES,
                timeout_s: budget,
                rules: route_rules,
                net_widths: BTreeMap::new(),
                only_nets: left,
                ..Default::default()
            },
            kicad,
        );
        match r {
            Ok(r) => {
                retry.strip_zone_fills();
                retry.save(Some(pcb))?;
                run.note(format!(
                    "reroute: {} tracks, {} vias in {:.0}s",
                    r.tracks_added, r.vias_added, r.seconds
                ));
                stitch(&mut run, &mut retry, pcb, rules.clearance)?;
                let after = checks::check(kicad, pcb)?;
                if after.unconnected <= report.unconnected {
                    report = after;
                } else {
                    std::fs::write(pcb, &before)?;
                    run.note("the reroute left more open than before; keeping the first result");
                }
            }
            Err(e) => {
                std::fs::write(pcb, &before)?;
                run.note(format!("reroute failed: {}", first_line(&e.to_string())));
            }
        }
    }

    Ok(finish(run, report))
}

/// Tie the ground pour back into one piece, repeating while each pass still finds islands: a
/// stitching via changes the fill, which can strand a fragment the previous pass could not see.
fn stitch(run: &mut Run, board: &mut Board, pcb: &Path, clearance: f64) -> anyhow::Result<()> {
    if !run.opts.gnd_zone {
        return Ok(());
    }
    let Some(gnd) = ground_net(board) else {
        return Ok(());
    };
    let mut total = 0usize;
    for _ in 0..STITCH_ROUNDS {
        if run.left_s() < 8.0 {
            break;
        }
        match crate::stitch::stitch_pours(run.kicad, board, pcb, &gnd.name, clearance) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                board.strip_zone_fills();
                board.save(Some(pcb))?;
            }
            Err(e) => {
                run.note(format!("pour stitching failed: {}", first_line(&e.to_string())));
                break;
            }
        }
    }
    if total > 0 {
        run.note(format!("{total} ground stitching via(s)"));
    }
    Ok(())
}

/// A note is a line for a human, not a router transcript.
fn first_line(msg: &str) -> String {
    msg.lines().next().unwrap_or("").chars().take(200).collect()
}

fn finish(run: Run, report: CheckReport) -> AutoReport {
    AutoReport {
        ok: report.ok,
        completion: report.completion,
        unrouted: report.unconnected,
        drc_errors: report.errors,
        drc_warnings: report.warnings,
        outline_mm: report.outline_mm,
        notes: run.notes,
        seconds: run.started.elapsed().as_secs_f64(),
    }
}

/// DRC + completion for a board, without changing it.
pub fn check(kicad: &KicadInstallation, pcb: &Path) -> anyhow::Result<CheckReport> {
    checks::check(kicad, pcb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holes_land_on_the_corners_first() {
        let bb = BBox::new(0.0, 0.0, 100.0, 60.0);
        assert_eq!(hole_positions(&bb, 4, 4.0).len(), 4);
        assert_eq!(hole_positions(&bb, 4, 4.0)[0], (4.0, 4.0));
        assert_eq!(hole_positions(&bb, 6, 4.0).len(), 6);
        assert!(hole_positions(&bb, 4, 40.0).is_empty());
    }
}
