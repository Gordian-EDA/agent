//! The holistic floorplanner — the single-sheet DE-SPRAWL pass. DEFAULT-ON in the `cluster`
//! engine; validated full-dataset (13/40 liftable boards
//! de-sprawl, 0 regressions). Repetitive power-IC arrays go a step further via the rail idiom
//! ([`rail_relayout`]); everything else stops here.
//!
//! A human-vs-machine logistic fit over the 500 boards (`tools/learn_layout.py`) showed the
//! engine's #1 deficiency is SPRAWL (humans ~24, the SA ~69 — 3× too spread) + ISLAND-scatter
//! (2.4× more clusters): connected parts the SA leaves far apart, chiefly the `decouple:`
//! sugar's caps strung in a far row instead of hugging their IC. [`holistic_relayout`] fixes
//! it the way the field does — NOT by editing the SA's placed sheet (where the space beside an
//! IC is already occupied, so every incremental re-bank/force/scale collides or spreads), but
//! by laying each MODULE out cleanly IN ISOLATION (hub, a single-row decoupling bank above it,
//! and its satellites), taking that module's footprint, and PACKING the footprints in
//! connectivity order. The decoupling bank lands beside its IC AND nothing collides, because
//! the room was reserved before packing.
//!
//! Two details were load-bearing (each cost real warnings until fixed): (1) the decoupling
//! caps must form ONE module (`modules` folds rail-only caps into the IC on their V+ rail —
//! `anchor_tap` drops them as singletons because GND touches everything), laid in a SINGLE
//! ROW (a grid collides the tall 3V3/cap/GND legs' labels); (2) the footprint must inflate by
//! the power-glyph / net-label PENNANT overhang `item_rect` omits, or packed modules collide
//! those glyphs. With both, a clean MCU board goes 48→34 sprawl (−29%, toward human), 0
//! warnings; on the gate-driver array 0cdac the holistic pass reaches 54, then [`rail_relayout`]
//! finishes it at sprawl ~11. The gate ([`compact_clusters`]) keeps it only when it strictly
//! de-sprawls without regressing the routed metrics — so a dual-IC / huge-bank board where the
//! single-row bank gets too wide (411be) is safely left to the SA.

use std::collections::{BTreeMap, BTreeSet};

use geom::{Point2, Rect};
use sch_check::model::Design;
use sch_place::ir::LayoutIr;
use sch_place::item::{Incidence, Item};

use sch_floorplan::contract::RoutedEvaluator;
use sch_floorplan::engine_support::{
    align_idiom_clusters, align_led_chains, build_anchor_blocks, decongest, item_rect, orient_angle,
};

use crate::eval::{restore, save, score};

/// Whitespace per part of a RENDERED extent — the same `bbox_area / (n·cell)` ratio the
/// validation oracle uses, but on [`RoutedEvaluator::rendered`] (post text-solve + orphan
/// label-columns), so it reflects the SHIPPED label-inclusive sheet. Captures the orphan-column
/// balloon a part-origin bbox misses — but is conversely dominated by label width, so it can
/// MISS a pure part-spread (a pose move). The safety net Pareto-checks this AND [`part_sprawl`].
pub(crate) fn rendered_sprawl(extent: &Rect, n: usize) -> f64 {
    const CELL: f64 = 6.35 * 5.08;
    (extent.max_x - extent.min_x) * (extent.max_y - extent.min_y) / (n.max(1) as f64 * CELL)
}

/// Whitespace per part over part ORIGINS (`item.at`) — exactly the validation oracle's
/// symbol-instance bbox. Sensitive to part SPREAD (a pose move that splays a dual IC) where
/// [`rendered_sprawl`] is not, so the two together are a complete sprawl guard.
pub(crate) fn part_sprawl(items: &[Item]) -> f64 {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for it in items {
        x0 = x0.min(it.at.x);
        y0 = y0.min(it.at.y);
        x1 = x1.max(it.at.x);
        y1 = y1.max(it.at.y);
    }
    if items.is_empty() || x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    const CELL: f64 = 6.35 * 5.08;
    (x1 - x0) * (y1 - y0) / (items.len() as f64 * CELL)
}

