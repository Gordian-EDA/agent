//! Seat connectors flush with a board edge, mating face outward.
//!
//! [`seat_on_edges`] puts each ref on the edge `edge_for` names, on `"any"` side with room, or on
//! the side nearest it, clear of the other parts; the mating direction is the courtyard centre
//! minus the pad centroid (as-is under 0.3 mm).

use std::collections::{BTreeMap, BTreeSet};

use super::cluster::{pad_hops, partner_pads};
use super::*;
use crate::geom::{BBox, Point, box_in_polygon, rotate};
use crate::model::{Board, Footprint};

/// Floor for how far inside the outline a seat sits; the seat itself sits [`EDGE_SEAT_SLACK`]
/// inside the board's own edge clearance (see [`seat_inset`]).
pub const EDGE_INSET: f64 = 0.25;
pub const EDGES: [&str; 4] = ["left", "right", "top", "bottom"];
/// `edge_for` value meaning "any side with room for it".
pub const EDGE_ANY: &str = "any";
/// mm of partner distance charged per mm pushed in off the edge, per unit of net weight in play.
pub const EDGE_PUSH_COST: f64 = 0.35;
/// How far an obstacle may push a seated connector in off its edge.
pub const EDGE_PUSH_LIMIT: f64 = 10.0;

const CONNECTOR_LIB: [&str; 6] = ["connector", "usb", "terminal", "header", "jack", "socket"];

/// How far inside the outline a seat sits: the board's own copper-to-edge clearance plus a hair,
/// never below [`EDGE_INSET`]. Anything less is a `copper_edge_clearance` error.
pub fn seat_inset(board: &Board) -> f64 {
    EDGE_INSET.max(board_edge_clearance(board) + EDGE_SEAT_SLACK)
}

/// Gap between a courtyard and each side of the outline bbox (negative = sticking out).
pub fn edge_distances(box_: &BBox, outline: &BBox) -> [(&'static str, f64); 4] {
    [
        ("left", box_.x0 - outline.x0),
        ("right", outline.x1 - box_.x1),
        ("top", box_.y0 - outline.y0),
        ("bottom", outline.y1 - box_.y1),
    ]
}

fn edge_distance(box_: &BBox, outline: &BBox, edge: &str) -> f64 {
    edge_distances(box_, outline)
        .iter()
        .find(|(e, _)| *e == edge)
        .map(|(_, d)| *d)
        .unwrap_or(0.0)
}

pub fn is_connector(fp: &Footprint) -> bool {
    let lib = fp.lib_id.to_ascii_lowercase();
    fp.ref_.starts_with(['J', 'j']) || CONNECTOR_LIB.iter().any(|k| lib.contains(k))
}

/// The outline a seat must stay inside, or `None` when it is a plain rectangle the bbox covers.
fn seat_polygon(board: &Board) -> Option<Vec<Point>> {
    board.outline_polygon().filter(|p| !is_rect(p))
}

fn outward(edge: &str) -> Point {
    match edge {
        "left" => (-1.0, 0.0),
        "right" => (1.0, 0.0),
        "top" => (0.0, -1.0),
        _ => (0.0, 1.0),
    }
}

/// The long dimension of a part: how much of a board side it eats when seated on one.
fn side_extent(fp: &Footprint) -> f64 {
    let b = part_extent(fp);
    if b.valid() { (b.x1 - b.x0).max(b.y1 - b.y0) } else { 0.0 }
}

fn nearest_edge(pos: Point, bb: &BBox) -> &'static str {
    let d = edge_distances(&BBox::new(pos.0, pos.1, pos.0, pos.1), bb);
    d.iter()
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap()
        .0
}

