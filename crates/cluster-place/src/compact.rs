//! De-sprawl passes — env-gated (`CLUSTER_COMPACT`), OFF by default because they are not
//! yet a clean net win, but they pin down EXACTLY where the single-sheet headroom is.
//!
//! A human-vs-machine logistic fit over the 500 boards (`tools/learn_layout.py`) shows the
//! engine's #1 measurable deficiency is SPRAWL (humans ~24, the SA ~69 — 3× too spread) and
//! ISLAND-scatter (2.4× more clusters): connected parts the SA leaves far apart, chiefly the
//! `decouple:` sugar's caps strung in a far row instead of hugging their IC. So these passes
//! optimise SPRAWL directly (the learned feature [`layout_sprawl`]), each gated to never
//! regress the routed (truthfulness, warnings, crossings):
//! - [`bank_decoupling`] re-seats each IC's decoupling caps in a grid beside it.
//! - `force_group` pulls signal-connected modules together (footprint repulsion vs net
//!   attraction, non-ground rails weak so a regulator stays near the IC it feeds).
//!
//! THE WALL they hit (measured, not assumed): banking the caps DOES cut sprawl (48→39 on a
//! clean MCU board) but the space beside the IC is occupied (the header + its pull-ups), so
//! it collides → warnings → the gate reverts it; "near the IC" is congested, "far" is the
//! SA's clean row. Realising the de-sprawl CLEANLY needs HOLISTIC placement (move the
//! neighbours out of the way first — the floorplanner), not an incremental pass over the SA's
//! layout. The additive gate keeps them safe (never ship worse) but mostly inert until then.

use std::collections::BTreeMap;

use geom::Point2;
use sch_place::ir::LayoutIr;
use sch_place::item::{Incidence, Item};

use sch_floorplan::contract::{
    RoutedEvaluator, build_anchor_blocks, decongest, item_rect, orient_angle,
};

use crate::eval::{restore, save, score};

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
        // A compact near-square grid as wide as the IC, hugging its top edge — the tight
        // form that FITS in the space above the IC (a wider grid collides the parts already
        // there). It reads a touch cramped but the data says the tighter, IC-adjacent bank
        // is the more human layout (lower sprawl / fewer islands / fewer labels).
        let ncols = ((hb.max_x - hb.min_x) / BANK_PITCH)
            .floor()
            .clamp(1.0, n as f64)
            .max((n as f64).sqrt().ceil()) as usize;
        let nrows = n.div_ceil(ncols);
        let row_gap = BANK_PITCH;
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

/// SPRAWL — the single feature that most separates the engine from human layouts (a
/// human-vs-machine logistic fit over the 500 boards put it far ahead: humans ~24, the SA
/// ~69). It is the part bounding-box AREA per part (whitespace proxy), which the engine's
/// half-perimeter `spread` + wire-length cost does NOT capture — so a regrouping that
/// genuinely de-sprawls can leave the base cost flat and get wrongly reverted. Gating
/// compaction on THIS makes the search optimise the thing humans actually do better.
fn layout_sprawl(items: &[Item]) -> f64 {
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
    if mods.len() < 3 {
        return false;
    }
    // A frozen member (a recognised decoupling/crystal idiom) rides WITH its module — a
    // rigid module slide preserves the idiom's internal arrangement, so frozen items are
    // fine here (frozen forbids the per-part SEARCH from moving them, not a block move).
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
/// De-sprawl the sheet toward the human distribution: (1) bank each IC's decoupling caps
/// beside it, (2) group the remaining signal-connected modules. Each step is kept only when
/// it strictly lowers [`layout_sprawl`] — the feature the corpus says most separates human
/// from machine — WITHOUT regressing the shipped (truthfulness, warnings, crossings). So it
/// optimises the thing humans do better (less whitespace, fewer islands) while the routed
/// gate guarantees it never ships a more-tangled or colliding sheet than the SA.
pub(crate) fn compact_clusters(
    eval: &RoutedEvaluator,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
) {
    let force = std::env::var_os("CLUSTER_FORCE").is_some();
    let step = |apply: &dyn Fn(&mut [Item]) -> bool, items: &mut [Item]| {
        let base = save(items);
        let s0 = score(eval, inc, ir, items);
        let spr0 = layout_sprawl(items);
        if !apply(items) {
            return;
        }
        decongest(items);
        let s = score(eval, inc, ir, items);
        // No shipped regression AND a real sprawl reduction (the learned human feature).
        let keep = (s.0, s.1, s.2) <= (s0.0, s0.1, s0.2) && layout_sprawl(items) + 1e-3 < spr0;
        if !keep && !force {
            restore(items, &base);
        }
    };
    step(&|it| bank_decoupling(it, inc, ir), items);
    step(&|it| force_group(it, inc, ir), items);
}