/// Lay a bank of repeated active channels as identical cells instead of preserving the
/// annealer's unrelated per-channel offsets.  This deliberately recognizes only the common
/// "N identical hubs, each fed by one private 2-pin signal part" shape (opto/driver/input
/// banks).  Everything else is a support part and occupies a separate top band.
///
/// The caller evaluates the fully routed rendering and restores the input placement unless
/// this is a strict no-regression win, so this function only proposes geometry.
fn repeated_channel_relayout(items: &mut [Item], inc: &Incidence, ir: &LayoutIr) -> bool {
    use circuit_graph::netclass::is_power_net;

    let is_rail = |n: &str| ir.rails.contains_key(n) || is_power_net(n);
    let mut by_part: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.refdes.starts_with('U') && it.geom.pins.len() >= 3 {
            by_part.entry(&it.part).or_default().push(i);
        }
    }
    let Some((_, mut hubs)) = by_part.into_iter().max_by_key(|(_, v)| v.len()) else {
        return false;
    };
    if hubs.len() < 4 {
        return false;
    }
    let natural_refdes = |i: usize| {
        let rd = items[i].refdes.as_str();
        let (alpha, num) = sch_check::model::refdes_key(rd);
        (alpha.to_owned(), num, rd.to_owned())
    };
    hubs.sort_by_key(|&i| natural_refdes(i));

    // A private satellite shares a two-terminal non-rail net with exactly this hub.
    let mut channels = Vec::with_capacity(hubs.len());
    let mut used_sat = BTreeSet::new();
    for &hub in &hubs {
        let mut candidates: Vec<(usize, String)> = Vec::new();
        for net in items[hub]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.as_deref())
            .filter(|n| !is_rail(n))
        {
            let Some(pins) = inc.get(net) else { continue };
            if pins.len() != 2 {
                continue;
            }
            if let Some(sat) = pins
                .iter()
                .map(|&(i, _)| i)
                .find(|&i| i != hub && items[i].geom.pins.len() == 2)
            {
                candidates.push((sat, net.to_owned()));
            }
        }
        candidates.sort();
        candidates.dedup();
        if candidates.len() != 1 || !used_sat.insert(candidates[0].0) {
            return false;
        }
        channels.push((hub, candidates[0].0, candidates[0].1.clone()));
    }

    const GRID: f64 = 1.27;
    const MARGIN: f64 = 12.7;
    const SUPPORT_GAP: f64 = 17.78;
    const BAND_GAP: f64 = 25.4;
    const CELL_X: f64 = 60.96;
    const CELL_Y: f64 = 30.48;

    let channel_items: BTreeSet<usize> = channels.iter().flat_map(|&(h, s, _)| [h, s]).collect();
    let mut supports: Vec<usize> = (0..items.len())
        .filter(|i| !channel_items.contains(i))
        .collect();
    supports.sort_by_key(|&i| {
        // Big boundaries first, then stable/natural reference order.
        (
            std::cmp::Reverse(items[i].geom.pins.len()),
            natural_refdes(i),
        )
    });

    // Support band: headers, resistor array and bypass parts read as one compact prelude.
    let mut sx = MARGIN;
    let mut support_bottom = MARGIN;
    for i in supports {
        if items[i].geom.pins.len() == 2
            && items[i]
                .pins
                .iter()
                .filter_map(|(_, _, n)| n.as_deref())
                .all(&is_rail)
        {
            items[i].angle = orient_angle(&items[i].geom, sch_place::ir::Orient::Down);
        } else {
            items[i].angle = 0.0;
        }
        items[i].mirror = false;
        let r = item_rect(&items[i], Point2::new(0.0, 0.0));
        items[i].at = geom::GRID_50_MIL.snap_point(Point2::new(sx - r.min_x, MARGIN - r.min_y));
        let seated = item_rect(&items[i], items[i].at);
        sx = seated.max_x + SUPPORT_GAP;
        support_bottom = support_bottom.max(seated.max_y);
        items[i].frozen = true;
    }

    let ncols = if channels.len() >= 6 { 2 } else { 1 };
    let base_y = ((support_bottom + BAND_GAP) / GRID).ceil() * GRID;
    for (k, (hub, sat, shared)) in channels.into_iter().enumerate() {
        let (col, row) = (k % ncols, k / ncols);
        items[hub].angle = 0.0;
        items[hub].mirror = false;
        items[hub].at = geom::GRID_50_MIL.snap_point(Point2::new(
            MARGIN + 25.4 + col as f64 * CELL_X,
            base_y + row as f64 * CELL_Y,
        ));

        let hub_pin = items[hub]
            .pins
            .iter()
            .find(|(_, _, n)| n.as_deref() == Some(shared.as_str()))
            .map(|(num, _, _)| num.clone())
            .unwrap();
        let sat_pin = items[sat]
            .pins
            .iter()
            .find(|(_, _, n)| n.as_deref() == Some(shared.as_str()))
            .map(|(num, _, _)| num.clone())
            .unwrap();
        let pin_offset = |item: &Item, num: &str, angle: f64| {
            item.geom
                .pins
                .iter()
                .find(|p| p.number == num)
                .map(|p| {
                    let p = p.at.transform_offset(angle, false);
                    Point2::new(p[0], p[1])
                })
                .unwrap_or(Point2::new(0.0, 0.0))
        };
        let sat_angle = [0.0, 90.0, 180.0, 270.0]
            .into_iter()
            .max_by(|a, b| {
                pin_offset(&items[sat], &sat_pin, *a)
                    .x
                    .total_cmp(&pin_offset(&items[sat], &sat_pin, *b).x)
            })
            .unwrap();
        items[sat].angle = sat_angle;
        items[sat].mirror = false;
        let hp = pin_offset(&items[hub], &hub_pin, items[hub].angle);
        let sp = pin_offset(&items[sat], &sat_pin, sat_angle);
        // Keep the satellite beyond the neighbouring hub pin's label strip.  A shorter lead
        // seats the resistor body over the adjacent input's pennant on 2.54-mm pin rows.
        items[sat].at = geom::GRID_50_MIL.snap_point(Point2::new(
            items[hub].at.x + hp.x - 12.7 - sp.x,
            items[hub].at.y + hp.y - sp.y,
        ));
        items[hub].frozen = true;
        items[sat].frozen = true;
    }
    true
}

