//! The holistic floorplanner — the single-sheet DE-SPRAWL pass. Env-gated (`CLUSTER_COMPACT`)
//! pending broad validation; the default `cluster` engine is pose-only.
//!
//! A human-vs-machine logistic fit over the 500 boards (`tools/learn_layout.py`) showed the
//! engine's #1 deficiency is SPRAWL (humans ~24, the SA ~69 — 3× too spread) + ISLAND-scatter
//! (2.4× more clusters): connected parts the SA leaves far apart, chiefly the `decouple:`
//! sugar's caps strung in a far row instead of hugging their IC. [`holistic_relayout`] fixes
//! it the way the field does — NOT by editing the SA's placed sheet (where the space beside an
//! IC is already occupied, so every incremental re-bank/force/scale collides or spreads), but
//! by laying each MODULE out cleanly IN ISOLATION (hub + a single-row decoupling bank above it
//! + its satellites), taking that module's footprint, and PACKING the footprints in
//! connectivity order. The decoupling bank lands beside its IC AND nothing collides, because
//! the room was reserved before packing.
//!
//! Two details were load-bearing (each cost real warnings until fixed): (1) the decoupling
//! caps must form ONE module (`modules` folds rail-only caps into the IC on their V+ rail —
//! `anchor_tap` drops them as singletons because GND touches everything), laid in a SINGLE
//! ROW (a grid collides the tall 3V3/cap/GND legs' labels); (2) the footprint must inflate by
//! the power-glyph / net-label PENNANT overhang `item_rect` omits, or packed modules collide
//! those glyphs. With both, a clean MCU board goes 48→34 sprawl (−29%, toward human), 0
//! warnings; 0cdac 68→54. The gate ([`compact_clusters`]) keeps it only when it strictly
//! de-sprawls without regressing the routed metrics — so a dual-IC / huge-bank board where the
//! single-row bank gets too wide (411be) is safely left to the SA.

use std::collections::BTreeMap;

use circuit_lang::model::Design;
use geom::{Point2, Rect};
use sch_place::ir::LayoutIr;
use sch_place::item::{Incidence, Item};

use sch_floorplan::contract::{
    RoutedEvaluator, align_idiom_clusters, align_led_chains, build_anchor_blocks, decongest,
    item_rect, orient_angle,
};

use crate::eval::{restore, save, score};

