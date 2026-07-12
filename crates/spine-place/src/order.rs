//! Global arrangement: Sugiyama-style ordering of scene nodes (modules, chain
//! runs, junction points), then deterministic coordinate assignment — columns
//! left→right by signal flow, in-column stacking, then a port-alignment sweep
//! that straightens inter-column wires (the corpus 79 % zero-bend law).

use std::collections::BTreeMap;

use geom::Point2;
use sch_place::item::Item;

use crate::chain::{NodeKind, Reduced};
use crate::scene::Scene;

/// Signal direction of a pin as far as flow layering cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinDir {
    Source,
    Sink,
}

/// Column gap between node envelopes (channel width added per crossing chain).
const COL_GAP: f64 = 7.62;
/// Vertical gap between stacked nodes in a column.
const ROW_GAP: f64 = 7.62;
const GRID: f64 = 1.27;

fn snap(v: f64) -> f64 {
    (v / GRID).round() * GRID
}

/// A strap islet: a stub module or a small free module — the population of the
/// dedicated strap column.
pub fn is_strapish(scene: &Scene, v: usize) -> bool {
    v < scene.nodes.len()
        && (scene.nodes[v].strap
            || (scene.nodes[v].places.len() <= 2
                && scene.nodes[v].anchor.is_some()
                && !scene
                    .ends
                    .values()
                    .any(|(a, b)| matches!((a, b), (Some(x), Some(y)) if *x == v || *y == v))
                && (scene.nodes[v].env_max.x - scene.nodes[v].env_min.x) < 22.0
                && (scene.nodes[v].env_max.y - scene.nodes[v].env_min.y) < 22.0))
}

/// The arrangement graph: scene nodes plus zero-size junction vertices.
/// Final world origins for scene nodes (junction vertices are dropped — they
/// guide ordering only; the realizer routes their nets).
/// The gated layout variants one arrange run applies; each is proven per
/// sheet by an A/B gate in the engine.
#[derive(Clone, Copy, Default)]
pub struct Variants {
    pub fold: bool,
    pub strap_col: bool,
    pub shelf: bool,
    pub hop_align: bool,
}