/// A module = a hub + the satellites that tap it, or a lone unclustered part. Frozen items
/// stay in their own singleton (never moved, but still anchor others' targets).
fn modules(items: &[Item], inc: &Incidence, ir: &sch_place::ir::LayoutIr) -> Vec<Vec<usize>> {
    let hubs: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3)
        .collect();
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3)
        .collect();
    let blocks = build_anchor_blocks(items, inc, &hubs, &sats, ir);
    // Fold each rail-only DECOUPLING cap into the module of the IC it bypasses — `anchor_tap`
    // (and thus build_anchor_blocks) drops them because their only nets are rails (GND touches
    // everything), so without this they become singleton modules and the bank never forms.
    let mut extra: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    {
        use circuit_graph::netclass::{is_ground, is_power_net};
        let is_rail = |n: &str| ir.rails.contains_key(n) || is_power_net(n);
        let assigned: std::collections::BTreeSet<usize> =
            blocks.values().flatten().copied().collect();
        for &si in &sats {
            if assigned.contains(&si) || items[si].geom.pins.len() != 2 {
                continue;
            }
            let nets: Vec<&str> = items[si]
                .pins
                .iter()
                .filter_map(|(_, _, n)| n.as_deref())
                .collect();
            if nets.len() != 2 || !nets.iter().all(|n| is_rail(n)) {
                continue;
            }
            let Some(vplus) = nets.iter().find(|n| !is_ground(n)) else {
                continue;
            };
            let ic = inc
                .get(*vplus)
                .into_iter()
                .flatten()
                .map(|&(j, _)| j)
                .filter(|&j| items[j].geom.pins.len() >= 4)
                .fold(BTreeMap::<usize, usize>::new(), |mut m, j| {
                    *m.entry(j).or_default() += 1;
                    m
                })
                .into_iter()
                .max_by_key(|&(_, c)| c)
                .map(|(j, _)| j);
            if let Some(ic) = ic {
                extra.entry(ic).or_default().push(si);
            }
        }
    }
    let mut in_module = vec![false; items.len()];
    let mut out = Vec::new();
    for &h in &hubs {
        let mut m = vec![h];
        in_module[h] = true;
        for &s in blocks
            .get(&h)
            .into_iter()
            .flatten()
            .chain(extra.get(&h).into_iter().flatten())
        {
            if !in_module[s] {
                m.push(s);
                in_module[s] = true;
            }
        }
        out.push(m);
    }
    for (i, &in_mod) in in_module.iter().enumerate() {
        if !in_mod {
            out.push(vec![i]);
        }
    }
    out
}

