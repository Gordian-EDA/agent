//! Module shelf-packing — the WIP path through the sprawl wall, env-gated (`CLUSTER_COMPACT`)
//! and OFF by default because it is not yet a net win.
//!
//! The dominant critic defect on a single sheet is sprawl: a lifted analog board names every
//! net, so most connections are LABELS, and the SA's HPWL cost counts only DRAWN wires — a
//! label-connected part exerts ZERO placement pull, so the SA scatters such parts across the
//! whole sheet. The fix is NOT a force pull (pure attraction collapses every module onto a
//! point → congestion, reconfirmed) but SLOT packing: each module is a rigid box laid in
//! connectivity order on left→right shelves, non-overlapping BY CONSTRUCTION.
//!
//! What's still missing to make it WIN (documented for the next pass): (1) the module
//! FOOTPRINT must include the NET-LABEL PENNANTS the text solver draws at each connecting pin
//! AFTER placement — `item_rect` reserves the part's own text but not those, so packed
//! modules still collide labels; (2) each module's INTERNAL layout must be cleanly TEMPLATED
//! first (the SA's scattered intra-module geometry just compresses into overlaps when packed).
//! Both are the same clean-vs-compact wall every prior compaction hit. Until they're solved,
//! the additive gate in [`compact_clusters`] reverts this on real boards — it never ships worse.

use std::collections::BTreeMap;

use geom::Point2;
use sch_place::ir::LayoutIr;
use sch_place::item::{Incidence, Item};

use sch_floorplan::contract::{
    RoutedEvaluator, build_anchor_blocks, decongest, item_rect, orient_angle,
};

use crate::eval::{improves, restore, save, score};

/// Label-safe pitch between banked caps (6 grid).
const BANK_PITCH: f64 = 7.62;

/// BANK each IC's decoupling caps beside it — the #1 single-sheet defect the SA leaves
/// (the `decouple:` sugar's caps get strung in a far row instead of hugging the IC they
/// bypass, because rail-only caps have no signal wire to pull them home). For each IC,
/// the rail-only caps `cohesion_targets` assigns to its supply pins are laid in a compact
/// near-square grid centred on the IC's supply-pin column, just above its top edge, each a
/// clean vertical leg. With the IC's actual decoupling beside it the sheet reads as a
/// proper module; `decongest` + the additive gate keep it only when it doesn't crowd.
fn bank_decoupling(items: &mut [Item], inc: &Incidence, ir: &sch_place::ir::LayoutIr) -> bool {
    use sch_place::netclass::{is_ground, is_power_net};
    let is_rail = |n: &str| ir.rails.contains_key(n) || is_power_net(n);
    // Decoupling caps are FROZEN by the idiom system (so `cohesion_targets` skips them) and
    // seated in a band that can land far from their IC — so find them + their IC DIRECTLY:
    // a 2-pin cap whose pins are both rails, assigned to the anchor (≥4 pins) carrying the
    // most pins on its V+ (non-ground) rail.
    let mut by_ic: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for si in 0..items.len() {
        if items[si].geom.pins.len() != 2 {
            continue;
        }
        let nets: Vec<String> = items[si]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.clone())
            .collect();
        if nets.len() != 2 || !nets.iter().all(|n| is_rail(n)) {
            continue;
        }
        let vplus = nets.iter().find(|n| !is_ground(n));
        let Some(vplus) = vplus else { continue }; // both ground: not a bypass cap
        // The anchor with the most pins on this V+ rail is the IC this cap bypasses.
        let best = inc
            .get(vplus)
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
        if let Some(ai) = best {
            by_ic.entry(ai).or_default().push(si);
        }
    }
    let mut moved = false;
    for (ai, caps) in by_ic {
        if caps.len() < 2 {
            continue;
        }
        let hb = item_rect(&items[ai], items[ai].at);
        let n = caps.len();
        // A compact near-square grid (≈2 rows) hugging the IC, with GENEROUS vertical pitch
        // so a cap's value label never abuts the row above it (the crowding that sank the
        // first 2-row attempt).
        let ncols = (n as f64 / 2.0).ceil().max(1.0) as usize;
        let nrows = n.div_ceil(ncols);
        let row_gap = BANK_PITCH * 2.0; // 12 grid between rows so labels never abut
        let grid_w = (ncols as f64 - 1.0) * BANK_PITCH;
        let cx0 = (hb.min_x + hb.max_x) / 2.0 - grid_w / 2.0;
        let angle = orient_angle(&items[caps[0]].geom, sch_place::ir::Orient::Down);
        for (k, &si) in caps.iter().enumerate() {
            let (col, row) = (k % ncols, k / ncols);
            let x = cx0 + col as f64 * BANK_PITCH;
            let y = hb.min_y - BANK_PITCH - (nrows - 1 - row) as f64 * row_gap;
            let np = geom::GRID_50_MIL.snap_point(Point2::new(x, y));
            if items[si].at != np {
                items[si].at = np;
                moved = true;
            }
            items[si].angle = angle;
            items[si].mirror = false;
        }
    }
    moved
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
    let mut in_module = vec![false; items.len()];
    let mut out = Vec::new();
    for &h in &hubs {
        let mut m = vec![h];
        in_module[h] = true;
        if let Some(b) = blocks.get(&h) {
            for &s in b {
                if !in_module[s] {
                    m.push(s);
                    in_module[s] = true;
                }
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
    use sch_place::netclass::is_power_net;
    let mut adj: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); mods.len()];
    for (net, pins) in inc.iter() {
        if ir.rails.contains_key(net) || is_power_net(net) {
            continue;
        }
        let ms: Vec<usize> = pins
            .iter()
            .filter_map(|&(i, _)| (mod_of[i] != usize::MAX).then_some(mod_of[i]))
            .collect();
        for a in 0..ms.len() {
            for b in (a + 1)..ms.len() {
                if ms[a] != ms[b] {
                    *adj[ms[a]].entry(ms[b]).or_default() += 1.0;
                    *adj[ms[b]].entry(ms[a]).or_default() += 1.0;
                }
            }
        }
    }
    adj
}

