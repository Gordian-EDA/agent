//! `place::idioms` — circuit-idiom gather/align passes (decoupling banks, crystal
//! clusters, bootstrap stages, bridge/pull-up resistors, repeated-motif columns) plus
//! the anchor-block / cohesion-target helpers the placement search and the idiom
//! re-seating share.

use std::collections::{BTreeMap, BTreeSet};

use kicad_symbol::geometry::SymbolGeometry;

use super::*;
use sch_place::item::{Incidence, Item};
use sch_place::netclass::{is_connector_like, is_ground, is_neg_supply, is_power_net};

// The disjoint-set forest (over a caller-owned `parent` slice) lives in
// `geom::union_find`, shared with circuit-lang's pin reconciler.
use super::super::infer::anchor_tap;
use geom::{uf_find, uf_union};
use sch_place::ir::{LayoutIr, Orient};

/// each load cap two gaps out, level with its osc pin. Returns true if it moved
/// anything (so the caller re-runs `decongest`). The cluster members are frozen, so
/// the placement search has already finished around them and won't undo this.
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
            Some(crate::write::pin_endpoint(
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
            let w = crate::write::pin_endpoint(pg, items[ai].at, items[ai].angle, items[ai].mirror);
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
    if std::env::var("IDIOM_DEBUG").is_ok() {
        for (i, at, ang) in &moves {
            eprintln!(
                "ALIGN {} -> [{:.1},{:.1}] ang={ang:?}",
                items[*i].refdes, at[0], at[1]
            );
        }
    }
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
    if std::env::var("IDIOM_DEBUG").is_ok() {
        for (i, at, _) in &moves {
            eprintln!(
                "ALIGN-LED {} -> [{:.1},{:.1}]",
                items[*i].refdes, at[0], at[1]
            );
        }
    }
    for (i, at, angle) in moves {
        items[i].at = at.into();
        items[i].angle = angle;
    }
    moved
}

/// Assign each two-pin rail bypass cap to the nearest non-connector ≥3-pin anchor on its V+ rail and
/// return only the banks of ≥3 caps — the SINGLE source of truth for "which caps form a decoupling bank
/// gather will re-seat". `gather_decoupling_bank` uses this to lay each bank beside its anchor;
/// `align_rail_cap_rows` uses it to know which caps to DEFER (an anchor with <3 bypass caps yields no
/// bank, so its caps stay in the bulk row rather than being stranded by a defer gather never honours).
/// Mirrors gather's candidate filter exactly: a 2-pin `C*` with both pins on rails, ≥1 V+ pin whose rail
/// reaches an anchor pin, bound to a V+ rail an anchor actually carries (preferring the V+ pin), assigned
/// to the nearest such anchor.
pub(crate) fn decoupling_bank_of(items: &[Item], ir: &LayoutIr) -> BTreeMap<usize, Vec<usize>> {
    let is_rail = |n: &str| is_power_net(n) || ir.rails.contains_key(n);
    let is_vp = |n: &str| is_rail(n) && !is_ground(n) && !is_neg_supply(n);
    let anchor_idxs: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3 && !is_connector_like(&items[i].part))
        .collect();
    let mut bank_of: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (ci, it) in items.iter().enumerate() {
        if !it.refdes.starts_with('C') || it.geom.pins.len() != 2 {
            continue;
        }
        let nets: Vec<&str> = it
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.as_deref())
            .collect();
        if nets.len() != 2 || !nets.iter().all(|n| is_rail(n)) || !nets.iter().any(|n| is_vp(n)) {
            continue;
        }
        let Some(vp_net) = nets.iter().copied().filter(|n| is_vp(n)).find(|n| {
            anchor_idxs.iter().any(|&ai| {
                items[ai]
                    .pins
                    .iter()
                    .any(|(_, _, an)| an.as_deref() == Some(*n))
            })
        }) else {
            continue;
        };
        let best = anchor_idxs
            .iter()
            .copied()
            .filter(|&ai| {
                items[ai]
                    .pins
                    .iter()
                    .any(|(_, _, n)| n.as_deref() == Some(vp_net))
            })
            .min_by(|&a, &b| {
                let d = |ai: usize| {
                    let dx = items[ai].at[0] - it.at[0];
                    let dy = items[ai].at[1] - it.at[1];
                    dx * dx + dy * dy
                };
                d(a).total_cmp(&d(b))
            });
        if let Some(ai) = best {
            bank_of.entry(ai).or_default().push(ci);
        }
    }
    // A real bank is ≥3 bypass caps (the decoupling pattern's `Role::many(.., 3, 64)` threshold); a lone
    // cap or pair is not a "scattered bank" and gather leaves it untouched.
    bank_of.retain(|_, bank| bank.len() >= 3);
    bank_of
}

/// The flat set of cap indices that `gather_decoupling_bank` will re-seat (every member of every ≥3 bank).
pub(crate) fn decoupling_bank_caps(
    items: &[Item],
    ir: &LayoutIr,
) -> std::collections::HashSet<usize> {
    decoupling_bank_of(items, ir)
        .into_values()
        .flatten()
        .collect()
}