pub fn arrange(
    items: &[Item],
    g: &Reduced,
    scene: &Scene,
    dirs: &BTreeMap<(usize, String), PinDir>,
    variants: Variants,
    bundle_free: &std::collections::BTreeSet<usize>,
) -> Vec<Point2> {
    let n_scene = scene.nodes.len();

    // ── Junction vertices: one per Junction node — EXCEPT high-fanout nets,
    // which the realizer will label at each pin anyway; letting them pull the
    // ordering (or widen channels) contorts the layout for wires that will
    // never be drawn.
    const LABEL_FANOUT: usize = 5;
    let mut junction_vertex: BTreeMap<usize, usize> = BTreeMap::new();
    let mut n_total = n_scene;
    for (ni, nk) in g.nodes.iter().enumerate() {
        if let NodeKind::Junction(_) = nk {
            let fanout = g
                .chains
                .iter()
                .filter(|c| c.a.node == ni || c.b.node == ni)
                .count();
            if fanout >= LABEL_FANOUT {
                continue;
            }
            junction_vertex.insert(ni, n_total);
            n_total += 1;
        }
    }

    // Per-vertex edges: (other vertex, direction hint from self, port y offset on self).
    let mut adj: Vec<Vec<(usize, Option<bool>, f64)>> = vec![Vec::new(); n_total];

    // Map a chain terminal to (vertex, direction hint at that end, port y).
    // Direction hint: Some(true) = this end is a SOURCE (flow leaves it).
    let end_vertex = |t: &crate::chain::Terminal,
                      scene_end: Option<usize>|
     -> Option<(usize, Option<bool>, f64)> {
        if let Some(sn) = scene_end {
            let hint = match &g.nodes[t.node] {
                NodeKind::Part(i) => dirs.get(&(*i, t.pin.clone())).map(|d| *d == PinDir::Source),
                _ => None,
            };
            let py = scene.nodes[sn]
                .ports
                .iter()
                .find(|p| {
                    p.chain
                        == scene
                            .ends
                            .iter()
                            .find(|(_, (a, b))| *a == Some(sn) || *b == Some(sn))
                            .map(|(ci, _)| *ci)
                            .unwrap_or(usize::MAX)
                })
                .map(|p| p.at.y)
                .unwrap_or(0.0);
            return Some((sn, hint, py));
        }
        if let NodeKind::Junction(_) = &g.nodes[t.node] {
            return junction_vertex.get(&t.node).map(|v| (*v, None, 0.0));
        }
        None // rail
    };

    // ── Edges from chains: A—run—B when the chain has a run node, else A—B.
    // Which scene node is a given chain's run? It is the node whose ports carry
    // that chain twice (both ends) and has no anchor.
    let mut run_of_chain: BTreeMap<usize, usize> = BTreeMap::new();
    for (sn, node) in scene.nodes.iter().enumerate() {
        if node.anchor.is_none() {
            for p in &node.ports {
                run_of_chain.insert(p.chain, sn);
            }
        }
    }

    for (ci, c) in g.chains.iter().enumerate() {
        let Some(&(ea, eb)) = scene.ends.get(&ci) else {
            continue;
        };
        let va = end_vertex(&c.a, ea);
        let vb = end_vertex(&c.b, eb);
        let run = run_of_chain.get(&ci).copied();
        let mut connect = |x: Option<(usize, Option<bool>, f64)>,
                           y: Option<(usize, Option<bool>, f64)>| {
            if let (Some((vx, hx, px)), Some((vy, hy, py))) = (x, y)
                && vx != vy
            {
                // Direction: prefer x's hint, else invert y's.
                let dir_xy = hx.or(hy.map(|s| !s));
                adj[vx].push((vy, dir_xy, px));
                adj[vy].push((vx, dir_xy.map(|d| !d), py));
            }
        };
        match run {
            Some(rn) => {
                let r = Some((rn, None, 0.0));
                connect(va, r);
                connect(r, vb);
            }
            None => connect(va, vb),
        }
    }

    // ── Layering. Three steps, classic Sugiyama shape:
    // 1. Seeds: connectors, plus TRUE typed sources (typed-out edges, no
    //    typed-in) — a mid-chain part with one typed edge is NOT a source.
    // 2. BFS distance from the seeds over all edges; untyped edges get
    //    directed downhill (toward greater distance).
    // 3. Longest-path layering over the resulting DAG (cycle edges dropped).
    let mut typed_in = vec![0usize; n_total];
    let mut typed_out = vec![0usize; n_total];
    for (v, adjs) in adj.iter().enumerate() {
        for &(_, hint, _) in adjs {
            match hint {
                Some(true) => typed_out[v] += 1,
                Some(false) => typed_in[v] += 1,
                None => {}
            }
        }
    }
    // Only MODULE nodes can be sources — a chain run or junction sits between
    // its terminals by construction, whatever its local edge typing says.
    let mut seeds: Vec<usize> = (0..n_scene)
        .filter(|&v| scene.nodes[v].anchor.is_some() && typed_out[v] > 0 && typed_in[v] == 0)
        .collect();
    // Connectors seed only when NO typed source exists: with a real source the
    // BFS naturally pushes output connectors to the far end; force-seeding them
    // at 0 runs their chain backwards through the source chain's columns.
    if seeds.is_empty() {
        seeds.extend((0..n_scene).filter(|&v| {
            !adj[v].is_empty()
                && scene.nodes[v]
                    .anchor
                    .is_some_and(|a| sch_place::netclass::is_connector_like(&items[a].part))
        }));
    }
    seeds.sort_unstable();
    seeds.dedup();
    if seeds.is_empty() && n_total > 0 {
        seeds.push(0);
    }

    let mut dist = vec![usize::MAX; n_total];
    let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for &s in &seeds {
        dist[s] = 0;
        queue.push_back(s);
    }
    while let Some(v) = queue.pop_front() {
        for &(w, _, _) in &adj[v] {
            if dist[w] == usize::MAX {
                dist[w] = dist[v] + 1;
                queue.push_back(w);
            }
        }
    }

    // Directed edge list: typed edges keep their direction; untyped run
    // downhill by BFS distance (ties by index, deterministic).
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (v, adjs) in adj.iter().enumerate() {
        for &(w, hint, _) in adjs {
            match hint {
                Some(true) => edges.push((v, w)),
                Some(false) => {} // recorded from the other side
                None if v < w => {
                    let (dv, dw) = (dist[v], dist[w]);
                    if dv <= dw {
                        edges.push((v, w))
                    } else {
                        edges.push((w, v))
                    }
                }
                None => {}
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();

    // Kahn longest-path; edges that would close a cycle are dropped (greedy
    // Eades-lite: process in deterministic order, skip back edges).
    let mut layer = vec![0usize; n_total];
    {
        let mut indeg = vec![0usize; n_total];
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); n_total];
        for &(u, v) in &edges {
            out[u].push(v);
            indeg[v] += 1;
        }
        let mut ready: std::collections::BTreeSet<usize> =
            (0..n_total).filter(|&v| indeg[v] == 0).collect();
        let mut done = vec![false; n_total];
        let mut remaining = n_total;
        while remaining > 0 {
            let v = match ready.iter().next().copied() {
                Some(v) => v,
                // Cycle: force the lowest not-done vertex (drops its back edges).
                None => (0..n_total).find(|&v| !done[v]).unwrap(),
            };
            ready.remove(&v);
            if done[v] {
                continue;
            }
            done[v] = true;
            remaining -= 1;
            for &w in &out[v] {
                if done[w] {
                    continue;
                }
                layer[w] = layer[w].max(layer[v] + 1);
                indeg[w] -= 1;
                if indeg[w] == 0 {
                    ready.insert(w);
                }
            }
        }
    }

    if std::env::var_os("SPINE_DEBUG").is_some() {
        for v in 0..n_total {
            let name = if v < n_scene {
                scene.nodes[v]
                    .anchor
                    .map(|a| items[a].refdes.clone())
                    .unwrap_or_else(|| {
                        scene.nodes[v]
                            .places
                            .first()
                            .map(|p| format!("run:{}", items[p.item].refdes))
                            .unwrap_or_else(|| format!("v{v}"))
                    })
            } else {
                format!("junction#{v}")
            };
            let adj: Vec<String> = adj[v]
                .iter()
                .map(|(w, h, _)| {
                    format!(
                        "{w}{}",
                        match h {
                            Some(true) => ">",
                            Some(false) => "<",
                            None => "-",
                        }
                    )
                })
                .collect();
            eprintln!("[layer] v{v} {name} layer={} adj={adj:?}", layer[v]);
        }
    }

    // ── Compress layers: only layers holding a SCENE node become physical
    // columns — a junction is a routing point, not a column; giving it its own
    // layer inserts an empty column whose gaps push every chain hop past the
    // wire threshold. Junction layers collapse onto the nearest scene column.
    let scene_layers: Vec<usize> = {
        let mut v: Vec<usize> = (0..n_scene).map(|n| layer[n]).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let compress = |l: usize| -> usize {
        match scene_layers.binary_search(&l) {
            Ok(i) => i,
            Err(i) => i.min(scene_layers.len().saturating_sub(1)),
        }
    };
    let layer: Vec<usize> = layer.iter().map(|&l| compress(l)).collect();

    // ── Strap column: stub islets and small free modules gather in ONE
    // dedicated trailing column, refdes-sorted — the human "straps region",
    // decided here so packing and channels respect it from the start.
    let mut layer = layer;

    // Shelf packing: FREE module nodes (label islands — no wired inter-module
    // chain) are strung wide by weak junction edges on label-heavy boards; a
    // page reads better with them shelved into a ~square block of columns,
    // refdes-ordered (infer.rs shelves inferred anchors the same way).
    if variants.shelf {
        let wired: std::collections::BTreeSet<usize> = scene
            .ends
            .values()
            .filter_map(|(a, b)| match (a, b) {
                (Some(x), Some(y)) => Some([*x, *y]),
                _ => None,
            })
            .flatten()
            .collect();
        let mut free: Vec<usize> = (0..n_scene)
            .filter(|&v| {
                scene.nodes[v].anchor.is_some()
                    && (!wired.contains(&v) || bundle_free.contains(&v))
                    && !(variants.strap_col && is_strapish(scene, v))
            })
            .collect();
        if free.len() >= 3 {
            free.sort_by_key(|&v| {
                crate::bands::refdes_key(&items[scene.nodes[v].anchor.unwrap_or(0)].refdes)
            });
            let base_layer = layer
                .iter()
                .enumerate()
                .filter(|&(v, _)| v < n_scene && wired.contains(&v))
                .map(|(_, &l)| l)
                .max()
                .unwrap_or(0)
                + 1;
            // First-fit-decreasing by HEIGHT into the column count whose packed
            // block lands nearest a landscape page: mixed-size islands (a tall
            // header next to a 3-pin sensor) pack tight this way, where a
            // uniform grid inflates every cell to the largest member.
            let size = |v: usize| {
                (
                    scene.nodes[v].env_max.x - scene.nodes[v].env_min.x,
                    scene.nodes[v].env_max.y - scene.nodes[v].env_min.y,
                )
            };
            let ffd = |ncols: usize| -> (Vec<Vec<usize>>, f64) {
                let mut order = free.clone();
                order.sort_by(|&a, &b| size(b).1.total_cmp(&size(a).1).then(a.cmp(&b)));
                let mut bins: Vec<(f64, Vec<usize>)> = vec![(0.0, Vec::new()); ncols];
                for v in order {
                    let bin = bins
                        .iter_mut()
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                        .expect("ncols >= 1");
                    bin.0 += size(v).1 + 7.62;
                    bin.1.push(v);
                }
                let w: f64 = bins
                    .iter()
                    .map(|(_, m)| m.iter().map(|&v| size(v).0).fold(0.0, f64::max) + 7.62)
                    .sum();
                let h = bins.iter().map(|(h, _)| *h).fold(1.0, f64::max);
                (bins.into_iter().map(|(_, m)| m).collect(), w / h)
            };
            let ncols = (1..=free.len())
                .min_by(|&a, &b| (ffd(a).1 - 1.4).abs().total_cmp(&(ffd(b).1 - 1.4).abs()))
                .unwrap_or(1);
            for (col, members) in ffd(ncols).0.into_iter().enumerate() {
                for v in members {
                    layer[v] = base_layer + col;
                }
            }
        }
    }

    if variants.strap_col {
        let strap_layer = layer.iter().copied().max().unwrap_or(0) + 1;
        for (v, item_layer) in layer.iter_mut().enumerate().take(n_scene) {
            if is_strapish(scene, v) {
                *item_layer = strap_layer;
            }
        }
    }

    // ── In-layer order: barycenter sweeps over neighbor mean positions.
    let max_layer = layer.iter().copied().max().unwrap_or(0);
    let mut cols: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    for v in 0..n_total {
        cols[layer[v]].push(v);
    }
    let mut pos = vec![0.0f64; n_total];
    for col in &cols {
        for (i, &v) in col.iter().enumerate() {
            pos[v] = i as f64;
        }
    }
    for _ in 0..4 {
        for col in cols.iter_mut() {
            let mut keyed: Vec<(f64, usize)> = col
                .iter()
                .map(|&v| {
                    let ns: Vec<f64> = adj[v].iter().map(|&(w, _, _)| pos[w]).collect();
                    let bc = if ns.is_empty() {
                        pos[v]
                    } else {
                        ns.iter().sum::<f64>() / ns.len() as f64
                    };
                    (bc, v)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            *col = keyed.into_iter().map(|(_, v)| v).collect();
            for (i, &v) in col.iter().enumerate() {
                pos[v] = i as f64;
            }
        }
    }

    // ── Coordinates. Column x: cumulative width; junction vertices are 0-size.
    let width = |v: usize| -> f64 {
        if v < n_scene {
            scene.nodes[v].env_max.x - scene.nodes[v].env_min.x
        } else {
            0.0
        }
    };
    let height = |v: usize| -> f64 {
        if v < n_scene {
            scene.nodes[v].env_max.y - scene.nodes[v].env_min.y
        } else {
            0.0
        }
    };

    // Aspect folding: a column taller than the landscape target splits into
    // adjacent sub-columns at node boundaries (a label-stitched board with no
    // flow structure otherwise degenerates into one endless stack).
    let area: f64 = (0..n_scene).map(|v| width(v) * height(v)).sum();
    let target_h = (area / 1.4).sqrt().max(80.0);
    let mut cols: Vec<Vec<usize>> = cols
        .into_iter()
        .flat_map(|col| {
            let mut out: Vec<Vec<usize>> = vec![Vec::new()];
            let mut h = 0.0;
            for v in col {
                if h > 0.0 && h + height(v) > target_h {
                    out.push(Vec::new());
                    h = 0.0;
                }
                out.last_mut().unwrap().push(v);
                h += height(v) + ROW_GAP;
            }
            out
        })
        .filter(|c| !c.is_empty())
        .collect();

    // Column of each vertex (needed for channel sizing before coordinates).
    let mut col_of = vec![0usize; n_total];
    for (l, col) in cols.iter().enumerate() {
        for &v in col {
            col_of[v] = l;
        }
    }

    // Channel width between adjacent columns grows with the nets that must
    // cross it — parallel wires need lanes, and their labels need air.
    let mut spans = vec![0usize; cols.len().saturating_sub(1)];
    for (v, adj) in adj.iter().enumerate() {
        for &(w, _, _) in adj {
            if v < w {
                let (lo, hi) = (col_of[v].min(col_of[w]), col_of[v].max(col_of[w]));
                for b in spans.iter_mut().take(hi).skip(lo) {
                    *b += 1;
                }
            }
        }
    }
    // Strap column members order by refdes (numeric-aware), not barycenter.
    if !cols.is_empty() {
        let strap_l = cols.len() - 1;
        let is_strap_col = variants.strap_col
            && cols[strap_l]
                .iter()
                .all(|&v| v < n_scene && scene.nodes[v].anchor.is_some() && is_strapish(scene, v));
        if is_strap_col && cols[strap_l].len() >= 2 {
            cols[strap_l].sort_by_key(|&v| {
                crate::bands::refdes_key(&items[scene.nodes[v].anchor.unwrap_or(0)].refdes)
            });
        }
    }

    // Row folding: a deep series graph makes one very wide layer sequence;
    // humans wrap the chain into rows like text (the corpus RF sheets). Fold
    // consecutive layers into rows once the running width passes the landscape
    // target; each row restarts at x = 0.
    let col_w: Vec<f64> = cols
        .iter()
        .map(|col| col.iter().map(|&v| width(v)).fold(0.0, f64::max))
        .collect();
    let col_h: Vec<f64> = cols
        .iter()
        .map(|col| col.iter().map(|&v| height(v) + ROW_GAP).sum::<f64>())
        .collect();
    let total_w: f64 = col_w
        .iter()
        .enumerate()
        .map(|(l, w)| w + COL_GAP + 1.27 * spans.get(l).map_or(0, |&n| n.min(6)) as f64)
        .sum();
    let target_w = (area * 1.4).sqrt().max(160.0);
    let fold = variants.fold && total_w > target_w * 1.3;

    let mut col_x = vec![0.0f64; cols.len()];
    let mut row_of_col = vec![0usize; cols.len()];
    let mut edge = 0.0;
    let mut row = 0usize;
    for (l, _) in cols.iter().enumerate() {
        let channel = COL_GAP + 1.27 * spans.get(l).map_or(0, |&n| n.min(4)) as f64;
        if fold && edge > 0.0 && edge + col_w[l] > target_w {
            row += 1;
            edge = 0.0;
        }
        row_of_col[l] = row;
        col_x[l] = edge + col_w[l] / 2.0;
        edge += col_w[l] + channel;
    }
    let mut row_base = vec![0.0f64; row + 1];
    for r in 1..=row {
        let prev_h = (0..cols.len())
            .filter(|&l| row_of_col[l] == r - 1)
            .map(|l| col_h[l])
            .fold(0.0, f64::max);
        row_base[r] = row_base[r - 1] + prev_h + ROW_GAP * 2.0;
    }

    // Stack in columns; origin = node origin (env asymmetric, so shift).
    let mut origin = vec![Point2::new(0.0, 0.0); n_total];
    for (l, col) in cols.iter().enumerate() {
        let mut y = row_base[row_of_col[l]];
        for &v in col {
            if v < n_scene {
                let n = &scene.nodes[v];
                origin[v] = Point2::new(
                    snap(col_x[l] - (n.env_min.x + n.env_max.x) / 2.0),
                    snap(y - n.env_min.y),
                );
                y += height(v) + ROW_GAP;
            } else {
                origin[v] = Point2::new(snap(col_x[l]), snap(y));
                y += ROW_GAP;
            }
        }
    }

    // ── Port-alignment sweep: for each chain between two scene nodes in
    // adjacent columns, shift the RIGHT node vertically so the connected port
    // Ys match — greedy top-to-bottom, skip when it would collide.
    let mut aligned: Vec<bool> = vec![false; n_total];
    let mut chain_list: Vec<(usize, usize, f64, f64)> = Vec::new(); // (va, vb, ya, yb)
    for (ci, (ea, eb)) in &scene.ends {
        let (Some(a), Some(b)) = (ea, eb) else {
            continue;
        };
        let pa = scene.nodes[*a]
            .ports
            .iter()
            .find(|p| p.chain == *ci && p.is_a);
        let pb = scene.nodes[*b]
            .ports
            .iter()
            .find(|p| p.chain == *ci && !p.is_a);
        if let (Some(pa), Some(pb)) = (pa, pb) {
            chain_list.push((*a, *b, pa.at.y, pb.at.y));
        }
    }
    // One junction hop: two chains that meet at the same low-fanout junction,
    // each with ONE placed node, align those nodes (a pot feeding an amp input
    // through its coupling cap reads as one row, exactly as a direct chain
    // would). GATED (hop_align): snapping rows can merge nets — the caller's
    // breaks-first A/B keeps it only where the netlist survives.
    if variants.hop_align {
        let mut at_junction: BTreeMap<usize, Vec<(usize, f64)>> = BTreeMap::new();
        for (ci, (ea, eb)) in &scene.ends {
            let c = &g.chains[*ci];
            let (node, is_a, jt) = match (ea, eb) {
                (Some(a), None) => (*a, true, &c.b),
                (None, Some(b)) => (*b, false, &c.a),
                _ => continue,
            };
            if !matches!(&g.nodes[jt.node], NodeKind::Junction(_))
                || !junction_vertex.contains_key(&jt.node)
            {
                continue;
            }
            let py = scene.nodes[node]
                .ports
                .iter()
                .find(|p| p.chain == *ci && p.is_a == is_a)
                .map(|p| p.at.y);
            if let Some(py) = py {
                at_junction.entry(jt.node).or_default().push((node, py));
            }
        }
        for members in at_junction.values() {
            if let [(a, ya), (b, yb)] = members[..]
                && a != b
                // Neighbouring columns only: a same-column snap can land two
                // stubs collinear and MERGE nets (the uart breaks=1); far-apart
                // columns aren't a visual row anyway.
                && (1..=2).contains(&col_of[a].abs_diff(col_of[b]))
            {
                chain_list.push((a, b, ya, yb));
            }
        }
    }
    chain_list.sort_by(|x, y| {
        (origin[x.0].y + x.2)
            .total_cmp(&(origin[y.0].y + y.2))
            .then(x.0.cmp(&y.0))
    });
    for (va, vb, ya, yb) in chain_list {
        // Move the deeper-layer node.
        let (fixed, mv, yf, ym) = if layer[va] <= layer[vb] {
            (va, vb, ya, yb)
        } else {
            (vb, va, yb, ya)
        };
        if aligned[mv] || fixed == mv {
            continue;
        }
        let dy = (origin[fixed].y + yf) - (origin[mv].y + ym);
        if dy == 0.0 {
            aligned[mv] = true;
            continue;
        }
        // Collision check within mv's column.
        let cand_y = origin[mv].y + dy;
        let (lo, hi) = (
            cand_y + scene_env(scene, mv).0,
            cand_y + scene_env(scene, mv).1,
        );
        let collides = cols[col_of[mv]].iter().any(|&o| {
            o != mv && {
                let (olo, ohi) = (
                    origin[o].y + scene_env(scene, o).0,
                    origin[o].y + scene_env(scene, o).1,
                );
                lo < ohi + ROW_GAP && olo < hi + ROW_GAP
            }
        });
        if !collides {
            origin[mv].y = snap(cand_y);
            aligned[mv] = true;
        }
    }

    origin.truncate(n_scene);
    origin
}

fn scene_env(scene: &Scene, v: usize) -> (f64, f64) {
    if v < scene.nodes.len() {
        (scene.nodes[v].env_min.y, scene.nodes[v].env_max.y)
    } else {
        (0.0, 0.0)
    }
}