/// Module adjacency: `weight[a][b]` = number of SIGNAL-net incidences shared between
/// modules a and b. POWER/GROUND rails are EXCLUDED — they touch nearly every module, so
/// counting them swamps the matrix into a uniform all-attracts-all field that carries no
/// placement signal (the reason a naive pull does nothing). What's left is the signal
/// connectivity that should set who sits next to whom (`order_anchors`' rule).
fn module_adjacency(
    mods: &[Vec<usize>],
    inc: &Incidence,
    mod_of: &[usize],
    ir: &sch_place::ir::LayoutIr,
) -> Vec<BTreeMap<usize, f64>> {
    use circuit_graph::netclass::{is_ground, is_power_net};
    let mut adj: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); mods.len()];
    for (net, pins) in inc.iter() {
        // GROUND touches everything → no placement signal, skip. A non-ground POWER rail
        // (3V3/VBUS) does carry weak signal — it keeps a power-delivery pair (regulator ↔
        // the IC it feeds) together — so weight it low; SIGNAL nets dominate (`order_anchors`).
        if is_ground(net) {
            continue;
        }
        let w = if ir.rails.contains_key(net) || is_power_net(net) {
            0.25
        } else {
            1.0
        };
        let ms: Vec<usize> = pins
            .iter()
            .filter_map(|&(i, _)| (mod_of[i] != usize::MAX).then_some(mod_of[i]))
            .collect();
        for a in 0..ms.len() {
            for b in (a + 1)..ms.len() {
                if ms[a] != ms[b] {
                    *adj[ms[a]].entry(ms[b]).or_default() += w;
                    *adj[ms[b]].entry(ms[a]).or_default() += w;
                }
            }
        }
    }
    adj
}

