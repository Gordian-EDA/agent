//! Constrained stress-majorization 2-D layout, built on the [`vpsc`](crate::vpsc) solver.
//!
//! Minimises the stress
//!
//! ```text
//!   σ(P) = Σ_{i<j, connected}  w_ij ( ‖Pᵢ − Pⱼ‖ − d_ij )²,   w_ij = 1/d_ij²
//! ```
//!
//! where `d_ij` is the *ideal* distance between nodes (graph-hop distance × `ideal_len`),
//! subject to per-axis separation / alignment / equality constraints.
//!
//! Method (constrained SMACOF, the libcola principle): repeatedly, for each axis, take one
//! stress-majorization (Guttman) step toward the unconstrained optimum, then **project** that
//! target onto the feasible region with VPSC (the nearest constraint-satisfying positions).
//! Alternating the axes and iterating drives stress down while always honouring constraints.
//! VPSC only does a diagonal (per-variable-weighted) projection, so off-axis coupling is
//! approximated — appropriate at schematic scale and what makes the constrained solve tractable.
//!
//! Pure: no RNG, deterministic given the inputs (initial positions are caller-supplied).

use crate::vpsc::{Constraint, Solver};
use std::collections::VecDeque;

/// A stress-majorization layout problem over `n` nodes with a fixed graph topology.
pub struct StressMajorizer {
    n: usize,
    /// Ideal distance between every pair (row-major n×n); 0 on the diagonal.
    d: Vec<f64>,
    /// Hop distance between every pair (row-major n×n); 0 = disconnected or self.
    hops: Vec<u32>,
}

impl StressMajorizer {
    /// Build from an undirected node graph. `edges` are `(u, v)` index pairs; `ideal_len` is
    /// the desired distance between directly-connected nodes (longer pairs scale by hop count).
    pub fn new(n: usize, edges: &[(usize, usize)], ideal_len: f64) -> Self {
        let hops = all_pairs_hops(n, edges);
        let d = hops.iter().map(|&h| h as f64 * ideal_len).collect();
        StressMajorizer { n, d, hops }
    }

    /// Run constrained stress majorization from the given initial positions, returning the
    /// final `(x, y)`. `cons_x` / `cons_y` are the separation/alignment/equality constraints
    /// for each axis (variable indices match node indices).
    pub fn run(
        &self,
        x0: &[f64],
        y0: &[f64],
        cons_x: &[Constraint],
        cons_y: &[Constraint],
        max_iter: usize,
    ) -> (Vec<f64>, Vec<f64>) {
        assert_eq!(x0.len(), self.n);
        assert_eq!(y0.len(), self.n);
        let mut x = x0.to_vec();
        let mut y = y0.to_vec();
        let mut last = self.stress(&x, &y);
        for _ in 0..max_iter {
            self.majorize_axis(&mut x, &y, cons_x);
            self.majorize_axis(&mut y, &x, cons_y);
            let s = self.stress(&x, &y);
            if (last - s).abs() <= 1e-6 * last.max(1e-9) {
                break;
            }
            last = s;
        }
        (x, y)
    }

    /// One SMACOF (Guttman) majorization step along one axis, then projected onto `cons`.
    /// `a` is the axis being updated (in/out); `other` is the fixed orthogonal axis.
    fn majorize_axis(&self, a: &mut Vec<f64>, other: &[f64], cons: &[Constraint]) {
        let n = self.n;
        let mut target = a.clone();
        let mut weight = vec![1.0; n];
        for i in 0..n {
            let mut num = 0.0;
            let mut wsum = 0.0;
            for j in 0..n {
                if i == j {
                    continue;
                }
                let p = self.hops[i * n + j];
                if p == 0 {
                    continue; // disconnected: no force
                }
                let d = self.d[i * n + j];
                let dx = a[i] - a[j];
                let dy = other[i] - other[j];
                let mut l = (dx * dx + dy * dy).sqrt();
                if l > d && p > 1 {
                    continue; // no long-range attraction (matches libcola)
                }
                if l < 1e-9 {
                    l = 1e-9;
                }
                let w = 1.0 / (d * d);
                // Guttman update term: pull node i so |i-j| → d_ij.
                num += w * (a[j] + d * (a[i] - a[j]) / l);
                wsum += w;
            }
            if wsum > 0.0 {
                target[i] = num / wsum;
                weight[i] = wsum;
            }
        }
        if cons.is_empty() {
            *a = target;
        } else {
            *a = Solver::new(&target, &weight, cons).solve();
        }
    }