/// Greedy connectivity ORDER: start at the highest-degree module, then repeatedly append the
/// unplaced module most strongly connected to those already placed, so consecutive modules in
/// the sequence (and thus the shelf) share the most nets.
fn connectivity_order(adj: &[BTreeMap<usize, f64>]) -> Vec<usize> {
    let n = adj.len();
    let mut placed = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let Some(start) = (0..n).max_by(|&a, &b| {
        adj[a].values().sum::<f64>().total_cmp(&adj[b].values().sum::<f64>())
    }) else {
        return order;
    };
    order.push(start);
    placed[start] = true;
    while order.len() < n {
        let mut best = (f64::NEG_INFINITY, usize::MAX);
        for cand in 0..n {
            if placed[cand] {
                continue;
            }
            let tie: f64 = adj[cand]
                .iter()
                .filter(|(m, _)| placed[**m])
                .map(|(_, &w)| w)
                .sum();
            if tie > best.0 {
                best = (tie, cand);
            }
        }
        let pick = if best.1 == usize::MAX {
            (0..n).find(|&i| !placed[i]).unwrap()
        } else {
            best.1
        };
        order.push(pick);
        placed[pick] = true;
    }
    order
}

/// Shelf-pack the modules: each a rigid box with a (part-text-inclusive) footprint, laid
/// left→right in connectivity order, wrapping to new shelves with a gutter. Non-overlapping
/// by construction (no pile to congest), internal geometry preserved.
fn pack_modules(items: &mut [Item], inc: &Incidence, ir: &sch_place::ir::LayoutIr) -> bool {
    let mods = modules(items, inc, ir);
    if mods.len() < 3 || mods.iter().flatten().any(|&i| items[i].frozen) {
        return false;
    }
    let mut mod_of = vec![usize::MAX; items.len()];
    for (mi, m) in mods.iter().enumerate() {
        for &i in m {
            mod_of[i] = mi;
        }
    }
    let foot = |items: &[Item], m: &[usize]| -> geom::Rect {
        let mut r = item_rect(&items[m[0]], items[m[0]].at);
        for &i in &m[1..] {
            let ir2 = item_rect(&items[i], items[i].at);
            r = geom::Rect::new(
                r.min_x.min(ir2.min_x),
                r.min_y.min(ir2.min_y),
                r.max_x.max(ir2.max_x),
                r.max_y.max(ir2.max_y),
            );
        }
        r
    };
    let foots: Vec<geom::Rect> = mods.iter().map(|m| foot(items, m)).collect();
    let total_area: f64 = foots
        .iter()
        .map(|r| (r.max_x - r.min_x) * (r.max_y - r.min_y))
        .sum();
    let widest = foots.iter().map(|r| r.max_x - r.min_x).fold(0.0, f64::max);
    let target_w = (total_area.sqrt() * 1.6).max(widest);
    const GUT: f64 = 7.62;

    let order = connectivity_order(&module_adjacency(&mods, inc, &mod_of, ir));
    let (mut cx, mut cy, mut row_h, margin) = (12.7_f64, 12.7_f64, 0.0_f64, 12.7_f64);
    let mut moved = false;
    for &mi in &order {
        let r = foots[mi];
        let (w, h) = (r.max_x - r.min_x, r.max_y - r.min_y);
        if cx > margin && cx + w > target_w {
            cx = margin;
            cy += row_h + GUT;
            row_h = 0.0;
        }
        let (dx, dy) = (cx - r.min_x, cy - r.min_y);
        if dx.abs() > 0.01 || dy.abs() > 0.01 {
            for &i in &mods[mi] {
                let p = Point2::new(items[i].at.x + dx, items[i].at.y + dy);
                items[i].at = geom::GRID_50_MIL.snap_point(p);
            }
            moved = true;
        }
        cx += w + GUT;
        row_h = row_h.max(h);
    }
    moved
}