/// Whitespace per part of a RENDERED extent — the same `bbox_area / (n·cell)` ratio the
/// validation oracle uses, but on [`RoutedEvaluator::rendered_extent`] (post text-solve +
/// orphan label-columns), so it reflects what actually ships.
pub(crate) fn rendered_sprawl(extent: &Rect, n: usize) -> f64 {
    const CELL: f64 = 6.35 * 5.08;
    (extent.max_x - extent.min_x) * (extent.max_y - extent.min_y) / (n.max(1) as f64 * CELL)
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
        use sch_place::netclass::{is_ground, is_power_net};
        let is_rail = |n: &str| ir.rails.contains_key(n) || is_power_net(n);
        let assigned: std::collections::BTreeSet<usize> =
            blocks.values().flatten().copied().collect();
        for &si in &sats {
            if assigned.contains(&si) || items[si].geom.pins.len() != 2 {
                continue;
            }
            let nets: Vec<&str> = items[si].pins.iter().filter_map(|(_, _, n)| n.as_deref()).collect();
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
        for &s in blocks.get(&h).into_iter().flatten().chain(extra.get(&h).into_iter().flatten()) {
            if !in_module[s] {
                m.push(s);
                in_module[s] = true;
            }
        }
        out.push(m);
    }
    for i in 0..items.len() {
        if !in_module[i] {
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
    use sch_place::netclass::{is_ground, is_power_net};
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
fn holistic_relayout(items: &mut [Item], inc: &Incidence, ir: &sch_place::ir::LayoutIr) -> bool {
    use sch_place::netclass::{is_ground, is_power_net};
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
        let hub = *m
            .iter()
            .max_by_key(|&&i| items[i].geom.pins.len())
            .unwrap();
        let hp = items[hub].at;
        let caps: Vec<usize> = m
            .iter()
            .copied()
            .filter(|&i| {
                items[i].geom.pins.len() == 2 && {
                    let nets: Vec<&str> =
                        items[i].pins.iter().filter_map(|(_, _, n)| n.as_deref()).collect();
                    nets.len() == 2 && nets.iter().all(|n| is_rail(n)) && nets.iter().any(|n| !is_ground(n))
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
        let ncols = if ncap == 0 { 1 } else if ncap <= 8 { ncap } else { 6 };
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
            adj[a].values().sum::<f64>().total_cmp(&adj[b].values().sum::<f64>())
        });
        if let Some(s) = start {
            ord.push(s);
            placed_o[s] = true;
            while ord.len() < n {
                let pick = (0..n)
                    .filter(|&c| !placed_o[c])
                    .max_by(|&a, &b| {
                        let ta: f64 = adj[a].iter().filter(|(m, _)| placed_o[**m]).map(|(_, &w)| w).sum();
                        let tb: f64 = adj[b].iter().filter(|(m, _)| placed_o[**m]).map(|(_, &w)| w).sum();
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
    // Wide gutter: item_rect footprints DON'T include the net-label pennants the text solver
    // draws at each connecting pin, so the inter-module gap must reserve that pennant + the
    // channel for inter-module wires, or packed modules collide their labels.
    const GUT: f64 = 17.78;
    let (mut cx, mut cy, mut row_h, margin) = (12.7_f64, 12.7_f64, 0.0_f64, 12.7_f64);
    for &mi in &order {
        let p = &placed[mi];
        if cx > margin && cx + p.w > target_w {
            cx = margin;
            cy += row_h + GUT;
            row_h = 0.0;
        }
        let origin = Point2::new(cx, cy);
        for (k, &i) in p.members.iter().enumerate() {
            items[i].at = geom::GRID_50_MIL
                .snap_point(Point2::new(origin.x + p.off[k].x, origin.y + p.off[k].y));
            items[i].angle = p.ang[k];
        }
        cx += p.w + GUT;
        row_h = row_h.max(p.h);
    }
    true
}

/// De-sprawl the sheet toward the human distribution via the [`holistic_relayout`]
/// floorplanner, kept ONLY when it clearly lowers the RENDERED sprawl (the feature the corpus
/// says most separates human from machine) WITHOUT regressing the shipped (truthfulness,
/// warnings, crossings). So it optimises the thing humans do better — less whitespace, fewer
/// islands, decoupling beside its IC — while the routed gate guarantees it never ships a
/// more-tangled or colliding sheet than the SA (and reverts on the boards it can't improve).
///
/// `baseline_rendered` is the SA placement's [`rendered_sprawl`] (before pose) — measured the
/// SAME way as the candidate, through [`RoutedEvaluator::rendered_extent`], so BOTH include the
/// emit's post-`place()` orphan label-columns. That closes the last blindness: the floorplanner
/// packs modules with gutters so inter-module nets become edge labels, which on a dense board
/// balloon the rendered bbox far past the raw geometry — invisible to an origin-bbox gate, so
/// such boards used to regress unseen; now the gate sees the true shipped extent and reverts.
pub(crate) fn compact_clusters(
    eval: &RoutedEvaluator,
    design: &Design,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    baseline_rendered: f64,
) {
    let force = std::env::var_os("CLUSTER_FORCE").is_some();
    let base = save(items);
    let s0 = score(eval, inc, ir, items);
    if !holistic_relayout(items, inc, ir) {
        return;
    }
    // FREEZE just the decoupling caps the floorplanner banked: the emit's post-`place()`
    // gather pile (align_rail_cap_rows, gather_banked_decoupling) would otherwise re-row them
    // and collide the clean single-row bank. Freezing ONLY the caps stops that while leaving
    // every other part mutable so `decongest` can still clear residual overlaps.
    {
        use sch_place::netclass::is_power_net;
        let is_rail = |n: &str| ir.rails.contains_key(n) || is_power_net(n);
        for it in items.iter_mut() {
            let rail_cap = it.geom.pins.len() == 2
                && it.pins.iter().filter_map(|(_, _, n)| n.as_deref()).filter(|n| is_rail(n)).count() == 2;
            if rail_cap {
                it.frozen = true;
            }
        }
    }
    // Apply the SAME finalize the gate's `score` clone runs (decongest + idiom/LED re-seat) to
    // the REAL items — the emit realizes these directly without re-running it, so without this
    // the gate would judge a cleaner aligned layout than actually ships (the score-clone≠emit
    // blindness that let dense boards regress unseen). Now the gate measures the ship.
    decongest(items);
    if align_idiom_clusters(items, ir) {
        decongest(items);
    }
    if align_led_chains(items, inc, ir) {
        decongest(items);
    }
    let s = score(eval, inc, ir, items);
    // The SHIPPED sprawl: the rendered extent INCLUDING the emit's orphan label-columns (the
    // measure that finally matches what ships). Keep only a clear win (>=10% under the SA),
    // never a routed regression. A 5% margin guards float/measurement noise.
    let hol = eval
        .rendered_extent(design, items)
        .map(|r| rendered_sprawl(&r, items.len()))
        .unwrap_or(f64::MAX);
    let keep = (s.0, s.1, s.2) <= (s0.0, s0.1, s0.2) && hol < 0.90 * baseline_rendered;
    if std::env::var_os("CLUSTER_DEBUG").is_some() {
        eprintln!(
            "[holistic] rendered sprawl baseline={baseline_rendered:.1} -> {hol:.1}  tb/w/x {:?} (base {:?})  keep={keep}",
            (s.0, s.1, s.2),
            (s0.0, s0.1, s0.2)
        );
    }
    if !keep && !force {
        restore(items, &base);
    }
}
