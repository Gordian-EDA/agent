//! Tie the surface-mount ground pins to the plane before anything is routed.
//!
//! A ground pad under a pour is "connected" only for as long as the pour around it stays joined to
//! the rest. The router then lays its copper, cuts the fill into islands, and a VSS pin on a
//! 0.5 mm-pitch package is left on a crumb of pour that reaches nothing — with no room beside it
//! for a stitching via and no lane out between its neighbours. Nothing downstream can fix that,
//! because the escape was never possible.
//!
//! A fanout is the standard answer and it is cheap here: before the board is routed there is no
//! copper in the way, so each ground pad gets a short stub out of the pin field to a via, and the
//! pin is joined to the plane for good.

use crate::geom::{dist, point_in_polygon, point_rect_dist, rotate, seg_point_dist, BBox, Point};
use crate::model::{Board, Rules};

/// How far out from the pad edge a fanout via may sit.
const MAX_STUB_MM: f64 = 2.2;
/// Step of the search along the escape direction.
const STEP_MM: f64 = 0.1;

/// Copper the stub and its via have to clear, with the radius each owes it.
struct Blocker {
    box_: BBox,
    drill: f64,
}

/// Drop a fanout stub and via on every surface-mount pad of `net`, returning how many landed.
///
/// Only pads whose pour is worth insuring: a through-hole pad already spans the stack, and a pad
/// with a via of its own net already beside it needs nothing.
pub fn fanout_ground(board: &mut Board, net: &str, rules: &Rules) -> usize {
    let copper = board.copper_layers();
    if copper.len() < 2 {
        return 0;
    }
    let Some(gnd) = board.net_by_name(net) else {
        return 0;
    };
    let (top, bottom) = (copper[0].clone(), copper[copper.len() - 1].clone());
    // both layers have to be poured, or the via lands on nothing at the far end
    let poured: Vec<String> = board
        .zones()
        .into_iter()
        .filter(|z| z.keepout.is_none() && z.net_name == net)
        .flat_map(|z| z.layers)
        .collect();
    if !poured.contains(&top) || !poured.contains(&bottom) {
        return 0;
    }
    let shapes: Vec<Vec<Point>> = board
        .zones()
        .into_iter()
        .filter(|z| z.keepout.is_none() && z.net_name == net && z.polygon.len() >= 3)
        .map(|z| z.polygon)
        .collect();

    let width = crate::rules::signal_track_width(rules, None);
    let (via_size, via_drill) = (rules.via_size, rules.via_drill);
    let clearance = rules.clearance;

    // every foreign pad, and every hole whatever its net
    let mut blockers: Vec<Blocker> = Vec::new();
    let mut targets: Vec<(String, String, Point, f64, f64, Point)> = Vec::new();
    for f in board.footprints() {
        for p in &f.pads {
            let drill = p.drill.unwrap_or(0.0);
            // a foreign pad is copper to keep clear of; a same-net pad is not, but its hole is
            if p.net_id != gnd.id || drill > 0.0 {
                blockers.push(Blocker {
                    box_: p.bbox(),
                    drill,
                });
            }
            if p.net_id == gnd.id && !p.is_through() && p.copper_layers().contains(&top) {
                targets.push((
                    f.ref_.clone(),
                    p.number.clone(),
                    p.pos,
                    p.size.0,
                    p.size.1,
                    f.pos,
                ));
            }
        }
    }
    // a pad the router will find a via beside anyway does not need one drilled now
    let mut placed: Vec<Point> = board
        .vias()
        .iter()
        .filter(|v| v.net_id == gnd.id)
        .map(|v| v.pos)
        .collect();

    let track_keep = clearance + width / 2.0;
    let via_keep = clearance.max(rules.hole_clearance) + via_size / 2.0;
    let hole_keep = rules.hole_clearance + via_drill / 2.0;

    let mut added = 0usize;
    for (_ref_, _pad, pos, w, h, origin) in targets {
        if placed.iter().any(|q| dist(*q, pos) < MAX_STUB_MM + via_size) {
            continue;
        }
        // out of the pin field: away from the part's own centre, along whichever axis is freer
        let away = (pos.0 - origin.0, pos.1 - origin.1);
        let len = away.0.hypot(away.1);
        let base = if len > 1e-6 {
            (away.0 / len, away.1 / len)
        } else {
            (1.0, 0.0)
        };
        let dirs = [
            base,
            (-base.0, -base.1),
            (-base.1, base.0),
            (base.1, -base.0),
        ];
        let half = (w.max(h)) / 2.0;
        let mut done = false;
        for d in dirs {
            if done {
                break;
            }
            let start = half + via_size / 2.0 + clearance;
            let mut t = start;
            while t <= MAX_STUB_MM + half {
                let c = (pos.0 + d.0 * t, pos.1 + d.1 * t);
                t += STEP_MM;
                if !shapes.iter().any(|poly| point_in_polygon(c, poly)) {
                    continue;
                }
                let via_ok = blockers.iter().all(|b| {
                    point_rect_dist(c, &b.box_) > via_keep
                        && (b.drill <= 0.0
                            || dist(c, b.box_.center()) > hole_keep + b.drill / 2.0)
                });
                let stub_ok = blockers
                    .iter()
                    .all(|b| seg_dist_to_box(pos, c, &b.box_) > track_keep);
                if !via_ok || !stub_ok {
                    continue;
                }
                if placed.iter().any(|q| dist(*q, c) < via_size + clearance) {
                    continue;
                }
                board.add_track(pos, c, width, &top, gnd.id);
                board.add_via(c, via_size, via_drill, gnd.id, (&top, &bottom));
                placed.push(c);
                added += 1;
                done = true;
                break;
            }
        }
    }
    added
}

/// Distance from segment `a`-`b` to an axis-aligned box, sampled at the box's own corners and
/// centre — exact enough for a clearance test on pads this small.
fn seg_dist_to_box(a: Point, b: Point, box_: &BBox) -> f64 {
    let mut best = f64::INFINITY;
    for c in box_.corners() {
        best = best.min(seg_point_dist(a, b, c));
    }
    best = best.min(seg_point_dist(a, b, box_.center()));
    // and the other way round: the box may straddle the segment
    for p in [a, b] {
        best = best.min(point_rect_dist(p, box_));
    }
    if box_.contains(a) || box_.contains(b) {
        return 0.0;
    }
    best
}

/// Rotate a pad-local offset into the board frame; kept for callers that need the pad's own axis.
pub fn pad_axis(rot: f64) -> Point {
    rotate((1.0, 0.0), rot)
}
