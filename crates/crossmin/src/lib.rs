//! `crossmin` — layered crossing-minimisation layout for schematic graphs.
//!
//! A schematic is a dataflow graph: signal leaves a part's OUTPUT pin and enters
//! others' INPUT pins. Drawing it cleanly = the classic **Sugiyama layered graph
//! drawing** problem, and the dominant readability defect (wires crossing) is exactly
//! what its middle stage minimises. The pipeline:
//!
//! 1. **Layering** — longest-path on the dataflow DAG (cycles bounded): each part gets
//!    a COLUMN, sources (input connectors) on the left, sinks on the right.
//! 2. **Ordering** — within each column, order parts by the **barycenter** of their
//!    neighbours in the adjacent column, swept up/down to a fixpoint; the ordering with
//!    the fewest bilayer crossings is kept. This is the crossing reducer.
//! 3. **Coordinates** — column index + within-column rank, returned as ordinals for the
//!    caller to project to mm.
//!
//! Pure + dependency-free (no KiCAD/geometry): the caller (the floorplan engine) builds
//! the [`Graph`] from pin directions + nets and maps the [`Placement`] back to cells —
//! the same split-of-concerns `circuit-graph` uses for idioms. Deterministic: every tie
//! breaks by node index, so the same graph always yields the same layout.

use std::collections::BTreeMap;

/// An input graph for layout. Two edge sets over the same `n` nodes (`0..n`):
/// `directed` (dataflow `src → dst`, drives LAYERING) and `undirected` (all
/// connectivity, drives ORDERING). They overlap in practice; keeping them separate
/// lets a bidirectional net (no clear flow) still inform ordering without forcing a
/// layer relation.
#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub n: usize,
    pub directed: Vec<(usize, usize, f64)>,
    pub undirected: Vec<(usize, usize, f64)>,
}

/// The computed layout: per node, its `layer` (column, 0 = leftmost) and `order`
/// (rank within the column, 0 = topmost). Both ordinal.
#[derive(Clone, Debug, Default)]
pub struct Placement {
    pub layer: Vec<i32>,
    pub order: Vec<i32>,
}

impl Graph {
    pub fn new(n: usize) -> Self {
        Graph { n, directed: Vec::new(), undirected: Vec::new() }
    }
    /// Add a dataflow edge (drives layering) AND its undirected connectivity (drives
    /// ordering) in one call — the common case for a real signal net.
    pub fn flow(&mut self, src: usize, dst: usize, w: f64) {
        if src != dst {
            self.directed.push((src, dst, w));
            self.undirected.push((src, dst, w));
        }
    }
    /// Add connectivity with NO flow direction (a bidirectional / passive net): it
    /// informs ordering but imposes no layer relation.
    pub fn link(&mut self, a: usize, b: usize, w: f64) {
        if a != b {
            self.undirected.push((a, b, w));
        }
    }
}

/// Tuning. Defaults are good for schematics; exposed for the integration to sweep.
#[derive(Clone, Copy, Debug)]
pub struct Opts {
    /// Barycenter sweep rounds (each = one down + one up pass). More = better ordering,
    /// diminishing past ~8.
    pub sweeps: usize,
}
impl Default for Opts {
    fn default() -> Self {
        Opts { sweeps: 12 }
    }
}

/// Layout `g` and return the layered [`Placement`].
pub fn layout(g: &Graph, opts: &Opts) -> Placement {
    if g.n == 0 {
        return Placement::default();
    }
    let layer = layer_assignment(g);
    let order = order_within_layers(g, &layer, opts);
    Placement { layer, order }
}