/// Align stray BULK rail caps into a tidy row. A power-only sheet (a connector + a couple of bulk
/// caps, the load IC being a cross-sheet port) has no IC for the decoupling-bank idiom to align the
/// caps against, so the search drops one cap far from the other — vertical sprawl the critic flags
/// ("place C2 beside C1 at the same height"). This rows any group of 2+ non-frozen caps whose BOTH
/// pins are power/ground (a bulk cap has no signal pin to hug, so rowing can't strand it).
///
/// SAFETY: a cap is DEFERRED to `gather_decoupling_bank` only when that pass will ACTUALLY re-seat it —
/// i.e. it is one of a ≥3 bank of two-pin rail bypass caps assigned (nearest-anchor) to a non-connector
/// ≥3-pin anchor on its V+ rail (the same bank-assignment + `bank.len() >= 3` floor gather applies). Such
/// a cap is that IC's distributed decoupling: centralising it in the bulk row lengthens every supply wire,
/// and the critic PRAISES the bank beside the part. A cap whose anchor carries only 1–2 bypass caps is NOT
/// deferred — gather skips it, so rowing it here is the only thing keeping it from vertical sprawl; a
/// 3-pin regulator/MOSFET/relay (VR*/Q*/K*) with just a couple of bulk caps therefore does NOT suppress
/// rowing, and those caps stay in the bulk row. Frozen idiom-bank caps are skipped too. Multi-sheet only
/// (gated).
pub(crate) fn align_rail_cap_rows(items: &mut [Item], ir: &LayoutIr) -> bool {
    if std::env::var("MULTISHEET_REFINE").is_err() {
        return false;
    }
    // A net is a "rail" if it's a recognized power token OR the engine treats it as a rail (covers
    // board-specific names like VM/VSW that is_power_net's token list misses).
    let is_rail = |n: &str| is_power_net(n) || ir.rails.contains_key(n);
    let snap = |v| geom::GRID_50_MIL.snap(v);
    const PITCH: f64 = 12.7; // cap body + value/refdes label width, on grid (7.62 packed the labels
    // tight enough that the follow-up decongest scattered the whole row back out)
    // The exact set of caps `gather_decoupling_bank` will re-seat — only THESE may be deferred from the
    // bulk row. A bypass cap deferred here but skipped by gather (its anchor's bank is <3) would be left
    // stranded, the vertical-sprawl defect this pass exists to fix.
    let deferred = decoupling_bank_caps(items, ir);
    let mut groups: std::collections::BTreeMap<(String, String), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.frozen || !it.refdes.starts_with('C') || it.geom.pins.len() != 2 {
            continue;
        }
        let nets: Vec<String> = it.pins.iter().filter_map(|(_, _, n)| n.clone()).collect();
        if nets.len() != 2 || !nets.iter().all(|n| is_rail(n)) || deferred.contains(&i) {
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
        // Compact UP to the topmost cap's row (toward the input); any common y reads aligned.
        let row_y = idxs
            .iter()
            .map(|&i| items[i].at[1])
            .fold(f64::MAX, f64::min);
        if idxs
            .iter()
            .all(|&i| (items[i].at[1] - row_y).abs() < GRID_KEY)
        {
            continue; // already a row
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

/// GATHER an IC-anchored decoupling bank back beside its IC (the "decoupling caps scattered across a
/// sparse sheet" defect, critic 6). On a busy MCU sub-sheet the bypass caps connect 3V3↔GND but, with
/// the IC explicitly gridded by the author (`layout:`), the decoupling idiom is dropped (so the caps
/// are NOT frozen and never reported), and they flow through normal placement; the finalize
/// `decongest_off_labels` pass then nudges each cap off the many off-sheet port labels (LED_CTL, SPI_*,
/// I2C_*…) and scatters the bank across the sheet's empty area. `align_rail_cap_rows` deliberately DEFERS
/// exactly the caps this pass will take — the members of each ≥3 bank (`decoupling_bank_of`) — so the two
/// passes share one notion of "what is a bank" and never strand a cap between them.
///
/// This is the SIBLING of `align_rail_cap_rows`/`align_repeated_columns`: a DEAD-LAST, overlap-safe,
/// MULTISHEET_REFINE-gated re-row. The bank is re-derived GENERICALLY from the placed netlist by
/// `decoupling_bank_of` (NOT from `ir.idioms`, which the gridded-anchor case drops, NOR from `frozen`,
/// which that case clears): each non-connector ≥3-pin anchor with ≥3 of the 2-pin caps bridging its V+
/// rail. This pass lays each such bank in a tidy row HUGGING the anchor — one cap pitch off its supply-pin edge, evenly
/// spaced and centred on the supply pins. Each cap keeps its angle (V+ up / GND down), so its short risers
/// drop straight to the rails. The bank's distributed look is PRESERVED (a row of separate caps, not one
/// merged blob), which the critic praises — the only change is that the row sits BESIDE the IC, not flung
/// across empty space.
///
/// SAFETY: this commits ONLY when the proposed row introduces NO new overlap against ANY item on the
/// sheet (moved bank cap, anchor, or unrelated bystander) — there is no follow-up decongest to repair a
/// collision (and re-running one would just re-scatter the caps). A pre-existing overlap is not ours to
/// relitigate. Multi-sheet only (gated) ⇒ single-sheet reference snapshots stay byte-identical.
pub(crate) fn gather_decoupling_bank(items: &mut [Item], ir: &LayoutIr) -> bool {
    if std::env::var("MULTISHEET_REFINE").is_err() {
        return false;
    }
    let snap = |v| geom::GRID_50_MIL.snap(v);
    let is_rail = |n: &str| is_power_net(n) || ir.rails.contains_key(n);
    // A positive supply net (the rail whose pins the bank hugs): a power net that is neither ground
    // nor a negative supply.
    let is_vp = |n: &str| is_rail(n) && !is_ground(n) && !is_neg_supply(n);
    const GAP: f64 = 7.62; // anchor edge → first cap row, on grid

    // The ≥3 bypass-cap banks, each keyed by its nearest non-connector ≥3-pin anchor — the same
    // assignment `align_rail_cap_rows` consults to decide what to DEFER, so the two passes never drift.
    let bank_of = decoupling_bank_of(items, ir);

    let mut moves: Vec<(usize, [f64; 2], f64)> = Vec::new();
    for (&ai, bank) in &bank_of {
        // Which V+ rail does the bank predominantly serve?
        let mut vp_count: BTreeMap<String, usize> = BTreeMap::new();
        for &ci in bank {
            for (_, _, n) in &items[ci].pins {
                if let Some(n) = n.as_deref()
                    && is_vp(n)
                {
                    *vp_count.entry(n.to_string()).or_insert(0) += 1;
                }
            }
        }
        let Some(vp) = vp_count.into_iter().max_by_key(|(_, c)| *c).map(|(n, _)| n) else {
            continue;
        };
        // World positions of the anchor's pins on that V+ rail (the supply edge the bank should hug).
        let supply_pts: Vec<[f64; 2]> = items[ai]
            .pins
            .iter()
            .filter(|(_, _, n)| n.as_deref() == Some(vp.as_str()))
            .filter_map(|(num, _, _)| {
                items[ai]
                    .geom
                    .pins
                    .iter()
                    .find(|p| &p.number == num)
                    .map(|pg| {
                        crate::write::pin_endpoint(
                            pg,
                            items[ai].at,
                            items[ai].angle,
                            items[ai].mirror,
                        )
                    })
            })
            .collect();
        if supply_pts.is_empty() {
            continue;
        }
        // The anchor's pin bbox, to decide which edge the supply pins hug.
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for pg in &items[ai].geom.pins {
            let w = crate::write::pin_endpoint(pg, items[ai].at, items[ai].angle, items[ai].mirror);
            lo[0] = lo[0].min(w[0]);
            lo[1] = lo[1].min(w[1]);
            hi[0] = hi[0].max(w[0]);
            hi[1] = hi[1].max(w[1]);
        }
        let supply_cx = supply_pts.iter().map(|p| p[0]).sum::<f64>() / supply_pts.len() as f64;
        let supply_cy = supply_pts.iter().map(|p| p[1]).sum::<f64>() / supply_pts.len() as f64;
        // Nearest edge of the IC the supply pins sit on (same classifier as align_idiom_clusters).
        let (dl, dr, dt, db) = (
            supply_cx - lo[0],
            hi[0] - supply_cx,
            supply_cy - lo[1],
            hi[1] - supply_cy,
        );
        let m = dl.min(dr).min(dt).min(db);
        let half = item_rect(&items[ai], items[ai].at);
        let n = bank.len();
        // A top/bottom supply edge ⇒ the row of (tall) caps runs horizontally; a left/right edge ⇒ it
        // runs vertically. Order caps by current position along the row axis so the re-row is minimal.
        let horizontal_row = m == dt || m == db;
        let mut sorted = bank.clone();
        if horizontal_row {
            sorted.sort_by(|&a, &b| items[a].at[0].total_cmp(&items[b].at[0]));
        } else {
            sorted.sort_by(|&a, &b| items[a].at[1].total_cmp(&items[b].at[1]));
        }
        // ORIENT every bank cap the same way: its V+ pin points TOWARD the supply rail (the edge the
        // bank hugs), the other pin toward ground. This makes the grid uniform (the search leaves some
        // caps drawn sideways — C16/C18 in the scattered layout — which both look ragged and stack their
        // value/refdes label over a power glyph). A top/bottom-edge bank stands the caps vertically; a
        // left/right-edge bank lays them horizontally; in both the V+ pin faces the rail.
        let rail_orient = if horizontal_row {
            if m == dt { Orient::Up } else { Orient::Down }
        } else if m == dl {
            Orient::Left
        } else {
            Orient::Right
        };
        let cap_angle = |i: usize| -> f64 {
            // orient_angle takes the desired pin1→pin2 direction. We want the V+ pin to face the rail.
            let p1_is_vp = items[i]
                .pins
                .first()
                .and_then(|(_, _, n)| n.as_deref())
                .is_some_and(is_vp);
            let face = if p1_is_vp {
                // pin1 (V+) faces the rail ⇒ pin1→pin2 points AWAY from the rail.
                match rail_orient {
                    Orient::Up => Orient::Down,
                    Orient::Down => Orient::Up,
                    Orient::Left => Orient::Right,
                    Orient::Right => Orient::Left,
                }
            } else {
                // pin2 (V+) faces the rail ⇒ pin1→pin2 points TOWARD the rail.
                rail_orient
            };
            orient_angle(&items[i].geom, face)
        };
        let new_angle: BTreeMap<usize, f64> = bank.iter().map(|&i| (i, cap_angle(i))).collect();
        // Cap extents at the TARGET orientation (the field-stack-inclusive rect `item_rect` reserves)
        // drive the cell pitch, so adjacent caps never overlap — derived from geometry exactly as
        // align_repeated_columns does its column pitch.
        let cap_extent = |i: usize, horizontal: bool| -> f64 {
            let mut probe = items[i].clone();
            probe.angle = new_angle[&i];
            let r = item_rect(&probe, items[i].at);
            if horizontal { r[2] - r[0] } else { r[3] - r[1] }
        };
        let cw = bank
            .iter()
            .map(|&i| cap_extent(i, true))
            .fold(0.0_f64, f64::max);
        let ch = bank
            .iter()
            .map(|&i| cap_extent(i, false))
            .fold(0.0_f64, f64::max);
        // GRID, not one long row: 6 caps in a single 95 mm row overran the crystal cluster. Lay the bank
        // as a compact grid hugging the supply edge — its along-edge span kept near the IC body width so
        // the bank reads as a tidy block beside the IC, not a sprawling line. `lane` = the along-edge
        // axis (x for a top/bottom edge, y for a side edge), `depth` = the perpendicular outward axis.
        let xpitch = snap(cw + 2.54);
        let ypitch = snap(ch + 2.54);
        let (lane_pitch, depth_pitch) = if horizontal_row {
            (xpitch, ypitch)
        } else {
            (ypitch, xpitch)
        };
        // Columns along the edge: enough to span ~the IC body extent, but at least 2 and at most n.
        let body_extent = if horizontal_row {
            half[2] - half[0]
        } else {
            half[3] - half[1]
        };
        let ncol = ((body_extent / lane_pitch).floor() as usize)
            .clamp(2, n)
            .min(n);
        let nrow = n.div_ceil(ncol);
        // Lane origin: centre the grid on the supply pins. Depth origin: first cell one GAP + half a cap
        // off the body edge, growing OUTWARD (away from the IC).
        let lane0 = if horizontal_row { supply_cx } else { supply_cy }
            - (ncol as f64 - 1.0) * lane_pitch / 2.0;
        let (depth0, depth_sign) = if horizontal_row {
            if m == dt {
                (half[1] - GAP - ch / 2.0, -1.0)
            } else {
                (half[3] + GAP + ch / 2.0, 1.0)
            }
        } else if m == dl {
            (half[0] - GAP - cw / 2.0, -1.0)
        } else {
            (half[2] + GAP + cw / 2.0, 1.0)
        };
        let targets: Vec<[f64; 2]> = (0..n)
            .map(|k| {
                let col = k % ncol;
                let row = k / ncol;
                let lane = snap(lane0 + col as f64 * lane_pitch);
                let depth = snap(depth0 + depth_sign * row as f64 * depth_pitch);
                if horizontal_row {
                    [lane, depth]
                } else {
                    [depth, lane]
                }
            })
            .collect();
        let _ = nrow;
        // No-op guard: skip if the bank is already at the proposed row with the right orientation.
        if sorted.iter().zip(&targets).all(|(&i, t)| {
            (items[i].at[0] - t[0]).abs() < GRID_KEY
                && (items[i].at[1] - t[1]).abs() < GRID_KEY
                && (items[i].angle - new_angle[&i]).abs() < 0.5
        }) {
            continue;
        }
        // OVERLAP-SAFETY against the WHOLE sheet: there is no follow-up decongest to repair a collision
        // (and one would just re-scatter the caps). Reject the gather if any proposed cap position+angle
        // would newly overlap ANY other item (anchor, other bank cap, or unrelated bystander) that it
        // does not overlap today. `rect_at` evaluates item_rect under a hypothetical angle.
        let proposed: BTreeMap<usize, [f64; 2]> = sorted
            .iter()
            .copied()
            .zip(targets.iter().copied())
            .collect();
        let rect_at = |idx: usize, at: [f64; 2], angle: f64| -> ::geom::Rect {
            let mut probe = items[idx].clone();
            probe.angle = angle;
            item_rect(&probe, at)
        };
        let at_now = |idx: usize| item_rect(&items[idx], items[idx].at);
        let at_new = |idx: usize| -> ::geom::Rect {
            match proposed.get(&idx) {
                Some(&at) => rect_at(idx, at, new_angle[&idx]),
                None => item_rect(&items[idx], items[idx].at),
            }
        };
        let mut ok = true;
        'check: for &c in &sorted {
            for other in 0..items.len() {
                if other == c {
                    continue;
                }
                if at_new(c).overlaps(&at_new(other)) && !at_now(c).overlaps(&at_now(other)) {
                    ok = false;
                    break 'check;
                }
            }
        }
        if !ok {
            continue;
        }
        for (&i, &at) in &proposed {
            moves.push((i, at, new_angle[&i]));
        }
    }
    let moved = !moves.is_empty();
    for (i, at, angle) in moves {
        items[i].at = at.into();
        items[i].angle = angle;
    }
    moved
}

/// GATHER a BANKED multi-unit IC's free-floating decoupling bank into one compact, aligned grid
/// of vertical caps (V+ up / GND down — the praised convention) seated in the clear space just
/// LEFT of the IC's bank column, vertically centred on it.
///
/// A banked symbol (an FPGA's U3A..U3E) renders as a tall narrow stack and its bypass caps
/// connect only by RAIL LABELS (distributed power), so they have no wire pulling them anywhere —
/// the search scatters the whole bank to the far edge and across the empty middle (the BGA "vast
/// empty space / decoupling on the opposite edge" defect). `align_rail_cap_rows` skips them (on
/// IC nets) and `gather_decoupling_bank` would try to hug the IC's narrow side edge, stacking the
/// caps' wide right-side value labels into an overprinted mess. This instead lays them as a tidy
/// standalone block in the open space beside the stack — distributed look preserved, near the IC.
///
/// `banked` are the IC's unit item indices (from [`multi_unit_siblings`]). Self-contained and
/// OVERLAP-SAFE: it commits only when every proposed cap position introduces NO new overlap
/// against any other item — there is no follow-up repair (a decongest would re-scatter the bank).
/// No-op (returns false) when there is no banked IC or no clean placement, so it never regresses.
pub(crate) fn gather_banked_decoupling(
    items: &mut [Item],
    ir: &LayoutIr,
    banked: &[usize],
) -> bool {
    if banked.is_empty() {
        return false;
    }
    let snap = |v| geom::GRID_50_MIL.snap(v);
    let is_rail = |n: &str| is_power_net(n) || ir.rails.contains_key(n);
    let is_vp = |n: &str| is_rail(n) && !is_ground(n) && !is_neg_supply(n);

    let ic_refdes: BTreeSet<&str> = banked.iter().map(|&i| items[i].refdes.as_str()).collect();
    // The rails the banked IC's units actually carry — only caps on these are its decoupling.
    let ic_rails: BTreeSet<String> = banked
        .iter()
        .flat_map(|&i| items[i].pins.iter())
        .filter_map(|(_, _, n)| n.as_deref())
        .filter(|n| is_rail(n))
        .map(|n| n.to_string())
        .collect();
    // Caps the engine FROZE as this IC's decoupling idiom: those are recognized as the IC's
    // bypass bank but `align_idiom_clusters` only re-seats CRYSTAL idioms, so a frozen decoupling
    // bank is stranded wherever the seed left it. We re-seat it here, overriding the freeze for
    // these caps (we are the pass that gives the recognized bank its coherent home).
    let frozen_bank: BTreeSet<&str> = ir
        .idioms
        .iter()
        .filter(|d| d.kind == "decoupling" && ic_refdes.contains(d.anchor.as_str()))
        .flat_map(|d| d.parts.iter().map(|s| s.as_str()))
        .collect();

    // The bank: pure V+↔GND bypass caps whose BOTH rails the IC carries (so a regulator's own
    // input cap, on a rail the FPGA never sees, is excluded). A cap frozen as THIS IC's
    // decoupling idiom is admitted even though frozen — we re-seat the recognized bank.
    let mut bank: Vec<usize> = (0..items.len())
        .filter(|&i| {
            let it = &items[i];
            let claimed = frozen_bank.contains(it.refdes.as_str());
            if (it.frozen && !claimed) || !it.refdes.starts_with('C') || it.geom.pins.len() != 2 {
                return false;
            }
            let nets: Vec<&str> = it
                .pins
                .iter()
                .filter_map(|(_, _, n)| n.as_deref())
                .collect();
            nets.len() == 2
                && nets.iter().all(|n| ic_rails.contains(*n))
                && nets.iter().any(|n| is_vp(n))
                && nets.iter().any(|n| is_ground(n))
        })
        .collect();
    if bank.len() < 3 {
        return false;
    }

    // The IC's bank column: bbox of its unit bodies.
    let (mut ic_lo, mut ic_hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for &i in banked {
        let r = item_rect(&items[i], items[i].at);
        ic_lo[0] = ic_lo[0].min(r[0]);
        ic_lo[1] = ic_lo[1].min(r[1]);
        ic_hi[0] = ic_hi[0].max(r[2]);
        ic_hi[1] = ic_hi[1].max(r[3]);
    }

    // Orient every cap vertical with its V+ pin UP, GND down (KiCAD draws fields to the right).
    let cap_angle = |i: usize| -> f64 {
        let p1_is_vp = items[i]
            .pins
            .first()
            .and_then(|(_, _, n)| n.as_deref())
            .is_some_and(is_vp);
        // orient_angle takes the desired pin1→pin2 direction; V+ up ⇒ pin1→pin2 points down when
        // pin1 is V+, up otherwise.
        orient_angle(
            &items[i].geom,
            if p1_is_vp { Orient::Down } else { Orient::Up },
        )
    };
    let new_angle: BTreeMap<usize, f64> = bank.iter().map(|&i| (i, cap_angle(i))).collect();
    let cell_w = bank
        .iter()
        .map(|&i| {
            let mut p = items[i].clone();
            p.angle = new_angle[&i];
            let r = item_rect(&p, [0.0, 0.0]);
            r[2] - r[0]
        })
        .fold(0.0_f64, f64::max);
    let cell_h = bank
        .iter()
        .map(|&i| {
            let mut p = items[i].clone();
            p.angle = new_angle[&i];
            let r = item_rect(&p, [0.0, 0.0]);
            r[3] - r[1]
        })
        .fold(0.0_f64, f64::max);
    let xpitch = snap(cell_w + 2.54);
    let ypitch = snap(cell_h + 2.54);

    // Grid: a near-square block, caps grouped by their V+ rail then index for a tidy look.
    let vp_of = |i: usize| -> String {
        items[i]
            .pins
            .iter()
            .find_map(|(_, _, n)| n.as_deref().filter(|x| is_vp(x)))
            .unwrap_or("")
            .to_string()
    };
    bank.sort_by(|&a, &b| vp_of(a).cmp(&vp_of(b)).then(a.cmp(&b)));
    let n = bank.len();
    let ncol = (n as f64).sqrt().ceil() as usize;
    let nrow = n.div_ceil(ncol);
    // Block placed in the gap LEFT of the IC column, one xpitch off its left body edge, growing
    // leftward; vertically centred on the IC stack.
    let block_w = ncol as f64 * xpitch;
    let block_h = nrow as f64 * ypitch;
    let right_x = ic_lo[0] - xpitch;
    let x0 = right_x - block_w + xpitch / 2.0;
    let y0 = (ic_lo[1] + ic_hi[1]) / 2.0 - block_h / 2.0 + ypitch / 2.0;
    let targets: Vec<[f64; 2]> = (0..n)
        .map(|k| {
            let col = k % ncol;
            let row = k / ncol;
            [
                snap(x0 + col as f64 * xpitch),
                snap(y0 + row as f64 * ypitch),
            ]
        })
        .collect();

    // No-op guard: already a tidy block at the target?
    if bank.iter().zip(&targets).all(|(&i, t)| {
        (items[i].at[0] - t[0]).abs() < GRID_KEY
            && (items[i].at[1] - t[1]).abs() < GRID_KEY
            && (items[i].angle - new_angle[&i]).abs() < 0.5
    }) {
        return false;
    }

    // OVERLAP-SAFETY against the whole sheet (no follow-up decongest).
    let proposed: BTreeMap<usize, [f64; 2]> =
        bank.iter().copied().zip(targets.iter().copied()).collect();
    let rect_at = |idx: usize, at: [f64; 2], angle: f64| -> ::geom::Rect {
        let mut probe = items[idx].clone();
        probe.angle = angle;
        item_rect(&probe, at)
    };
    let at_new = |idx: usize| -> ::geom::Rect {
        match proposed.get(&idx) {
            Some(&at) => rect_at(idx, at, new_angle[&idx]),
            None => item_rect(&items[idx], items[idx].at),
        }
    };
    let at_now = |idx: usize| item_rect(&items[idx], items[idx].at);
    for &c in &bank {
        for other in 0..items.len() {
            if other == c {
                continue;
            }
            if at_new(c).overlaps(&at_new(other)) && !at_now(c).overlaps(&at_now(other)) {
                return false;
            }
        }
    }
    for (&i, &at) in &proposed {
        items[i].at = at.into();
        items[i].angle = new_angle[&i];
    }
    true
}

/// GATHER a scattered crystal cluster back beside its MCU's oscillator pins (the "crystal load cap
/// stranded on the far side, separated from its sibling and the crystal" defect, critic 6). On a busy
/// MCU sub-sheet the crystal `Y*` and its two load caps `C*` form an oscillator block hugging the
/// OSC_IN/OSC_OUT pins — but when the author GRIDS the MCU anchor (`layout: [[~, U2, J2]]`), the
/// `gridded` guard in the idiom loop DROPS the crystal idiom, so Y* + its caps are never frozen and
/// never reach `ir.idioms`. They flow through normal placement and the finalize `decongest_off_labels`
/// pass nudges them apart off the many off-sheet port labels, stranding one load cap far from the
/// crystal. A pass keyed on `ir.idioms`/`frozen` cannot see them.
///
/// This is the SIBLING of `gather_decoupling_bank`: a DEAD-LAST, overlap-safe, MULTISHEET_REFINE-gated
/// re-gather. It re-derives the cluster GENERICALLY from the placed netlist (NOT from `ir.idioms`, which
/// the gridded case drops, NOR from `frozen`, which it clears): find a 2-pin crystal (`part` ~ Crystal/
/// Resonator/Oscillator, or refdes `Y*`), its two osc nets (its non-ground pins), the anchor IC (≥3-pin,
/// non-connector) that taps BOTH osc nets, and the two 2-pin load caps (`C*`) each bridging one osc net
/// to ground. It then lays Y* + its two caps as ONE compact group hugging the anchor's OSC pins — the
/// textbook block: crystal one gap out from the osc-pin midpoint (oriented so its pins run toward the
/// IC), a load cap two gaps out on each leg — reusing `align_idiom_clusters`' exact placement geometry.
///
/// SAFETY: identical to `gather_decoupling_bank`. It commits a cluster ONLY when the proposed positions
/// introduce NO new overlap against ANY item on the sheet — there is no follow-up decongest to repair a
/// collision (one would just re-scatter the caps). A pre-existing overlap is not ours to relitigate.
/// Multi-sheet only (gated) ⇒ single-sheet reference snapshots stay byte-identical.
pub(crate) fn gather_crystal_cluster(items: &mut [Item], _ir: &LayoutIr) -> bool {
    if std::env::var("MULTISHEET_REFINE").is_err() {
        return false;
    }
    let snap = |v| geom::GRID_50_MIL.snap(v);
    const GAP: f64 = 7.62;
    let is_crystal = |it: &Item| {
        it.geom.pins.len() == 2
            && (it.part.contains("Crystal")
                || it.part.contains("Resonator")
                || it.part.contains("Oscillator")
                || it.refdes.starts_with('Y'))
    };
    // Candidate IC anchors: ≥3-pin, non-connector (same exclusion gather_decoupling_bank makes — a
    // power/SWD header touches rails but is not the oscillator host). Ordered by index for determinism.
    let anchor_idxs: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3 && !is_connector_like(&items[i].part))
        .collect();

    // (item, target position, target angle).
    let mut moves: Vec<(usize, [f64; 2], f64)> = Vec::new();
    // Items already claimed by a committed cluster (so two crystals can't fight over a shared cap).
    let mut claimed: BTreeSet<usize> = BTreeSet::new();

    for yi in (0..items.len()).filter(|&i| is_crystal(&items[i])) {
        if claimed.contains(&yi) {
            continue;
        }
        // The crystal's two OSC nets = its non-ground pin nets (a grounded-case 2-pin crystal still
        // returns its case via a separate symbol; here we model the common 2-pin variant).
        let onets: Vec<String> = items[yi]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.clone())
            .filter(|n| !is_ground(n))
            .collect();
        if onets.len() != 2 || onets[0] == onets[1] {
            continue;
        }
        // The anchor IC = the ≥3-pin non-connector that taps BOTH osc nets. If several do (rare),
        // pick the NEAREST to the crystal — the one the cluster should hug.
        let taps_both = |ai: usize| -> bool {
            onets.iter().all(|net| {
                items[ai]
                    .pins
                    .iter()
                    .any(|(_, _, n)| n.as_deref() == Some(net.as_str()))
            })
        };
        let Some(ai) = anchor_idxs
            .iter()
            .copied()
            .filter(|&ai| ai != yi && taps_both(ai))
            .min_by(|&a, &b| {
                let d = |ai: usize| {
                    let dx = items[ai].at[0] - items[yi].at[0];
                    let dy = items[ai].at[1] - items[yi].at[1];
                    dx * dx + dy * dy
                };
                d(a).total_cmp(&d(b))
            })
        else {
            continue;
        };
        // The two load caps: a 2-pin C* whose pins are {one osc net, ground}, one per osc leg.
        let cap_on = |net: &str| -> Option<usize> {
            (0..items.len())
                .filter(|&ci| {
                    ci != yi
                        && !claimed.contains(&ci)
                        && items[ci].refdes.starts_with('C')
                        && items[ci].geom.pins.len() == 2
                })
                .find(|&ci| {
                    let nets: Vec<&str> = items[ci]
                        .pins
                        .iter()
                        .filter_map(|(_, _, n)| n.as_deref())
                        .collect();
                    nets.len() == 2 && nets.contains(&net) && nets.iter().any(|n| is_ground(n))
                })
        };
        let (Some(ca), Some(cb)) = (cap_on(&onets[0]), cap_on(&onets[1])) else {
            continue;
        };
        if ca == cb {
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
            Some(crate::write::pin_endpoint(
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
        // Outward direction = the IC EDGE the osc pins hug (classify by nearest edge of the IC's pin
        // bbox — a corner osc pin sticks out the SIDE even if it's more vertically offset). Identical
        // classifier to align_idiom_clusters.
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for pg in &items[ai].geom.pins {
            let w = crate::write::pin_endpoint(pg, items[ai].at, items[ai].angle, items[ai].mirror);
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
        // Project the cluster OUT from the IC's item_rect EDGE (not just the pin tip): the IC reserves a
        // field/text stack beyond its pins, so one GAP off the pin tip can still leave the crystal
        // overlapping the IC's rect (and the whole-sheet overlap check then rejects the gather). Like
        // gather_decoupling_bank, take the body edge on the `dir` side as the depth origin and grow out.
        let body = item_rect(&items[ai], items[ai].at);
        let body_edge = if dir[0] > 0.0 {
            body[2]
        } else if dir[0] < 0.0 {
            body[0]
        } else if dir[1] > 0.0 {
            body[3]
        } else {
            body[1]
        };
        // Crystal extent along `dir` at its target angle ⇒ a half-extent margin so its near edge clears
        // the body, and a step pitch that keeps each load cap one cap clear of the crystal.
        let extent_along = |idx: usize, angle: f64| -> f64 {
            let mut probe = items[idx].clone();
            probe.angle = angle;
            let r = item_rect(&probe, items[idx].at);
            if dir[0] != 0.0 {
                r[2] - r[0]
            } else {
                r[3] - r[1]
            }
        };
        let cry_ext = extent_along(yi, cry_angle);
        let cap_ext = extent_along(ca, items[ca].angle).max(extent_along(cb, items[cb].angle));
        // Sign of the outward `dir` along its nonzero axis (+1 grows away from the IC, −1 toward).
        let dir_sign = if dir[0] != 0.0 { dir[0] } else { dir[1] };
        // The along-edge midpoint coordinate (perp axis) the cluster centres on.
        let lane_mid = if dir[0] != 0.0 { mid[1] } else { mid[0] };
        // Depth (the `dir` axis) of the crystal: body edge + GAP + half the crystal.
        let cry_depth = body_edge + dir_sign * (GAP + cry_ext / 2.0);
        let cry_at = if dir[0] != 0.0 {
            [snap(cry_depth), snap(lane_mid)]
        } else {
            [snap(lane_mid), snap(cry_depth)]
        };
        // Each load cap one step FURTHER out (so it sits past the crystal) and two perp gaps to its osc
        // pin's side of the midpoint — the textbook oscillator block, room for labels.
        let cap_depth = cry_depth + dir_sign * (cry_ext / 2.0 + GAP + cap_ext / 2.0);
        let cap_at = |w: [f64; 2]| -> [f64; 2] {
            let pin_lane = if dir[0] != 0.0 { w[1] } else { w[0] };
            let side = (pin_lane - lane_mid).signum();
            let side = if side == 0.0 { 1.0 } else { side };
            let lane = lane_mid + side * GAP * 2.0;
            if dir[0] != 0.0 {
                [snap(cap_depth), snap(lane)]
            } else {
                [snap(lane), snap(cap_depth)]
            }
        };
        // Proposed (item, new_at, new_angle). Caps keep their angle (only the crystal is re-oriented,
        // matching align_idiom_clusters).
        let proposed: Vec<(usize, [f64; 2], f64)> = vec![
            (yi, cry_at, cry_angle),
            (ca, cap_at(wa), items[ca].angle),
            (cb, cap_at(wb), items[cb].angle),
        ];
        // No-op guard: skip if the cluster already sits at the proposed geometry.
        if proposed.iter().all(|&(i, at, ang)| {
            (items[i].at[0] - at[0]).abs() < GRID_KEY
                && (items[i].at[1] - at[1]).abs() < GRID_KEY
                && (items[i].angle - ang).abs() < 0.5
        }) {
            continue;
        }
        // OVERLAP-SAFETY against the WHOLE sheet — there is no follow-up decongest to repair a
        // collision. Reject if any moved member would NEWLY overlap an item it doesn't overlap today.
        let prop: BTreeMap<usize, ([f64; 2], f64)> = proposed
            .iter()
            .map(|&(i, at, ang)| (i, (at, ang)))
            .collect();
        let rect_at = |idx: usize, at: [f64; 2], angle: f64| -> ::geom::Rect {
            let mut probe = items[idx].clone();
            probe.angle = angle;
            item_rect(&probe, at)
        };
        let at_now = |idx: usize| item_rect(&items[idx], items[idx].at);
        let at_new = |idx: usize| -> ::geom::Rect {
            match prop.get(&idx) {
                Some(&(at, ang)) => rect_at(idx, at, ang),
                None => item_rect(&items[idx], items[idx].at),
            }
        };
        let members = [yi, ca, cb];
        let mut ok = true;
        'check: for &c in &members {
            for other in 0..items.len() {
                if other == c {
                    continue;
                }
                if at_new(c).overlaps(&at_new(other)) && !at_now(c).overlaps(&at_now(other)) {
                    ok = false;
                    break 'check;
                }
            }
        }
        if !ok {
            continue;
        }
        claimed.extend(members);
        moves.extend(proposed);
    }
    let moved = !moves.is_empty();
    for (i, at, angle) in moves {
        items[i].at = at.into();
        items[i].angle = angle;
    }
    moved
}

/// GATHER each scattered high-side bootstrap stage back beside its gate-driver IC's per-phase VB/VS
/// pins (the BLDC `gate_drive` modal-6 defect). A 3-phase gate driver (IR2133 etc.) exposes per-phase
/// high-side pins VB1/VS1, VB2/VS2, VB3/VS3. Each phase's bootstrap network = a bootstrap CAP bridging
/// VBx↔VSx + a bootstrap DIODE with its cathode on VBx's net (anode on the VCC/bootstrap rail). The SA
/// optimizes each phase's wires locally and flings the three stages to opposite edges of the IC (one
/// top, one right, one bottom) with long detoured runs, instead of each {diode, cap} sitting at its own
/// VB/VS pins — the critic calls this out directly (the modal-6 outlier over ~10 reads).
///
/// This is the SIBLING of `gather_decoupling_bank`/`gather_crystal_cluster`: a DEAD-LAST, overlap-safe,
/// MULTISHEET_REFINE-gated re-gather. It re-derives the stages GENERICALLY from the placed netlist (NOT
/// from `ir.idioms`/`frozen`): find a gate-driver IC = a ≥3-pin non-connector whose pins host ≥2 phase
/// PAIRS, where a pair {VBx, VSx} is two of its pins such that a 2-pin cap bridges VBx↔VSx AND a 2-pin
/// diode taps VBx's net. For each phase pair it seats that {diode, cap} stage at the VBx/VSx pins —
/// projected OUT from the IC's `item_rect` body edge + GAP (the same depth-origin technique the sibling
/// passes use to clear the IC's field/text stack), the cap centred on the VB/VS midpoint and the diode
/// one step further out on the VBx leg. A per-stage `claimed` set stops two phases grabbing the same
/// part.
///
/// SAFETY: identical to the siblings. It commits a stage ONLY when the proposed positions introduce NO
/// new overlap against ANY item on the sheet — there is no follow-up decongest to repair a collision.
/// Multi-sheet only (gated) ⇒ single-sheet reference snapshots stay byte-identical.
pub(crate) fn gather_bootstrap_stages(items: &mut [Item], _ir: &LayoutIr) -> bool {
    if std::env::var("MULTISHEET_REFINE").is_err() {
        return false;
    }
    let snap = |v| geom::GRID_50_MIL.snap(v);
    const GAP: f64 = 7.62;

    // A 2-pin part's net set (filters None). Used to match the bridging cap / feeding diode.
    let nets_of = |i: usize| -> Vec<String> {
        items[i]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.clone())
            .collect()
    };
    // The bootstrap CAP for a phase: a 2-pin `C*` whose two pins are exactly {vb, vs}.
    let cap_bridging = |vb: &str, vs: &str, claimed: &BTreeSet<usize>| -> Option<usize> {
        (0..items.len()).find(|&ci| {
            !claimed.contains(&ci)
                && items[ci].refdes.starts_with('C')
                && items[ci].geom.pins.len() == 2
                && {
                    let ns = nets_of(ci);
                    ns.len() == 2 && ns.iter().any(|n| n == vb) && ns.iter().any(|n| n == vs)
                }
        })
    };
    // The bootstrap DIODE for a phase: a 2-pin `D*` with ONE pin on vb's net (cathode on VBx, anode on
    // the bootstrap rail). Its other pin must NOT be vs (that would be the cap, not a feed diode).
    let diode_on = |vb: &str, vs: &str, claimed: &BTreeSet<usize>| -> Option<usize> {
        (0..items.len()).find(|&di| {
            !claimed.contains(&di)
                && items[di].refdes.starts_with('D')
                && items[di].geom.pins.len() == 2
                && {
                    let ns = nets_of(di);
                    ns.len() == 2 && ns.iter().any(|n| n == vb) && !ns.iter().any(|n| n == vs)
                }
        })
    };

    // Candidate gate-driver ICs: ≥3-pin, non-connector. Ordered by index for determinism.
    let anchor_idxs: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3 && !is_connector_like(&items[i].part))
        .collect();

    let mut moves: Vec<(usize, [f64; 2], f64)> = Vec::new();
    let mut claimed: BTreeSet<usize> = BTreeSet::new();

    for ai in anchor_idxs {
        // Discover this IC's phase pairs GENERICALLY: a net `vb` (a non-ground IC pin net) is a
        // bootstrap node iff SOME 2-pin diode taps it AND SOME 2-pin cap bridges it to ANOTHER non-ground
        // IC net `vs`. We collect (vb, vs) without consuming the cap/diode yet, so the per-stage
        // `claimed` seating below decides ownership.
        let pin_nets: Vec<String> = items[ai]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.clone())
            .filter(|n| !is_ground(n))
            .collect();
        // Unique nets, index-ordered, dedup-preserving (deterministic pair order).
        let mut uniq: Vec<String> = Vec::new();
        for n in &pin_nets {
            if !uniq.contains(n) {
                uniq.push(n.clone());
            }
        }
        let empty: BTreeSet<usize> = BTreeSet::new();
        let mut pairs: Vec<(String, String)> = Vec::new();
        for vb in &uniq {
            // A diode must tap vb (necessary for a bootstrap node).
            let has_diode = (0..items.len()).any(|di| {
                items[di].refdes.starts_with('D')
                    && items[di].geom.pins.len() == 2
                    && nets_of(di).iter().any(|n| n == vb)
            });
            if !has_diode {
                continue;
            }
            // Some OTHER IC net vs with a 2-pin cap bridging vb↔vs.
            let vs = uniq
                .iter()
                .find(|vs| vs.as_str() != vb.as_str() && cap_bridging(vb, vs, &empty).is_some());
            if let Some(vs) = vs
                && !pairs.iter().any(|(b, _)| b == vb)
            {
                pairs.push((vb.clone(), vs.clone()));
            }
        }
        // A gate driver hosts ≥2 such bootstrap phase pairs; a lone {cap, diode} is some other network.
        if pairs.len() < 2 {
            continue;
        }

        // The IC's pin-tip world position for a given net.
        let pin_world = |net: &str| -> Option<[f64; 2]> {
            let num = items[ai]
                .pins
                .iter()
                .find(|(_, _, n)| n.as_deref() == Some(net))?
                .0
                .clone();
            let pg = items[ai].geom.pins.iter().find(|p| p.number == num)?;
            Some(crate::write::pin_endpoint(
                pg,
                items[ai].at,
                items[ai].angle,
                items[ai].mirror,
            ))
        };
        // The IC's pin bbox (to classify which edge a phase pair hugs).
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for pg in &items[ai].geom.pins {
            let w = crate::write::pin_endpoint(pg, items[ai].at, items[ai].angle, items[ai].mirror);
            lo[0] = lo[0].min(w[0]);
            lo[1] = lo[1].min(w[1]);
            hi[0] = hi[0].max(w[0]);
            hi[1] = hi[1].max(w[1]);
        }
        let body = item_rect(&items[ai], items[ai].at);
        // The IC's right/left edges are a WALL of port labels (GATE_xH, PHASE_x are cross-sheet and keep
        // their pennants; BOOT_x is local). A stage hugging the body edge lands ON that label band, so
        // project past it: the band extends from the pin tip outward by ~text_width(net)+2.54 (the same
        // obstacle `port_label_obstacle` reserves). Take the widest label among the IC's pins as the
        // band depth so every phase clears it.
        let max_label_w = items[ai]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.as_deref())
            .map(|n| crate::label::text_width(n) + 2.54)
            .fold(0.0_f64, f64::max);

        // STAGE 1 — resolve each phase's {cap, diode} parts and its outward edge. The cap+diode of a
        // phase form a horizontal ROW projecting out from the IC; the rows STACK on the lane axis. The
        // IC's VB/VS phase pins sit ~7.62 mm apart, but a 100nF cap's field-stack footprint is ~14 mm,
        // so the rows CANNOT sit at the pin pitch — they must spread to a uniform component pitch,
        // centred on the pin-pair centroid, the same way `gather_decoupling_bank` derives its grid pitch
        // from `item_rect` rather than cramming caps at the pin spacing. Short wires bridge each row to
        // its phase's pins (exact pin alignment isn't needed — wires connect).
        struct Stage {
            ci: usize,
            di: usize,
            vb: String, // the bootstrap node (cap↔diode junction net), to orient each part's pins
            lane_pin: f64, // the pin-pair centroid on the lane (stack) axis — the row's natural slot
        }
        let mut stages: Vec<Stage> = Vec::new();
        // In-progress claims for THIS anchor's phases (so two phases of the same IC can't both grab a
        // shared part) layered on top of the cross-anchor `claimed` set.
        let mut local_claim = claimed.clone();
        // The shared outward edge: the edge the phase pins predominantly hug (all phases share an IC
        // edge in practice). Decide it from the FIRST resolvable phase's midpoint.
        let mut dir: Option<[f64; 2]> = None;
        for (vb, vs) in &pairs {
            let (Some(ci), Some(di)) = (
                cap_bridging(vb, vs, &local_claim),
                diode_on(vb, vs, &local_claim),
            ) else {
                continue;
            };
            if ci == di {
                continue;
            }
            local_claim.insert(ci);
            local_claim.insert(di);
            let (Some(wb), Some(wvs)) = (pin_world(vb), pin_world(vs)) else {
                continue;
            };
            let mid = [(wb[0] + wvs[0]) / 2.0, (wb[1] + wvs[1]) / 2.0];
            if dir.is_none() {
                let (dl, dr, dt, db) = (
                    mid[0] - lo[0],
                    hi[0] - mid[0],
                    mid[1] - lo[1],
                    hi[1] - mid[1],
                );
                let m = dl.min(dr).min(dt).min(db);
                dir = Some(if m == dl {
                    [-1.0, 0.0]
                } else if m == dr {
                    [1.0, 0.0]
                } else if m == dt {
                    [0.0, -1.0]
                } else {
                    [0.0, 1.0]
                });
            }
            let d = dir.unwrap();
            let lane_pin = if d[0] != 0.0 { mid[1] } else { mid[0] };
            // Reserve the parts NOW so a later anchor / phase can't grab them, but only COMMIT the moves
            // if the whole stack is overlap-free below.
            stages.push(Stage {
                ci,
                di,
                vb: vb.clone(),
                lane_pin,
            });
        }
        let Some(dir) = dir else { continue };
        if stages.len() < 2 {
            continue;
        }
        let dir_sign = if dir[0] != 0.0 { dir[0] } else { dir[1] };

        // STAGE 2 — lay the rows. The row reads IC → cap → diode → rail along `dir`, so the local
        // bootstrap net `vb` (BOOT_x) must be the cap↔diode junction in the MIDDLE: orient the cap so its
        // vb pin faces OUTWARD (toward the diode) and its PHASE_x pin faces the IC (so that cross-sheet
        // pennant draws inward, short); orient the diode so its vb pin (cathode) faces INWARD (toward the
        // cap) and its rail pin (VIN, cross-sheet) faces out — keeping the diode body off the pennants.
        let opposite = |o: Orient| match o {
            Orient::Left => Orient::Right,
            Orient::Right => Orient::Left,
            Orient::Up => Orient::Down,
            Orient::Down => Orient::Up,
        };
        let outward = if dir[0] < 0.0 {
            Orient::Left
        } else if dir[0] > 0.0 {
            Orient::Right
        } else if dir[1] < 0.0 {
            Orient::Up
        } else {
            Orient::Down
        };
        let inward = opposite(outward);
        // Angle that makes `idx`'s pin on net `net` point toward `face`. `orient_angle(o)` aims the
        // pin1→pin2 axis at `o`, i.e. pin1 sits on the side OPPOSITE `o`. So to seat the named pin on the
        // `face` side: if pin1 is the named pin, aim pin1→pin2 at the OPPOSITE of `face`; otherwise
        // (pin2 is named) aim pin1→pin2 straight at `face`.
        let face_net = |idx: usize, net: &str, face: Orient| -> f64 {
            let p1_on_net = items[idx].pins.first().and_then(|(_, _, n)| n.as_deref()) == Some(net);
            let dirn = if p1_on_net { opposite(face) } else { face };
            orient_angle(&items[idx].geom, dirn)
        };
        // Extent of an item along an axis at a hypothetical angle. `along_dir=true` ⇒ the depth axis.
        let extent = |idx: usize, angle: f64, along_dir: bool| -> f64 {
            let mut probe = items[idx].clone();
            probe.angle = angle;
            let r = item_rect(&probe, items[idx].at);
            let on_x = if along_dir {
                dir[0] != 0.0
            } else {
                dir[0] == 0.0
            };
            if on_x { r[2] - r[0] } else { r[3] - r[1] }
        };
        // Uniform ROW PITCH on the lane axis = the tallest stage member's lane extent + a gap, so no two
        // rows ever collide regardless of the IC's tight pin pitch.
        let row_pitch = stages
            .iter()
            .flat_map(|s| {
                [
                    extent(s.ci, orient_angle(&items[s.ci].geom, outward), false),
                    extent(s.di, face_net(s.di, &s.vb, inward), false),
                ]
            })
            .fold(0.0_f64, f64::max)
            + 2.54;
        let n = stages.len();
        // Each row WANTS to sit at its own phase's pin-pair midpoint (so its wires to the VB/VS pins run
        // straight — minimising the fan-out knot). But the rows need `row_pitch` of lane room and the IC
        // pins are far tighter than that, so resolve overlaps with a minimal symmetric 1D spread: keep
        // phases in pin order, then push neighbours apart only as far as `row_pitch` demands, centred so
        // the block stays beside its pins. This keeps each row as close to its pins as collision allows.
        stages.sort_by(|a, b| a.lane_pin.total_cmp(&b.lane_pin));
        let mut row_lane: Vec<f64> = stages.iter().map(|s| s.lane_pin).collect();
        // Forward pass: enforce a minimum gap walking up the order.
        for i in 1..n {
            let lo = row_lane[i - 1] + row_pitch;
            if row_lane[i] < lo {
                row_lane[i] = lo;
            }
        }
        // Re-centre the spread block on the mean of the desired pin midpoints so it doesn't drift off one
        // end (the forward pass only ever pushes outward/up).
        let want_centre = stages.iter().map(|s| s.lane_pin).sum::<f64>() / n as f64;
        let got_centre = row_lane.iter().sum::<f64>() / n as f64;
        let shift = want_centre - got_centre;
        for l in &mut row_lane {
            *l += shift;
        }
        // Depth origin: past the IC body edge AND the port-label band, so rows clear the GATE_xH/PHASE_x
        // pennant wall the IC's pins carry.
        let body_edge = (if dir[0] > 0.0 {
            body[2]
        } else if dir[0] < 0.0 {
            body[0]
        } else if dir[1] > 0.0 {
            body[3]
        } else {
            body[1]
        }) + dir_sign * max_label_w;

        // Build the full proposed move set for the whole stack, then commit it ATOMICALLY only if every
        // member is overlap-free against the rest of the sheet (and against each other).
        let mut proposed: Vec<(usize, [f64; 2], f64)> = Vec::new();
        for (row, s) in stages.iter().enumerate() {
            // Cap bridges BOOT_x↔PHASE_x along `dir`; diode's vb (cathode) faces the cap so BOOT_x is the
            // junction between them and the diode's VIN (rail) pin faces out. Keeping the cap on a fixed
            // along-edge orientation (not vb-aware) draws BOOT_x as a single clean junction WIRE between
            // the parts — orienting the cap's BOOT pin outward instead made the writer label BOOT_x twice.
            let cap_angle = orient_angle(&items[s.ci].geom, outward);
            let dio_angle = face_net(s.di, &s.vb, inward);
            let cap_ext = extent(s.ci, cap_angle, true);
            let dio_ext = extent(s.di, dio_angle, true);
            let lane = snap(row_lane[row]);
            let cap_depth = body_edge + dir_sign * (GAP + cap_ext / 2.0);
            // The cap's outer pin carries the cross-sheet PHASE_x net, whose pennant the writer draws
            // ~one label-width outward; seat the diode PAST that band so its body never lands on the
            // pennant (the recurring "diode over PHASE_x label" defect).
            let phase_band = stages
                .iter()
                .filter_map(|s| {
                    items[s.ci].pins.iter().find_map(|(_, _, n)| {
                        n.as_deref()
                            .filter(|n| *n != s.vb)
                            .map(crate::label::text_width)
                    })
                })
                .fold(0.0_f64, f64::max)
                + 2.54;
            let dio_depth =
                cap_depth + dir_sign * (cap_ext / 2.0 + phase_band + 2.54 + dio_ext / 2.0);
            let pos = |depth: f64| -> [f64; 2] {
                if dir[0] != 0.0 {
                    [snap(depth), lane]
                } else {
                    [lane, snap(depth)]
                }
            };
            proposed.push((s.ci, pos(cap_depth), cap_angle));
            proposed.push((s.di, pos(dio_depth), dio_angle));
        }
        // No-op guard: if the stack already sits at the proposal, just claim and move on.
        if proposed.iter().all(|&(i, at, ang)| {
            (items[i].at[0] - at[0]).abs() < GRID_KEY
                && (items[i].at[1] - at[1]).abs() < GRID_KEY
                && (items[i].angle - ang).abs() < 0.5
        }) {
            for &(i, _, _) in &proposed {
                claimed.insert(i);
            }
            continue;
        }
        // OVERLAP-SAFETY against the WHOLE sheet — no follow-up decongest repairs a collision. Fold both
        // this stack's proposal AND moves committed by earlier anchors into the rects, so the baseline
        // ("didn't overlap before") and the proposal are evaluated in the same post-move world.
        let prop: BTreeMap<usize, ([f64; 2], f64)> = proposed
            .iter()
            .map(|&(i, at, ang)| (i, (at, ang)))
            .collect();
        let committed: BTreeMap<usize, ([f64; 2], f64)> =
            moves.iter().map(|&(i, at, ang)| (i, (at, ang))).collect();
        let rect_at = |idx: usize, at: [f64; 2], angle: f64| -> ::geom::Rect {
            let mut probe = items[idx].clone();
            probe.angle = angle;
            item_rect(&probe, at)
        };
        let settled = |idx: usize| -> ::geom::Rect {
            match committed.get(&idx) {
                Some(&(at, ang)) => rect_at(idx, at, ang),
                None => item_rect(&items[idx], items[idx].at),
            }
        };
        let at_now = |idx: usize| settled(idx);
        let at_new = |idx: usize| -> ::geom::Rect {
            match prop.get(&idx) {
                Some(&(at, ang)) => rect_at(idx, at, ang),
                None => settled(idx),
            }
        };
        let mut ok = true;
        'check: for &(c, _, _) in &proposed {
            for other in 0..items.len() {
                if other == c {
                    continue;
                }
                if at_new(c).overlaps(&at_new(other)) && !at_now(c).overlaps(&at_now(other)) {
                    ok = false;
                    break 'check;
                }
            }
        }
        if !ok {
            continue;
        }
        for &(i, _, _) in &proposed {
            claimed.insert(i);
        }
        moves.extend(proposed);
    }
    let moved = !moves.is_empty();
    for (i, at, angle) in moves {
        items[i].at = at.into();
        items[i].angle = angle;
    }
    moved
}

