//! `place::idioms` — idiom align passes and anchor-block helpers for the search.

use std::collections::{BTreeMap, BTreeSet};

use kicad_symbol::geometry::SymbolGeometry;

use crate::topology::anchor_tap;
use circuit_graph::netclass::{is_ground, is_power_net};
use crate::ir::{LayoutIr, Orient};
use crate::geometry::GRID_KEY;
use crate::item::{Incidence, Item};

/// each load cap two gaps out, level with its osc pin. Returns true if it moved
/// anything (so the caller re-runs `decongest`). The cluster members are frozen, so
/// the placement search has already finished around them and won't reverse this.
pub fn align_idiom_clusters(items: &mut [Item], ir: &LayoutIr) -> bool {
    const GAP: f64 = 7.62;
    let snap = |v| geom::GRID_50_MIL.snap(v);
    // (item, position, optional forced angle). The crystal gets a forced angle so its pins
    // run TOWARD the IC (along `dir`); the anneal otherwise leaves it on the perpendicular
    // axis, which forces both 3-pin OSC nets to wrap around the body → route-fail → a bridging
    // net-label overlapping the crystal (the recurring MCU-sheet defect). caps keep their angle.
    let mut moves: Vec<(usize, [f64; 2], Option<f64>)> = Vec::new();
    for idiom in &ir.idioms {
        if idiom.kind != "crystal" {
            continue;
        }
        let in_idiom = |it: &Item| idiom.parts.contains(&it.refdes);
        let Some(ai) = items.iter().position(|it| it.refdes == idiom.anchor) else {
            continue;
        };
        let Some(yi) = items.iter().position(|it| {
            in_idiom(it) && (it.part.contains("Crystal") || it.part.contains("Resonator"))
        }) else {
            continue;
        };
        let y_refdes = items[yi].refdes.clone();
        let onets: Vec<String> = items[yi]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.clone())
            .collect();
        if onets.len() != 2 {
            continue;
        }
        // The IC's pin-tip world position for an osc net.
        let osc_world = |net: &str| -> Option<[f64; 2]> {
            let num = items[ai]
                .pins
                .iter()
                .find(|(_, _, n)| n.as_deref() == Some(net))?
                .0
                .clone();
            let pg = items[ai].geom.pins.iter().find(|p| p.number == num)?;
            Some(crate::geometry::pin_endpoint(
                pg,
                items[ai].at,
                items[ai].angle,
                items[ai].mirror,
            ))
        };
        let (Some(wa), Some(wb)) = (osc_world(&onets[0]), osc_world(&onets[1])) else {
            continue;
        };
        let mid = [(wa[0] + wb[0]) / 2.0, (wa[1] + wb[1]) / 2.0];
        // Outward direction = the IC EDGE the osc pins hug (NOT the dominant axis of
        // mid−centre: a corner osc pin is more vertically than horizontally offset yet
        // still sticks out the SIDE). Classify by nearest edge of the IC's pin bbox.
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for pg in &items[ai].geom.pins {
            let w = crate::geometry::pin_endpoint(pg, items[ai].at, items[ai].angle, items[ai].mirror);
            lo[0] = lo[0].min(w[0]);
            lo[1] = lo[1].min(w[1]);
            hi[0] = hi[0].max(w[0]);
            hi[1] = hi[1].max(w[1]);
        }
        let (dl, dr, dt, db) = (
            mid[0] - lo[0],
            hi[0] - mid[0],
            mid[1] - lo[1],
            hi[1] - mid[1],
        );
        let m = dl.min(dr).min(dt).min(db);
        let dir: [f64; 2] = if m == dl {
            [-1.0, 0.0]
        } else if m == dr {
            [1.0, 0.0]
        } else if m == dt {
            [0.0, -1.0]
        } else {
            [0.0, 1.0]
        };
        // Unit vector perpendicular to `dir` (the edge the cluster runs ALONG).
        let perp = [dir[1].abs(), dir[0].abs()];
        // The crystal sits one gap out, centred between the two oscillator pins, oriented so its
        // pin1→pin2 axis runs along `dir` (toward/away the IC). With pins along dir, each OSC node
        // {IC pin, Y1 pin, cap} is a small local cluster with Y1's BODY outside it, so route_local_tee
        // wires it cleanly — vs the anneal's perpendicular angle, where the body sits inside each
        // node's bbox and the route wraps + fails. (Empirically: dir-aligned → 0 warnings/crossings;
        // perpendicular → 3 warnings incl. the OSC bridge label.)
        let dir_orient = if dir[0] < 0.0 {
            Orient::Left
        } else if dir[0] > 0.0 {
            Orient::Right
        } else if dir[1] < 0.0 {
            Orient::Up
        } else {
            Orient::Down
        };
        let cry_angle = orient_angle(&items[yi].geom, dir_orient);
        moves.push((
            yi,
            [snap(mid[0] + dir[0] * GAP), snap(mid[1] + dir[1] * GAP)],
            Some(cry_angle),
        ));
        // Each load cap sits two gaps out and TWO gaps to its osc pin's side of the midpoint
        // (perp axis). The pins are one 2.54 mm pitch apart but a cap is ~7.6 mm tall, so the
        // caps must clear both the pin rows AND each other; one gap off-centre left them packed
        // tight against Y1, so the crystal's own ref/value text ("Y1"/"25 MHz") collided with the
        // caps' GND symbols (the critic's "garbled stacked GND/25 MHz" — a MAJOR text-overlap).
        // Two gaps off-centre (perp only, so the dir-aligned OSC routing is unaffected) gives the
        // cluster room for its labels while keeping the load-cap leads short.
        for (net, w) in [(&onets[0], wa), (&onets[1], wb)] {
            if let Some(ci) = items.iter().position(|it| {
                in_idiom(it)
                    && it.refdes != y_refdes
                    && it
                        .pins
                        .iter()
                        .any(|(_, _, n)| n.as_deref() == Some(net.as_str()))
            }) {
                // Which side of the midpoint this osc pin lies on, along the edge.
                let side = if dir[0] != 0.0 {
                    (w[1] - mid[1]).signum()
                } else {
                    (w[0] - mid[0]).signum()
                };
                let side = if side == 0.0 { 1.0 } else { side };
                moves.push((
                    ci,
                    [
                        snap(mid[0] + dir[0] * GAP * 2.0 + perp[0] * side * GAP * 2.0),
                        snap(mid[1] + dir[1] * GAP * 2.0 + perp[1] * side * GAP * 2.0),
                    ],
                    None,
                ));
            }
        }
    }
    let moved = !moves.is_empty();
    for (i, at, ang) in moves {
        items[i].at = at.into();
        if let Some(a) = ang {
            items[i].angle = a;
        }
    }
    moved
}