/// Sides to try when the one a part asked for has no seat left: the adjacent sides (emptiest
/// first), then the opposite one -- any board edge still answers "connectors on the edges".
fn fallback_sides(
    edge: &str,
    used: &BTreeMap<&'static str, f64>,
    side_len: &BTreeMap<&'static str, f64>,
) -> Vec<&'static str> {
    let opposite = match edge {
        "left" => "right",
        "right" => "left",
        "top" => "bottom",
        _ => "top",
    };
    let mut adjacent: Vec<&'static str> = EDGES
        .iter()
        .copied()
        .filter(|e| *e != edge && *e != opposite)
        .collect();
    adjacent.sort_by(|a, b| {
        let f = |e: &str| used[e] / side_len[e].max(1e-6);
        f(a).partial_cmp(&f(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    adjacent.push(opposite);
    adjacent
}

/// Courtyard plus pads: everything of a part that has to stay inside the outline.
pub fn part_extent(fp: &Footprint) -> BBox {
    let c = fp.courtyard_bbox();
    let mut box_ = if c.valid() { c } else { BBox::empty() };
    for p in &fp.pads {
        box_.add_bbox(&p.bbox());
    }
    box_
}

fn mating_rotation(part: &Part, edge: &str) -> f64 {
    if part.pads.is_empty() {
        return part.rot;
    }
    let n = part.pads.len() as f64;
    let px = part.pads.iter().map(|(_, _, o)| o.0).sum::<f64>() / n;
    let py = part.pads.iter().map(|(_, _, o)| o.1).sum::<f64>() / n;
    let (x0, y0, x1, y1) = part.crt;
    // pads sit on the inside of a connector
    let inside = (px - (x0 + x1) / 2.0, py - (y0 + y1) / 2.0);
    if inside.0.hypot(inside.1) < 0.3 {
        return part.rot;
    }
    // the inside direction points into the board
    let o = outward(edge);
    let want = (-o.0, -o.1);
    [0.0f64, 90.0, 180.0, 270.0]
        .into_iter()
        .max_by(|a, b| {
            let score = |r: f64| {
                let v = rotate(inside, r);
                v.0 * want.0 + v.1 * want.1
            };
            score(*a)
                .partial_cmp(&score(*b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap()
}

fn flush_position(part: &Part, rot: f64, edge: &str, outline: &BBox, pos: Point, inset: f64) -> Point {
    let bb = part.bbox_at(0.0, 0.0, rot);
    let (mut x, mut y) = pos;
    match edge {
        "left" => x = outline.x0 + inset - bb.x0,
        "right" => x = outline.x1 - inset - bb.x1,
        "top" => y = outline.y0 + inset - bb.y0,
        _ => y = outline.y1 - inset - bb.y1,
    }
    (x, y)
}

/// Moves that seat the parts flush with their edge; an `edge_for` of `"any"` is seated after the
/// named refs, longest first, on the emptiest side it fits. `obstacles` are extra boxes to walk
/// past. A crowded side is re-seated packed; a NAMED ref then falls back a side.
#[allow(clippy::too_many_arguments)]
pub fn seat_on_edges(
    board: &Board,
    refs: &[String],
    edge_for: &BTreeMap<String, String>,
    spacing: f64,
    grid: f64,
    obstacles: &[BBox],
    seated_edges: &mut BTreeMap<String, String>,
    seated_report: &mut BTreeMap<String, SeatReport>,
    unseated: &mut Vec<String>,
    fallback: bool,
) -> Vec<Move> {
    let Some(outline) = board.outline_bbox() else {
        return vec![];
    };
    let fps: BTreeMap<String, Footprint> = board
        .footprints()
        .into_iter()
        .map(|f| (f.ref_.clone(), f))
        .collect();
    // `any` means "whichever side has room": a target like the rest, it just picks its own edge
    let want: BTreeMap<String, &'static str> = edge_for
        .iter()
        .filter_map(|(r, e)| {
            EDGES
                .iter()
                .find(|k| **k == e.as_str())
                .map(|k| (r.clone(), *k))
        })
        .collect();
    let any_edge: BTreeSet<String> = edge_for
        .keys()
        .filter(|r| !want.contains_key(*r))
        .cloned()
        .collect();
    let targets: Vec<String> = refs.iter().filter(|r| fps.contains_key(*r)).cloned().collect();

    // the clearance, not just `spacing`: a seat is a DRC-visible pose, so every box here carries
    // half the copper clearance and two set hard against each other are still a clearance apart
    let clearance = copper_clearance(board);
    let mut obst: Vec<BBox> = obstacles.to_vec();
    for f in fps.values() {
        if !targets.contains(&f.ref_) && (f.locked || f.lib_id.contains("MountingHole")) {
            obst.extend(obstacle_boxes(f, clearance));
        }
    }
    // a rule area is board geometry, and so is the outline polygon: on a notched board a spot
    // flush with the bbox side can be off the board
    obst.extend(keepout_boxes(board));
    let poly = seat_polygon(board);
    let inset = seat_inset(board);
    let side_len: BTreeMap<&'static str, f64> = BTreeMap::from([
        ("left", outline.y1 - outline.y0),
        ("right", outline.y1 - outline.y0),
        ("top", outline.x1 - outline.x0),
        ("bottom", outline.x1 - outline.x0),
    ]);

    /// A flush spot for `part` on `edge`, or `None` when that side has no room left. Nearest to
    /// `pos`, or with `pack` walked inward from the corner nearest `pos` so each part takes the
    /// first spot the side has. `guard` is `part` grown by half the clearance.
    #[allow(clippy::too_many_arguments)]
    fn spot_on(
        part: &Part,
        guard: &Part,
        pos: Point,
        edge: &str,
        seated: &[BBox],
        pack: bool,
        outline: &BBox,
        inset: f64,
        grid: f64,
        spacing: f64,
        obst: &[BBox],
        poly: &Option<Vec<Point>>,
    ) -> Option<(f64, f64, f64)> {
        let rot = mating_rotation(part, edge);
        let (x, y) = flush_position(part, rot, edge, outline, pos, inset);
        let along = if edge == "left" || edge == "right" { 0 } else { 1 };
        let span = if along == 0 {
            (outline.y0, outline.y1)
        } else {
            (outline.x0, outline.x1)
        };
        let step = grid.max(0.5);
        let deltas: Vec<f64> = if pack {
            let b0 = part.bbox_at(0.0, 0.0, rot);
            let (lo0, hi0) = if along == 0 { (b0.y0, b0.y1) } else { (b0.x0, b0.x1) };
            // centre coordinates that put the whole part inside the ends of this side
            let (c0, c1) = (span.0 + inset - lo0, span.1 - inset - hi0);
            let here = if along == 0 { pos.1 } else { pos.0 };
            let (base, sign) = if (here - c0).abs() <= (here - c1).abs() {
                (c0, 1.0)
            } else {
                (c1, -1.0)
            };
            let n = ((c1 - c0).max(0.0) / step) as usize + 2;
            (0..n).map(|k| base + sign * k as f64 * step).collect()
        } else {
            let base = if along == 0 { y } else { x };
            let mut out = vec![base];
            for k in 1..400 {
                out.push(base + k as f64 * step);
                out.push(base - k as f64 * step);
            }
            out
        };
        for v in deltas {
            let v = snap(v, grid);
            let (cx, cy) = if along == 0 { (x, v) } else { (v, y) };
            let bb = part.bbox_at(cx, cy, rot);
            let (lo, hi) = if along == 0 { (bb.y0, bb.y1) } else { (bb.x0, bb.x1) };
            if lo < span.0 + inset || hi > span.1 - inset {
                continue;
            }
            let gb = guard.bbox_at(cx, cy, rot);
            if obst.iter().chain(seated).any(|o| hits(&gb, o, spacing)) {
                continue;
            }
            if let Some(poly) = poly
                && !box_in_polygon(&bb, poly) {
                    continue;
                }
            return Some((cx, cy, rot));
        }
        None
    }

    let by_pos = |a: &Footprint, b: &Footprint| {
        a.pos
            .0
            .partial_cmp(&b.pos.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.pos.1.partial_cmp(&b.pos.1).unwrap_or(std::cmp::Ordering::Equal))
    };
    // biggest first per side: a long part must claim its strip before a short one takes the middle
    let mut named: Vec<String> = targets.iter().filter(|r| want.contains_key(*r)).cloned().collect();
    named.sort_by(|a, b| {
        want[a]
            .cmp(want[b])
            .then(
                side_extent(&fps[b])
                    .partial_cmp(&side_extent(&fps[a]))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(by_pos(&fps[a], &fps[b]))
    });
    // "an edge" is seated longest first (a 50-pin header fits on few sides); auto-detected
    // connectors keep their old order and their old one-edge seat
    let mut asked: Vec<String> = targets
        .iter()
        .filter(|r| any_edge.contains(*r) && !want.contains_key(*r))
        .cloned()
        .collect();
    asked.sort_by(|a, b| {
        side_extent(&fps[b])
            .partial_cmp(&side_extent(&fps[a]))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(nearest_edge(fps[a].pos, &outline).cmp(nearest_edge(fps[b].pos, &outline)))
            .then(by_pos(&fps[a], &fps[b]))
    });
    let mut free: Vec<String> = targets
        .iter()
        .filter(|r| !want.contains_key(*r) && !any_edge.contains(*r))
        .cloned()
        .collect();
    free.sort_by(|a, b| {
        nearest_edge(fps[a].pos, &outline)
            .cmp(nearest_edge(fps[b].pos, &outline))
            .then(by_pos(&fps[a], &fps[b]))
    });
    let order: Vec<String> = named.into_iter().chain(asked).chain(free).collect();

    type Pass = (
        Vec<Move>,
        BTreeMap<String, String>,
        Vec<String>,
        BTreeMap<String, SeatReport>,
    );
    // One whole seating: (moves, ref -> edge, misses, ref -> report row).
    let seat_pass = |pack_sides: &BTreeSet<&'static str>, allow_fallback: bool| -> Pass {
        let mut seated: Vec<BBox> = Vec::new();
        let mut moves: Vec<Move> = Vec::new();
        let mut misses: Vec<String> = Vec::new();
        let mut used: BTreeMap<&'static str, f64> = EDGES.iter().map(|e| (*e, 0.0)).collect();
        let mut edges_out: BTreeMap<String, String> = BTreeMap::new();
        let mut report: BTreeMap<String, SeatReport> = BTreeMap::new();
        for r in &order {
            let fp = &fps[r];
            let part = part_from(fp, true, None, 0.0, false);
            let guard = part_from(fp, true, None, clearance, false);
            let candidates: Vec<&'static str> = if let Some(e) = want.get(r) {
                vec![*e]
            } else {
                let here = nearest_edge(fp.pos, &outline);
                if any_edge.contains(r) {
                    // nearest side first, but a staging pile shares one, so a part ASKED for an
                    // edge falls back to the emptiest of the others
                    let mut rest: Vec<&'static str> =
                        EDGES.iter().copied().filter(|e| *e != here).collect();
                    rest.sort_by(|a, b| {
                        let f = |e: &str| used[e] / side_len[e].max(1e-6);
                        f(a).partial_cmp(&f(b)).unwrap_or(std::cmp::Ordering::Equal)
                    });
                    std::iter::once(here).chain(rest).collect()
                } else {
                    vec![here]
                }
            };
            // packed on a side this pass was told to re-seat, and on one being fallen back onto
            let mut tries: Vec<(&'static str, bool)> = candidates
                .into_iter()
                .map(|e| (e, pack_sides.contains(e)))
                .collect();
            if allow_fallback
                && let Some(e) = want.get(r) {
                    tries.extend(fallback_sides(e, &used, &side_len).into_iter().map(|s| (s, true)));
                }
            let mut placed = None;
            let mut chosen = "";
            for (edge, pack) in tries {
                placed = spot_on(
                    &part, &guard, fp.pos, edge, &seated, pack, &outline, inset, grid, spacing,
                    &obst, &poly,
                );
                if placed.is_some() {
                    chosen = edge;
                    break;
                }
            }
            let Some((x, y, rot)) = placed else {
                misses.push(r.clone());
                continue;
            };
            let bb = part.bbox_at(x, y, rot);
            // what the NEXT part has to keep its copper off
            seated.push(guard.bbox_at(x, y, rot));
            *used.get_mut(chosen).unwrap() += if chosen == "left" || chosen == "right" {
                bb.y1 - bb.y0
            } else {
                bb.x1 - bb.x0
            };
            moves.push(Move {
                ref_: r.clone(),
                x: (x * 1000.0).round() / 1000.0,
                y: (y * 1000.0).round() / 1000.0,
                rot: (rot.rem_euclid(360.0) * 10.0).round() / 10.0,
                side: fp.side().to_string(),
            });
            edges_out.insert(r.clone(), chosen.to_string());
            if want.contains_key(r) || any_edge.contains(r) {
                report.insert(
                    r.clone(),
                    SeatReport {
                        side_requested: want
                            .get(r)
                            .map(|e| (*e).to_string())
                            .unwrap_or_else(|| EDGE_ANY.to_string()),
                        side_used: chosen.to_string(),
                        gap_mm: (edge_distance(&bb, &outline, chosen) * 100.0).round() / 100.0,
                    },
                );
            }
        }
        (moves, edges_out, misses, report)
    };

    // The sides to re-seat packed: the ones a named ref could not get on, and every side when an
    // `any` ref found none (it had already tried them all).
    let failed_sides = |misses: &[String]| -> BTreeSet<&'static str> {
        let mut sides: BTreeSet<&'static str> =
            misses.iter().filter_map(|r| want.get(r).copied()).collect();
        if misses.iter().any(|r| any_edge.contains(r)) {
            sides.extend(EDGES);
        }
        sides
    };
    let requested = |misses: &[String]| -> usize {
        misses
            .iter()
            .filter(|r| want.contains_key(*r) || any_edge.contains(*r))
            .count()
    };

    let mut best = seat_pass(&BTreeSet::new(), false);
    if requested(&best.2) > 0 {
        let packed = seat_pass(&failed_sides(&best.2), false);
        if requested(&packed.2) < requested(&best.2) {
            best = packed;
        }
    }
    if fallback && requested(&best.2) > 0 {
        let moved = seat_pass(&failed_sides(&best.2), true);
        if requested(&moved.2) < requested(&best.2) {
            best = moved;
        }
    }
    let (moves, edges_out, misses, report) = best;
    unseated.extend(misses);
    seated_edges.extend(edges_out);
    seated_report.extend(report);
    moves
}

// ---- sliding a seated connector to the thing it connects to -------------------------

/// Every pose `(x, y, push)` this seated part could take on its edge, nearest the edge first.
/// `push` is how far in off the edge it is: 0 wherever a flush seat is legal, otherwise the least
/// a keepout leaves. Only one push per position along the edge, so nothing is pulled inland for
/// its own sake.
#[allow(clippy::too_many_arguments)]
pub fn seat_candidates(
    parts: &Parts,
    ref_: &str,
    edge: &str,
    region: &Region,
    grid: f64,
    spacing: f64,
    ignore: &BTreeSet<String>,
    avoid_movable: bool,
    inset: f64,
) -> Vec<(f64, f64, f64)> {
    let Some(p) = parts.get(ref_) else { return vec![] };
    let home = outline_region(region);
    // only board geometry may push a connector inland; a part in the way is answered along the edge
    let blockers: Vec<BBox> = parts
        .iter()
        .filter(|q| q.ref_ != ref_ && !ignore.contains(&q.ref_) && (avoid_movable || !q.movable))
        .map(|q| q.bbox())
        .collect();
    let along = if edge == "left" || edge == "right" { 0 } else { 1 };
    let span = if along == 0 {
        (home.y0(), home.y1())
    } else {
        (home.x0(), home.x1())
    };
    let o = outward(edge);
    let inward = (-o.0, -o.1);
    let step = grid.max(0.25);
    // recomputed, not read off the part: an earlier pass may have pushed it in already
    let flush = flush_position(p, p.rot, edge, &home.bbox, (p.x, p.y), inset);
    let floor_flush = flush_position(p, p.rot, edge, &home.bbox, (p.x, p.y), EDGE_INSET);
    let at = |q: Point, i: usize| if i == 0 { q.0 } else { q.1 };
    let mut out = Vec::new();
    for i in 0..((span.1 - span.0) / step) as usize + 2 {
        let base = snap(span.0 + i as f64 * step, grid);
        let mut push = 0.0;
        while push <= EDGE_PUSH_LIMIT + 1e-9 {
            // the FLUSH coordinate is not snapped (rounding a fractional inset to the grid would
            // put the courtyard hard against the outline); a PUSHED one is, measured from the
            // floor inset, so how far a part stands off a rule area does not depend on edge
            // clearance. `along` indexes the coordinate ACROSS the edge here (0 = x for left/right).
            let across =
                at(if push == 0.0 { flush } else { floor_flush }, along) + at(inward, along) * push;
            let (cx, cy) = if along == 0 { (across, base) } else { (base, across) };
            let bb = p.bbox_at(cx, cy, p.rot);
            let (lo, hi) = if along == 0 { (bb.y0, bb.y1) } else { (bb.x0, bb.x1) };
            if lo < span.0 + inset || hi > span.1 - inset {
                break;
            }
            if !home.accepts(&bb) {
                // off the board or in a rule area: stand further in
                push += step;
                continue;
            }
            if !blockers.iter().any(|ob| hits(&bb, ob, spacing)) {
                out.push((cx, cy, push));
            }
            // a part in the way is answered along the edge, not inland
            break;
        }
    }
    out
}

/// How far this pose leaves the part's pads from the pads they connect to. Power and ground count
/// for little (poured, not routed). The push is charged at the FULL weight in play, so leaving the
/// edge is worthless unless forced.
fn seat_score(
    p: &Part,
    x: f64,
    y: f64,
    targets: &BTreeMap<i64, Vec<Point>>,
    power: &BTreeSet<i64>,
    weights: &BTreeMap<i64, f64>,
    push: f64,
) -> f64 {
    let (c, total) = pad_hops(p, x, y, p.rot, targets, power, weights);
    c + push * total * EDGE_PUSH_COST
}

/// Slide each seated connector along its edge to where its pads are nearest their partners', and
/// return the refs that moved; the edge itself is kept. `avoid_movable = false` is for the pass
/// before the cloud is legalised.
#[allow(clippy::too_many_arguments)]
pub fn reseat_along_edge(
    parts: &mut Parts,
    seated: &BTreeMap<String, String>,
    region: &Region,
    grid: f64,
    spacing: f64,
    power: &BTreeSet<i64>,
    avoid_movable: bool,
    weights: &BTreeMap<i64, f64>,
    inset: f64,
) -> Vec<String> {
    let mut moved = Vec::new();
    for (ref_, edge) in seated {
        if parts.get(ref_).is_none() {
            continue;
        }
        let targets = partner_pads(parts, ref_);
        if targets.is_empty() {
            continue;
        }
        let cands = seat_candidates(
            parts,
            ref_,
            edge,
            region,
            grid,
            spacing,
            &BTreeSet::new(),
            avoid_movable,
            inset,
        );
        let p = parts.get(ref_).unwrap();
        let mut best: Option<(f64, f64, f64, f64, f64)> = None;
        for (x, y, push) in cands {
            let cand = (
                seat_score(p, x, y, &targets, power, weights, push),
                push,
                (x - p.x).abs() + (y - p.y).abs(),
                x,
                y,
            );
            let better = best.is_none_or(|b| {
                (cand.0, cand.1, cand.2, cand.3, cand.4) < (b.0, b.1, b.2, b.3, b.4)
            });
            if better {
                best = Some(cand);
            }
        }
        if let Some(b) = best
            && ((b.3 - p.x).abs() > 1e-6 || (b.4 - p.y).abs() > 1e-6) {
                let p = parts.get_mut(ref_).unwrap();
                p.x = b.3;
                p.y = b.4;
                moved.push(ref_.clone());
            }
    }
    moved
}

/// Grid positions from `lo` to `hi`, `step` apart, ALWAYS including both ends: a part as wide as
/// the board leaves a band a coarse scan from an unsnapped start would step over.
pub fn axis(lo: f64, hi: f64, grid: f64, step: f64) -> Vec<f64> {
    let lo_g = ((lo - 1e-9) / grid).ceil() * grid;
    let hi_g = ((hi + 1e-9) / grid).floor() * grid;
    if lo_g > hi_g + 1e-9 {
        return vec![];
    }
    let mut out = Vec::new();
    let mut v = lo_g;
    while v < hi_g - 1e-9 {
        out.push((v * 1e6).round() / 1e6);
        v += step.max(grid);
    }
    out.push((hi_g * 1e6).round() / 1e6);
    out
}