/// GROUP the modules by a force-directed relaxation — gentle, not tight. The SA scatters
/// connected modules into isolated islands separated by empty regions (its HPWL counts
/// only DRAWN wires, so label-connected modules feel no pull), and the islands are too far
/// apart to wire, so everything degrades to net-LABELS (the "connectivity carried by labels
/// not wires" + "scattered islands" critic defects). This pulls modules that SHARE NETS
/// together (attraction over the FULL incidence, labels included) while a footprint-sized
/// REPULSION holds them apart — so they cluster into readable groups WITHOUT collapsing
/// onto a point (the failure of a pure pull) and without packing so tight they congest. It
/// only ever closes the egregious empty gaps; the additive gate stops it before it crowds.
fn force_group(items: &mut [Item], inc: &Incidence, ir: &sch_place::ir::LayoutIr) -> bool {
    let mods = modules(items, inc, ir);
    if mods.len() < 3 || mods.iter().flatten().any(|&i| items[i].frozen) {
        return false;
    }
    let mut mod_of = vec![usize::MAX; items.len()];
    for (mi, m) in mods.iter().enumerate() {
        for &i in m {
            mod_of[i] = mi;
        }
    }
    let adj = module_adjacency(&mods, inc, &mod_of, ir);
    // Module centroid + footprint "radius" (half-diagonal of the label-inclusive bbox).
    let mut pos: Vec<Point2> = Vec::with_capacity(mods.len());
    let mut rad: Vec<f64> = Vec::with_capacity(mods.len());
    for m in &mods {
        let mut r = item_rect(&items[m[0]], items[m[0]].at);
        let (mut cx, mut cy) = (0.0, 0.0);
        for &i in m {
            cx += items[i].at.x;
            cy += items[i].at.y;
            let ir2 = item_rect(&items[i], items[i].at);
            r = geom::Rect::new(
                r.min_x.min(ir2.min_x),
                r.min_y.min(ir2.min_y),
                r.max_x.max(ir2.max_x),
                r.max_y.max(ir2.max_y),
            );
        }
        pos.push(Point2::new(cx / m.len() as f64, cy / m.len() as f64));
        let (w, h) = (r.max_x - r.min_x, r.max_y - r.min_y);
        rad.push(0.5 * (w * w + h * h).sqrt());
    }
    let start = pos.clone();
    const MARGIN: f64 = 10.16; // 8-grid breathing room between module footprints
    const ITERS: usize = 80;
    let n = mods.len();
    for it in 0..ITERS {
        let cool = 1.0 - it as f64 / ITERS as f64; // anneal the step down
        let mut disp = vec![Point2::new(0.0, 0.0); n];
        // Footprint repulsion (all pairs): strong inside the touching distance, fading out.
        for a in 0..n {
            for b in (a + 1)..n {
                let (dx, dy) = (pos[a].x - pos[b].x, pos[a].y - pos[b].y);
                let dist = (dx * dx + dy * dy).sqrt().max(1.0);
                let want = rad[a] + rad[b] + MARGIN;
                let f = (want * want) / dist; // ~want at touching, fades with distance
                disp[a].x += dx / dist * f;
                disp[a].y += dy / dist * f;
                disp[b].x -= dx / dist * f;
                disp[b].y -= dy / dist * f;
            }
        }
        // Shared-net attraction (linear in distance, weighted by #shared incidences).
        for a in 0..n {
            for (&b, &w) in &adj[a] {
                let (dx, dy) = (pos[b].x - pos[a].x, pos[b].y - pos[a].y);
                let dist = (dx * dx + dy * dy).sqrt().max(1.0);
                let f = 0.05 * w * dist;
                disp[a].x += dx / dist * f;
                disp[a].y += dy / dist * f;
            }
        }
        // Apply, capping the per-iter step so a far island eases in rather than overshoots.
        let cap = 25.4 * cool;
        for a in 0..n {
            let d = (disp[a].x * disp[a].x + disp[a].y * disp[a].y).sqrt().max(1e-6);
            let s = d.min(cap) / d;
            pos[a].x += disp[a].x * s;
            pos[a].y += disp[a].y * s;
        }
    }
    // Translate each module rigidly by its centroid delta (snap to grid).
    let mut moved = false;
    for mi in 0..n {
        let dx = geom::GRID_50_MIL.snap(pos[mi].x - start[mi].x);
        let dy = geom::GRID_50_MIL.snap(pos[mi].y - start[mi].y);
        if dx.abs() > 0.01 || dy.abs() > 0.01 {
            for &i in &mods[mi] {
                items[i].at = Point2::new(items[i].at.x + dx, items[i].at.y + dy);
            }
            moved = true;
        }
    }
    moved
}