/// Snap each recognized LED indicator's series resistor into a clean vertical leg
/// directly below the LED (a GPIO → LED → R → GND drop), instead of letting it sit in
/// a spare column with a long node wire back to the LED. Runs on FINAL mm positions in
/// `emit` (after the search has seated the LED), so it can't collide a frozen cluster;
/// the caller re-runs `decongest` to nudge anything the moved resistor now overlaps.
/// Returns true if it moved anything.
pub fn align_led_chains(items: &mut [Item], _inc: &Incidence, ir: &LayoutIr) -> bool {
    const DROP: f64 = 10.16; // LED half + gap + resistor half, on grid.
    let snap = |v| geom::GRID_50_MIL.snap(v);
    let mut moves: Vec<(usize, [f64; 2], f64)> = Vec::new();
    for idiom in &ir.idioms {
        if idiom.kind != "led_indicator" {
            continue;
        }
        let Some(li) = items.iter().position(|it| it.refdes == idiom.anchor) else {
            continue;
        };
        let Some(res_rd) = idiom.parts.first() else {
            continue;
        };
        let Some(ri) = items.iter().position(|it| &it.refdes == res_rd) else {
            continue;
        };
        // The node the LED and resistor share (the LED cathode → resistor top).
        let led_nets: Vec<String> = items[li]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.clone())
            .collect();
        let Some(shared) = items[ri]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.clone())
            .find(|n| led_nets.contains(n))
        else {
            continue;
        };
        // Re-orient the LED VERTICAL too — cathode (the shared node) DOWN toward the
        // resistor, anode UP toward the driving pin — so the LED and resistor read as one
        // collinear series string rather than an L-bend (a horizontal LED over a vertical
        // resistor). Keep the LED's position; only its angle changes.
        let l_pin1 = items[li].pins.first().and_then(|p| p.2.clone());
        let l_orient = if l_pin1.as_deref() == Some(shared.as_str()) {
            Orient::Up
        } else {
            Orient::Down
        };
        let l_angle = orient_angle(&items[li].geom, l_orient);
        moves.push((li, items[li].at.into(), l_angle));
        let at = [snap(items[li].at[0]), snap(items[li].at[1] + DROP)];
        // Vertical, shared (cathode) pin UP toward the LED above, GND pin DOWN.
        let r_pin1 = items[ri].pins.first().and_then(|p| p.2.clone());
        let orient = if r_pin1.as_deref() == Some(shared.as_str()) {
            Orient::Down
        } else {
            Orient::Up
        };
        let angle = orient_angle(&items[ri].geom, orient);
        moves.push((ri, at, angle));
    }
    let moved = !moves.is_empty();
    for (i, at, angle) in moves {
        items[i].at = at.into();
        items[i].angle = angle;
    }
    moved
}

