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

/// The arrangement graph: scene nodes plus zero-size junction vertices.
struct Arrange {
    /// scene-node count (junctions appended after).
    n_scene: usize,
    /// per-vertex edges: (other vertex, direction hint from self, port y offset on self).
    adj: Vec<Vec<(usize, Option<bool>, f64)>>,
}

/// Final world origins for scene nodes (junction vertices are dropped — they
/// guide ordering only; the realizer routes their nets).
pub fn arrange(
    items: &[Item],
    g: &Reduced,
    scene: &Scene,
    dirs: &BTreeMap<(usize, String), PinDir>,
) -> Vec<Point2> {
    let n_scene = scene.nodes.len();

    // ── Junction vertices: one per Junction node — EXCEPT high-fanout nets,
    // which the realizer will label at each pin anyway; letting them pull the
    // ordering (or widen channels) contorts the layout for wires that will
    // never be drawn.
    const LABEL_FANOUT: usize = 4;
    let mut junction_vertex: BTreeMap<usize, usize> = BTreeMap::new();
    let mut n_total = n_scene;
    for (ni, nk) in g.nodes.iter().enumerate() {
        if let NodeKind::Junction(net) = nk {
            let fanout = g
                .chains
                .iter()
                .filter(|c| c.a.node == ni || c.b.node == ni)
                .count();
            let _ = net;
            if fanout >= LABEL_FANOUT {
                continue;
            }
            junction_vertex.insert(ni, n_total);
            n_total += 1;
        }
    }

    let mut ar = Arrange { n_scene, adj: vec![Vec::new(); n_total] };

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
        let Some(&(ea, eb)) = scene.ends.get(&ci).as_deref() else {
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
                ar.adj[vx].push((vy, dir_xy, px));
                ar.adj[vy].push((vx, dir_xy.map(|d| !d), py));
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

    // ── Layering: seeds = vertices whose typed edges all point OUT (pure
    // sources); fall back to connector-anchored nodes, then vertex 0. Longest
    // path from seeds over ALL edges (typed edges honored, untyped follow BFS).
    let mut layer = vec![usize::MAX; n_total];
    let seeds: Vec<usize> = {
        let mut typed_out: Vec<usize> = (0..n_total)
            .filter(|&v| {
                let hints: Vec<bool> =
                    ar.adj[v].iter().filter_map(|(_, h, _)| *h).collect();
                !hints.is_empty() && hints.iter().all(|&h| h)
            })
            .collect();
        if typed_out.is_empty() {
            typed_out = (0..n_scene)
                .filter(|&v| {
                    scene.nodes[v].anchor.is_some_and(|a| {
                        sch_place::netclass::is_connector_like(&items[a].part)
                    })
                })
                .collect();
        }
        if typed_out.is_empty() && n_total > 0 {
            typed_out.push(0);
        }
        typed_out
    };
    // BFS longest-path-ish: relax repeatedly (graphs are tiny; O(V·E) fine).
    for &s in &seeds {
        layer[s] = 0;
    }
    for _ in 0..n_total {
        let mut changed = false;
        for v in 0..n_total {
            if layer[v] == usize::MAX {
                continue;
            }
            for &(w, hint, _) in &ar.adj[v] {
                let want = match hint {
                    Some(true) => layer[v] + 1,          // v → w
                    Some(false) => layer[v].saturating_sub(1),
                    None => layer[v] + 1,                // untyped flows outward
                };
                if layer[w] == usize::MAX || (hint == Some(true) && layer[w] < want) {
                    layer[w] = want.min(n_total);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    for l in layer.iter_mut() {
        if *l == usize::MAX {
            *l = 0; // isolated (rail-only) nodes: power-entry column
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
                    let ns: Vec<f64> = ar.adj[v].iter().map(|&(w, _, _)| pos[w]).collect();
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
    let cols: Vec<Vec<usize>> = cols
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
    for (v, adj) in ar.adj.iter().enumerate() {
        for &(w, _, _) in adj {
            if v < w {
                let (lo, hi) = (col_of[v].min(col_of[w]), col_of[v].max(col_of[w]));
                for b in spans.iter_mut().take(hi).skip(lo) {
                    *b += 1;
                }
            }
        }
    }
    let mut col_x = vec![0.0f64; cols.len()];
    let mut edge = 0.0;
    for (l, col) in cols.iter().enumerate() {
        let w = col.iter().map(|&v| width(v)).fold(0.0, f64::max);
        col_x[l] = edge + w / 2.0;
        let channel = COL_GAP + 2.54 * spans.get(l).map_or(0, |&n| n.min(6)) as f64;
        edge += w + channel;
    }

    // Stack in columns; origin = node origin (env asymmetric, so shift).
    let mut origin = vec![Point2::new(0.0, 0.0); n_total];
    for (l, col) in cols.iter().enumerate() {
        let mut y = 0.0;
        for &v in col {
            if v < n_scene {
                let n = &scene.nodes[v];
                origin[v] = Point2::new(snap(col_x[l] - (n.env_min.x + n.env_max.x) / 2.0),
                                        snap(y - n.env_min.y));
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
        let (Some(a), Some(b)) = (ea, eb) else { continue };
        let pa = scene.nodes[*a].ports.iter().find(|p| p.chain == *ci && p.is_a);
        let pb = scene.nodes[*b].ports.iter().find(|p| p.chain == *ci && !p.is_a);
        if let (Some(pa), Some(pb)) = (pa, pb) {
            chain_list.push((*a, *b, pa.at.y, pb.at.y));
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