/// Pack the modules if it does not regress the shipped sheet. Snapshot, pack, decongest,
/// score the finalized clone; keep only when it ties-or-beats truthfulness/warnings/crossings
/// and is strictly tighter — otherwise restore. On a real board the missing footprint /
/// templating (see module docs) makes it regress, so it reverts.
pub(crate) fn compact_clusters(
    eval: &RoutedEvaluator,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
) {
    let force = std::env::var_os("CLUSTER_FORCE").is_some();
    let incumbent = score(eval, inc, ir, items);
    // (1) Bank each IC's decoupling caps beside it — the #1 defect. This is a hard
    // CONVENTION (caps belong next to the IC they bypass), and because the caps connect by
    // power SYMBOLS not drawn wires the straightness cost can't see the gain — so it is
    // gated on NO-REGRESSION of the shipped (truthfulness, warnings, crossings), not on a
    // base-cost improvement it would never show.
    {
        let base = save(items);
        if bank_decoupling(items, inc, ir) {
            decongest(items);
            let s = score(eval, inc, ir, items);
            let regress = (s.0, s.1, s.2) > (incumbent.0, incumbent.1, incumbent.2);
            if regress && !force {
                restore(items, &base);
            }
        }
    }
    // (2) Group the remaining signal-connected modules (gentle force-directed) — kept only
    // on a STRICT shipped-metric improvement, since unlike the bank it has no convention
    // backing and could wander.
    {
        let base = save(items);
        let incumbent = score(eval, inc, ir, items);
        if force_group(items, inc, ir) {
            decongest(items);
            if !improves(score(eval, inc, ir, items), incumbent) && !force {
                restore(items, &base);
            }
        }
    }
}