/// Align stray BULK rail caps into a tidy row on power-only sheets.
pub fn align_rail_cap_rows(items: &mut [Item], ir: &LayoutIr) -> bool {
    let is_rail = |n: &str| is_power_net(n) || ir.rails.contains_key(n);
    let snap = |v| geom::GRID_50_MIL.snap(v);
    const PITCH: f64 = 12.7;
    let mut groups: std::collections::BTreeMap<(String, String), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.frozen || !it.refdes.starts_with('C') || it.geom.pins.len() != 2 {
            continue;
        }
        let nets: Vec<String> = it.pins.iter().filter_map(|(_, _, n)| n.clone()).collect();
        if nets.len() != 2 || !nets.iter().all(|n| is_rail(n)) {
            continue;
        }
        let mut np = [nets[0].clone(), nets[1].clone()];
        np.sort();
        groups
            .entry((np[0].clone(), np[1].clone()))
            .or_default()
            .push(i);
    }
    let mut moves: Vec<(usize, [f64; 2])> = Vec::new();
    for idxs in groups.values() {
        if idxs.len() < 2 {
            continue;
        }
        let row_y = idxs
            .iter()
            .map(|&i| items[i].at[1])
            .fold(f64::MAX, f64::min);
        if idxs
            .iter()
            .all(|&i| (items[i].at[1] - row_y).abs() < GRID_KEY)
        {
            continue;
        }
        let mut sorted = idxs.clone();
        sorted.sort_by(|&a, &b| items[a].at[0].total_cmp(&items[b].at[0]));
        let x0 = items[sorted[0]].at[0];
        for (k, &i) in sorted.iter().enumerate() {
            moves.push((i, [snap(x0 + k as f64 * PITCH), snap(row_y)]));
        }
    }
    let moved = !moves.is_empty();
    for (i, at) in moves {
        items[i].at = at.into();
    }
    moved
}

/// Rotation (degrees) so a 2-pin part's pin1→pin2 axis matches `orient`.
pub fn orient_angle(geom: &SymbolGeometry, orient: Orient) -> f64 {
    if geom.pins.len() != 2 {
        return 0.0;
    }
    let pin = |n: &str| geom.pins.iter().find(|p| p.number == n);
    let (p1, p2) = match (pin("1"), pin("2")) {
        (Some(a), Some(b)) => (a, b),
        _ => (&geom.pins[0], &geom.pins[1]),
    };
    let (dx, dy) = (p2.at[0] - p1.at[0], p2.at[1] - p1.at[1]);
    let want = match orient {
        Orient::Right => (1.0, 0.0),
        Orient::Left => (-1.0, 0.0),
        Orient::Down => (0.0, 1.0),
        Orient::Up => (0.0, -1.0),
    };
    for deg in [0.0_f64, 90.0, 180.0, 270.0] {
        let (s, c) = deg.to_radians().sin_cos();
        let (sx, sy) = (dx * c - dy * s, -(dx * s + dy * c));
        let card = if sx.abs() >= sy.abs() {
            (sx.signum(), 0.0)
        } else {
            (0.0, sy.signum())
        };
        if (card.0 - want.0).abs() < 0.5 && (card.1 - want.1).abs() < 0.5 {
            return deg;
        }
    }
    0.0
}


/// Each anchor's cluster: the satellites that tap it + the idiom members it anchors,
/// the rigid group the block move slides. Built once; includes frozen members so a
/// recognized cluster travels intact.
pub fn build_anchor_blocks(
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    sats: &[usize],
    ir: &LayoutIr,
) -> BTreeMap<usize, Vec<usize>> {
    let mut blocks: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &si in sats {
        if let Some((ai, _, _)) = anchor_tap(items, inc, anchors, si, &ir.rails) {
            blocks.entry(ai).or_default().push(si);
        }
    }
    for idiom in &ir.idioms {
        if let Some(ai) = items.iter().position(|it| it.refdes == idiom.anchor) {
            for part in &idiom.parts {
                if let Some(mi) = items.iter().position(|it| &it.refdes == part) {
                    blocks.entry(ai).or_default().push(mi);
                }
            }
        }
    }
    for v in blocks.values_mut() {
        v.sort_unstable();
        v.dedup();
    }
    blocks
}