/// GATHER a "bridge resistor" back onto the IC pins it bridges, so its net labels sit TIGHT and LOCAL
/// beside those pins instead of scattering across the sheet and colliding with the IC's other pin labels
/// (the fresh8 instrumentation-amp modal-6 defect: the INA's gain resistor `Rg` placed far from the INA,
/// with its CH_RG1/CH_RG2 net labels colliding with the CH_IN_N/CH_IN_P input labels, so the input region
/// renders as an unreadable label cluster). A bridge resistor = a 2-pin part whose BOTH pins land on the
/// pins of ONE ≥3-pin IC, each via a PURELY LOCAL net (both endpoints on this sheet — NOT a cross-sheet
/// port). The textbook example is an INA gain resistor (Rg1↔Rg2) or an op-amp feedback resistor (OUT↔IN−);
/// the two-pin part WANTS to sit right at the two IC pins it shorts, not be scattered across the sheet.
///
/// Geometry: the two bridged IC pins sit on ONE edge, offset ALONG it (the INA's Rg pins 1/8 are stacked
/// 5.08 mm apart on the left edge). Seat the resistor PARALLEL to that edge so its two pins line up with
/// the two IC pins, and project it one GAP OFF the IC's body edge centred on their midpoint. When the
/// natural seat is blocked (the INA input region is crowded — an anti-alias cap squats in it) it steps one
/// slot further out / along the edge. NOTE: whether the net then renders as a short WIRE or as a tidy
/// LOCAL label pair is the router's call — a TRIANGLE amplifier symbol attaches its Rg pins partway up the
/// hypotenuse so their connection points fall INSIDE the symbol's bounding box, and `route_edge` rightly
/// refuses a wire that would graze the body, so those stay label-bridged; the win here is that the labels
/// are now COMPACT and adjacent (modal 6→8 on fresh8), not strewn over the input pins.
///
/// This is the SIBLING of `gather_decoupling_bank`/`gather_crystal_cluster`/`gather_bootstrap_stages`: a
/// DEAD-LAST, overlap-safe, MULTISHEET_REFINE-gated re-gather, re-derived GENERICALLY from the placed
/// netlist (NOT from `ir.idioms`/`frozen`). It DELIBERATELY skips any net the author marked a port
/// (`ir.ports`) or a power/rail net — those are correctly labelled and must not be re-wired. It SWEEPS a
/// small ladder of seats and commits the FIRST that introduces NO new overlap against ANY item on the
/// sheet (there is no follow-up decongest to repair a collision). Multi-sheet only (gated) ⇒ single-sheet
/// reference snapshots stay byte-identical.
pub(crate) fn gather_bridge_resistors(items: &mut [Item], ir: &LayoutIr) -> bool {
    if std::env::var("MULTISHEET_REFINE").is_err() {
        return false;
    }
    let snap = |v| geom::GRID_50_MIL.snap(v);
    const GAP: f64 = 5.08; // bridged-pin edge → bridge part, on grid (short stub each side)

    // A net is PURELY LOCAL iff the author did not mark it a port (cross-sheet) AND it is not a
    // power/rail net. Only such a net is drawn as a wire — labelling it would be a lie. Identical
    // local-net test the route path uses (`ir.ports` carries every cross-sheet hop).
    let is_local = |n: &str| {
        !ir.ports.contains_key(n) && !is_power_net(n) && !ir.rails.contains_key(n) && !is_ground(n)
    };
    // Candidate IC anchors: ≥3-pin, non-connector (same exclusion the sibling passes make). Index order.
    let anchor_idxs: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3 && !is_connector_like(&items[i].part))
        .collect();

    let mut moves: Vec<(usize, [f64; 2], f64)> = Vec::new();
    let mut claimed: BTreeSet<usize> = BTreeSet::new();

    // A bridge part: a 2-pin `R*` (gain/feedback resistor — the recurring case) whose two pins both
    // carry purely-local nets. Index-ordered for determinism.
    for ri in (0..items.len()).filter(|&i| {
        items[i].refdes.starts_with('R') && items[i].geom.pins.len() == 2 && !items[i].frozen
    }) {
        if claimed.contains(&ri) {
            continue;
        }
        let bnets: Vec<String> = items[ri]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.clone())
            .collect();
        if bnets.len() != 2 || bnets[0] == bnets[1] || !bnets.iter().all(|n| is_local(n)) {
            continue;
        }
        // The anchor IC = the ≥3-pin non-connector that taps BOTH of the resistor's nets. If several do
        // (rare), pick the NEAREST — the one the bridge should hug.
        let taps_both = |ai: usize| -> bool {
            bnets.iter().all(|net| {
                items[ai]
                    .pins
                    .iter()
                    .any(|(_, _, n)| n.as_deref() == Some(net.as_str()))
            })
        };
        let Some(ai) = anchor_idxs
            .iter()
            .copied()
            .filter(|&ai| ai != ri && taps_both(ai))
            .min_by(|&a, &b| {
                let d = |ai: usize| {
                    let dx = items[ai].at[0] - items[ri].at[0];
                    let dy = items[ai].at[1] - items[ri].at[1];
                    dx * dx + dy * dy
                };
                d(a).total_cmp(&d(b))
            })
        else {
            continue;
        };
        // The IC's pin-tip world position for one of the bridged nets.
        let pin_world = |net: &str| -> Option<[f64; 2]> {
            let num = items[ai]
                .pins
                .iter()
                .find(|(_, _, n)| n.as_deref() == Some(net))?
                .0
                .clone();
            let pg = items[ai].geom.pins.iter().find(|p| p.number == num)?;
            Some(crate::write::pin_endpoint(
                pg,
                items[ai].at,
                items[ai].angle,
                items[ai].mirror,
            ))
        };
        let (Some(wa), Some(wb)) = (pin_world(&bnets[0]), pin_world(&bnets[1])) else {
            continue;
        };
        let mid = [(wa[0] + wb[0]) / 2.0, (wa[1] + wb[1]) / 2.0];
        // Outward direction = the IC EDGE the two bridged pins hug (nearest edge of the IC's pin bbox to
        // their midpoint). Identical classifier to gather_crystal_cluster.
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for pg in &items[ai].geom.pins {
            let w = crate::write::pin_endpoint(pg, items[ai].at, items[ai].angle, items[ai].mirror);
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
        // Orient the resistor PARALLEL to the edge (perpendicular to `dir`), so its two pins line up with
        // the two bridged IC pins (which are separated ALONG the edge) — a short stub joins each. For a
        // left/right edge the pins separate vertically ⇒ stand the resistor UP; for a top/bottom edge they
        // separate horizontally ⇒ lay it flat. Seat its pin-1 on the SAME side as the IC pin carrying
        // net[0] so the two stubs don't cross.
        let along_vertical = dir[0] != 0.0; // side edge ⇒ pins stacked vertically
        let net0_lane = if along_vertical { wa[1] } else { wa[0] };
        let net1_lane = if along_vertical { wb[1] } else { wb[0] };
        let p1_orient = if along_vertical {
            // pin1 toward the smaller-y (upper) of the two bridged pins.
            if net0_lane <= net1_lane {
                Orient::Up
            } else {
                Orient::Down
            }
        } else if net0_lane <= net1_lane {
            Orient::Left
        } else {
            Orient::Right
        };
        let res_angle = orient_angle(&items[ri].geom, p1_orient);
        // Project the resistor OUT from the IC's item_rect body edge, centred on the bridged-pin
        // midpoint. `body_edge` = the IC body edge on the `dir` side; `lane_mid` = the along-edge midpoint.
        let body = item_rect(&items[ai], items[ai].at);
        let body_edge = if dir[0] > 0.0 {
            body[2]
        } else if dir[0] < 0.0 {
            body[0]
        } else if dir[1] > 0.0 {
            body[3]
        } else {
            body[1]
        };
        let dir_sign = if dir[0] != 0.0 { dir[0] } else { dir[1] };
        let res_ext = {
            let mut probe = items[ri].clone();
            probe.angle = res_angle;
            let r = item_rect(&probe, items[ri].at);
            if dir[0] != 0.0 {
                r[2] - r[0]
            } else {
                r[3] - r[1]
            }
        };
        let lane_mid = if dir[0] != 0.0 { mid[1] } else { mid[0] };
        // Overlap test against the WHOLE sheet (the sibling discipline): a seat is admissible only if it
        // introduces NO new overlap — there is no follow-up decongest to repair a collision. Fold earlier
        // committed moves into both the baseline and the proposal so they're judged in the same world.
        let committed: BTreeMap<usize, ([f64; 2], f64)> =
            moves.iter().map(|&(i, at, ang)| (i, (at, ang))).collect();
        let rect_at = |idx: usize, at: [f64; 2], angle: f64| -> ::geom::Rect {
            let mut probe = items[idx].clone();
            probe.angle = angle;
            item_rect(&probe, at)
        };
        let settled = |idx: usize| -> ::geom::Rect {
            match committed.get(&idx) {
                Some(&(at, ang)) => rect_at(idx, at, ang),
                None => item_rect(&items[idx], items[idx].at),
            }
        };
        let seat_ok = |at: [f64; 2]| -> bool {
            let r = rect_at(ri, at, res_angle);
            (0..items.len()).all(|other| {
                other == ri
                    || !(r.overlaps(&settled(other)) && !settled(ri).overlaps(&settled(other)))
            })
        };
        // SWEEP a small ladder of seats and take the FIRST overlap-free one (the way a human nudges the
        // part off a bystander): the natural seat is one GAP off the body edge centred on the pin
        // midpoint; if blocked, step FURTHER out, then slide ALONG the edge ± to either side. All seats
        // keep the resistor parallel to + beside its two bridged pins, so the engine wires it short.
        let mut chosen: Option<[f64; 2]> = None;
        'seat: for d in 0..4 {
            let depth = body_edge + dir_sign * (GAP + res_ext / 2.0 + d as f64 * 2.54);
            for lane_step in [0.0_f64, -5.08, 5.08, -10.16, 10.16] {
                let lane = lane_mid + lane_step;
                let at = if dir[0] != 0.0 {
                    [snap(depth), snap(lane)]
                } else {
                    [snap(lane), snap(depth)]
                };
                // No-op: if this seat is where the part already sits, it's already good — stop.
                if (items[ri].at[0] - at[0]).abs() < GRID_KEY
                    && (items[ri].at[1] - at[1]).abs() < GRID_KEY
                    && (items[ri].angle - res_angle).abs() < 0.5
                {
                    break 'seat;
                }
                if seat_ok(at) {
                    chosen = Some(at);
                    break 'seat;
                }
            }
        }
        let Some(res_at) = chosen else { continue };
        claimed.insert(ri);
        moves.push((ri, res_at, res_angle));
    }
    let moved = !moves.is_empty();
    for (i, at, angle) in moves {
        items[i].at = at.into();
        items[i].angle = angle;
    }
    moved
}

