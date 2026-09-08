//! One seeded placement pass and the passes it is built from: seeding, springs, satellite slots,
//! legalisation, a greedy sweep, a tidy-up and an HPWL local search.

use std::collections::{BTreeMap, BTreeSet};
use std::f64::consts::PI;

use fastrand::Rng;

use super::edges;
use super::*;
use crate::geom::{BBox, Point, dist, point_in_polygon, rotate, seg_point_dist};
use crate::model::{Board, Footprint};

type Pose = (f64, f64, f64);
type Override = BTreeMap<String, Pose>;

fn uniform(rng: &mut Rng, a: f64, b: f64) -> f64 {
    a + (b - a) * rng.f64()
}

fn round_to(v: f64, digits: i32) -> f64 {
    let k = 10f64.powi(digits);
    (v * k).round() / k
}

/// One seeded pass of the placement described in the module docstring.
pub fn plan_once(board: &Board, fps: &[Footprint], opts: &PlanOptions, seed: u64) -> PlacementPlan {
    let mut notes: Vec<String> = Vec::new();
    let mut unplaced: Vec<(String, String, Option<Pose>)> = Vec::new();
    let Some(bb) = board.outline_bbox() else {
        notes.push("board has no closed outline; set an outline first".into());
        return PlacementPlan { notes, ..Default::default() };
    };
    let no_go = keepout_boxes(board);
    // the polygon, not just the bbox: a part in a notch of a tabbed outline is off the board
    let region = Region::new(
        bb.inflate(-OUTLINE_INSET),
        board.outline_polygon(),
        no_go,
        OUTLINE_INSET,
    );
    let (_names, power, ground) = net_classes(board);
    let mut rng = Rng::with_seed(seed);
    let grid = opts.grid;

    // every gap is floored here: two courtyards may touch, two pads may not
    let copper_gap = copper_clearance(board);
    let spacing = opts.spacing.max(copper_gap);

    let mut parts = Parts::default();
    for fp in fps {
        let movable = !(fp.locked || opts.fixed.contains(&fp.ref_) || is_hole(fp));
        let mut part = part_from(fp, movable, None, 0.0, true);
        if !movable {
            part.role = Role::Fixed;
        } else if let Some(&(x, y)) = opts.anchors.get(&fp.ref_) {
            part.role = Role::Anchor;
            part.x = x;
            part.y = y;
        }
        parts.push(part);
    }
    for r in opts.anchors.keys() {
        if parts.get(r).is_none() {
            notes.push(format!("anchor {r} not on board"));
        }
    }
    // a grouping the NETS do not carry, so every net-derived term below would ignore it
    let mut near: Vec<(String, Vec<String>, f64)> = Vec::new();
    for (a, refs, rad) in near_groups(&opts.near) {
        if parts.get(&a).is_none() {
            continue;
        }
        let kept: Vec<String> = refs.into_iter().filter(|r| parts.get(r).is_some()).collect();
        if kept.is_empty() {
            notes.push(format!("near {a}: none of the named parts are on the board"));
            continue;
        }
        near.push((a, kept, rad));
    }
    let near_of: BTreeMap<String, String> = near
        .iter()
        .flat_map(|(a, refs, _)| refs.iter().map(move |r| (r.clone(), a.clone())))
        .collect();

    // holes first: a hole has a handful of legal spots, a connector a whole edge to slide along
    let hole_edge_gap = board_edge_clearance(board) + EDGE_SEAT_SLACK;
    seat_holes(
        &mut parts,
        &region,
        grid,
        spacing,
        &mut notes,
        Some(&mut unplaced),
        &opts.fixed,
        &BTreeSet::new(),
        hole_edge_gap,
    );

    let mut seated_edges: BTreeMap<String, String> = BTreeMap::new();
    let mut seated_report: BTreeMap<String, SeatReport> = BTreeMap::new();
    if opts.seat_connectors || !opts.edge_for.is_empty() {
        // `seat_connectors` is the Python `edge_refs=[]` mode: auto-detect the connectors, and an
        // `edge_for` ref is seated as well as those
        let mut refs: Vec<String> = if opts.seat_connectors {
            fps.iter()
                .filter(|f| edges::is_connector(f) && !f.locked)
                .map(|f| f.ref_.clone())
                .collect()
        } else {
            Vec::new()
        };
        for r in opts.edge_for.keys() {
            if parts.get(r).is_some() && !refs.contains(r) {
                refs.push(r.clone());
            }
        }
        let reserved: Vec<BBox> = parts
            .iter()
            .filter(|p| is_hole(&p.fp))
            .map(|p| p.bbox())
            .collect();
        let mut unseated: Vec<String> = Vec::new();
        let moves = edges::seat_on_edges(
            board,
            &refs,
            &opts.edge_for,
            spacing.max(1.0),
            grid,
            &reserved,
            &mut seated_edges,
            &mut seated_report,
            &mut unseated,
            true,
        );
        for m in &moves {
            if let Some(p) = parts.get_mut(&m.ref_) {
                p.x = m.x;
                p.y = m.y;
                p.rot = m.rot;
                p.movable = false;
                p.seated = true;
                p.role = Role::Fixed;
            }
        }
        // first in `notes`: only the first few are reported, and a connector on another side is news
        let mut head: Vec<String> = unseated
            .iter()
            .filter(|r| opts.edge_for.contains_key(*r))
            .map(|r| format!("edge seating: {r} fits on no edge at all; it keeps its pose"))
            .collect();
        head.extend(seated_report.iter().filter_map(|(r, row)| {
            (row.side_requested != row.side_used && row.side_requested != edges::EDGE_ANY).then(|| {
                format!(
                    "edge seating: {r} asked {}, seated {} ({} edge full)",
                    row.side_requested, row.side_used, row.side_requested
                )
            })
        }));
        head.extend(notes);
        notes = head;
    }

    let wl_before = hpwl(&parts);

    // --- net graph (signal nets only) -------------------------------------------------
    let mut net_members: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for p in parts.iter() {
        for (_, net, _) in &p.pads {
            net_members.entry(*net).or_default().insert(p.ref_.clone());
        }
    }
    let pad_count: BTreeMap<i64, usize> = board
        .pads_by_net()
        .into_iter()
        .map(|(k, v)| (k, v.len()))
        .collect();
    let mut adjacency: BTreeMap<String, BTreeMap<String, f64>> =
        parts.iter().map(|p| (p.ref_.clone(), BTreeMap::new())).collect();
    // sorted, not a set's own order: the float sums accumulate in it, so hash order would drift
    for (nid, members) in &net_members {
        if power.contains(nid) || members.len() < 2 {
            continue;
        }
        // a two-pad net is the one net that can be made almost zero-length, so it weighs more
        // ... between two real parts: a probe pad's net has two pads too and belongs on the edge
        let short = pad_count.get(nid).copied().unwrap_or(0) == 2
            && members.iter().all(|r| parts.get(r).unwrap().pads.len() > 1);
        let w = if short { TWO_PAD_ATTRACT } else { 1.0 };
        for a in members {
            for b in members {
                if a != b {
                    *adjacency.get_mut(a).unwrap().entry(b.clone()).or_insert(0.0) += w;
                }
            }
        }
    }
    let mut attract = adjacency.clone();
    // a rail of <= SMALL_RAIL_PADS pads is a block's own supply, so it groups like a signal
    let small_rails: BTreeSet<i64> = power
        .iter()
        .copied()
        .filter(|n| {
            !ground.contains(n) && (1..=SMALL_RAIL_PADS).contains(&pad_count.get(n).copied().unwrap_or(0))
        })
        .collect();
    let mut hop_weights: BTreeMap<i64, f64> =
        small_rails.iter().map(|n| (*n, SMALL_RAIL_HOP)).collect();
    // rails join their members with a weight split over the net: enough to hold a block together
    for (nid, members) in &net_members {
        if !power.contains(nid) || ground.contains(nid) || members.len() < 2 {
            continue;
        }
        let w = if small_rails.contains(nid) { SMALL_RAIL_ATTRACT } else { RAIL_ATTRACT }
            / (members.len() - 1) as f64;
        for a in members {
            for b in members {
                if a != b {
                    *attract.entry(a.clone()).or_default().entry(b.clone()).or_insert(0.0) += w;
                }
            }
        }
    }
    // a stated grouping outweighs every net: members pulled to the anchor and to each other
    for (a, refs, _) in &near {
        for r in refs {
            *attract.entry(r.clone()).or_default().entry(a.clone()).or_insert(0.0) += NEAR_ATTRACT;
            *attract.entry(a.clone()).or_default().entry(r.clone()).or_insert(0.0) += NEAR_ATTRACT;
            for other in refs {
                if other != r {
                    *attract.entry(r.clone()).or_default().entry(other.clone()).or_insert(0.0) +=
                        NEAR_ATTRACT / 2.0;
                }
            }
        }
    }

    // --- satellites --------------------------------------------------------------------
    let candidates_parent: Vec<usize> = (0..parts.len())
        .filter(|&i| !(parts.list[i].pads.len() <= 2 && parts.list[i].movable))
        .collect();
    for i in 0..parts.len() {
        if parts.list[i].role != Role::Free || parts.list[i].pads.len() != 2 {
            continue;
        }
        // a `near` member follows its anchor, not the pin its net happens to touch
        if near_of.contains_key(&parts.list[i].ref_) {
            continue;
        }
        let p_ref = parts.list[i].ref_.clone();
        let nets: Vec<i64> = parts.list[i].pads.iter().map(|(_, n, _)| *n).collect();
        let decoupling = p_ref.starts_with(['C', 'c'])
            && nets.iter().any(|n| ground.contains(n))
            && nets.iter().any(|n| power.contains(n) && !ground.contains(n));
        let mut done = false;
        if decoupling {
            let pnet = *nets.iter().find(|n| power.contains(n) && !ground.contains(n)).unwrap();
            let (px, py) = (parts.list[i].x, parts.list[i].y);
            let mut best: Option<(f64, String, Point)> = None;
            for &qi in &candidates_parent {
                let q = &parts.list[qi];
                if q.ref_ == p_ref {
                    continue;
                }
                let Some(pos) = q.pad_positions().into_iter().find(|(n, _)| *n == pnet).map(|(_, p)| p)
                else {
                    continue;
                };
                let d = (pos.0 - px).hypot(pos.1 - py);
                if best.as_ref().map_or(true, |b| d < b.0) {
                    best = Some((d, q.ref_.clone(), pos));
                }
            }
            if let Some((_, parent, pos)) = best {
                let p = &mut parts.list[i];
                p.role = Role::Satellite;
                p.parent = Some(parent);
                p.target = Some(pos);
                done = true;
            }
        }
        if done {
            continue;
        }
        let mut best: Option<(f64, usize, String)> = None;
        for (r, c) in adjacency.get(&p_ref).into_iter().flatten() {
            let q = parts.get(r).unwrap();
            if q.role == Role::Satellite || (q.pads.len() <= 2 && q.movable) {
                continue;
            }
            let key = (*c, q.pads.len());
            if best.as_ref().map_or(true, |b| key.0 > b.0 || (key.0 == b.0 && key.1 < b.1)) {
                best = Some((key.0, key.1, r.clone()));
            }
        }
        if let Some((_, _, parent)) = best {
            let p = &mut parts.list[i];
            p.role = Role::Satellite;
            p.parent = Some(parent);
        }
    }
    let satellites: Vec<String> = parts
        .iter()
        .filter(|p| p.role == Role::Satellite)
        .map(|p| p.ref_.clone())
        .collect();
    let free: Vec<String> = parts
        .iter()
        .filter(|p| p.role == Role::Free)
        .map(|p| p.ref_.clone())
        .collect();
    // a single-pad part (test point, probe pad) belongs where a probe can reach it, not inland
    let peripheral: BTreeSet<String> = free
        .iter()
        .filter(|r| {
            let p = parts.get(r).unwrap();
            p.pads.len() <= 1 && !is_hole(&p.fp)
        })
        .cloned()
        .collect();
    // a `near` member is held out of the local searches: its own nets would pull it back out
    let mut settle_skip = peripheral.clone();
    settle_skip.extend(near_of.keys().cloned());

    // --- spread target: the box the placed cloud should fill --------------------------
    let (cx, cy) = region.center();
    let region_area = region.w() * region.h();
    let grown = region.inflate(0.5);
    let inside_area: f64 = parts
        .iter()
        .filter(|p| p.movable || grown.accepts(&p.bbox()))
        .map(|p| p.area())
        .sum();
    let courtyard_fraction = if region_area > 0.0 { inside_area / region_area } else { 0.0 };
    let packing = opts.spread <= 0.0 || courtyard_fraction >= opts.spread;
    let spread_box = if packing {
        region.bbox
    } else {
        let k = (inside_area / (opts.spread * region_area)).sqrt().max(0.2);
        BBox::new(
            cx - region.w() * k / 2.0,
            cy - region.h() * k / 2.0,
            cx + region.w() * k / 2.0,
            cy + region.h() * k / 2.0,
        )
    };

    // --- seeding: hubs on a grid across the spread box, the rest beside their hub ----
    let anchored: Vec<usize> = (0..parts.len())
        .filter(|&i| matches!(parts.list[i].role, Role::Fixed | Role::Anchor))
        .collect();
    // block by block: cluster on the attraction graph, each cluster gets its own patch
    if !free.is_empty() {
        let target = ((parts.len() as f64 / CLUSTER_SIZE).round() as usize).max(1);
        let groups = cluster_parts(&free, &attract, &parts, target);
        let boxes = blocks(&groups, &parts, spread_box);
        for (gi, (refs, box_)) in groups.iter().zip(boxes.iter()).enumerate() {
            for r in refs {
                parts.get_mut(r).unwrap().group = gi as i64;
                for j in 0..parts.len() {
                    if parts.list[j].role == Role::Satellite
                        && parts.list[j].parent.as_deref() == Some(r.as_str())
                    {
                        parts.list[j].group = gi as i64;
                    }
                }
            }
            let mut members: Vec<usize> = refs.iter().map(|r| parts.index(r).unwrap()).collect();
            members.sort_by(|&a, &b| {
                parts.list[b]
                    .area()
                    .partial_cmp(&parts.list[a].area())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            seed_grid(&mut parts, &members, box_);
            // break exact ties so the springs can separate them
            for &m in &members {
                parts.list[m].x += uniform(&mut rng, -0.3, 0.3);
                parts.list[m].y += uniform(&mut rng, -0.3, 0.3);
            }
        }
    }
    // ring a `near` group around its anchor: the legalisation that follows is local
    for (a, refs, rad) in &near {
        let anchor = parts.get(a).unwrap();
        let (ax, ay, asize) = (anchor.x, anchor.y, anchor.size());
        let members: Vec<usize> = refs
            .iter()
            .filter_map(|r| parts.index(r))
            .filter(|&i| parts.list[i].movable && parts.list[i].ref_ != *a)
            .collect();
        if members.is_empty() {
            continue;
        }
        let r0 = asize.0.hypot(asize.1) / 2.0 + spacing;
        let n = members.len();
        for (i, &m) in members.iter().enumerate() {
            let ang = 2.0 * PI * (i as f64 + 0.5) / n as f64 + 0.4;
            let s = parts.list[m].size();
            let reach = (r0 + s.0.hypot(s.1) / 2.0 + 0.5 * (i / 8) as f64).min(rad.max(1.0));
            parts.list[m].x = snap(ax + reach * ang.cos(), grid);
            parts.list[m].y = snap(ay + reach * ang.sin(), grid);
        }
    }

    let free_idx: Vec<usize> = free.iter().map(|r| parts.index(r).unwrap()).collect();
    for &i in &free_idx {
        parts.list[i].seed = Some((parts.list[i].x, parts.list[i].y));
    }

    // --- free parts: attraction along nets, repulsion to a uniform density ----------
    let n_cloud = free.len() + anchored.iter().filter(|&&i| parts.list[i].role == Role::Anchor).count();
    let d_min = if packing {
        0.0
    } else {
        0.9 * (spread_box.w() * spread_box.h() / n_cloud.max(1) as f64).sqrt()
    };
    let mean_w = if free.is_empty() {
        0.0
    } else {
        free_idx.iter().map(|&i| parts.list[i].size().0).sum::<f64>() / free.len() as f64
    };
    let mean_h = if free.is_empty() {
        0.0
    } else {
        free_idx.iter().map(|&i| parts.list[i].size().1).sum::<f64>() / free.len() as f64
    };
    let (target_w, target_h) = ((spread_box.w() - mean_w).max(0.0), (spread_box.h() - mean_h).max(0.0));
    let parent_idx: BTreeMap<String, usize> = parts
        .iter()
        .filter_map(|p| match (&p.role, &p.parent) {
            (Role::Satellite, Some(par)) => parts.index(par).map(|i| (p.ref_.clone(), i)),
            _ => None,
        })
        .collect();
    for it in 0..120 {
        let step = 0.6 * (1.0 - it as f64 / 120.0) + 0.05;
        for &i in &free_idx {
            let (px, py) = (parts.list[i].x, parts.list[i].y);
            let p_ref = parts.list[i].ref_.clone();
            let (mut fx, mut fy, mut w) = (0.0, 0.0, 0.0);
            for (other, cnt) in attract.get(&p_ref).into_iter().flatten() {
                let mut qi = match parts.index(other) {
                    Some(q) => q,
                    None => continue,
                };
                if let Some(&par) = parent_idx.get(other) {
                    qi = par;
                }
                if parts.list[qi].ref_ == p_ref {
                    continue;
                }
                fx += (parts.list[qi].x - px) * cnt;
                fy += (parts.list[qi].y - py) * cnt;
                w += cnt;
            }
            if w != 0.0 {
                parts.list[i].x += fx / w * step;
                parts.list[i].y += fy / w * step;
            } else if let Some((sx, sy)) = parts.list[i].seed {
                parts.list[i].x += (sx - px) * 0.1 * step;
                parts.list[i].y += (sy - py) * 0.1 * step;
            }
        }
        if !packing && free_idx.len() >= 2 {
            for a in 0..free_idx.len() {
                for b in a + 1..free_idx.len() {
                    let (i, j) = (free_idx[a], free_idx[b]);
                    let (mut dx, mut dy) = (parts.list[j].x - parts.list[i].x, parts.list[j].y - parts.list[i].y);
                    let mut d = dx.hypot(dy);
                    // the space BETWEEN blocks is the routing channel: full push across clusters only
                    let same = parts.list[i].group == parts.list[j].group && parts.list[i].group >= 0;
                    let lim = d_min * if same { SAME_GROUP_SPREAD } else { 1.0 };
                    if d >= lim {
                        continue;
                    }
                    if d < 1e-6 {
                        dx = uniform(&mut rng, -0.1, 0.1);
                        dy = uniform(&mut rng, -0.1, 0.1);
                        d = 0.1;
                    }
                    let push = (lim - d) * 0.25 * step / d;
                    parts.list[i].x -= dx * push;
                    parts.list[i].y -= dy * push;
                    parts.list[j].x += dx * push;
                    parts.list[j].y += dy * push;
                }
            }
            let xs: Vec<f64> = free_idx.iter().map(|&i| parts.list[i].x).collect();
            let ys: Vec<f64> = free_idx.iter().map(|&i| parts.list[i].y).collect();
            let (x_lo, x_hi) = (xs.iter().cloned().fold(f64::INFINITY, f64::min), xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
            let (y_lo, y_hi) = (ys.iter().cloned().fold(f64::INFINITY, f64::min), ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
            let (cw, ch) = (x_hi - x_lo, y_hi - y_lo);
            let (ccx, ccy) = ((x_hi + x_lo) / 2.0, (y_hi + y_lo) / 2.0);
            let kx = if cw > 1e-6 && cw < target_w { (target_w / cw).min(1.15) } else { 1.0 };
            let ky = if ch > 1e-6 && ch < target_h { (target_h / ch).min(1.15) } else { 1.0 };
            if kx != 1.0 || ky != 1.0 {
                for &i in &free_idx {
                    parts.list[i].x = ccx + (parts.list[i].x - ccx) * kx;
                    parts.list[i].y = ccy + (parts.list[i].y - ccy) * ky;
                }
            }
        }
        for &i in &free_idx {
            for j in free_idx.iter().copied().chain(anchored.iter().copied()) {
                if j == i {
                    continue;
                }
                let (ox, oy) = overlap(&parts.list[i].bbox(), &parts.list[j].bbox(), spacing);
                if ox > 0.0 && oy > 0.0 {
                    let mut dx = parts.list[i].x - parts.list[j].x;
                    let mut dy = parts.list[i].y - parts.list[j].y;
                    if dx == 0.0 {
                        dx = uniform(&mut rng, -0.1, 0.1);
                    }
                    if dy == 0.0 {
                        dy = uniform(&mut rng, -0.1, 0.1);
                    }
                    let heavy = if parts.list[j].movable { 1.0 } else { 2.0 };
                    if ox < oy {
                        parts.list[i].x += (ox / 2.0).copysign(dx) * heavy;
                    } else {
                        parts.list[i].y += (oy / 2.0).copysign(dy) * heavy;
                    }
                }
            }
            clamp(&mut parts.list[i], &region);
        }
    }

    // out to the nearest side, keeping the axis the springs chose
    for r in &peripheral {
        let i = parts.index(r).unwrap();
        let (d, _) = side_moves(&parts.list[i].bbox(), &region)[0];
        parts.list[i].x += d.0;
        parts.list[i].y += d.1;
    }

    // --- satellites: slots along the parent's courtyard edges --------------------------
    let mut placed_boxes: Vec<(String, BBox)> = parts
        .iter()
        .filter(|p| matches!(p.role, Role::Fixed | Role::Anchor | Role::Free))
        .map(|p| (p.ref_.clone(), p.bbox()))
        .collect();
    place_satellites(
        &satellites,
        &mut parts,
        &mut placed_boxes,
        &region,
        grid,
        spacing,
        &power,
        &mut notes,
        false,
        &hop_weights,
    );

    // --- legalise: greedy re-seating, biggest first, near the planned position -------
    let mut settled: Vec<(String, BBox)> = parts
        .iter()
        .filter(|p| !p.movable)
        .map(|p| (p.ref_.clone(), p.bbox()))
        .collect();
    // only the holes `seat_holes` itself seated may be asked to give the spot back
    let hole_refs: BTreeSet<String> = parts
        .iter()
        .filter(|p| is_hole(&p.fp) && p.seated)
        .map(|p| p.ref_.clone())
        .collect();
    let mut evicted: BTreeSet<String> = BTreeSet::new();
    // edge parts claim the border first: being small, by area alone they would be legalised last
    let mut order: Vec<String> = parts
        .iter()
        .filter(|p| p.movable)
        .map(|p| p.ref_.clone())
        .collect();
    order.sort_by(|a, b| {
        let (pa, pb) = (parts.get(a).unwrap(), parts.get(b).unwrap());
        let key = |p: &Part| {
            (
                p.role == Role::Satellite,
                !peripheral.contains(&p.ref_),
                -p.area(),
            )
        };
        let (ka, kb) = (key(pa), key(pb));
        ka.0.cmp(&kb.0)
            .then(ka.1.cmp(&kb.1))
            .then(ka.2.partial_cmp(&kb.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    // ... and a part's satellites follow it immediately, while the space beside it is still free
    let mut sats_of: Vec<(String, Vec<String>)> = Vec::new();
    for r in &order {
        let p = parts.get(r).unwrap();
        if p.role == Role::Satellite {
            if let Some(par) = p.parent.clone() {
                if parts.get(&par).is_some() {
                    match sats_of.iter_mut().find(|(k, _)| *k == par) {
                        Some((_, v)) => v.push(r.clone()),
                        None => sats_of.push((par, vec![r.clone()])),
                    }
                }
            }
        }
    }
    let mut ordered: Vec<String> = Vec::new();
    for r in &order {
        if parts.get(r).unwrap().role == Role::Satellite {
            continue;
        }
        ordered.push(r.clone());
        if let Some(pos) = sats_of.iter().position(|(k, _)| k == r) {
            let (_, sats) = sats_of.remove(pos);
            ordered.extend(sats);
        }
    }
    for (_, sats) in &sats_of {
        ordered.extend(sats.iter().cloned());
    }

    for r in &ordered {
        let i = parts.index(r).unwrap();
        let bb = parts.list[i].bbox();
        if bb.w() > region.w() || bb.h() > region.h() {
            unplaced.push((r.clone(), "courtyard larger than region".into(), None));
            continue;
        }
        let p = &parts.list[i];
        let near_pt = match (p.role, p.target) {
            (Role::Satellite, Some(t)) => t,
            _ => (p.x, p.y),
        };
        let rots = [p.rot, p.rot + 90.0, p.rot + 180.0, p.rot + 270.0];
        let mut spot = free_spot(p, near_pt, &region, &settled, grid, spacing, &rots)
            .or_else(|| free_spot(p, near_pt, &region, &settled, grid, copper_gap, &rots));
        if spot.is_none() && settled.iter().any(|(r, _)| hole_refs.contains(r)) {
            // a hole claimed its extremity before there was a layout: it gives the spot back here
            let lean: Vec<(String, BBox)> = settled
                .iter()
                .filter(|(r, _)| !hole_refs.contains(r))
                .cloned()
                .collect();
            spot = free_spot(&parts.list[i], near_pt, &region, &lean, grid, spacing, &rots);
            if let Some(s) = spot {
                let bb2 = parts.list[i].bbox_at(s.0, s.1, s.2);
                for (r, hb) in &settled {
                    if hole_refs.contains(r) && hits(&bb2, hb, spacing) {
                        evicted.insert(r.clone());
                    }
                }
                for r in &evicted {
                    if let Some(q) = parts.get_mut(r) {
                        // back to where it came from
                        q.seated = false;
                        q.x = q.fp.pos.0;
                        q.y = q.fp.pos.1;
                    }
                }
                settled.retain(|(r, _)| !evicted.contains(r));
            }
        }
        let Some((x, y, rot)) = spot else {
            unplaced.push((r.clone(), "no free spot in region".into(), None));
            continue;
        };
        let p = &mut parts.list[i];
        p.x = x;
        p.y = y;
        p.rot = rot;
        settled.push((r.clone(), p.bbox()));
    }

    let radius: f64 = if parts.len() <= 30 { 8.0 } else { 5.0 };
    // few parts: let the local search roam, the spread box hardly matters
    let small = parts.len() <= 15;
    let sweeps = if parts.len() <= 30 { 3 } else { 2 };
    let reach = if packing || small { radius } else { radius.min(3.0) };
    improve(
        &mut parts,
        &region,
        grid,
        spacing,
        sweeps,
        reach,
        &settle_skip,
        &BTreeMap::new(),
    );
    // re-slot satellites against their (possibly moved) parents, then tidy rows and columns
    let sats: Vec<String> = parts
        .iter()
        .filter(|p| p.role == Role::Satellite && p.movable)
        .map(|p| p.ref_.clone())
        .collect();
    // every part, satellites included: one that keeps its slot is still a real obstacle
    let mut boxes = parts.boxes();
    place_satellites(
        &sats,
        &mut parts,
        &mut boxes,
        &region,
        grid,
        spacing,
        &power,
        &mut notes,
        true,
        &hop_weights,
    );
    tidy(&mut parts, &region, spacing);
    let gain = refine(&mut parts, &region, grid, spacing, &settle_skip, &mut rng, &mut notes);
    if gain > 0.5 {
        notes.push(format!("local search took {gain:.0} mm off the wirelength"));
    }
    center(&mut parts, &region, grid, spacing);
    seat_peripheral(&mut parts, &peripheral, &region, grid, spacing);
    seat_holes(
        &mut parts,
        &region,
        grid,
        spacing,
        &mut notes,
        Some(&mut unplaced),
        &opts.fixed,
        &evicted,
        hole_edge_gap,
    );
    // last, after every pass that could have carried a member off
    enforce_near(&mut parts, &near, &region, grid, spacing, &mut notes);

    hop_weights.clear();
    let overlaps = count_overlaps(&parts);
    let wl_after = hpwl(&parts);
    let mut moves = Vec::new();
    for p in parts.iter() {
        if !p.movable && !p.seated {
            continue;
        }
        let turned = ((p.rot - p.fp.rot + 180.0).rem_euclid(360.0) - 180.0).abs() > 1e-6;
        if (p.x - p.fp.pos.0).abs() > 1e-6
            || (p.y - p.fp.pos.1).abs() > 1e-6
            || p.side != p.fp.side()
            || turned
        {
            moves.push(Move {
                ref_: p.ref_.clone(),
                x: round_to(p.x, 3),
                y: round_to(p.y, 3),
                rot: round_to(p.rot.rem_euclid(360.0), 1),
                side: p.side.clone(),
            });
        }
    }
    PlacementPlan {
        moves,
        unplaced,
        wirelength_before: wl_before,
        wirelength_after: wl_after,
        overlaps_after: overlaps,
        notes,
        seated: seated_report,
    }
}

/// Put hubs on the centres of a grid that spans `box_`, largest first, last row centred.
fn seed_grid(parts: &mut Parts, hubs: &[usize], box_: &BBox) {
    let n = hubs.len();
    if n == 0 {
        return;
    }
    let cols = ((n as f64 * box_.w() / box_.h().max(1e-6)).sqrt().round() as usize)
        .clamp(1, n);
    let rows = (n as f64 / cols as f64).ceil() as usize;
    let (cw, ch) = (box_.w() / cols as f64, box_.h() / rows.max(1) as f64);
    for (i, &h) in hubs.iter().enumerate() {
        let (r, c) = (i / cols, i % cols);
        let in_row = cols.min(n - r * cols);
        let x0 = box_.x0 + (box_.w() - in_row as f64 * cw) / 2.0;
        parts.list[h].x = x0 + (c as f64 + 0.5) * cw;
        parts.list[h].y = box_.y0 + (r as f64 + 0.5) * ch;
    }
}

/// Agglomerative clustering of `refs` on the attraction graph (average linkage) down to `target`
/// clusters, in dendrogram order. Satellites count toward their parent, never members.
fn cluster_parts(
    refs: &[String],
    attract: &BTreeMap<String, BTreeMap<String, f64>>,
    parts: &Parts,
    target: usize,
) -> Vec<Vec<String>> {
    if refs.is_empty() {
        return vec![];
    }
    let mut order: Vec<String> = refs.to_vec();
    order.sort();
    let mut clusters: Vec<Vec<String>> = order.iter().map(|r| vec![r.clone()]).collect();
    let pull = |a: &str, b: &str| -> f64 {
        let mut v = attract.get(a).and_then(|m| m.get(b)).copied().unwrap_or(0.0);
        // a satellite's pull counts for its parent, which is where it will actually sit
        for s in parts.iter() {
            if s.role == Role::Satellite && s.parent.as_deref() == Some(a) {
                v += attract.get(&s.ref_).and_then(|m| m.get(b)).copied().unwrap_or(0.0);
            }
        }
        v
    };
    let n = clusters.len();
    let mut w: Vec<BTreeMap<usize, f64>> = Vec::with_capacity(n);
    for i in 0..n {
        let mut row = BTreeMap::new();
        for j in 0..n {
            if i == j {
                continue;
            }
            let v = pull(&clusters[i][0], &clusters[j][0]);
            if v > 0.0 {
                row.insert(j, v);
            }
        }
        w.push(row);
    }
    let mut alive: Vec<usize> = (0..n).collect();
    let mut snapshot: Option<Vec<Vec<String>>> =
        (clusters.len() <= target).then(|| clusters.clone());
    while alive.len() > 1 {
        let mut best: Option<(f64, usize, usize)> = None;
        for &i in &alive {
            for (&j, &v) in &w[i] {
                if j <= i || !alive.contains(&j) {
                    continue;
                }
                let s = v / (clusters[i].len() * clusters[j].len()) as f64;
                if best.map_or(true, |b| s > b.0) {
                    best = Some((s, i, j));
                }
            }
        }
        let Some((_, i, j)) = best else { break };
        let tail = clusters[j].clone();
        clusters[i].extend(tail);
        let row_j: Vec<(usize, f64)> = w[j].iter().map(|(&k, &v)| (k, v)).collect();
        for (k, v) in row_j {
            if k != i {
                *w[i].entry(k).or_insert(0.0) += v;
                *w[k].entry(i).or_insert(0.0) += v;
            }
        }
        for k in 0..w.len() {
            w[k].remove(&j);
        }
        w[i].remove(&j);
        alive.retain(|&k| k != j);
        clusters[j].clear();
        if alive.len() == target {
            snapshot = Some(alive.iter().map(|&k| clusters[k].clone()).collect());
        }
    }
    let mut snapshot = snapshot.unwrap_or_else(|| {
        alive
            .iter()
            .filter(|&&k| !clusters[k].is_empty())
            .map(|&k| clusters[k].clone())
            .collect()
    });
    let root: Vec<String> = alive.iter().flat_map(|&k| clusters[k].clone()).collect();
    let pos: BTreeMap<String, usize> = root.iter().cloned().zip(0..).collect();
    for c in snapshot.iter_mut() {
        c.sort_by_key(|r| pos.get(r).copied().unwrap_or(0));
    }
    snapshot.sort_by_key(|c| c.first().and_then(|r| pos.get(r).copied()).unwrap_or(0));
    snapshot.retain(|c| !c.is_empty());
    snapshot
}

/// Cut `box_` into one patch per cluster, area proportional to the courtyard area it holds, by
/// recursive bisection of the dendrogram order across the longer side.
fn blocks(groups: &[Vec<String>], parts: &Parts, box_: BBox) -> Vec<BBox> {
    let mut sat_area: BTreeMap<String, f64> = BTreeMap::new();
    for s in parts.iter() {
        if s.role == Role::Satellite {
            if let Some(par) = &s.parent {
                *sat_area.entry(par.clone()).or_insert(0.0) += s.area();
            }
        }
    }
    let areas: Vec<f64> = groups
        .iter()
        .map(|g| {
            let a: f64 = g
                .iter()
                .map(|r| {
                    parts.get(r).map_or(0.0, |p| p.area())
                        + sat_area.get(r).copied().unwrap_or(0.0)
                })
                .sum();
            if a == 0.0 { 1e-6 } else { a }
        })
        .collect();
    let mut out: Vec<Option<BBox>> = vec![None; groups.len()];
    cut(0, groups.len(), box_, &areas, &mut out);
    out.into_iter().map(|b| b.unwrap_or(box_)).collect()
}

fn cut(lo: usize, hi: usize, b: BBox, areas: &[f64], out: &mut Vec<Option<BBox>>) {
    if hi - lo == 1 {
        out[lo] = Some(b);
        return;
    }
    let total: f64 = areas[lo..hi].iter().sum();
    let (mut half, mut k) = (0.0, lo);
    for i in lo..hi - 1 {
        k = i;
        half += areas[i];
        if half >= total / 2.0 {
            break;
        }
    }
    k += 1;
    let f = if total > 0.0 { areas[lo..k].iter().sum::<f64>() / total } else { 0.5 };
    if b.w() >= b.h() {
        let xm = b.x0 + b.w() * f;
        cut(lo, k, BBox::new(b.x0, b.y0, xm, b.y1), areas, out);
        cut(k, hi, BBox::new(xm, b.y0, b.x1, b.y1), areas, out);
    } else {
        let ym = b.y0 + b.h() * f;
        cut(lo, k, BBox::new(b.x0, b.y0, b.x1, ym), areas, out);
        cut(k, hi, BBox::new(b.x0, ym, b.x1, b.y1), areas, out);
    }
}

/// Shift the movable cloud onto the region centre if it fits; a satellite of a fixed parent stays.
fn center(parts: &mut Parts, region: &Region, grid: f64, spacing: f64) {
    let shift: Vec<usize> = (0..parts.len())
        .filter(|&i| {
            let p = &parts.list[i];
            p.movable
                && !(p.role == Role::Satellite
                    && p.parent.as_ref().is_some_and(|r| {
                        parts.get(r).map_or(false, |q| !q.movable)
                    }))
        })
        .collect();
    if shift.is_empty() {
        return;
    }
    let stay: Vec<BBox> = (0..parts.len())
        .filter(|i| !shift.contains(i))
        .map(|i| parts.list[i].bbox())
        .collect();
    let mut bb = BBox::empty();
    for &i in &shift {
        bb.add_bbox(&parts.list[i].bbox());
    }
    let want = (
        region.center().0 - bb.center().0,
        region.center().1 - bb.center().1,
    );
    for axis in 0..2 {
        for f in [1.0, 0.75, 0.5, 0.25] {
            let d = snap(if axis == 0 { want.0 } else { want.1 } * f, grid);
            if d.abs() < grid / 2.0 {
                break;
            }
            let (sx, sy) = if axis == 0 { (d, 0.0) } else { (0.0, d) };
            let fits = shift.iter().all(|&i| {
                let p = &parts.list[i];
                let b = p.bbox_at(p.x + sx, p.y + sy, p.rot);
                region.accepts(&b) && !stay.iter().any(|ob| hits(&b, ob, spacing))
            });
            if fits {
                for &i in &shift {
                    parts.list[i].x += sx;
                    parts.list[i].y += sy;
                }
                break;
            }
        }
    }
}

/// net id -> where that net's other pads are, for the nets `ref_` is on; nets with none dropped.
pub(crate) fn partner_pads(parts: &Parts, ref_: &str) -> BTreeMap<i64, Vec<Point>> {
    let me = parts.get(ref_).unwrap();
    let want: BTreeSet<i64> = me.pads.iter().map(|(_, n, _)| *n).filter(|n| *n != 0).collect();
    let mut out: BTreeMap<i64, Vec<Point>> = want.iter().map(|n| (*n, Vec::new())).collect();
    for q in parts.iter() {
        if q.ref_ == ref_ {
            continue;
        }
        for (net, pos) in q.pad_positions() {
            if let Some(v) = out.get_mut(&net) {
                v.push(pos);
            }
        }
    }
    out.retain(|_, v| !v.is_empty());
    out
}

/// Weighted distance from `p`'s pads to the nearest pad they connect to, and the total weight;
/// power and ground count for little, being poured rather than routed.
pub(crate) fn pad_hops(
    p: &Part,
    x: f64,
    y: f64,
    rot: f64,
    targets: &BTreeMap<i64, Vec<Point>>,
    power: &BTreeSet<i64>,
    weights: &BTreeMap<i64, f64>,
) -> (f64, f64) {
    let (mut c, mut total) = (0.0, 0.0);
    for (net, pos) in p.pad_positions_at(x, y, rot) {
        let Some(pts) = targets.get(&net) else { continue };
        if pts.is_empty() {
            continue;
        }
        let k = weights
            .get(&net)
            .copied()
            .unwrap_or(if power.contains(&net) { 0.3 } else { 1.0 });
        c += pts.iter().map(|q| dist(pos, *q)).fold(f64::INFINITY, f64::min) * k;
        total += k;
    }
    (c, total)
}

/// net id -> `[(ref, pad position)]` over these parts, at the poses they hold now.
pub(crate) fn net_pads(parts: &Parts) -> BTreeMap<i64, Vec<(String, Point)>> {
    let mut out: BTreeMap<i64, Vec<(String, Point)>> = BTreeMap::new();
    for p in parts.iter() {
        for (net, pos) in p.pad_positions() {
            out.entry(net).or_default().push((p.ref_.clone(), pos));
        }
    }
    out
}

/// Per net `p` is on, the box of that net's OTHER pads; nets with none are dropped.
pub(crate) fn others_box(
    net_pads: &BTreeMap<i64, Vec<(String, Point)>>,
    p: &Part,
) -> BTreeMap<i64, (f64, f64, f64, f64)> {
    let mut out = BTreeMap::new();
    for (_, net, _) in &p.pads {
        let mut b = BBox::empty();
        if let Some(v) = net_pads.get(net) {
            for (r, q) in v {
                if r != &p.ref_ {
                    b.add_point(*q);
                }
            }
        }
        if b.valid() {
            out.insert(*net, (b.x0, b.y0, b.x1, b.y1));
        }
    }
    out
}

/// HPWL of `p`'s nets at this pose against an [`others_box`] index, scaled by `weight(net)`.
pub(crate) fn box_cost(
    p: &Part,
    boxes: &BTreeMap<i64, (f64, f64, f64, f64)>,
    x: f64,
    y: f64,
    rot: f64,
    weight: &dyn Fn(i64) -> f64,
) -> f64 {
    let mut c = 0.0;
    for (net, (px, py)) in p.pad_positions_at(x, y, rot) {
        if let Some(o) = boxes.get(&net) {
            c += (o.2.max(px) - o.0.min(px) + o.3.max(py) - o.1.min(py)) * weight(net);
        }
    }
    c
}

/// Greedy local search: each movable part to the nearby legal pose with the least wirelength.
/// `weights` scales a net -- one ending on a bolted-down part can only be shortened here.
fn improve(
    parts: &mut Parts,
    region: &Region,
    grid: f64,
    spacing: f64,
    sweeps: usize,
    radius: f64,
    skip: &BTreeSet<String>,
    weights: &BTreeMap<i64, f64>,
) {
    let mut np = net_pads(parts);
    let weight = |n: i64| weights.get(&n).copied().unwrap_or(1.0);
    let step = grid.max(0.5);
    let offsets: Vec<(f64, f64)> = frange(-radius, radius, step)
        .into_iter()
        .flat_map(|dx| frange(-radius, radius, step).into_iter().map(move |dy| (dx, dy)))
        .filter(|(dx, dy)| dx * dx + dy * dy <= radius * radius)
        .collect();
    for _ in 0..sweeps {
        let mut improved = false;
        for i in 0..parts.len() {
            {
                let p = &parts.list[i];
                if !p.movable || p.role == Role::Satellite || skip.contains(&p.ref_) {
                    continue;
                }
            }
            let boxes: Vec<BBox> = (0..parts.len())
                .filter(|&j| j != i)
                .map(|j| parts.list[j].bbox())
                .collect();
            let p = &parts.list[i];
            let ob = others_box(&np, p);
            let mut best = (box_cost(p, &ob, p.x, p.y, p.rot, &weight), p.x, p.y, p.rot);
            let rots: Vec<f64> = if p.pads.len() > 2 {
                // all four when there is a seat to face: a half turn cannot reach the right corner
                if weights.is_empty() {
                    vec![p.rot, p.rot + 180.0]
                } else {
                    vec![p.rot, p.rot + 90.0, p.rot + 180.0, p.rot + 270.0]
                }
            } else {
                vec![0.0, 90.0]
            };
            for (dx, dy) in &offsets {
                let (x, y) = (snap(p.x + dx, grid), snap(p.y + dy, grid));
                for &rot in &rots {
                    let c = box_cost(p, &ob, x, y, rot, &weight);
                    if c >= best.0 - 1e-6 {
                        continue;
                    }
                    let bb = p.bbox_at(x, y, rot);
                    if !region.accepts(&bb) || boxes.iter().any(|o| hits(&bb, o, spacing)) {
                        continue;
                    }
                    best = (c, x, y, rot);
                }
            }
            if (best.1, best.2, best.3) != (p.x, p.y, p.rot) {
                let r = p.ref_.clone();
                let p = &mut parts.list[i];
                p.x = best.1;
                p.y = best.2;
                p.rot = best.3.rem_euclid(360.0);
                improved = true;
                for (net, pos) in parts.list[i].pad_positions() {
                    let v = np.entry(net).or_default();
                    v.retain(|(q, _)| *q != r);
                    v.push((r.clone(), pos));
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// Candidate `(x, y, rot, ring)` poses for `sat` along `parent`'s courtyard edges, long axis across
/// the edge, ring 0 touching: at the parent's pad coordinates, thinned, then at a regular pitch.
fn slots(parent: &Part, sat: &Part, spacing: f64, grid: f64) -> Vec<(f64, f64, f64, usize)> {
    let pb = parent.bbox();
    let (w, h) = sat.size();
    let (long_, short) = (w.max(h), w.min(h));
    // the rotation that makes the satellite horizontal (long axis along x)
    let rot_h = if w >= h { 0.0 } else { 90.0 };
    let pitch = short + spacing;
    let pads: Vec<Point> = parent.pad_positions().into_iter().map(|(_, p)| p).collect();

    let coords = |lo: f64, hi: f64, pad_coords: Vec<f64>| -> Vec<f64> {
        let mut snapped: Vec<f64> = pad_coords.into_iter().map(|v| snap(v, grid)).collect();
        snapped.sort_by(|a, b| a.partial_cmp(b).unwrap());
        snapped.dedup();
        let mut out: Vec<f64> = Vec::new();
        for c in snapped {
            if lo + short / 2.0 - 1e-6 <= c
                && c <= hi - short / 2.0 + 1e-6
                && out.last().map_or(true, |o| c - o >= pitch - 1e-6)
            {
                out.push(c);
            }
        }
        let mut c = lo + short / 2.0;
        while c <= hi - short / 2.0 + 1e-6 {
            if out.iter().all(|o| (c - o).abs() >= pitch - 1e-6) {
                out.push(snap(c, grid));
            }
            c += pitch;
        }
        out
    };

    let ys = coords(pb.y0, pb.y1, pads.iter().map(|p| p.1).collect());
    let xs = coords(pb.x0, pb.x1, pads.iter().map(|p| p.0).collect());
    let mut out = Vec::new();
    for ring in 0..2 {
        let off = spacing + long_ / 2.0 + ring as f64 * (long_ + spacing);
        for x in [pb.x0 - off, pb.x1 + off] {
            out.extend(ys.iter().map(|&y| (snap(x, grid), y, rot_h, ring)));
        }
        for y in [pb.y0 - off, pb.y1 + off] {
            out.extend(xs.iter().map(|&x| (x, snap(y, grid), (rot_h + 90.0) % 360.0, ring)));
        }
    }
    if long_ > 2.5 * short {
        // long parts (axial resistors, diodes) may also lie parallel to the edge, like a human would
        let pitch_l = long_ + spacing;
        for ring in 0..2 {
            let off = spacing + short / 2.0 + ring as f64 * (short + spacing);
            for x in [pb.x0 - off, pb.x1 + off] {
                let mut y = pb.y0 + long_ / 2.0;
                while y <= pb.y1 - long_ / 2.0 + 1e-6 {
                    out.push((snap(x, grid), snap(y, grid), (rot_h + 90.0) % 360.0, ring));
                    y += pitch_l;
                }
            }
            for y in [pb.y0 - off, pb.y1 + off] {
                let mut x = pb.x0 + long_ / 2.0;
                while x <= pb.x1 - long_ / 2.0 + 1e-6 {
                    out.push((snap(x, grid), snap(y, grid), rot_h, ring));
                    x += pitch_l;
                }
            }
        }
    }
    out
}

/// Each satellite to the best free slot beside its parent (shortest pad hops, inner ring and both
/// 180-degree variants tried); falls back to a ring search.
#[allow(clippy::too_many_arguments)]
fn place_satellites(
    satellites: &[String],
    parts: &mut Parts,
    placed_boxes: &mut Vec<(String, BBox)>,
    region: &Region,
    grid: f64,
    spacing: f64,
    power: &BTreeSet<i64>,
    notes: &mut Vec<String>,
    relocate: bool,
    weights: &BTreeMap<i64, f64>,
) {
    let mut order: Vec<String> = satellites.to_vec();
    order.sort_by_key(|r| {
        let p = parts.get(r).unwrap();
        (
            p.target.is_none(),
            p.parent.clone().unwrap_or_default(),
            p.ref_.clone(),
        )
    });
    for r in &order {
        let i = parts.index(r).unwrap();
        let Some(parent) = parts.list[i].parent.clone().and_then(|p| parts.index(&p)) else {
            if !relocate {
                parts.list[i].role = Role::Free;
            }
            continue;
        };
        let targets = partner_pads(parts, r);
        let s = &parts.list[i];
        let mut best: Option<(f64, f64, f64, f64)> = None;
        for (x, y, rot, ring) in slots(&parts.list[parent], s, spacing, grid) {
            for rr in [rot, (rot + 180.0) % 360.0] {
                let bb = s.bbox_at(x, y, rr);
                if !region.accepts(&bb) {
                    continue;
                }
                if placed_boxes
                    .iter()
                    .any(|(pr, ob)| pr != r && hits(&bb, ob, spacing))
                {
                    continue;
                }
                let cost = pad_hops(s, x, y, rr, &targets, power, weights).0 + 1.5 * ring as f64;
                if best.map_or(true, |b| cost < b.0) {
                    best = Some((cost, x, y, rr));
                }
            }
        }
        if best.is_none() {
            let target = s.target.unwrap_or((parts.list[parent].x, parts.list[parent].y));
            let obst: Vec<(String, BBox)> = placed_boxes
                .iter()
                .filter(|(pr, _)| pr != r)
                .cloned()
                .collect();
            match free_spot(s, target, region, &obst, grid, spacing, &[0.0, 90.0]) {
                Some(spot) => best = Some((0.0, spot.0, spot.1, spot.2)),
                None => {
                    if !relocate {
                        let pr = parts.list[parent].ref_.clone();
                        notes.push(format!("{r}: no free spot next to {pr}, placed freely"));
                        parts.list[i].role = Role::Free;
                    }
                    continue;
                }
            }
        }
        let (_, x, y, rot) = best.unwrap();
        let p = &mut parts.list[i];
        p.x = x;
        p.y = y;
        p.rot = rot;
        let bb = parts.list[i].bbox();
        placed_boxes.retain(|(pr, _)| pr != r);
        placed_boxes.push((r.clone(), bb));
    }
}

/// Per side of `region`: the `(dx, dy)` landing `bb` flush against it and its gap, nearest first.
fn side_moves(bb: &BBox, region: &Region) -> Vec<(Point, f64)> {
    let mut d = vec![
        ((-(bb.x0 - region.x0()), 0.0), bb.x0 - region.x0()),
        ((region.x1() - bb.x1, 0.0), region.x1() - bb.x1),
        ((0.0, -(bb.y0 - region.y0())), bb.y0 - region.y0()),
        ((0.0, region.y1() - bb.y1), region.y1() - bb.y1),
    ];
    d.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    d
}

fn edge_gap(bb: &BBox, region: &Region) -> f64 {
    (bb.x0 - region.x0())
        .min(region.x1() - bb.x1)
        .min(bb.y0 - region.y0())
        .min(region.y1() - bb.y1)
}

/// Final pass: every single-pad part to the closest free spot on a side of the region.
fn seat_peripheral(
    parts: &mut Parts,
    peripheral: &BTreeSet<String>,
    region: &Region,
    grid: f64,
    spacing: f64,
) {
    for r in peripheral {
        let Some(i) = parts.index(r) else { continue };
        if !parts.list[i].movable {
            continue;
        }
        let gap = edge_gap(&parts.list[i].bbox(), region);
        if gap <= grid {
            continue;
        }
        let boxes: Vec<(String, BBox)> = (0..parts.len())
            .filter(|&j| j != i)
            .map(|j| (parts.list[j].ref_.clone(), parts.list[j].bbox()))
            .collect();
        let p = &parts.list[i];
        let mut best: Option<(f64, Pose)> = None;
        // every side, not just the nearest: the side it drifted towards is often the one that filled
        for (d, _) in side_moves(&p.bbox(), region) {
            let Some(spot) = free_spot(
                p,
                (p.x + d.0, p.y + d.1),
                region,
                &boxes,
                grid,
                spacing,
                &[p.rot],
            ) else {
                continue;
            };
            let g = edge_gap(&p.bbox_at(spot.0, spot.1, spot.2), region);
            if best.map_or(true, |b| g < b.0) {
                best = Some((g, spot));
            }
        }
        if let Some((g, spot)) = best {
            if g < gap {
                let p = &mut parts.list[i];
                p.x = spot.0;
                p.y = spot.1;
                p.rot = spot.2;
            }
        }
    }
}

/// Snap nearly aligned movable parts onto common rows/columns when it barely costs wirelength.
fn tidy(parts: &mut Parts, region: &Region, spacing: f64) {
    let (tol, max_loss) = (TIDY_TOL, 0.05);
    let mut np = net_pads(parts);
    let one = |_n: i64| 1.0;
    let mut movable: Vec<usize> = (0..parts.len()).filter(|&i| parts.list[i].movable).collect();
    movable.sort_by(|&a, &b| {
        parts.list[b]
            .area()
            .partial_cmp(&parts.list[a].area())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for axis in [1usize, 0] {
        // rows first, then columns
        for &i in &movable {
            let p = &parts.list[i];
            let mine = if axis == 1 { p.y } else { p.x };
            let ob = others_box(&np, p);
            let mut moved: Option<(f64, f64)> = None;
            for j in 0..parts.len() {
                if j == i {
                    continue;
                }
                let theirs = if axis == 1 { parts.list[j].y } else { parts.list[j].x };
                if (theirs - mine).abs() < 1e-6 || (theirs - mine).abs() > tol {
                    continue;
                }
                let p = &parts.list[i];
                let (x, y) = if axis == 1 { (p.x, theirs) } else { (theirs, p.y) };
                let bb = p.bbox_at(x, y, p.rot);
                if !region.accepts(&bb)
                    || (0..parts.len())
                        .filter(|&k| k != i)
                        .any(|k| hits(&bb, &parts.list[k].bbox(), spacing))
                {
                    continue;
                }
                let before = box_cost(p, &ob, p.x, p.y, p.rot, &one);
                let after = box_cost(p, &ob, x, y, p.rot, &one);
                if after <= before * (1.0 + max_loss) + 1e-6 {
                    moved = Some((x, y));
                    break;
                }
            }
            if let Some((x, y)) = moved {
                let r = parts.list[i].ref_.clone();
                parts.list[i].x = x;
                parts.list[i].y = y;
                for (net, pos) in parts.list[i].pad_positions() {
                    let v = np.entry(net).or_default();
                    v.retain(|(q, _)| *q != r);
                    v.push((r.clone(), pos));
                }
            }
        }
    }
}

fn frange(a: f64, b: f64, step: f64) -> Vec<f64> {
    let mut out = Vec::new();
    let mut v = a;
    while v <= b + 1e-9 {
        out.push(v);
        v += step;
    }
    out
}

/// Nearest grid position (ring scan around `near`) where `p` fits without overlap. The hot loop:
/// the clearance test is inlined against boxes pre-grown by `spacing`.
pub fn free_spot(
    p: &Part,
    near: Point,
    region: &Region,
    boxes: &[(String, BBox)],
    grid: f64,
    spacing: f64,
    rots: &[f64],
) -> Option<Pose> {
    let step = grid.max(0.5);
    // `near` counts: a target parked off the board needs a radius that can reach back to it
    let r_max = region.w().hypot(region.h())
        + (near.0 - region.center().0).hypot(near.1 - region.center().1);
    let mut grown: Vec<(f64, f64, f64, f64)> = boxes
        .iter()
        .map(|(_, b)| (b.x0 - spacing, b.y0 - spacing, b.x1 + spacing, b.y1 + spacing))
        .collect();
    // boxes nearest `near` first: at ring r only those within r + the part's reach can be in the
    // way. The reach is ORIGIN to the furthest courtyard corner, + a step for the snap past r.
    let (cx0, cy0, cx1, cy1) = p.crt;
    let half = [cx0, cx1]
        .into_iter()
        .flat_map(|a| [cy0, cy1].into_iter().map(move |b| a.hypot(b)))
        .fold(f64::NEG_INFINITY, f64::max)
        + step;
    let gap = |b: &(f64, f64, f64, f64)| {
        (b.0 - near.0).max(0.0).max(near.0 - b.2).hypot((b.1 - near.1).max(0.0).max(near.1 - b.3))
    };
    grown.sort_by(|a, b| gap(a).partial_cmp(&gap(b)).unwrap_or(std::cmp::Ordering::Equal));
    let gaps: Vec<f64> = grown.iter().map(gap).collect();
    let (rx0, ry0, rx1, ry1) = (
        region.x0() - 1e-6,
        region.y0() - 1e-6,
        region.x1() + 1e-6,
        region.y1() + 1e-6,
    );
    let mut live = 0usize;
    let mut r = 0.0;
    while r <= r_max {
        while live < gaps.len() && gaps[live] <= r + half {
            live += 1;
        }
        let near_boxes = &grown[..live];
        let n = if r > 0.0 { ((2.0 * PI * r / step) as usize).max(1) } else { 1 };
        for k in 0..n {
            let a = 2.0 * PI * k as f64 / n as f64;
            let x = snap(near.0 + r * a.cos(), grid);
            let y = snap(near.1 + r * a.sin(), grid);
            for &rot in rots {
                let bb = p.bbox_at(x, y, rot);
                if bb.x0 < rx0 || bb.y0 < ry0 || bb.x1 > rx1 || bb.y1 > ry1 {
                    continue;
                }
                if near_boxes
                    .iter()
                    .any(|o| bb.x1 > o.0 && o.2 > bb.x0 && bb.y1 > o.1 && o.3 > bb.y0)
                {
                    continue;
                }
                // outline polygon and rule areas
                if region.strict && !region.accepts(&bb) {
                    continue;
                }
                return Some((x, y, rot.rem_euclid(360.0)));
            }
        }
        r += step;
    }
    None
}

/// Walk every `near` member outside its radius back to the nearest legal free spot by the anchor;
/// best effort, so nowhere to go is noted and merely closer is still taken.
fn enforce_near(
    parts: &mut Parts,
    groups: &[(String, Vec<String>, f64)],
    region: &Region,
    grid: f64,
    spacing: f64,
    notes: &mut Vec<String>,
) {
    for (anchor_ref, refs, radius) in groups {
        let Some(ai) = parts.index(anchor_ref) else { continue };
        let home = (parts.list[ai].x, parts.list[ai].y);
        for r in refs {
            let Some(i) = parts.index(r) else { continue };
            if !parts.list[i].movable {
                continue;
            }
            let was = (parts.list[i].x - home.0).hypot(parts.list[i].y - home.1);
            if was <= radius + 1e-6 {
                continue;
            }
            let boxes: Vec<(String, BBox)> = (0..parts.len())
                .filter(|&j| j != i)
                .map(|j| (parts.list[j].ref_.clone(), parts.list[j].bbox()))
                .collect();
            let p = &parts.list[i];
            let rots = [p.rot, p.rot + 90.0];
            let spot = free_spot(p, home, region, &boxes, grid, spacing, &rots)
                .or_else(|| free_spot(p, home, region, &boxes, grid, 0.0, &rots));
            let Some(spot) = spot else {
                notes.push(format!("{r}: no free spot within reach of {anchor_ref}"));
                continue;
            };
            let now = (spot.0 - home.0).hypot(spot.1 - home.1);
            if now >= was {
                notes.push(format!(
                    "{r} is {was:.1} mm from {anchor_ref}, nothing nearer is free"
                ));
                continue;
            }
            let p = &mut parts.list[i];
            p.x = spot.0;
            p.y = spot.1;
            p.rot = spot.2;
            if now > *radius {
                notes.push(format!(
                    "{r} seated {now:.1} mm from {anchor_ref} ({radius:.0} mm asked)"
                ));
            }
        }
    }
}

/// How far a hole centred at `c` with copper radius `r` is from legal, in mm; 0 when legal.
/// A disc, not a box, so the filleted corners a hole belongs in are not rejected.
fn hole_violation(home: &Region, c: Point, r: f64, bb: &BBox) -> f64 {
    let mut v = 0.0;
    if let Some(poly) = &home.poly {
        let d = home
            .edges
            .iter()
            .map(|(a, b)| seg_point_dist(*a, *b, c))
            .fold(f64::INFINITY, f64::min);
        v += if !point_in_polygon(c, poly) { r + d } else { (r - d).max(0.0) };
    } else {
        let b = home.bbox;
        v += (b.x0 + r - c.0)
            .max(c.0 - (b.x1 - r))
            .max(b.y0 + r - c.1)
            .max(c.1 - (b.y1 - r))
            .max(0.0);
    }
    for k in &home.no_go {
        if bb.overlaps(k) {
            v += (bb.x1.min(k.x1) - bb.x0.max(k.x0)) + (bb.y1.min(k.y1) - bb.y0.max(k.y0));
        }
    }
    v
}

/// Millimetres of courtyard this box overlaps, summed over the obstacles.
fn pen(bb: &BBox, obst: &[(String, BBox)], spacing: f64) -> f64 {
    obst.iter()
        .map(|(_, ob)| {
            let (a, b) = overlap(bb, ob, spacing);
            a.min(b)
        })
        .sum()
}

/// Radius of the hole's COPPER plus `edge_gap` -- what has to be on the board, the courtyard being
/// silkscreen that may overhang: the furthest copper any pad reaches, else drill+annulus, else size.
fn hole_radius(p: &Part, edge_gap: f64) -> f64 {
    let mut r: f64 = 0.0;
    for pad in &p.fp.pads {
        if !pad.layers.iter().any(|l| l.ends_with(".Cu")) {
            continue;
        }
        let (sx, sy) = pad.size;
        let round_pad = {
            let s = pad.shape.to_ascii_lowercase();
            s.starts_with("circle") || s.starts_with("oval")
        };
        let half = if round_pad { sx.max(sy) / 2.0 } else { sx.hypot(sy) / 2.0 };
        let off = (pad.pos.0 - p.fp.pos.0).hypot(pad.pos.1 - p.fp.pos.1);
        r = r.max(off + half);
    }
    if r > 0.0 {
        return r + edge_gap;
    }
    let drill = p
        .fp
        .pads
        .iter()
        .filter_map(|pad| pad.drill)
        .fold(f64::NEG_INFINITY, f64::max);
    if drill.is_finite() {
        return drill / 2.0 + HOLE_ANNULUS + edge_gap;
    }
    let (w, h) = p.size();
    (w.min(h) / 2.0 - HOLE_GAP).max(0.0)
}

/// Seat every mounting hole that is illegal where it stands at the legal spot furthest from the
/// board centre and the seated holes -- a hole wants an extremity of the outline, which is not one
/// of the four bbox corners. A locked or `pinned` hole never moves.
#[allow(clippy::too_many_arguments)]
fn seat_holes(
    parts: &mut Parts,
    region: &Region,
    grid: f64,
    spacing: f64,
    notes: &mut Vec<String>,
    mut unplaced: Option<&mut Vec<(String, String, Option<Pose>)>>,
    pinned: &BTreeSet<String>,
    force: &BTreeSet<String>,
    edge_gap: f64,
) {
    let home = outline_region(region);
    // a hole is on the board when its copper is, not its whole courtyard
    let viol = |p: &Part, bb: &BBox| -> f64 {
        let c = ((bb.x0 + bb.x1) / 2.0, (bb.y0 + bb.y1) / 2.0);
        hole_violation(&home, c, hole_radius(p, edge_gap), bb)
    };
    let mut holes: Vec<usize> = (0..parts.len())
        .filter(|&i| {
            let p = &parts.list[i];
            p.role == Role::Fixed
                && !p.seated
                && is_hole(&p.fp)
                && !p.fp.locked
                && !pinned.contains(&p.ref_)
                && (force.contains(&p.ref_) || viol(p, &p.bbox()) > 1e-6)
        })
        .collect();
    if holes.is_empty() {
        return;
    }
    holes.sort_by_key(|&i| parts.list[i].ref_.clone());
    let (cx, cy) = region.center();
    let mut seated: Vec<Point> = parts
        .iter()
        .filter(|p| is_hole(&p.fp) && p.seated)
        .map(|p| (p.x, p.y))
        .collect();
    // coarse sweep: a hole is small and never rotates
    let step = grid.max(1.0);
    let poses = |p: &Part| -> Vec<(f64, f64, BBox)> {
        let (n_x, n_y) = ((home.w() / step) as usize + 1, (home.h() / step) as usize + 1);
        let mut out = Vec::with_capacity((n_x + 2) * (n_y + 2));
        for i in 0..=n_x + 1 {
            for j in 0..=n_y + 1 {
                let x = snap(home.x0() + i as f64 * step, grid);
                let y = snap(home.y0() + j as f64 * step, grid);
                out.push((x, y, p.bbox_at(x, y, p.rot)));
            }
        }
        out
    };

    for hi in holes {
        let obst: Vec<(String, BBox)> = (0..parts.len())
            .filter(|&j| j != hi)
            .map(|j| (parts.list[j].ref_.clone(), parts.list[j].bbox()))
            .collect();
        // the legal spot furthest from the board centre and the seated holes, not the nearest gap
        let sweep = |p: &Part, obst: &[(String, BBox)]| -> Option<Point> {
            let mut best: Option<(f64, f64, f64)> = None;
            for (x, y, bb) in poses(p) {
                if viol(p, &bb) > 1e-6 || obst.iter().any(|(_, ob)| hits(&bb, ob, spacing)) {
                    continue;
                }
                let mut score = (x - cx).hypot(y - cy);
                if !seated.is_empty() {
                    score += 0.4
                        * seated
                            .iter()
                            .map(|(sx, sy)| (x - sx).hypot(y - sy))
                            .fold(f64::INFINITY, f64::min);
                }
                if best.map_or(true, |b| score > b.0) {
                    best = Some((score, x, y));
                }
            }
            best.map(|b| (b.1, b.2))
        };
        let mut spot = sweep(&parts.list[hi], &obst);
        if spot.is_none() {
            // parts SMALLER than the hole may be shoved aside, all of them or none
            let area = parts.list[hi].area();
            let heavy: Vec<(String, BBox)> = (0..parts.len())
                .filter(|&j| j != hi && (!parts.list[j].movable || parts.list[j].area() >= area))
                .map(|j| (parts.list[j].ref_.clone(), parts.list[j].bbox()))
                .collect();
            spot = sweep(&parts.list[hi], &heavy);
            if let Some(s) = spot {
                if !shove_for(hi, s, parts, region, grid, spacing) {
                    spot = None;
                }
            }
        }
        let Some(spot) = spot else {
            // the least-illegal spot when nothing is legal, ties going to the spot furthest out
            let p = &parts.list[hi];
            let cur = p.bbox();
            let mut best = (
                viol(p, &cur) + pen(&cur, &obst, spacing),
                -(p.x - cx).hypot(p.y - cy),
                p.x,
                p.y,
            );
            for (x, y, bb) in poses(p) {
                let cand = (
                    viol(p, &bb) + pen(&bb, &obst, spacing),
                    -(x - cx).hypot(y - cy),
                    x,
                    y,
                );
                if cand.0 < best.0 - 1e-12 || ((cand.0 - best.0).abs() <= 1e-12 && cand.1 < best.1) {
                    best = cand;
                }
            }
            let (pen_mm, x, y) = (best.0, best.2, best.3);
            let head = format!(
                "{} (mounting hole) has no legal spot inside the outline",
                p.ref_
            );
            // one note per hole, not one per pass
            notes.retain(|n| !n.starts_with(&head));
            notes.push(format!("{head}; best-effort pose ({x:.2}, {y:.2})"));
            if let Some(u) = unplaced.as_deref_mut() {
                let r = p.ref_.clone();
                u.retain(|(ur, _, _)| *ur != r);
                u.push((
                    r,
                    format!(
                        "mounting hole: no legal spot inside the outline; the best spot is still \
                         {pen_mm:.2} mm short of clearing the edge, a rule area or another courtyard"
                    ),
                    Some((round_to(x, 3), round_to(y, 3), round_to(p.rot.rem_euclid(360.0), 1))),
                ));
            }
            continue;
        };
        let p = &mut parts.list[hi];
        p.x = spot.0;
        p.y = spot.1;
        p.seated = true;
        seated.push(spot);
        // an earlier pass may have given up on this hole
        if let Some(u) = unplaced.as_deref_mut() {
            let r = parts.list[hi].ref_.clone();
            u.retain(|(ur, _, _)| *ur != r);
        }
    }
}

/// Move the parts under `hole`'s new spot out of the way, all of them or none.
fn shove_for(
    hole: usize,
    spot: Point,
    parts: &mut Parts,
    region: &Region,
    grid: f64,
    spacing: f64,
) -> bool {
    let area = parts.list[hole].area();
    let bb = parts.list[hole].bbox_at(spot.0, spot.1, parts.list[hole].rot);
    let victims: Vec<usize> = (0..parts.len())
        .filter(|&j| {
            let q = &parts.list[j];
            q.movable && j != hole && q.area() < area && hits(&bb, &q.bbox(), spacing)
        })
        .collect();
    let undo: Vec<(usize, Pose)> = victims
        .iter()
        .map(|&j| (j, (parts.list[j].x, parts.list[j].y, parts.list[j].rot)))
        .collect();
    let mut blocked: Vec<(String, BBox)> = vec![(parts.list[hole].ref_.clone(), bb)];
    blocked.extend(
        (0..parts.len())
            .filter(|&j| j != hole && !victims.contains(&j))
            .map(|j| (parts.list[j].ref_.clone(), parts.list[j].bbox())),
    );
    for &j in &victims {
        let q = &parts.list[j];
        let moved = free_spot(q, (q.x, q.y), region, &blocked, grid, spacing, &[q.rot]);
        let Some(m) = moved else {
            for (k, (x, y, rot)) in undo {
                parts.list[k].x = x;
                parts.list[k].y = y;
                parts.list[k].rot = rot;
            }
            return false;
        };
        let q = &mut parts.list[j];
        q.x = m.0;
        q.y = m.1;
        q.rot = m.2;
        let b = parts.list[j].bbox();
        blocked.push((parts.list[j].ref_.clone(), b));
    }
    true
}

fn clamp(p: &mut Part, region: &Region) {
    let bb = p.bbox();
    if bb.x0 < region.x0() {
        p.x += region.x0() - bb.x0;
    }
    if bb.x1 > region.x1() {
        p.x -= bb.x1 - region.x1();
    }
    if bb.y0 < region.y0() {
        p.y += region.y0() - bb.y0;
    }
    if bb.y1 > region.y1() {
        p.y -= bb.y1 - region.y1();
    }
}

fn count_overlaps(parts: &Parts) -> usize {
    let boxes: Vec<BBox> = parts.iter().map(|p| p.bbox()).collect();
    let mut n = 0;
    for i in 0..parts.len() {
        for j in i + 1..parts.len() {
            if (parts.list[i].movable || parts.list[j].movable) && hits(&boxes[i], &boxes[j], 0.0) {
                n += 1;
            }
        }
    }
    n
}

// --- HPWL local search -----------------------------------------------------------------

/// Movable parts grouped into the units the local search moves as one: a part plus its satellites
/// (one whose parent cannot move is its own family).
fn families(parts: &Parts, skip: &BTreeSet<String>) -> BTreeMap<String, Vec<String>> {
    let mut heads: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for p in parts.iter() {
        if !p.movable || skip.contains(&p.ref_) {
            continue;
        }
        let mut head = p.ref_.clone();
        if p.role == Role::Satellite {
            if let Some(par) = p.parent.as_ref().and_then(|r| parts.get(r)) {
                if par.movable && !skip.contains(&par.ref_) {
                    head = par.ref_.clone();
                }
            }
        }
        heads.entry(head).or_default().insert(p.ref_.clone());
    }
    heads
        .into_iter()
        .map(|(h, m)| (h, m.into_iter().collect()))
        .collect()
}

fn median(vals: &mut Vec<f64>) -> f64 {
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    vals[vals.len() / 2]
}

struct Refine<'a> {
    parts: &'a mut Parts,
    region: &'a Region,
    grid: f64,
    spacing: f64,
    step: f64,
    fams: BTreeMap<String, Vec<String>>,
    head_of: BTreeMap<String, String>,
    net_members: BTreeMap<i64, Vec<(String, usize)>>,
    fam_nets: BTreeMap<String, Vec<i64>>,
    nets_of: BTreeMap<String, BTreeSet<i64>>,
    cur: BTreeMap<i64, f64>,
    boxes: BTreeMap<String, BBox>,
}

impl<'a> Refine<'a> {
    fn nets_for(&self, members: &[String]) -> Vec<i64> {
        let ms: BTreeSet<&String> = members.iter().collect();
        let mut out: BTreeSet<i64> = BTreeSet::new();
        for r in members {
            for n in self.nets_of.get(r).into_iter().flatten() {
                if self.net_members[n].iter().any(|(r2, _)| !ms.contains(r2)) {
                    out.insert(*n);
                }
            }
        }
        out.into_iter().collect()
    }

    fn pad_pos(&self, ref_: &str, i: usize, ov: Option<&Override>) -> Point {
        let p = self.parts.get(ref_).unwrap();
        let (x, y, rot) = ov
            .and_then(|o| o.get(ref_).copied())
            .unwrap_or((p.x, p.y, p.rot));
        let d = rotate(p.pads[i].2, rot);
        (x + d.0, y + d.1)
    }

    fn net_val(&self, net: i64, ov: Option<&Override>) -> f64 {
        let mut b = BBox::empty();
        for (r, i) in &self.net_members[&net] {
            b.add_point(self.pad_pos(r, *i, ov));
        }
        b.w() + b.h()
    }

    fn delta(&self, nets: &[i64], ov: &Override) -> f64 {
        nets.iter()
            .map(|n| self.net_val(*n, Some(ov)) - self.cur[n])
            .sum()
    }

    /// The parts the proposed poses land on; `None` if the move is impossible at all.
    fn collide(&self, ov: &Override) -> Option<BTreeSet<String>> {
        let mut hit: BTreeSet<String> = BTreeSet::new();
        let newb: Vec<(String, BBox)> = ov
            .iter()
            .map(|(r, pose)| (r.clone(), self.parts.get(r).unwrap().bbox_at(pose.0, pose.1, pose.2)))
            .collect();
        for (r, bb) in &newb {
            if !self.region.accepts(bb) {
                return None;
            }
            for (r2, ob) in &self.boxes {
                if ov.contains_key(r2) || !hits(bb, ob, self.spacing) {
                    continue;
                }
                if !self.head_of.contains_key(r2) {
                    return None;
                }
                hit.insert(r2.clone());
            }
            let _ = r;
        }
        for i in 0..newb.len() {
            for j in i + 1..newb.len() {
                if hits(&newb[i].1, &newb[j].1, self.spacing) {
                    return None;
                }
            }
        }
        Some(hit)
    }

    fn shift(&self, members: &[String], tx: f64, ty: f64) -> Override {
        members
            .iter()
            .map(|r| {
                let p = self.parts.get(r).unwrap();
                (r.clone(), (p.x + tx, p.y + ty, p.rot))
            })
            .collect()
    }

    fn fam_bbox(&self, members: &[String]) -> BBox {
        let mut bb = BBox::empty();
        for r in members {
            if let Some(b) = self.boxes.get(r) {
                bb.add_bbox(b);
            }
        }
        bb
    }

    /// Ring-scan for the nearest rigid translation putting `members` near `target`, inside the
    /// region and clear of `blocked` and of every part not in `busy`. `reach` is short on purpose.
    fn free_shift(
        &self,
        members: &[String],
        target: Point,
        blocked: &BTreeMap<String, BBox>,
        busy: &BTreeSet<String>,
        reach: f64,
    ) -> Option<Override> {
        let fb = self.fam_bbox(members);
        let c = fb.center();
        let span = fb.w().max(fb.h());
        let reach = if reach > 0.0 { reach } else { (1.5 * span + 2.0).min(10.0) };
        // only boxes that could touch a candidate pose; the bound must stay generous
        let (hx, hy) = (
            fb.w() / 2.0 + reach + self.spacing,
            fb.h() / 2.0 + reach + self.spacing,
        );
        let mut obst: Vec<BBox> = self
            .boxes
            .iter()
            .filter(|(r, b)| {
                !busy.contains(*r)
                    && (b.center().0 - target.0).abs() < hx + b.w() / 2.0 + self.grid
                    && (b.center().1 - target.1).abs() < hy + b.h() / 2.0 + self.grid
            })
            .map(|(_, b)| *b)
            .collect();
        obst.extend(blocked.values().copied());
        let mut r = 0.0;
        while r <= reach {
            let n = if r > 0.0 { ((2.0 * PI * r / self.step) as usize).max(1) } else { 1 };
            for k in 0..n {
                let a = 2.0 * PI * k as f64 / n as f64;
                let tx = snap(target.0 + r * a.cos() - c.0, self.grid);
                let ty = snap(target.1 + r * a.sin() - c.1, self.grid);
                let ov = self.shift(members, tx, ty);
                let ok = ov.iter().all(|(r, pose)| {
                    let bb = self.parts.get(r).unwrap().bbox_at(pose.0, pose.1, pose.2);
                    self.region.accepts(&bb) && !obst.iter().any(|o| hits(&bb, o, self.spacing))
                });
                if ok {
                    return Some(ov);
                }
            }
            r += self.step;
        }
        None
    }

    /// `ov` plus re-insertions for whatever it lands on, and the nets it disturbs; `None` if the
    /// pile-up cannot be resolved.
    fn ripple(&self, mut ov: Override, mut nets: BTreeSet<i64>) -> Option<(Override, BTreeSet<i64>)> {
        let mut busy: BTreeSet<String> = ov.keys().cloned().collect();
        let mut depth = 2usize;
        for _ in 0..depth + 1 {
            let hit = self.collide(&ov)?;
            if hit.is_empty() {
                return Some((ov, nets));
            }
            // heads, not the parts hit: shoving a family rigidly would drag its moved member back
            let heads: BTreeSet<String> = hit.iter().map(|r| self.head_of[r].clone()).collect();
            if heads.len() > depth
                || heads
                    .iter()
                    .any(|h| self.fams[h].iter().any(|m| busy.contains(m)))
            {
                return None;
            }
            let mut placed: BTreeMap<String, BBox> = ov
                .iter()
                .map(|(r, pose)| {
                    (r.clone(), self.parts.get(r).unwrap().bbox_at(pose.0, pose.1, pose.2))
                })
                .collect();
            for h in &heads {
                let members = self.fams[h].clone();
                busy.extend(members.iter().cloned());
                let spot = self.free_shift(&members, self.fam_bbox(&members).center(), &placed, &busy, 0.0)?;
                for (r, pose) in &spot {
                    placed.insert(
                        r.clone(),
                        self.parts.get(r).unwrap().bbox_at(pose.0, pose.1, pose.2),
                    );
                }
                ov.extend(spot);
                nets.extend(self.fam_nets[h].iter().copied());
            }
            // one round of displacement only; the next collide() must come back clean
            depth = 0;
        }
        None
    }

    fn commit(&mut self, ov: &Override, nets: &BTreeSet<i64>) {
        for (r, (x, y, rot)) in ov {
            let p = self.parts.get_mut(r).unwrap();
            p.x = *x;
            p.y = *y;
            p.rot = rot.rem_euclid(360.0);
            let b = p.bbox();
            self.boxes.insert(r.clone(), b);
        }
        for n in nets {
            let v = self.net_val(*n, None);
            self.cur.insert(*n, v);
        }
    }

    /// Try the move with displacement; keep it only if the disturbed nets still get shorter.
    fn accept(&mut self, ov: &Override, nets: &BTreeSet<i64>) -> bool {
        let Some((ov, nn)) = self.ripple(ov.clone(), nets.clone()) else {
            return false;
        };
        let keys: Vec<i64> = nn.iter().copied().collect();
        if self.delta(&keys, &ov) >= -1e-6 {
            return false;
        }
        self.commit(&ov, &nn);
        true
    }

    fn optimum(&self, members: &[String]) -> Option<(f64, f64)> {
        let ms: BTreeSet<&String> = members.iter().collect();
        let (mut exs, mut eys) = (Vec::new(), Vec::new());
        for net in self.nets_for(members) {
            let mut ob = BBox::empty();
            let mut mine = Vec::new();
            for (r, i) in &self.net_members[&net] {
                if ms.contains(r) {
                    mine.push((r.clone(), *i));
                } else {
                    ob.add_point(self.pad_pos(r, *i, None));
                }
            }
            if !ob.valid() {
                continue;
            }
            for (r, i) in mine {
                let (px, py) = self.pad_pos(&r, i, None);
                exs.push(ob.x0 - px);
                exs.push(ob.x1 - px);
                eys.push(ob.y0 - py);
                eys.push(ob.y1 - py);
            }
        }
        if exs.is_empty() {
            return None;
        }
        Some((median(&mut exs), median(&mut eys)))
    }

    fn try_relocate(&mut self, members: &[String]) -> bool {
        let nets: BTreeSet<i64> = self.nets_for(members).into_iter().collect();
        if nets.is_empty() {
            return false;
        }
        let Some((tx, ty)) = self.optimum(members) else {
            return false;
        };
        let mut cands: BTreeSet<(u64, u64)> = BTreeSet::new();
        let key = |v: f64| v.to_bits();
        let mut raw: Vec<(f64, f64)> = vec![
            (snap(tx, self.grid), snap(ty, self.grid)),
            (snap(tx, self.grid), 0.0),
            (0.0, snap(ty, self.grid)),
        ];
        for k in 1..=3 {
            for dx in [-(k as f64) * self.step, 0.0, k as f64 * self.step] {
                for dy in [-(k as f64) * self.step, 0.0, k as f64 * self.step] {
                    raw.push((snap(tx + dx, self.grid), snap(ty + dy, self.grid)));
                }
            }
        }
        // a total key: equal-distance candidates would otherwise be tried in the set's own order
        let mut ranked: Vec<(f64, f64)> = raw
            .into_iter()
            .filter(|c| (c.0.abs() > 1e-9 || c.1.abs() > 1e-9) && cands.insert((key(c.0), key(c.1))))
            .collect();
        ranked.sort_by(|a, b| {
            let da = (a.0 - tx).powi(2) + (a.1 - ty).powi(2);
            let db = (b.0 - tx).powi(2) + (b.1 - ty).powi(2);
            da.partial_cmp(&db)
                .unwrap()
                .then(a.0.partial_cmp(&b.0).unwrap())
                .then(a.1.partial_cmp(&b.1).unwrap())
        });
        // a two-pad part is half its own net length, so turning it can beat moving it
        let lone = (members.len() == 1
            && self.parts.get(&members[0]).unwrap().pads.len() <= 2)
            .then(|| members[0].clone());
        let rots: Vec<f64> = if lone.is_some() {
            vec![0.0, 90.0, 180.0, 270.0]
        } else {
            vec![0.0]
        };
        let keys: Vec<i64> = nets.iter().copied().collect();
        let mut blocked: Vec<(f64, Override)> = Vec::new();
        for (tdx, tdy) in &ranked {
            for dr in &rots {
                let mut ov = self.shift(members, *tdx, *tdy);
                if *dr != 0.0 {
                    let lr = lone.clone().unwrap();
                    let (x, y, r0) = ov[&lr];
                    ov = BTreeMap::from([(lr, (x, y, (r0 + dr).rem_euclid(360.0)))]);
                }
                let d = self.delta(&keys, &ov);
                if d >= -1e-6 {
                    continue;
                }
                let Some(hit) = self.collide(&ov) else { continue };
                if hit.is_empty() {
                    // nothing in the way: take it
                    self.commit(&ov, &nets);
                    return true;
                }
                blocked.push((d, ov));
            }
        }
        // only the best two: each displaced family costs a ring scan
        blocked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        for (_, ov) in blocked.into_iter().take(2) {
            if self.accept(&ov, &nets) {
                return true;
            }
        }
        // last resort: the optimum and its surroundings are taken, so take the nearest hole to it
        let c = self.fam_bbox(members).center();
        let ov = self.free_shift(
            members,
            (c.0 + tx, c.1 + ty),
            &BTreeMap::new(),
            &members.iter().cloned().collect(),
            REFINE_REACH,
        );
        if let Some(ov) = ov {
            if self.delta(&keys, &ov) < -1e-6 && self.collide(&ov).map_or(false, |h| h.is_empty()) {
                self.commit(&ov, &nets);
                return true;
            }
        }
        false
    }

    fn try_swap(&mut self, ma: &[String], mb: &[String]) -> bool {
        let mut nets: BTreeSet<i64> = self.nets_for(ma).into_iter().collect();
        nets.extend(self.nets_for(mb));
        if nets.is_empty() {
            return false;
        }
        let (ca, cb) = (self.fam_bbox(ma).center(), self.fam_bbox(mb).center());
        let mut ov = self.shift(ma, snap(cb.0 - ca.0, self.grid), snap(cb.1 - ca.1, self.grid));
        ov.extend(self.shift(mb, snap(ca.0 - cb.0, self.grid), snap(ca.1 - cb.1, self.grid)));
        let keys: Vec<i64> = nets.iter().copied().collect();
        if self.delta(&keys, &ov) >= -1e-6 {
            return false;
        }
        self.accept(&ov, &nets)
    }
}

/// Iterated local search on the real HPWL, after legalisation: move whole families to the analytic
/// optimum of the nets they touch and swap pairs of them, both *rippling* whatever they land on to
/// the nearest free spot, kept only when the disturbed nets get shorter. Returns mm.
fn refine(
    parts: &mut Parts,
    region: &Region,
    grid: f64,
    spacing: f64,
    skip: &BTreeSet<String>,
    rng: &mut Rng,
    notes: &mut Vec<String>,
) -> f64 {
    let t0 = std::time::Instant::now();
    let fams = families(parts, skip);
    if fams.len() < 2 {
        return 0.0;
    }
    let head_of: BTreeMap<String, String> = fams
        .iter()
        .flat_map(|(h, m)| m.iter().map(move |r| (r.clone(), h.clone())))
        .collect();
    let mut net_members: BTreeMap<i64, Vec<(String, usize)>> = BTreeMap::new();
    for p in parts.iter() {
        for (i, (_, net, _)) in p.pads.iter().enumerate() {
            if *net != 0 {
                net_members.entry(*net).or_default().push((p.ref_.clone(), i));
            }
        }
    }
    net_members.retain(|_, m| m.len() > 1);
    let mut nets_of: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
    for (net, members) in &net_members {
        for (r, _) in members {
            nets_of.entry(r.clone()).or_default().insert(*net);
        }
    }
    let n_parts = parts.len();
    let mut r = Refine {
        parts,
        region,
        grid,
        spacing,
        step: grid.max(0.75),
        fams: fams.clone(),
        head_of,
        net_members,
        fam_nets: BTreeMap::new(),
        nets_of,
        cur: BTreeMap::new(),
        boxes: BTreeMap::new(),
    };
    r.fam_nets = fams
        .iter()
        .map(|(h, m)| (h.clone(), r.nets_for(m)))
        .collect();
    r.cur = r
        .net_members
        .keys()
        .copied()
        .collect::<Vec<i64>>()
        .into_iter()
        .map(|n| (n, r.net_val(n, None)))
        .collect();
    r.boxes = r.parts.iter().map(|p| (p.ref_.clone(), p.bbox())).collect();
    let start: f64 = r.cur.values().sum();

    // a unit is a whole family or one part on its own; families first, so blocks move first
    let mut units: Vec<Vec<String>> = fams.values().cloned().collect();
    units.extend(
        fams.values()
            .filter(|m| m.len() > 1)
            .flat_map(|m| m.iter().map(|x| vec![x.clone()])),
    );

    // stops on a count, never the clock, so a board and rng give the same moves in the same order
    let cap = (3 * units.len()).max(REFINE_PROBES / n_parts.max(1));
    let mut probes = 0usize;
    let mut hit_ceiling = false;
    'descend: for _ in 0..REFINE_SWEEPS {
        let mut improved = false;
        let mut order: Vec<usize> = (0..units.len()).collect();
        rng.shuffle(&mut order);
        for u in &order {
            if probes >= cap {
                break 'descend;
            }
            if t0.elapsed().as_secs_f64() > REFINE_CEILING_S {
                hit_ceiling = true;
                break 'descend;
            }
            probes += 1;
            if r.try_relocate(&units[*u]) {
                improved = true;
            }
        }
        for _ in 0..2 * units.len() {
            if probes >= cap {
                break 'descend;
            }
            if t0.elapsed().as_secs_f64() > REFINE_CEILING_S {
                hit_ceiling = true;
                break 'descend;
            }
            probes += 1;
            let a = rng.usize(0..units.len());
            let mut b = rng.usize(0..units.len() - 1);
            if b >= a {
                b += 1;
            }
            let (ua, ub) = (units[a].clone(), units[b].clone());
            if !ua.iter().any(|x| ub.contains(x)) && r.try_swap(&ua, &ub) {
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
    if hit_ceiling {
        notes.push(format!(
            "local search stopped at the {REFINE_CEILING_S:.0}s safety ceiling after {probes} of \
             {cap} probes, so this plan is NOT reproducible from its seed"
        ));
    }
    start - r.cur.values().sum::<f64>()
}