/// Number of sibling units below which a multi-unit symbol is NOT bound rigidly.
const MULTI_UNIT_RIGID_MIN: usize = 3;

/// Per-unit symbol-pin count above which a multi-unit symbol counts as genuinely BANKED.
/// A dual/quad op-amp's units are tiny (an MCP6002 unit's symbol has 8 pins) and the
/// existing per-anchor path already places them well, so binding them rigidly only
/// perturbs a tuned layout (measured: it traded op-amp crossings the wrong way). A
/// banked IC's units are large (an iCE40 bank's symbol has ~120 pins); only those sprawl
/// and benefit from travelling as one block. 32 cleanly separates the two regimes.
const MULTI_UNIT_BANKED_PINS: usize = 32;

/// Map each anchor of a heavily-BANKED multi-unit symbol to its sibling anchor units:
/// same refdes, ≥[`MULTI_UNIT_RIGID_MIN`] units, each unit a LARGE body
/// (≥[`MULTI_UNIT_BANKED_PINS`] symbol pins). The banks of one such symbol (an FPGA's
/// U3A..U3E) are SEPARATE anchors sharing no signal nets, so the per-anchor cluster jump
/// carries each bank alone and they drift to opposite edges — the BGA sprawl. The jump
/// consults this so it can carry the whole symbol rigidly, keeping the seeded adjacent
/// vertical stack together. Empty for single-unit and small multi-unit refdes (op-amps,
/// every other board), so the jump degrades to the old single-anchor move there.
pub fn multi_unit_siblings(items: &[Item], anchors: &[usize]) -> BTreeMap<usize, Vec<usize>> {
    let mut by_refdes: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for &i in anchors {
        by_refdes
            .entry(items[i].refdes.as_str())
            .or_default()
            .push(i);
    }
    let banked = |g: &[usize]| {
        g.len() >= MULTI_UNIT_RIGID_MIN
            && g.iter()
                .all(|&i| items[i].geom.pins.len() >= MULTI_UNIT_BANKED_PINS)
    };
    let mut m: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for group in by_refdes.values().filter(|g| banked(g)) {
        for &i in group {
            m.insert(i, group.iter().copied().filter(|&j| j != i).collect());
        }
    }
    m
}

/// The rigid set a cluster jump on anchor `i` carries: `i`, its satellite block, and —
/// for a banked multi-unit symbol — every sibling unit plus each sibling's own block, so
/// the whole symbol slides as one (deduped, sorted).
pub fn cluster_group(
    i: usize,
    blocks: &BTreeMap<usize, Vec<usize>>,
    siblings: &BTreeMap<usize, Vec<usize>>,
) -> Vec<usize> {
    let mut group = vec![i];
    if let Some(b) = blocks.get(&i) {
        group.extend(b.iter().copied());
    }
    if let Some(sibs) = siblings.get(&i) {
        for &s in sibs {
            group.push(s);
            if let Some(b) = blocks.get(&s) {
                group.extend(b.iter().copied());
            }
        }
    }
    group.sort_unstable();
    group.dedup();
    group
}

/// For each satellite, the anchor PINS it should hug: its SIGNAL-net anchor pins
/// (a pull-up belongs by the pin it pulls, not by the rail), falling back to its
/// rail-net anchor pins only when it touches no signal anchor (a decoupling cap on
/// two rails → the IC supply pin). Returned as `(sat_idx, [(anchor_idx, pin_geom_idx)])`
/// so [`proxy_cost`] can recompute the live pin world position each move WITHOUT the
/// router. This is the cheap mirror of [`count_stray`]: the true cost knows a satellite
/// belongs by its pin, but the proxy did not — so the locality anneal never proposed
/// pulling a far-flung pull-up (R1 on NRST + a wide V+ rail, whose net bbox dilutes the
/// HPWL gradient) back to its pin. Precomputed once: which pins a part taps is fixed;
/// only positions move.
/// Manhattan distance from `from` to anchor `j`'s pin `pgi` in world coords (the
/// pin's live position under the anchor's placement). Used to pick the nearest
/// supply pin for a decoupling cap's cohesion target.
fn pin_world_dist(
    items: &[Item],
    j: usize,
    pgi: usize,
    from: impl Into<::geom::Point2>,
) -> f64 {
    let from = from.into();
    let p = crate::geometry::pin_endpoint(
        &items[j].geom.pins[pgi],
        items[j].at,
        items[j].angle,
        items[j].mirror,
    );
    from.manhattan(p.into())
}