/// HOLISTIC re-placement — the structural answer to the de-sprawl wall. Every incremental
/// pass fails because it edits the SA's placed sheet, where the space beside an IC is already
/// occupied. Instead, lay each module out CLEANLY IN ISOLATION (its hub + a decoupling bank
/// above it + its other satellites in their SA-relative spots — no neighbours to collide),
/// take that module's footprint, then PACK the footprints left→right in connectivity order,
/// non-overlapping by construction. The banked caps land beside their IC AND nothing
/// collides, because the room was reserved in the footprint before packing.
fn holistic_relayout(
    items: &mut [Item],
    inc: &Incidence,
    ir: &sch_place::ir::LayoutIr,
    gut: f64,
) -> bool {
    use circuit_graph::netclass::{is_ground, is_power_net};
    let mods = modules(items, inc, ir);
    if mods.len() < 2 {
        return false;
    }
    let is_rail = |n: &str| ir.rails.contains_key(n) || is_power_net(n);
    // Per module: the hub (largest-pin item) and each member's offset from the hub origin.
    // Decoupling caps (2-pin, both rails) are re-laid into a grid above the hub; everything
    // else keeps its SA-relative offset.
    let mut mod_of = vec![usize::MAX; items.len()];
    for (mi, m) in mods.iter().enumerate() {
        for &i in m {
            mod_of[i] = mi;
        }
    }
    struct Placed {
        members: Vec<usize>,
        off: Vec<Point2>, // local offset of each member from the module origin
        ang: Vec<f64>,
        w: f64,
        h: f64,
    }
    let mut placed: Vec<Placed> = Vec::new();
    for m in &mods {
        let hub = *m.iter().max_by_key(|&&i| items[i].geom.pins.len()).unwrap();
        let hp = items[hub].at;
        let caps: Vec<usize> = m
            .iter()
            .copied()
            .filter(|&i| {
                items[i].geom.pins.len() == 2 && {
                    let nets: Vec<&str> = items[i]
                        .pins
                        .iter()
                        .filter_map(|(_, _, n)| n.as_deref())
                        .collect();
                    nets.len() == 2
                        && nets.iter().all(|n| is_rail(n))
                        && nets.iter().any(|n| !is_ground(n))
                }
            })
            .collect();
        let hb = item_rect(&items[hub], items[hub].at);
        let (hw, hh) = (hb.max_x - hb.min_x, hb.max_y - hb.min_y);
        // Bank grid for the caps, just above the hub, centred on it. The pitch must clear a
        // cap's VALUE label (e.g. "100nF" extends ~8mm right), or the caps collide their own
        // labels — the real cause of the bank's warnings. ~12.7mm columns / rows do it.
        const BANK_COL: f64 = 15.24; // 12 grid — clears a cap's value label + power glyph
        const BANK_ROW: f64 = 20.32; // 16 grid — a cap renders as a tall 3V3/cap/GND leg
        let ncap = caps.len();
        // A SINGLE ROW of vertical cap legs reads cleanest (the classic decoupling row beside
        // the IC) for a small bank; a LARGE bank (a DDR's 24 caps) in one row gets absurdly
        // wide and re-sprawls, so cap the width at 6 and let it wrap (BANK_ROW clears the legs).
        let ncols = if ncap == 0 {
            1
        } else if ncap <= 8 {
            ncap
        } else {
            6
        };
        let nrows = ncap.div_ceil(ncols.max(1));
        let mut off = Vec::with_capacity(m.len());
        let mut ang = Vec::with_capacity(m.len());
        let cap_angle = caps
            .first()
            .map(|&c| orient_angle(&items[c].geom, sch_place::ir::Orient::Down));
        for &i in m {
            if let Some(k) = caps.iter().position(|&c| c == i) {
                let (col, row) = (k % ncols, k / ncols);
                let gx = (col as f64 - (ncols as f64 - 1.0) / 2.0) * BANK_COL;
                let gy = -hh / 2.0 - BANK_ROW * (1.0 + (nrows - 1 - row) as f64);
                off.push(Point2::new(gx, gy));
                ang.push(cap_angle.unwrap_or(items[i].angle));
            } else {
                off.push(Point2::new(items[i].at.x - hp.x, items[i].at.y - hp.y));
                ang.push(items[i].angle);
            }
        }
        // Footprint of this internal layout. `item_rect` covers the part's own body+text but
        // NOT the power-symbol glyphs (a 3V3 arrow / GND triangle) and net-label pennants the
        // writer draws at each pin AFTER placement — so inflate each rect by that overhang, or
        // the packing reserves too little room and adjacent modules collide those glyphs.
        const OVERHANG: f64 = 7.62; // 6 grid
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (k, &i) in m.iter().enumerate() {
            let r = item_rect(&items[i], Point2::new(off[k].x, off[k].y)).inflate(OVERHANG);
            x0 = x0.min(r.min_x);
            y0 = y0.min(r.min_y);
            x1 = x1.max(r.max_x);
            y1 = y1.max(r.max_y);
        }
        // Shift offsets so the footprint min-corner is the module origin.
        for o in &mut off {
            o.x -= x0;
            o.y -= y0;
        }
        let _ = (hw, hh);
        placed.push(Placed {
            members: m.clone(),
            off,
            ang,
            w: x1 - x0,
            h: y1 - y0,
        });
    }
    // Pack the module footprints left→right in connectivity order, wrapping shelves.
    let adj = module_adjacency(&mods, inc, &mod_of, ir);
    let order = {
        // Greedy: most-connected first, then nearest-connected.
        let n = placed.len();
        let mut placed_o = vec![false; n];
        let mut ord = Vec::new();
        let start = (0..n).max_by(|&a, &b| {
            adj[a]
                .values()
                .sum::<f64>()
                .total_cmp(&adj[b].values().sum::<f64>())
        });
        if let Some(s) = start {
            ord.push(s);
            placed_o[s] = true;
            while ord.len() < n {
                let pick = (0..n)
                    .filter(|&c| !placed_o[c])
                    .max_by(|&a, &b| {
                        let ta: f64 = adj[a]
                            .iter()
                            .filter(|(m, _)| placed_o[**m])
                            .map(|(_, &w)| w)
                            .sum();
                        let tb: f64 = adj[b]
                            .iter()
                            .filter(|(m, _)| placed_o[**m])
                            .map(|(_, &w)| w)
                            .sum();
                        ta.total_cmp(&tb)
                    })
                    .unwrap();
                ord.push(pick);
                placed_o[pick] = true;
            }
        }
        ord
    };
    let total_area: f64 = placed.iter().map(|p| p.w * p.h).sum();
    let widest = placed.iter().map(|p| p.w).fold(0.0, f64::max);
    let target_w = (total_area.sqrt() * 1.15).max(widest);
    // `gut` (the inter-module gap) reserves the net-label pennants + inter-module wire channel
    // that `item_rect` footprints omit. It is SEARCHED by the caller (tightening per board until
    // the rendered sheet would collide), so a loosely-coupled board packs dense while a dense
    // one stays loose — the only collision-free way to push sprawl toward the human ~24.
    let (mut cx, mut cy, mut row_h, margin) = (12.7_f64, 12.7_f64, 0.0_f64, 12.7_f64);
    for &mi in &order {
        let p = &placed[mi];
        if cx > margin && cx + p.w > target_w {
            cx = margin;
            cy += row_h + gut;
            row_h = 0.0;
        }
        let origin = Point2::new(cx, cy);
        for (k, &i) in p.members.iter().enumerate() {
            items[i].at = geom::GRID_50_MIL
                .snap_point(Point2::new(origin.x + p.off[k].x, origin.y + p.off[k].y));
            items[i].angle = p.ang[k];
        }
        cx += p.w + gut;
        row_h = row_h.max(p.h);
    }
    true
}