/// Longest-path layering on the dataflow DAG. `layer[v] = max(layer[u]+1)` over
/// directed edges `u→v`, relaxed `n` rounds so a true DAG converges exactly and any
/// cycle is simply bounded (no infinite loop). Sources (no incoming flow) stay at 0;
/// the result is normalised to start at 0.
fn layer_assignment(g: &Graph) -> Vec<i32> {
    let mut layer = vec![0i32; g.n];
    // Bellman-Ford-style longest path; n rounds bound any cycle.
    for _ in 0..g.n {
        let mut changed = false;
        for &(u, v, _) in &g.directed {
            if layer[v] < layer[u] + 1 {
                layer[v] = layer[u] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // A node with NO directed edges at all (only undirected links, or isolated) sits at
    // layer 0 by default; pull it toward the median layer of its undirected neighbours so
    // a passive part lands beside what it connects to rather than stranded in column 0.
    let touched: std::collections::BTreeSet<usize> =
        g.directed.iter().flat_map(|&(u, v, _)| [u, v]).collect();
    let mut nbr: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &(a, b, _) in &g.undirected {
        nbr.entry(a).or_default().push(b);
        nbr.entry(b).or_default().push(a);
    }
    for v in 0..g.n {
        if !touched.contains(&v)
            && let Some(ns) = nbr.get(&v) {
                let mut ls: Vec<i32> = ns.iter().map(|&u| layer[u]).collect();
                if !ls.is_empty() {
                    ls.sort_unstable();
                    layer[v] = ls[ls.len() / 2];
                }
            }
    }
    let min = *layer.iter().min().unwrap_or(&0);
    for l in &mut layer {
        *l -= min;
    }
    layer
}

/// Group node indices by layer, each list sorted by node index (deterministic seed).
fn layers_of(layer: &[i32], n: usize) -> Vec<Vec<usize>> {
    let maxl = layer.iter().copied().max().unwrap_or(0);
    let mut cols: Vec<Vec<usize>> = vec![Vec::new(); (maxl + 1) as usize];
    for v in 0..n {
        cols[layer[v] as usize].push(v);
    }
    cols
}

/// Barycenter ordering: sweep down then up, reordering each column by the mean rank of
/// each node's neighbours in the adjacent column, keeping the lowest-crossing ordering
/// seen. Returns the per-node within-column rank.
fn order_within_layers(g: &Graph, layer: &[i32], opts: &Opts) -> Vec<i32> {
    let mut cols = layers_of(layer, g.n);
    // Undirected adjacency (ordering uses all connectivity).
    let mut adj: BTreeMap<usize, Vec<(usize, f64)>> = BTreeMap::new();
    for &(a, b, w) in &g.undirected {
        adj.entry(a).or_default().push((b, w));
        adj.entry(b).or_default().push((a, w));
    }
    // pos[v] = current rank of v within its column.
    let mut pos = vec![0i32; g.n];
    let sync_pos = |cols: &[Vec<usize>], pos: &mut [i32]| {
        for col in cols {
            for (r, &v) in col.iter().enumerate() {
                pos[v] = r as i32;
            }
        }
    };
    sync_pos(&cols, &mut pos);

    let bary = |v: usize, pos: &[i32], want_layer: i32| -> Option<f64> {
        let ns = adj.get(&v)?;
        let (mut s, mut wsum) = (0.0f64, 0.0f64);
        for &(u, w) in ns {
            if layer[u] == want_layer {
                s += pos[u] as f64 * w;
                wsum += w;
            }
        }
        (wsum > 0.0).then_some(s / wsum)
    };

    let mut best = cols.clone();
    let mut best_x = count_crossings_cols(&cols, &adj, layer);
    let reorder = |cols: &mut Vec<Vec<usize>>, pos: &mut [i32], li: usize, want: i32| {
        // Barycenter for each node vs the `want` column; nodes without a neighbour
        // there keep their current rank (stable).
        let keyed: Vec<(f64, usize)> = cols[li]
            .iter()
            .map(|&v| (bary(v, pos, want).unwrap_or(pos[v] as f64), v))
            .collect();
        let mut idx: Vec<usize> = (0..keyed.len()).collect();
        // Stable sort by barycenter; tie-break by node index for determinism.
        idx.sort_by(|&i, &j| {
            keyed[i].0.partial_cmp(&keyed[j].0).unwrap_or(std::cmp::Ordering::Equal).then(keyed[i].1.cmp(&keyed[j].1))
        });
        cols[li] = idx.iter().map(|&i| keyed[i].1).collect();
        for (r, &v) in cols[li].iter().enumerate() {
            pos[v] = r as i32;
        }
    };

    let nl = cols.len();
    for _ in 0..opts.sweeps {
        // Down sweep: order each column against the one to its left.
        for li in 1..nl {
            reorder(&mut cols, &mut pos, li, li as i32 - 1);
        }
        // Up sweep: against the one to its right.
        for li in (0..nl.saturating_sub(1)).rev() {
            reorder(&mut cols, &mut pos, li, li as i32 + 1);
        }
        let x = count_crossings_cols(&cols, &adj, layer);
        if x < best_x {
            best_x = x;
            best = cols.clone();
        }
    }
    let mut order = vec![0i32; g.n];
    for col in &best {
        for (r, &v) in col.iter().enumerate() {
            order[v] = r as i32;
        }
    }
    order
}

/// Bilayer crossing count over adjacent columns, from a `cols` ordering.
fn count_crossings_cols(
    cols: &[Vec<usize>],
    adj: &BTreeMap<usize, Vec<(usize, f64)>>,
    layer: &[i32],
) -> usize {
    let mut pos = vec![0i32; layer.len()];
    for col in cols {
        for (r, &v) in col.iter().enumerate() {
            pos[v] = r as i32;
        }
    }
    let mut total = 0usize;
    for (li, col) in cols.iter().enumerate().take(cols.len().saturating_sub(1)) {
        // Edges from column li to li+1 as (rank_in_li, rank_in_li+1), dedup’d.
        let mut es: Vec<(i32, i32)> = Vec::new();
        for &u in col {
            if let Some(ns) = adj.get(&u) {
                for &(w, _) in ns {
                    if layer[w] == li as i32 + 1 {
                        es.push((pos[u], pos[w]));
                    }
                }
            }
        }
        // Count inverted pairs (a crossing): a<c but b>d.
        for i in 0..es.len() {
            for j in (i + 1)..es.len() {
                let (a, b) = es[i];
                let (c, d) = es[j];
                if (a < c && b > d) || (a > c && b < d) {
                    total += 1;
                }
            }
        }
    }
    total
}

/// Public crossing count for a finished [`Placement`] (tests + measurement).
pub fn count_crossings(g: &Graph, p: &Placement) -> usize {
    let cols = {
        let maxl = p.layer.iter().copied().max().unwrap_or(0);
        let mut c: Vec<Vec<usize>> = vec![Vec::new(); (maxl + 1) as usize];
        // place each node at its (layer, order)
        let mut by: Vec<Vec<(i32, usize)>> = vec![Vec::new(); (maxl + 1) as usize];
        for v in 0..g.n {
            by[p.layer[v] as usize].push((p.order[v], v));
        }
        for (li, col) in by.iter_mut().enumerate() {
            col.sort_unstable();
            c[li] = col.iter().map(|&(_, v)| v).collect();
        }
        c
    };
    let mut adj: BTreeMap<usize, Vec<(usize, f64)>> = BTreeMap::new();
    for &(a, b, w) in &g.undirected {
        adj.entry(a).or_default().push((b, w));
        adj.entry(b).or_default().push((a, w));
    }
    count_crossings_cols(&cols, &adj, &p.layer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_graph_is_empty() {
        let p = layout(&Graph::new(0), &Opts::default());
        assert!(p.layer.is_empty());
    }

    #[test]
    fn chain_lays_out_in_columns() {
        // 0 -> 1 -> 2 -> 3 : a straight pipeline, one per column.
        let mut g = Graph::new(4);
        for i in 0..3 {
            g.flow(i, i + 1, 1.0);
        }
        let p = layout(&g, &Opts::default());
        assert_eq!(p.layer, vec![0, 1, 2, 3]);
        assert_eq!(count_crossings(&g, &p), 0);
    }

    #[test]
    fn cycle_does_not_hang_and_bounds_layers() {
        // 0 -> 1 -> 2 -> 0 : a cycle must not loop forever.
        let mut g = Graph::new(3);
        g.flow(0, 1, 1.0);
        g.flow(1, 2, 1.0);
        g.flow(2, 0, 1.0);
        let p = layout(&g, &Opts::default());
        assert_eq!(p.layer.len(), 3);
        assert!(p.layer.iter().all(|&l| (0..3).contains(&l)));
    }

    #[test]
    fn barycenter_removes_an_avoidable_crossing() {
        // Two sources (0,1) in column 0, two sinks (2,3) in column 1, edges 0->3 and
        // 1->2. The seed order (0,1)/(2,3) crosses once; the optimal (0,1)/(3,2) is 0.
        let mut g = Graph::new(4);
        g.flow(0, 3, 1.0);
        g.flow(1, 2, 1.0);
        let p = layout(&g, &Opts::default());
        assert_eq!(p.layer[0], 0);
        assert_eq!(p.layer[2], 1);
        assert_eq!(count_crossings(&g, &p), 0, "barycenter should uncross");
    }

    #[test]
    fn complete_bipartite_k22_is_irreducible() {
        // K2,2: edges 0-2,0-3,1-2,1-3 across two columns — exactly 1 crossing is
        // unavoidable. The optimiser must not report fewer (sanity on the counter).
        let mut g = Graph::new(4);
        g.flow(0, 2, 1.0);
        g.flow(0, 3, 1.0);
        g.flow(1, 2, 1.0);
        g.flow(1, 3, 1.0);
        let p = layout(&g, &Opts::default());
        assert_eq!(count_crossings(&g, &p), 1);
    }

    #[test]
    fn deterministic() {
        let mut g = Graph::new(6);
        g.flow(0, 2, 1.0);
        g.flow(1, 2, 1.0);
        g.flow(2, 3, 1.0);
        g.flow(2, 4, 1.0);
        g.link(3, 5, 1.0);
        let a = layout(&g, &Opts::default());
        let b = layout(&g, &Opts::default());
        assert_eq!(a.layer, b.layer);
        assert_eq!(a.order, b.order);
    }
}