    /// Current stress σ(P).
    pub fn stress(&self, x: &[f64], y: &[f64]) -> f64 {
        let n = self.n;
        let mut s = 0.0;
        for u in 0..n {
            for v in (u + 1)..n {
                let p = self.hops[u * n + v];
                if p == 0 {
                    continue;
                }
                let d = self.d[u * n + v];
                let dx = x[u] - x[v];
                let dy = y[u] - y[v];
                let l = (dx * dx + dy * dy).sqrt();
                if l > d && p > 1 {
                    continue;
                }
                let rl = d - l;
                s += rl * rl / (d * d);
            }
        }
        s
    }
}

/// All-pairs unweighted shortest paths (hops) by BFS from each node. 0 = disconnected/self.
fn all_pairs_hops(n: usize, edges: &[(usize, usize)]) -> Vec<u32> {
    let mut adj = vec![Vec::new(); n];
    for &(u, v) in edges {
        if u != v {
            adj[u].push(v);
            adj[v].push(u);
        }
    }
    let mut hops = vec![0u32; n * n];
    for s in 0..n {
        let mut dist = vec![u32::MAX; n];
        dist[s] = 0;
        let mut q = VecDeque::new();
        q.push_back(s);
        while let Some(u) = q.pop_front() {
            for &w in &adj[u] {
                if dist[w] == u32::MAX {
                    dist[w] = dist[u] + 1;
                    q.push_back(w);
                }
            }
        }
        for t in 0..n {
            if t != s && dist[t] != u32::MAX {
                hops[s * n + t] = dist[t];
            }
        }
    }
    hops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dist(x: &[f64], y: &[f64], i: usize, j: usize) -> f64 {
        ((x[i] - x[j]).powi(2) + (y[i] - y[j]).powi(2)).sqrt()
    }

    #[test]
    fn chain_unfolds_to_ideal_spacing() {
        // path 0-1-2-3, ideal 10. From a compressed init it should expand so adjacent
        // nodes sit ~10 apart and the layout is (near) straight (stress → 0).
        let sm = StressMajorizer::new(4, &[(0, 1), (1, 2), (2, 3)], 10.0);
        let x0 = vec![0.0, 1.0, 2.0, 3.0];
        let y0 = vec![0.0, 0.3, -0.2, 0.1];
        let (x, y) = sm.run(&x0, &y0, &[], &[], 200);
        for i in 0..3 {
            let dl = dist(&x, &y, i, i + 1);
            assert!((dl - 10.0).abs() < 1.5, "adj {i}: {dl}");
        }
        // straight line ⇒ stress near zero
        assert!(sm.stress(&x, &y) < 0.05, "stress {}", sm.stress(&x, &y));
    }

    #[test]
    fn constraints_force_a_clean_row() {
        // Same chain, but constrain it to a horizontal row: all same y (equality), and a
        // left-to-right min separation in x. Expect a tidy row.
        let sm = StressMajorizer::new(4, &[(0, 1), (1, 2), (2, 3)], 10.0);
        let x0 = vec![0.0, 1.0, 0.5, 2.0];
        let y0 = vec![0.0, 5.0, -3.0, 2.0];
        let cons_x = [
            Constraint::sep(0, 1, 8.0),
            Constraint::sep(1, 2, 8.0),
            Constraint::sep(2, 3, 8.0),
        ];
        let cons_y = [
            Constraint::eq(0, 1, 0.0),
            Constraint::eq(1, 2, 0.0),
            Constraint::eq(2, 3, 0.0),
        ];
        let (x, y) = sm.run(&x0, &y0, &cons_x, &cons_y, 200);
        // all aligned in y
        for i in 0..3 {
            assert!((y[i] - y[i + 1]).abs() < 1e-6, "y not aligned: {y:?}");
        }
        // strictly left-to-right with ≥ the min gap
        for i in 0..3 {
            assert!(x[i + 1] >= x[i] + 8.0 - 1e-6, "x order/gap: {x:?}");
        }
        // and stress wants ~10 spacing, so gaps should be ~10 (≥8 enforced, ~10 preferred)
        for i in 0..3 {
            let g = x[i + 1] - x[i];
            assert!((8.0..=12.0).contains(&g), "gap {i}: {g}");
        }
    }

    #[test]
    fn star_keeps_leaves_near_center() {
        // star: center 0 connected to 1,2,3,4. Each leaf should sit ~ideal from center.
        let sm = StressMajorizer::new(5, &[(0, 1), (0, 2), (0, 3), (0, 4)], 10.0);
        let x0 = vec![0.0, 1.0, -1.0, 0.5, -0.5];
        let y0 = vec![0.0, 1.0, 1.0, -1.0, -1.0];
        let (x, y) = sm.run(&x0, &y0, &[], &[], 300);
        for leaf in 1..5 {
            let dl = dist(&x, &y, 0, leaf);
            assert!((dl - 10.0).abs() < 2.0, "leaf {leaf} dist {dl}");
        }
    }
}