/// "MODULES BETWEEN RAILS" — the layout that lets a shared power trunk replace the distributed
/// per-pin power glyphs (the dominant residual-sprawl source). Find the power net the most ICs
/// share, stand those ICs UPRIGHT in a single TOP-ALIGNED row (so their supply pins land at a
/// common Y, the trunk a clean horizontal line above them), and shelf-pack everything else
/// below. Returns the rail net for the caller to `rail_force` + gate; `None` if no group of ≥3
/// ICs shares a non-ground rail. Mutates `items` in place — the caller snapshots first so a
/// colliding result reverts.
pub(crate) fn rail_relayout(
    items: &mut [Item],
    inc: &Incidence,
    ir: &sch_place::ir::LayoutIr,
) -> Option<String> {
    use circuit_graph::netclass::{is_ground, is_power_net};
    let is_rail = |n: &str| ir.rails.contains_key(n) || is_power_net(n);
    // Dominant non-ground power rail = the one the most ≥4-pin ICs tap. Only ACTIVE ICs
    // (refdes "U") qualify as row members: the "modules between rails" geometry stands ICs in a
    // row with their power pins facing UP to the trunk, which fits chips — NOT connectors (whose
    // pins face the sheet edge as ports) or switches. Counting a single MCU's connectors as
    // "ICs" would fire the idiom on a board it can't lay out, then waste a realize reverting it.
    let mut tally: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.geom.pins.len() < 4 || !it.refdes.starts_with('U') {
            continue;
        }
        let mut seen = std::collections::BTreeSet::new();
        for n in it.pins.iter().filter_map(|(_, _, n)| n.as_deref()) {
            if is_rail(n) && !is_ground(n) && seen.insert(n.to_string()) {
                tally.entry(n.to_string()).or_default().push(i);
            }
        }
    }
    let (rail, rail_ics) = tally.into_iter().max_by_key(|(_, v)| v.len())?;
    if rail_ics.len() < 3 {
        return None;
    }
    let mods = modules(items, inc, ir);
    let rail_set: std::collections::BTreeSet<usize> = rail_ics.iter().copied().collect();
    // Partition modules: those whose hub is a rail IC go in the row; the rest pack below.
    let mut row: Vec<&Vec<usize>> = Vec::new();
    let mut rest: Vec<&Vec<usize>> = Vec::new();
    for m in &mods {
        let hub = *m.iter().max_by_key(|&&i| items[i].geom.pins.len()).unwrap();
        if rail_set.contains(&hub) {
            row.push(m);
        } else {
            rest.push(m);
        }
    }
    const MARGIN: f64 = 12.7;
    const ROW_Y: f64 = 30.48; // leave a band above for the trunk
    const GUT: f64 = 10.16;
    const CAP_PITCH: f64 = 12.7; // clears a cap's value label
    // A "rail leg" is any 2-pin part touching the trunk net (decoupling caps AND bootstrap
    // diodes): it MUST go in an inter-IC gap so its supply pin reaches the trunk straight up —
    // placed below the IC its riser would route up THROUGH the body (the residual crossings).
    let is_decouple = |it: &Item| {
        it.geom.pins.len() == 2
            && it
                .pins
                .iter()
                .any(|(_, _, n)| n.as_deref() == Some(rail.as_str()))
    };
    // The rail's decoupling caps (often ALL folded onto one IC since they share the rail) are
    // pooled and DISTRIBUTED evenly across the inter-IC gaps as upright legs, so their supply
    // pin reaches the trunk straight up without crossing any IC body and no two labels collide.
    let hubs: Vec<usize> = row
        .iter()
        .map(|m| *m.iter().max_by_key(|&&i| items[i].geom.pins.len()).unwrap())
        .collect();
    let hub_set: std::collections::BTreeSet<usize> = hubs.iter().copied().collect();
    // Pool EVERY rail-leg (even a bulk cap that `modules` left an off-rail singleton) into the
    // gaps — any part touching the trunk that sits below an IC routes its riser up through it.
    let caps: Vec<usize> = (0..items.len())
        .filter(|&i| !hub_set.contains(&i) && is_decouple(&items[i]))
        .collect();
    let cap_set: std::collections::BTreeSet<usize> = caps.iter().copied().collect();
    let mut others: Vec<usize> = Vec::new();
    let mut hub_of: Vec<usize> = vec![usize::MAX; items.len()]; // satellite → its row index
    for (idx, m) in row.iter().enumerate() {
        for &i in *m {
            if hub_set.contains(&i) || cap_set.contains(&i) {
                continue;
            }
            others.push(i);
            hub_of[i] = idx;
        }
    }
    let cap_angle = caps
        .first()
        .map(|&i| orient_angle(&items[i].geom, sch_place::ir::Orient::Down));
    let per_gap = caps.len().div_ceil(hubs.len().max(1));
    let mut cap_iter = caps.iter();
    let mut cx = MARGIN;
    let mut below_y: f64 = ROW_Y;
    let mut hub_x = vec![MARGIN; hubs.len()];
    for (idx, &hub) in hubs.iter().enumerate() {
        let hb = item_rect(&items[hub], items[hub].at);
        let hw = hb.max_x - hb.min_x;
        items[hub].angle = 0.0;
        items[hub].mirror = false;
        let hx = cx + hw / 2.0;
        hub_x[idx] = hx;
        items[hub].at = geom::GRID_50_MIL.snap_point(Point2::new(hx, ROW_Y));
        below_y = below_y.max(item_rect(&items[hub], items[hub].at).max_y);
        cx += hw + GUT;
        for _ in 0..per_gap {
            if let Some(&c) = cap_iter.next() {
                items[c].angle = cap_angle.unwrap_or(items[c].angle);
                items[c].mirror = false;
                items[c].at = geom::GRID_50_MIL.snap_point(Point2::new(cx, ROW_Y));
                cx += CAP_PITCH;
            }
        }
        cx += GUT;
    }
    for &c in cap_iter {
        items[c].angle = cap_angle.unwrap_or(items[c].angle);
        items[c].mirror = false;
        items[c].at = geom::GRID_50_MIL.snap_point(Point2::new(cx, ROW_Y));
        cx += CAP_PITCH;
    }
    // Each non-cap satellite stacks straight BELOW the IC PIN it shares a net with, so its wire
    // drops down to that pin without angling across the body (the residual IC crossings).
    let mut stack_y = vec![below_y + GUT + 7.62; hubs.len()];
    for &i in &others {
        let idx = hub_of[i];
        let hub = hubs[idx];
        let i_nets: std::collections::BTreeSet<&str> = items[i]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.as_deref())
            .collect();
        let px = items[hub]
            .pins
            .iter()
            .position(|(_, _, n)| n.as_deref().is_some_and(|nn| i_nets.contains(nn)))
            .and_then(|k| items[hub].geom.pins.get(k))
            .map_or(hub_x[idx], |gp| items[hub].at.x + gp.at.x);
        items[i].angle = 0.0;
        items[i].mirror = false;
        items[i].at = geom::GRID_50_MIL.snap_point(Point2::new(px, stack_y[idx]));
        let h = item_rect(&items[i], items[i].at);
        stack_y[idx] = h.max_y + GUT;
        below_y = below_y.max(h.max_y);
    }
    // Shelf-pack the rest below the rail row.
    let total_w = (cx - MARGIN).max(1.0);
    let (mut px, mut py, mut row_h) = (MARGIN, below_y + GUT, 0.0_f64);
    for m in &rest {
        // Rail-leg parts (incl. bulk caps) were already pooled into the gaps; skip them here,
        // and skip a module that was nothing BUT rail legs.
        if m.iter().all(|i| cap_set.contains(i)) {
            continue;
        }
        let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for &i in m.iter().filter(|i| !cap_set.contains(i)) {
            let r = item_rect(&items[i], items[i].at);
            x0 = x0.min(r.min_x);
            x1 = x1.max(r.max_x);
            y0 = y0.min(r.min_y);
            y1 = y1.max(r.max_y);
        }
        let (mw, mh) = (x1 - x0, y1 - y0);
        if px > MARGIN && px + mw > MARGIN + total_w {
            px = MARGIN;
            py += row_h + GUT;
            row_h = 0.0;
        }
        let shift = Point2::new(px - x0, py - y0);
        for &i in m.iter().filter(|i| !cap_set.contains(i)) {
            items[i].at = geom::GRID_50_MIL.snap_point(Point2::new(
                items[i].at.x + shift.x,
                items[i].at.y + shift.y,
            ));
        }
        px += mw + GUT;
        row_h = row_h.max(mh);
    }
    Some(rail)
}