/// DEAD-LAST re-gather of an I2C (bus) PULL-UP PAIR back tight against the IC's bus pins. A pull-up is
/// a 2-pin `R*` bridging a POWER RAIL (3V3/VCC) and a BUS SIGNAL that is a CROSS-SHEET PORT
/// (`I2C_SCL`/`I2C_SDA` to the MCU) — so UNLIKE the `gather_bridge_resistors` case the signal net is a
/// port and is correctly drawn as a label, never a wire across the sheet. The `i2c_pullup` idiom seats
/// the pair a couple of columns to the RIGHT of the IC (`acol+1`/`acol+2`), so on a sparse sheet they
/// land a wide gap from the IC's SCL/SDA pins; the bus net then SPLITS into several routed components
/// (the IC's pin, the far resistor, the port exit) and the writer names EACH split with its own label —
/// the same I2C net reads ~3× in one crowded knot (the gpt55test `i2c_sensors` "crowded, ambiguous net
/// labeling" defect).
///
/// Re-seat the pair as a TIDY ADJACENT VERTICAL pair (tap UP to power), one GAP off the IC's bus-pin
/// edge and centred on the two bus pins, so each resistor's bus (bottom) pin sits RIGHT BESIDE the IC
/// pin it pulls up. The writer then joins each resistor to its IC pin with a SHORT wire (one component
/// per bus net near the IC), so the bus net is named ONCE, compactly, instead of strewn across the knot.
///
/// SIBLING of `gather_decoupling_bank`/`gather_crystal_cluster`/`gather_bootstrap_stages`/
/// `gather_bridge_resistors`: a DEAD-LAST, overlap-safe, MULTISHEET_REFINE-gated re-gather, re-derived
/// GENERICALLY from the placed netlist (NOT from `ir.idioms`/`frozen`). Multi-sheet only (gated) ⇒
/// single-sheet reference snapshots stay byte-identical. Commits ONLY a group of seats that introduces
/// NO new overlap against ANY item on the sheet (there is no follow-up decongest to repair a collision).
pub(crate) fn gather_i2c_pullups(items: &mut [Item], ir: &LayoutIr) -> bool {
    if std::env::var("MULTISHEET_REFINE").is_err() {
        return false;
    }
    let snap = |v| geom::GRID_50_MIL.snap(v);
    const GAP: f64 = 5.08; // IC bus-pin edge → pull-up's near (bus) pin, on grid (short stub)

    // A pull-up bridges a power RAIL and a non-power BUS SIGNAL that the author marked a cross-sheet
    // port (so the signal is correctly drawn as a label, never re-wired across the sheet).
    let is_rail = |n: &str| is_power_net(n) || ir.rails.contains_key(n);
    let is_bus_port = |n: &str| !is_rail(n) && ir.ports.contains_key(n);

    // Candidate IC anchors: ≥3-pin, non-connector (same exclusion the sibling passes make). Index order.
    let anchor_idxs: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3 && !is_connector_like(&items[i].part))
        .collect();

    // Bind each candidate pull-up to (IC anchor, its bus net, the IC's bus-pin world position). A pull-up
    // is a 2-pin `R*` with exactly one rail pin + one bus-port pin, whose bus net taps a ≥3-pin IC (the
    // part it pulls up). The `i2c_pullup` idiom FREEZES the pair, so we DELIBERATELY include frozen items
    // (like `gather_decoupling_bank` re-seats its frozen idiom caps) — this is the dead-last word that
    // re-seats exactly that idiom placement. Index-ordered for determinism.
    struct Pullup {
        ri: usize,
        ai: usize,
        pin_world: [f64; 2],
    }
    let pin_world = |ai: usize, net: &str| -> Option<[f64; 2]> {
        let num = items[ai]
            .pins
            .iter()
            .find(|(_, _, n)| n.as_deref() == Some(net))?
            .0
            .clone();
        let pg = items[ai].geom.pins.iter().find(|p| p.number == num)?;
        Some(crate::write::pin_endpoint(
            pg,
            items[ai].at,
            items[ai].angle,
            items[ai].mirror,
        ))
    };
    let mut pullups: Vec<Pullup> = Vec::new();
    for ri in (0..items.len())
        .filter(|&i| items[i].refdes.starts_with('R') && items[i].geom.pins.len() == 2)
    {
        let bnets: Vec<String> = items[ri]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.clone())
            .collect();
        if bnets.len() != 2 || bnets[0] == bnets[1] {
            continue;
        }
        // Exactly one rail pin + one bus-port pin.
        let (rails, buses): (Vec<&String>, Vec<&String>) = bnets.iter().partition(|n| is_rail(n));
        if rails.len() != 1 || buses.len() != 1 || !is_bus_port(buses[0]) {
            continue;
        }
        let bus = buses[0].clone();
        // The IC the pull-up pulls up = the ≥3-pin non-connector that taps this bus net. If several do,
        // pick the NEAREST — the one the resistor should hug.
        let taps = |ai: usize| {
            items[ai]
                .pins
                .iter()
                .any(|(_, _, n)| n.as_deref() == Some(bus.as_str()))
        };
        let Some(ai) = anchor_idxs
            .iter()
            .copied()
            .filter(|&ai| ai != ri && taps(ai))
            .min_by(|&a, &b| {
                let d = |ai: usize| {
                    let dx = items[ai].at[0] - items[ri].at[0];
                    let dy = items[ai].at[1] - items[ri].at[1];
                    dx * dx + dy * dy
                };
                d(a).total_cmp(&d(b))
            })
        else {
            continue;
        };
        let Some(pw) = pin_world(ai, &bus) else {
            continue;
        };
        pullups.push(Pullup {
            ri,
            ai,
            pin_world: pw,
        });
    }
    if pullups.is_empty() {
        return false;
    }

    // Group pull-ups by their IC anchor — the pair (SCL+SDA) seats as one compact block. Anchor index
    // order, then bus-pin lane order WITHIN a group, both deterministic.
    let mut by_anchor: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (k, p) in pullups.iter().enumerate() {
        by_anchor.entry(p.ai).or_default().push(k);
    }

    let mut moves: Vec<(usize, [f64; 2], f64)> = Vec::new();
    let mut claimed: BTreeSet<usize> = BTreeSet::new();
    for (&ai, group) in &by_anchor {
        // The IC's bus-pin midpoint + the outward edge those pins hug (nearest edge of the IC's pin bbox
        // to their midpoint). Identical classifier to the sibling gathers.
        let (mut blo, mut bhi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for &k in group {
            let w = pullups[k].pin_world;
            blo[0] = blo[0].min(w[0]);
            blo[1] = blo[1].min(w[1]);
            bhi[0] = bhi[0].max(w[0]);
            bhi[1] = bhi[1].max(w[1]);
        }
        let busmid = [(blo[0] + bhi[0]) / 2.0, (blo[1] + bhi[1]) / 2.0];
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for pg in &items[ai].geom.pins {
            let w = crate::write::pin_endpoint(pg, items[ai].at, items[ai].angle, items[ai].mirror);
            lo[0] = lo[0].min(w[0]);
            lo[1] = lo[1].min(w[1]);
            hi[0] = hi[0].max(w[0]);
            hi[1] = hi[1].max(w[1]);
        }
        let (dl, dr, dt, db) = (
            busmid[0] - lo[0],
            hi[0] - busmid[0],
            busmid[1] - lo[1],
            hi[1] - busmid[1],
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
        // SEAT the pull-ups VERTICAL (tap 3V3 UP — the writer risers the rail), STACKED IN ONE COLUMN one
        // GAP off the IC's bus-pin edge, ordered top→bottom to MATCH the IC's bus-pin order. Each resistor's
        // BOTTOM pin then drops to its own bus pin (an L: down/over) and these L-routes never cross because
        // the resistors keep the same vertical order as the pins they serve. The column pitch ≈ a resistor
        // height so the bodies never overlap, and the 3V3 taps go straight UP — the textbook two-resistor
        // pull-up block. (A horizontal pair can't tap 3V3 up; a tight common-row pair let one bus stub cross
        // the other tall body and split the net into the redundant-label knot — this column avoids both.)
        let r0 = pullups[group[0]].ri; // a representative resistor item (all pull-ups are 2-pin R, same geom)
        let _ = busmid;
        let along_x = dir[0] != 0.0; // side (left/right) edge ⇒ the column sits beside the IC
        let res_angle = orient_angle(&items[r0].geom, Orient::Up);
        let body = item_rect(&items[ai], items[ai].at);
        let body_edge = if dir[0] > 0.0 {
            body[2]
        } else if dir[0] < 0.0 {
            body[0]
        } else if dir[1] > 0.0 {
            body[3]
        } else {
            body[1]
        };
        // The vertical resistor's real pin span (height), and the column pitch (a touch over the body so
        // two stacked resistors never lap).
        let res_h_real = {
            let mut probe = items[r0].clone();
            probe.angle = res_angle;
            let ys: Vec<f64> = probe
                .geom
                .pins
                .iter()
                .map(|pg| crate::write::pin_endpoint(pg, [0.0, 0.0], probe.angle, probe.mirror)[1])
                .collect();
            let (lo, hi) = ys
                .iter()
                .fold((f64::MAX, f64::MIN), |(l, h), &v| (l.min(v), h.max(v)));
            hi - lo
        };
        let col_pitch = res_h_real + 6.35; // body span + a clear gap (room for the shared rail tap + label)
        // TIGHT body rect of a vertical resistor (real pin span + thin margin, plus the field stack to the
        // RIGHT) — used for the INTER-MEMBER check so the column packs at ~col_pitch instead of the
        // conservative `item_rect`'s 12.7-mm reservation (which would force a ~13-mm pitch and long drops).
        let val_chars = items[r0]
            .value
            .chars()
            .count()
            .max(items[r0].refdes.chars().count()) as f64;
        let tight_at = |at: [f64; 2]| -> ::geom::Rect {
            let mut probe = items[r0].clone();
            probe.angle = res_angle;
            let mut lo = [f64::MAX; 2];
            let mut hi = [f64::MIN; 2];
            for pg in &probe.geom.pins {
                let w = crate::write::pin_endpoint(pg, at, probe.angle, probe.mirror);
                lo[0] = lo[0].min(w[0]);
                lo[1] = lo[1].min(w[1]);
                hi[0] = hi[0].max(w[0]);
                hi[1] = hi[1].max(w[1]);
            }
            ::geom::Rect::new(
                lo[0] - 0.9,
                lo[1] - 1.0,
                hi[0] + 0.9 + val_chars * 1.1,
                hi[1] + 1.0,
            )
        };

        let committed: BTreeMap<usize, ([f64; 2], f64)> =
            moves.iter().map(|&(i, at, ang)| (i, (at, ang))).collect();
        let rect_at = |idx: usize, at: [f64; 2], angle: f64| -> ::geom::Rect {
            let mut probe = items[idx].clone();
            probe.angle = angle;
            item_rect(&probe, at)
        };
        let settled = |idx: usize| -> ::geom::Rect {
            match committed.get(&idx) {
                Some(&(at, ang)) => rect_at(idx, at, ang),
                None => item_rect(&items[idx], items[idx].at),
            }
        };
        let group_set: BTreeSet<usize> = group.iter().map(|&k| pullups[k].ri).collect();

        // Order top→bottom by the IC bus pin each resistor serves (smaller y first), so the column's
        // vertical order matches the pins' — no crossed drops. Tie-break by refdes index for determinism.
        let mut ord: Vec<usize> = group.clone();
        ord.sort_by(|&a, &b| {
            pullups[a].pin_world[1]
                .total_cmp(&pullups[b].pin_world[1])
                .then(pullups[a].ri.cmp(&pullups[b].ri))
        });
        let n = ord.len();

        // Column x: one GAP off the IC's bus edge (the body's `dir` side) for a side edge; for a top/bottom
        // edge fall back to just past the bus-pin bbox. Column is centred on the bus-pin midpoint in y, the
        // members col_pitch apart, so the top resistor's bottom pin sits a little above the top bus pin.
        // Distance from the IC bus edge to the resistor CENTRE = one GAP + the resistor's half-width.
        let res_half_w = {
            let r = rect_at(r0, [0.0, 0.0], res_angle);
            (r[2] - r[0]) / 2.0
        };
        let col_x = if along_x {
            body_edge + dir[0].signum() * (GAP + res_half_w)
        } else {
            hi[0] + GAP + res_half_w
        };
        // First resistor's centre y so the STACK is centred on the bus-pin band's vertical midpoint.
        let bus_mid_y = (blo[1] + bhi[1]) / 2.0;
        let col_top_cy = bus_mid_y - col_pitch * ((n as f64) - 1.0) / 2.0;

        // Seats: one column. Try the natural column; if any seat clashes the sheet, step the WHOLE column
        // further out along the edge normal, then nudge it ± in the across direction.
        let mut seats: Vec<(usize, [f64; 2], f64)> = Vec::new();
        let mut ok = false;
        'col: for d in 0..5 {
            let cx = if along_x {
                col_x + dir[0].signum() * d as f64 * 2.54
            } else {
                col_x
            };
            for shift in [0.0_f64, -2.54, 2.54, -5.08, 5.08] {
                let mut trial: Vec<(usize, [f64; 2], f64)> = Vec::new();
                let mut placed: Vec<::geom::Rect> = Vec::new();
                let mut all_ok = true;
                for (slot, &k) in ord.iter().enumerate() {
                    let ri = pullups[k].ri;
                    let cy = col_top_cy + (slot as f64) * col_pitch + shift;
                    let at = if along_x {
                        [snap(cx), snap(cy)]
                    } else {
                        [
                            snap(cx + shift),
                            snap(col_top_cy + (slot as f64) * col_pitch),
                        ]
                    };
                    let r = rect_at(ri, at, res_angle);
                    // Against prior group seats (TIGHT — column packs close) AND the rest of the sheet
                    // (conservative; pre-existing overlaps aren't ours to relitigate).
                    if placed.iter().any(|pr| tight_at(at).overlaps(pr)) {
                        all_ok = false;
                        break;
                    }
                    let clash = (0..items.len()).find(|&other| {
                        !group_set.contains(&other)
                            && r.overlaps(&settled(other))
                            && !settled(ri).overlaps(&settled(other))
                    });
                    if clash.is_some() {
                        all_ok = false;
                        break;
                    }
                    placed.push(tight_at(at));
                    trial.push((ri, at, res_angle));
                }
                if all_ok {
                    seats = trial;
                    ok = true;
                    break 'col;
                }
            }
        }
        // No-op short-circuit: if every member already sits at its chosen seat + orientation, skip (no churn).
        if ok
            && seats.iter().all(|&(ri, at, ang)| {
                (items[ri].at[0] - at[0]).abs() < GRID_KEY
                    && (items[ri].at[1] - at[1]).abs() < GRID_KEY
                    && (items[ri].angle - ang).abs() < 0.5
            })
        {
            continue;
        }
        if ok {
            for (ri, at, ang) in seats {
                if claimed.insert(ri) {
                    moves.push((ri, at, ang));
                }
            }
        }
    }
    let moved = !moves.is_empty();
    for (i, at, angle) in moves {
        items[i].at = at.into();
        items[i].angle = angle;
    }
    moved
}

