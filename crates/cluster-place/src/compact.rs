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

use sch_floorplan::contract::{RoutedEvaluator, build_anchor_blocks, decongest, item_rect};

use crate::eval::{improves, restore, save, score};

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

/// Module adjacency: `weight[a][b]` = number of (net,pin) incidences shared between modules
/// a and b (labels included — the connectivity the SA's drawn-wire HPWL misses).
fn module_adjacency(
    mods: &[Vec<usize>],
    inc: &Incidence,
    mod_of: &[usize],
) -> Vec<BTreeMap<usize, f64>> {
    let mut adj: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); mods.len()];
    for pins in inc.values() {
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

    let order = connectivity_order(&module_adjacency(&mods, inc, &mod_of));
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
    let base = save(items);
    let incumbent = score(eval, inc, ir, items);
    if !pack_modules(items, inc, ir) {
        return;
    }
    decongest(items);
    if !improves(score(eval, inc, ir, items), incumbent) {
        restore(items, &base);
    }
}