/// De-sprawl the sheet toward the human distribution via the [`holistic_relayout`]
/// floorplanner, kept ONLY when it clearly lowers the RENDERED sprawl (the feature the corpus
/// says most separates human from machine) WITHOUT regressing the shipped (truthfulness,
/// warnings, crossings). So it optimises the thing humans do better — less whitespace, fewer
/// islands, decoupling beside its IC — while the routed gate guarantees it never ships a
/// more-tangled or colliding sheet than the SA (and reverts on the boards it can't improve).
///
/// `baseline_rendered` / `sa_warnings` are the SA placement's [`rendered_sprawl`] and shipped
/// warning count (before pose) — measured the SAME way as each candidate, through
/// [`RoutedEvaluator::rendered`], so BOTH include the emit's post-`place()` orphan label-columns.
/// That closes the last blindness: the floorplanner packs modules with gutters so inter-module
/// nets become edge labels, which on a dense board balloon the rendered bbox far past the raw
/// geometry — invisible to an origin-bbox gate. Now the gate sees the true shipped extent.
///
/// DENSITY SEARCH: human sheets are ~3× tighter than the SA, but how tight a board can pack
/// before its labels collide is board-specific (a loosely-coupled board packs dense; a dense
/// signal-coupled one balloons). So sweep the inter-module gutter tight→loose and keep the
/// TIGHTEST rendering that ships no new warnings, no routed regression, and a real de-sprawl.
pub(crate) fn compact_clusters(
    eval: &RoutedEvaluator,
    design: &Design,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    baseline_rendered: f64,
    sa_warnings: usize,
) {
    use circuit_graph::netclass::is_power_net;
    let force = false;
    let debug = super::DEBUG_DIAGNOSTICS;
    let base = save(items);
    let s0 = score(eval, inc, ir, items);
    let is_rail = |n: &str| ir.rails.contains_key(n) || is_power_net(n);
    // Repeated channels deserve a structural proposal before the generic module packer:
    // preserving SA-relative offsets is precisely what turns an isomorphic bank into islands.
    // This uses the same shipped gate as every other compact candidate and restores `base` on
    // any warning/crossing regression.
    if repeated_channel_relayout(items, inc, ir) {
        let s = score(eval, inc, ir, items);
        let rendered = eval
            .rendered(design, items)
            .map(|(w, rect)| (w, rendered_sprawl(&rect, items.len())));
        let ok = rendered.is_some_and(|(w, spr)| {
            (s.0, s.1, s.2) <= (s0.0, s0.1, s0.2)
                && w <= sa_warnings
                && spr + 1e-3 < baseline_rendered
        });
        if debug {
            tracing::debug!(
                "[channels] rendered={rendered:?} routed={:?} ok={ok}",
                (s.0, s.1, s.2)
            );
        }
        if ok {
            return;
        }
        restore(items, &base);
    }
    // Tightest collision-free pack wins. Warnings rise monotonically as the gutter tightens
    // (denser ⇒ more label collisions), so sweep tight→loose and TAKE THE FIRST gutter whose
    // rendering ships no new warnings, no routed regression, and a real de-sprawl — it is the
    // tightest acceptable one. If none qualifies, revert to the SA (post-pose) placement.
    let mut kept: Option<crate::eval::Snap> = None;
    for (idx, gut) in [7.62_f64, 10.16, 12.7, 15.24, 17.78, 20.32]
        .into_iter()
        .enumerate()
    {
        restore(items, &base);
        if !holistic_relayout(items, inc, ir, gut) {
            return; // fewer than 2 modules — nothing to pack, on any gutter
        }
        // Freeze the banked decoupling caps so the emit's gather pile can't re-row them.
        for it in items.iter_mut() {
            let rail_cap = it.geom.pins.len() == 2
                && it
                    .pins
                    .iter()
                    .filter_map(|(_, _, n)| n.as_deref())
                    .filter(|n| is_rail(n))
                    .count()
                    == 2;
            if rail_cap {
                it.frozen = true;
            }
        }
        // Apply the finalize the emit realizes directly (decongest + idiom/LED re-seat).
        decongest(items);
        if align_idiom_clusters(items, ir) {
            decongest(items);
        }
        if align_led_chains(items, inc, ir) {
            decongest(items);
        }
        let s = score(eval, inc, ir, items);
        let (w, spr) = match eval.rendered(design, items) {
            Some((w, rect)) => (w, rendered_sprawl(&rect, items.len())),
            None => continue,
        };
        let ok = (s.0, s.1, s.2) <= (s0.0, s0.1, s0.2)
            && w <= sa_warnings
            && spr + 1e-3 < baseline_rendered;
        if debug {
            tracing::debug!(
                "[holistic] gut={gut:.1} rendered={spr:.1} w={w} routed={:?} ok={ok}",
                (s.0, s.1, s.2)
            );
        }
        if ok {
            kept = Some(save(items));
            break;
        }
        // The tightest gutter is the minimum-sprawl pack; if even IT can't beat the SA's
        // rendered sprawl, no looser gutter will — skip the remaining trials and revert.
        if idx == 0 && spr >= baseline_rendered {
            break;
        }
    }
    match kept {
        Some(snap) => restore(items, &snap),
        None if !force => restore(items, &base),
        None => {}
    }
}