/// Repeated-motif column alignment (the 3-phase / N-stage fix). A sheet built from N copies of the
/// same building block — three half-bridges (each = 2 IRLZ44N FETs), three bootstrap stages (each =
/// a 1N5819 diode + cap) — reads best as N ALIGNED COLUMNS, same x-pitch, consistent internal
/// vertical order, so the repetition + symmetry is legible. The SA optimizes each copy's wires
/// locally and scatters the copies diagonally; the critic calls this out directly ("scattered instead
/// of three aligned half-bridge columns", score 5). This finalize pass detects such repeats and snaps
/// each instance to its own evenly-spaced column.
///
/// Why a FINAL pass works here where it failed for the bus-row (17 reverts, see
/// docs/specs/flow-aware-global-placement.md): the bus-row tried to CRAM clusters into ONE shared
/// column and collided on dense sheets; this gives each instance a DISTINCT x and these sheets have
/// EMPTY SPACE, so the overlap check passes. It runs DEAD LAST (after every decongest), carries each
/// instance's satellites by the same Δx (no stranding), and COMMITS A GROUP ONLY if the proposed
/// positions are overlap-free — exactly the `align_rail_cap_rows` discipline. Gated on
/// MULTISHEET_REFINE so single-sheet reference snapshots stay byte-identical.
pub(crate) fn align_repeated_columns(
    items: &mut [Item],
    ir: &LayoutIr,
    fields_above: &mut BTreeSet<String>,
) -> bool {
    if std::env::var("MULTISHEET_REFINE").is_err() {
        return false;
    }
    let snap = |v| geom::GRID_50_MIL.snap(v);
    let is_rail = |n: &str| is_power_net(n) || ir.rails.contains_key(n);
    // A "spine" is one instance of the repeated block: a MULTI-PIN ANCHOR (≥3 pins — same definition the
    // engine uses for `anchors`). Three half-bridges = six IRLZ44N FETs (3-pin). We deliberately do NOT
    // match 2-pin parts here: a sheet's same-value caps/diodes/resistors are heterogeneous (bypass +
    // bootstrap + filter share lib_id but are NOT a repeated motif), and column-snapping them scatters a
    // correctly placed bank (verified: it regressed gate_drive 20→28 wire-xings and current_sense). The
    // motif must be carried by the anchor; its 2-pin satellites RIDE the anchor's Δx (below).
    let mut by_part: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut refdes_seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.frozen || it.part.is_empty() || it.geom.pins.len() < 3 {
            continue;
        }
        // Skip the second+ UNIT of a multi-unit part (op-amp A/B/power share one refdes): they are ONE
        // device split across items, not N repeated devices, and column-spreading them tears the device
        // apart (verified: scattered current_sense's LM358 units). One item per refdes.
        let prev = refdes_seen.insert(it.refdes.as_str(), i);
        if prev.is_some() {
            by_part.entry(it.part.clone()).and_modify(|v| {
                v.retain(|&j| items[j].refdes != it.refdes);
            });
            continue;
        }
        by_part.entry(it.part.clone()).or_default().push(i);
    }

    let mut moved = false;
    // BTreeMap iteration is sorted by part name ⇒ deterministic.
    for members in by_part.values() {
        if members.len() < 3 {
            continue;
        }
        // Partition the copies into COLUMNS via shared non-power nets (union-find): the two FETs of a
        // half-bridge share PHASE_x, so they land in the same column (HS stacked over LS); three
        // independent bootstrap diodes share nothing ⇒ each is its own column. This recovers the
        // repeated UNIT generically from connectivity, not from refdes arithmetic.
        let n = members.len();
        let mut parent: Vec<usize> = (0..n).collect();
        // net -> first member (local index) seen carrying it; only signal nets join copies.
        let mut net_owner: BTreeMap<String, usize> = BTreeMap::new();
        for (k, &i) in members.iter().enumerate() {
            for (_, _, net) in &items[i].pins {
                let Some(net) = net else { continue };
                if is_rail(net) {
                    continue;
                }
                if let Some(&other) = net_owner.get(net) {
                    uf_union(&mut parent, k, other);
                } else {
                    net_owner.insert(net.clone(), k);
                }
            }
        }
        // Collect columns: root -> member item indices.
        let mut cols_map: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for k in 0..n {
            let r = uf_find(&mut parent, k);
            cols_map.entry(r).or_default().push(members[k]);
        }
        // Need ≥3 columns for this to be a "repeated columns" motif.
        if cols_map.len() < 3 {
            continue;
        }
        // Order columns left→right by current centroid x.
        let mut columns: Vec<Vec<usize>> = cols_map.into_values().collect();
        let col_cx = |c: &[usize]| c.iter().map(|&i| items[i].at[0]).sum::<f64>() / c.len() as f64;
        columns.sort_by(|a, b| col_cx(a).total_cmp(&col_cx(b)));
        // A clean repeat has uniform-size columns; skip ragged groups (mixed roles).
        let sz0 = columns[0].len();
        if !columns.iter().all(|c| c.len() == sz0) {
            continue;
        }
        // For each spine, find its satellites = nearby non-grouped 2-pin parts whose NEAREST grouped
        // spine is this one (within a radius). They ride along with their spine's Δx.
        let grouped: BTreeSet<usize> = members.iter().copied().collect();
        const SAT_R: f64 = 22.0; // ~2 grid cells: a tap part sits this close to its anchor
        let mut sat_of: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (j, jt) in items.iter().enumerate() {
            if grouped.contains(&j) || jt.frozen || jt.geom.pins.len() != 2 {
                continue;
            }
            // A satellite must be ELECTRICALLY LOCAL to exactly one anchor: it shares a SIGNAL (non-rail)
            // net with that anchor. A shared power/ground rail does NOT qualify — a bulk/bypass cap on
            // VIN-GND ties to ALL high-side FETs equally, so it is stage-shared decoupling, not a
            // per-instance satellite; carrying it with one column drags it across the others and self-
            // collides. Requiring a private signal net (GATE_x / a PHASE/SHUNT node) keeps only true taps.
            let jnets: BTreeSet<&str> = jt
                .pins
                .iter()
                .filter_map(|(_, _, n)| n.as_deref())
                .collect();
            let mut best: Option<(f64, usize)> = None;
            for &i in members.iter() {
                let shares_signal = items[i].pins.iter().any(|(_, _, n)| {
                    n.as_deref()
                        .is_some_and(|n| !is_rail(n) && jnets.contains(n))
                });
                if !shares_signal {
                    continue;
                }
                let dx = items[i].at[0] - jt.at[0];
                let dy = items[i].at[1] - jt.at[1];
                let d = (dx * dx + dy * dy).sqrt();
                if d <= SAT_R && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, i));
                }
            }
            if let Some((_, i)) = best {
                sat_of.entry(i).or_default().push(j);
            }
        }
        // Build a true GRID: m columns × sz0 rows. The members within a column are scattered (the SA
        // placed each FET by its own wires), so we must not only align their x but STACK them at shared
        // row-y slots in a consistent order — that is what makes the repeats read as aligned columns.
        let m = columns.len();
        // Column x: evenly spaced about the current centroid (minimizes total Δx for the existing
        // left→right order). x-pitch = widest member footprint + a cell of slack.
        let col_w = members
            .iter()
            .map(|&i| {
                let r = item_rect(&items[i], items[i].at);
                r[2] - r[0]
            })
            .fold(0.0_f64, f64::max);
        let xpitch = snap((col_w + 12.7).max(25.4));
        let cur_cx: Vec<f64> = columns.iter().map(|c| col_cx(c)).collect();
        let xcenter = cur_cx.iter().sum::<f64>() / cur_cx.len() as f64;
        let x0 = xcenter - xpitch * (m as f64 - 1.0) / 2.0;
        let target_cx: Vec<f64> = (0..m).map(|k| snap(x0 + k as f64 * xpitch)).collect();
        // Row y: order each column's members top→bottom by current y, then give role-rank r a COMMON y
        // across all columns = the median current y of that rank (keeps the grid where the SA already
        // put the mass) on a fixed pitch so a role-row is a clean horizontal line.
        let col_h = members
            .iter()
            .map(|&i| {
                let r = item_rect(&items[i], items[i].at);
                r[3] - r[1]
            })
            .fold(0.0_f64, f64::max);
        // Inter-row text headroom: a multi-pin anchor (the FET) carries refdes+value on a HORIZONTAL
        // band ABOVE and BELOW its body (~4.78 mm each, see emit::solve_text_positions), and the
        // BOTTOM role-row's source pin drops to a port/label (SHUNT_x_TOP) that occupies the band
        // directly below it. With only one cell of slack the top-row's value band, the bottom-row's
        // refdes band, and that hanging port all crowd the same narrow gap and collide (the
        // "bottom-row label/refdes text collisions" defect). Open the pitch to fit a clear text band
        // on BOTH sides of every body (two ~4.78 mm bands + a cell of margin) so the solver always has
        // a collision-free above/below spot. The overlap-safety check below still gates the move.
        let ypitch = snap((col_h + 15.24).max(30.48));
        // CONSISTENT internal order: rank each member by a connectivity ROLE so corresponding parts sit
        // in the same row across columns (every HS FET on top, every LS FET below) — the critic's "same
        // internal vertical order" ask. Role key = (#pins on a positive supply rail) DESCENDING: the
        // high-side FET's drain is on VIN (1 supply pin) so it ranks above the low-side FET (drain on
        // PHASE, source on a SHUNT ⇒ 0 supply pins). Ties (e.g. truly symmetric parts) fall back to the
        // SA's current y so a stable order is still chosen. Generalizes to any rail-anchored repeat.
        let is_pos_supply =
            |n: &str| is_power_net(n) && !is_ground(n) && !n.eq_ignore_ascii_case("GND");
        let supply_pins = |i: usize| -> i32 {
            items[i]
                .pins
                .iter()
                .filter(|(_, _, n)| n.as_deref().is_some_and(is_pos_supply))
                .count() as i32
        };
        let mut ranked: Vec<Vec<usize>> = columns.clone();
        for c in &mut ranked {
            c.sort_by(|&a, &b| {
                supply_pins(b)
                    .cmp(&supply_pins(a))
                    .then(items[a].at[1].total_cmp(&items[b].at[1]))
            });
        }
        let row_y: Vec<f64> = (0..sz0)
            .map(|r| {
                let mut ys: Vec<f64> = ranked.iter().map(|c| items[c[r]].at[1]).collect();
                ys.sort_by(f64::total_cmp);
                ys[ys.len() / 2] // median current y of this role-rank
            })
            .collect();
        // Snap each role-row to a fixed pitch anchored at the topmost row's median (uniform spacing).
        let y0 = row_y[0];
        let target_ry: Vec<f64> = (0..sz0).map(|r| snap(y0 + r as f64 * ypitch)).collect();

        // Propose per-item moves: each member goes to (target_cx[k], target_ry[r]); its satellites ride
        // by the SAME (Δx, Δy) so a tap cap stays glued to its anchor.
        let mut proposed: BTreeMap<usize, [f64; 2]> = BTreeMap::new();
        for (k, col) in ranked.iter().enumerate() {
            for (r, &i) in col.iter().enumerate() {
                let to = [target_cx[k], target_ry[r]];
                let dx = to[0] - items[i].at[0];
                let dy = to[1] - items[i].at[1];
                proposed.insert(i, to);
                for &s in sat_of.get(&i).into_iter().flatten() {
                    proposed.insert(s, [snap(items[s].at[0] + dx), snap(items[s].at[1] + dy)]);
                }
            }
        }
        // No-op guard: skip if already grid-aligned (every member within a grid cell of its target).
        if proposed.iter().all(|(&i, at)| {
            (items[i].at[0] - at[0]).abs() < GRID_KEY && (items[i].at[1] - at[1]).abs() < GRID_KEY
        }) {
            continue;
        }
        // OVERLAP-SAFETY: the cluster's INTERNAL integrity must hold — reject if the grid would make two
        // MOVED items (anchors and/or their carried satellites) collide, because the follow-up decongest
        // can't fix that without tearing the grid apart. A collision between a moved item and a STATIONARY
        // bystander (e.g. a VIN bypass cap that isn't part of the motif) is fine: the caller's decongest
        // pushes the bystander aside, exactly as it does after align_rail_cap_rows / align_led_chains.
        // (A pre-existing overlap is likewise not ours to relitigate.) This is the discipline that lets a
        // final pass make room on a sheet with empty space — the property the bus-row reverts lacked.
        let now_at = |idx: usize| item_rect(&items[idx], items[idx].at);
        let new_at = |idx: usize| -> ::geom::Rect {
            let at = proposed.get(&idx).copied().unwrap_or(items[idx].at.into());
            item_rect(&items[idx], at)
        };
        let mut ok = true;
        'check: for &a in proposed.keys() {
            for &b in proposed.keys() {
                if a >= b {
                    continue;
                }
                if new_at(a).overlaps(&new_at(b)) && !now_at(a).overlaps(&now_at(b)) {
                    ok = false;
                    break 'check;
                }
            }
        }
        if !ok {
            continue;
        }
        for (&i, &at) in &proposed {
            items[i].at = at.into();
        }
        // LOW-SIDE FIELD KEEPOUT: in each column the role-rank-0 member is the high
        // side (text below it, clear); a member in a LOWER role-row whose own
        // down-facing pin hangs a rotated global PORT label (the SHUNT_x source node)
        // would have its below-body refdes/value band crowd that label's vertical
        // strip — the two read as one garbled token ("V_LS" jammed under
        // "SHUNT_V_TOP"). Flag those so the emitter solves their fields ABOVE the body,
        // mirroring the high-side row. Done on the COMMITTED grid positions (the moves
        // are applied above) so the down-pin direction is the one that ships.
        for col in &ranked {
            for (r, &i) in col.iter().enumerate() {
                if r == 0 {
                    continue; // top role-row = high side: its below-body text is clear.
                }
                let cy = items[i].at[1];
                // The member's pin whose sheet endpoint sits LOWEST (furthest down)
                // is its down-facing pin; flag if that pin's net exits as a port.
                let down_port = items[i]
                    .pins
                    .iter()
                    .filter_map(|(num, _, net)| {
                        let net = net.as_ref()?;
                        if !ir.ports.contains_key(net) {
                            return None;
                        }
                        let pg = items[i].geom.pins.iter().find(|p| &p.number == num)?;
                        let ep = crate::write::pin_endpoint(
                            pg,
                            items[i].at,
                            items[i].angle,
                            items[i].mirror,
                        );
                        // Only a pin that genuinely points DOWN (its endpoint below the
                        // body centre) hangs its label into the below-body band.
                        (ep[1] > cy + 1.0).then_some(())
                    })
                    .next();
                if down_port.is_some() {
                    fields_above.insert(items[i].refdes.clone());
                }
            }
        }
        moved = true;
    }
    moved
}