pub fn cohesion_targets(
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
) -> Vec<(usize, Vec<(usize, usize)>)> {
    type PinLocations = Vec<(usize, usize)>;

    let is_anchor = |i: usize| items[i].geom.pins.len() >= 3;
    // A BANKED multi-unit IC (FPGA) is the dominant consumer of its rails, but it shares those
    // rails with the regulators that feed it — so "nearest supply pin" parks the FPGA's own
    // decoupling bank beside a regulator (the "decoupling on the opposite edge" defect). Bias a
    // pure bypass cap that reaches a banked IC toward the IC's own supply pins. Empty (no-op) on
    // every board without a banked symbol, so single-IC layouts are untouched.
    let anchor_idxs: Vec<usize> = (0..items.len()).filter(|&i| is_anchor(i)).collect();
    let banked: BTreeSet<usize> = multi_unit_siblings(items, &anchor_idxs)
        .into_keys()
        .collect();
    let mut out = Vec::new();
    for si in 0..items.len() {
        if items[si].geom.pins.len() >= 3 || items[si].frozen {
            continue;
        }
        let (mut sig, mut supply, mut gnd): (PinLocations, PinLocations, PinLocations) =
            (Vec::new(), Vec::new(), Vec::new());
        for (_, _, net) in &items[si].pins {
            let Some(net) = net else { continue };
            let is_rail = ir.rails.contains_key(net);
            for (j, num) in inc.get(net).into_iter().flatten() {
                if !is_anchor(*j) {
                    continue;
                }
                if let Some(pgi) = items[*j].geom.pins.iter().position(|p| &p.number == num) {
                    if !is_rail {
                        sig.push((*j, pgi));
                    } else if is_ground(net) {
                        gnd.push((*j, pgi));
                    } else {
                        supply.push((*j, pgi));
                    }
                }
            }
        }
        let tgt = if !sig.is_empty() {
            // Has a signal-pin home: hug those pins (a pull-up over the pin it pulls).
            sig
        } else {
            // A pure decoupling/bypass cap (both pins on rails) belongs at ONE IC
            // SUPPLY pin — the nearest pin on its non-ground (V+) rail, falling back
            // to the nearest ground pin. The OLD behaviour averaged EVERY rail pin
            // (all GND + all V+ pins on the board), whose centroid sits in the middle
            // of the sheet, so the proxy stranded the whole decoupling bank there —
            // the #1 critic complaint ("scattered decoupling caps"). One nearest
            // supply pin banks each cap tight to the IC it bypasses (mirrors
            // `supply_pin_target`, but router-free for the proxy).
            let pool = if !supply.is_empty() { &supply } else { &gnd };
            // If this cap's V+ rail reaches a BANKED IC, hug THAT IC's supply pins (it is the
            // real decoupling target) rather than a regulator that merely sources the rail.
            let banked_pool: Vec<(usize, usize)> = pool
                .iter()
                .copied()
                .filter(|&(j, _)| banked.contains(&j))
                .collect();
            let pool: &[(usize, usize)] = if banked_pool.is_empty() {
                pool
            } else {
                &banked_pool
            };
            let at = items[si].at;
            let nearest = pool.iter().copied().min_by(|&(ja, pa), &(jb, pb)| {
                let da = pin_world_dist(items, ja, pa, at);
                let db = pin_world_dist(items, jb, pb, at);
                da.total_cmp(&db)
            });
            match nearest {
                Some(p) => vec![p],
                None => Vec::new(),
            }
        };
        if !tgt.is_empty() {
            out.push((si, tgt));
        }
    }
    // MULTI-UNIT COHESION: the units of one chip (op-amp, FPGA — same refdes, ≥3 pins
    // each, so the satellite loop above skipped them as anchors) share no direct nets
    // beyond the power rails, so each unit follows its OWN signals and the symbol sprawls
    // across the sheet (an FPGA's U1A..U1E scattered with huge gaps — the BGA defect). Pull
    // every unit toward its siblings (one representative pin each) so a multi-unit symbol
    // places as ONE coherent cluster. Reuses the `cohere` term, so no new weight; only
    // fires when a refdes has ≥2 anchor units (single-unit boards/references untouched).
    let mut by_refdes: std::collections::BTreeMap<&str, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, item) in items.iter().enumerate() {
        if item.geom.pins.len() >= 3 {
            by_refdes.entry(item.refdes.as_str()).or_default().push(i);
        }
    }
    for group in by_refdes.values() {
        if group.len() < 2 {
            continue;
        }
        for &i in group {
            if items[i].frozen {
                continue;
            }
            let tgts: Vec<(usize, usize)> = group
                .iter()
                .copied()
                .filter(|&j| j != i)
                .map(|j| (j, 0usize))
                .collect();
            out.push((i, tgts));
        }
    }
    out
}