/// The KiCAD rotation (0/90/180/270) that makes a 2-pin part's pin1→pin2 axis
/// point the way [`Orient`] asks, derived from the symbol's own pin geometry so
/// it is correct whatever the part's native orientation. Multi-pin parts (ICs,
/// connectors) are pre-oriented and stay at 0° (use `mirror` to flip them).
///
/// A local pin `(lx, ly)` maps to sheet offset `(rx, -ry)` after a CCW rotation
/// by the instance angle (see `Point2::transform_offset`), so increasing the angle
/// turns the sheet-space axis clockwise. We test the four quarter-turns and pick
/// the one whose resulting cardinal axis matches the request.
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
    // Desired pin1→pin2 direction in sheet space (y grows downward).
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

/// Quantization for comparing coordinates by grid cell.
pub const GRID_KEY: f64 = geom::GRID_50_MIL.pitch();

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
pub(crate) const MULTI_UNIT_RIGID_MIN: usize = 3;

/// Per-unit symbol-pin count above which a multi-unit symbol counts as genuinely BANKED.
/// A dual/quad op-amp's units are tiny (an MCP6002 unit's symbol has 8 pins) and the
/// existing per-anchor path already places them well, so binding them rigidly only
/// perturbs a tuned layout (measured: it traded op-amp crossings the wrong way). A
/// banked IC's units are large (an iCE40 bank's symbol has ~120 pins); only those sprawl
/// and benefit from travelling as one block. 32 cleanly separates the two regimes.
pub(crate) const MULTI_UNIT_BANKED_PINS: usize = 32;

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
pub(crate) fn pin_world_dist(
    items: &[Item],
    j: usize,
    pgi: usize,
    from: impl Into<::geom::Point2>,
) -> f64 {
    let from = from.into();
    let p = crate::write::pin_endpoint(
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
        let (mut sig, mut supply, mut gnd): (
            Vec<(usize, usize)>,
            Vec<(usize, usize)>,
            Vec<(usize, usize)>,
        ) = (Vec::new(), Vec::new(), Vec::new());
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
    for i in 0..items.len() {
        if items[i].geom.pins.len() >= 3 {
            by_refdes
                .entry(items[i].refdes.as_str())
                .or_default()
                .push(i);
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
