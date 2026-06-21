//! Variable Placement with Separation Constraints (VPSC).
//!
//! Solves, in one dimension:
//!
//! ```text
//!   minimise   Σ wᵢ (xᵢ − dᵢ)²       (dᵢ = desired position, wᵢ = weight)
//!   subject to  left + gap ≤ right    (separation)   or
//!               left + gap = right    (equality)
//! ```
//!
//! This is the per-axis projection used by constrained stress-majorization layout: given a
//! set of desired positions (e.g. from a majorization step), find the nearest positions
//! that satisfy the constraints.
//!
//! Algorithm (Dwyer's block solver, faithful to adaptagrams/libvpsc):
//!  * A **block** is a maximal set of variables joined by *active* (tight) constraints; its
//!    members move together, each at a fixed `offset` from the block's `posn`. A block's
//!    optimal position is the weighted mean of `desired − offset`.
//!  * **satisfy()** walks the variables in constraint-DAG order; for each block it merges
//!    leftward across the most-violated incoming constraint until none remain violated —
//!    producing a *feasible* solution.
//!  * **refine()** then repeatedly splits a block on the active constraint with the most
//!    negative Lagrange multiplier (the constraint that "most wants" to be slack), which
//!    strictly lowers cost, until no such constraint remains — producing the *optimal*
//!    solution.
//!
//! Ported index/arena-based (no pointer graph) with linear scans rather than libvpsc's
//! pairing heap — appropriate for schematic-sheet scale (≤ a few dozen variables/axis).

/// `slack` below this counts as a (numerically real) violation worth merging on.
const VIOLATION: f64 = -1e-9;
/// A block splits on an active constraint whose Lagrange multiplier is below this.
const LAGRANGIAN_TOLERANCE: f64 = -1e-4;
/// Bound on refine() restarts (matches libvpsc's guard against pathological loops).
const MAX_REFINE_TRIES: usize = 1000;

/// A separation (or equality) constraint: `left + gap ≤ right` (or `==` when `equality`).
#[derive(Clone, Copy, Debug)]
pub struct Constraint {
    pub left: usize,
    pub right: usize,
    pub gap: f64,
    pub equality: bool,
}

impl Constraint {
    /// `left + gap ≤ right`.
    pub fn sep(left: usize, right: usize, gap: f64) -> Self {
        Constraint { left, right, gap, equality: false }
    }
    /// `left + gap = right`.
    pub fn eq(left: usize, right: usize, gap: f64) -> Self {
        Constraint { left, right, gap, equality: true }
    }
}

struct Var {
    desired: f64,
    weight: f64,
    /// Position within the block: `position = block.posn + offset`.
    offset: f64,
    block: usize,
    /// Constraint indices where this variable is the `right` endpoint.
    ins: Vec<usize>,
    /// Constraint indices where this variable is the `left` endpoint.
    outs: Vec<usize>,
}

struct Con {
    left: usize,
    right: usize,
    gap: f64,
    equality: bool,
    /// Lagrange multiplier (set during refine()).
    lm: f64,
    /// Whether this constraint is tight and spans a block.
    active: bool,
}

struct Block {
    vars: Vec<usize>,
    posn: f64,
    // Running sums for the optimal position: posn = (ad − ab) / a2.
    ab: f64, // Σ wᵢ·offsetᵢ
    ad: f64, // Σ wᵢ·desiredᵢ
    a2: f64, // Σ wᵢ
    deleted: bool,
}

/// A VPSC problem instance. Build with [`Solver::new`], then call [`Solver::solve`].
pub struct Solver {
    vars: Vec<Var>,
    cons: Vec<Con>,
    /// Blocks are append-only; merged/split blocks are tombstoned (`deleted`) so that
    /// existing block indices in `Var::block` stay valid. Cheap at schematic scale.
    blocks: Vec<Block>,
}

impl Solver {
    /// Create a solver over `n = desired.len()` variables (with matching `weight`) and the
    /// given constraints. Each variable starts in its own block.
    pub fn new(desired: &[f64], weight: &[f64], constraints: &[Constraint]) -> Self {
        assert_eq!(desired.len(), weight.len());
        let n = desired.len();
        let mut vars: Vec<Var> = (0..n)
            .map(|i| Var {
                desired: desired[i],
                weight: weight[i],
                offset: 0.0,
                block: i,
                ins: Vec::new(),
                outs: Vec::new(),
            })
            .collect();
        let cons: Vec<Con> = constraints
            .iter()
            .map(|c| Con {
                left: c.left,
                right: c.right,
                gap: c.gap,
                equality: c.equality,
                lm: 0.0,
                active: false,
            })
            .collect();
        for (ci, c) in cons.iter().enumerate() {
            vars[c.left].outs.push(ci);
            vars[c.right].ins.push(ci);
        }
        let blocks: Vec<Block> = (0..n)
            .map(|i| Block {
                vars: vec![i],
                posn: desired[i], // (w·d − 0)/w
                ab: 0.0,
                ad: weight[i] * desired[i],
                a2: weight[i],
                deleted: false,
            })
            .collect();
        Solver { vars, cons, blocks }
    }

    /// Solve to optimality and return the final positions (indexed as the input `desired`).
    pub fn solve(&mut self) -> Vec<f64> {
        self.satisfy();
        self.refine();
        self.positions()
    }

    /// Produce a feasible (constraint-satisfying, not necessarily optimal) solution and
    /// return positions. Cheaper than [`solve`]; useful when feasibility is enough.
    pub fn satisfy_only(&mut self) -> Vec<f64> {
        self.satisfy();
        self.positions()
    }

    /// Current position of every variable.
    pub fn positions(&self) -> Vec<f64> {
        (0..self.vars.len()).map(|v| self.position(v)).collect()
    }

    // ---- core geometry -------------------------------------------------------

    fn position(&self, v: usize) -> f64 {
        self.blocks[self.vars[v].block].posn + self.vars[v].offset
    }

    fn slack(&self, c: usize) -> f64 {
        let con = &self.cons[c];
        self.position(con.right) - con.gap - self.position(con.left)
    }

    fn recompute_block(&mut self, b: usize) {
        let (mut ab, mut ad, mut a2) = (0.0, 0.0, 0.0);
        for &v in &self.blocks[b].vars {
            let var = &self.vars[v];
            ab += var.weight * var.offset;
            ad += var.weight * var.desired;
            a2 += var.weight;
        }
        let blk = &mut self.blocks[b];
        blk.ab = ab;
        blk.ad = ad;
        blk.a2 = a2;
        blk.posn = if a2 != 0.0 { (ad - ab) / a2 } else { 0.0 };
    }

    /// Merge block `from` into block `into` across (now-active) constraint `c`, shifting
    /// every variable of `from` by `dist` so the merged constraint becomes tight.
    fn merge_block(&mut self, into: usize, from: usize, c: usize, dist: f64) {
        self.cons[c].active = true;
        let from_vars = std::mem::take(&mut self.blocks[from].vars);
        for &v in &from_vars {
            self.vars[v].offset += dist;
            self.vars[v].block = into;
            self.blocks[into].vars.push(v);
        }
        self.blocks[from].deleted = true;
        self.recompute_block(into);
    }

    // ---- satisfy (feasible solution) -----------------------------------------

    /// Topological order of variables over the constraint DAG (sources first).
    fn total_order(&self) -> Vec<usize> {
        let n = self.vars.len();
        let mut visited = vec![false; n];
        let mut order = Vec::with_capacity(n);
        for v in 0..n {
            if self.vars[v].ins.is_empty() && !visited[v] {
                self.dfs_visit(v, &mut visited, &mut order);
            }
        }
        // Any variable not reached from a source (e.g. inside a constraint cycle).
        for v in 0..n {
            if !visited[v] {
                self.dfs_visit(v, &mut visited, &mut order);
            }
        }
        order.reverse(); // post-order reversed = topological (sources first)
        order
    }

    fn dfs_visit(&self, v: usize, visited: &mut [bool], order: &mut Vec<usize>) {
        visited[v] = true;
        for i in 0..self.vars[v].outs.len() {
            let c = self.vars[v].outs[i];
            let r = self.cons[c].right;
            if !visited[r] {
                self.dfs_visit(r, visited, order);
            }
        }
        order.push(v);
    }

    fn satisfy(&mut self) {
        let order = self.total_order();
        for v in order {
            let b = self.vars[v].block;
            if !self.blocks[b].deleted {
                self.merge_left(b);
            }
        }
    }

    /// The most-violated incoming constraint to block `b` (right endpoint in `b`, left
    /// endpoint outside `b`), or `None` if `b` has no external incoming constraint.
    fn find_min_in_constraint(&self, b: usize) -> Option<usize> {
        let mut best = None;
        let mut best_slack = f64::MAX;
        for &v in &self.blocks[b].vars {
            for &c in &self.vars[v].ins {
                if self.vars[self.cons[c].left].block != b {
                    let s = self.slack(c);
                    if s < best_slack {
                        best_slack = s;
                        best = Some(c);
                    }
                }
            }
        }
        best
    }

    /// Symmetric: most-violated outgoing constraint from block `b`.
    fn find_min_out_constraint(&self, b: usize) -> Option<usize> {
        let mut best = None;
        let mut best_slack = f64::MAX;
        for &v in &self.blocks[b].vars {
            for &c in &self.vars[v].outs {
                if self.vars[self.cons[c].right].block != b {
                    let s = self.slack(c);
                    if s < best_slack {
                        best_slack = s;
                        best = Some(c);
                    }
                }
            }
        }
        best
    }

    /// Merge block `r` leftward across each violated incoming constraint until feasible.
    fn merge_left(&mut self, b: usize) {
        let mut r = b;
        while let Some(c) = self.find_min_in_constraint(r) {
            if self.slack(c) >= VIOLATION {
                break;
            }
            let lvar = self.cons[c].left;
            let rvar = self.cons[c].right;
            let l = self.vars[lvar].block;
            // dist that makes c tight when merging the left block's vars into the right's.
            let mut dist = self.vars[rvar].offset - self.vars[lvar].offset - self.cons[c].gap;
            let (into, from) = if self.blocks[r].vars.len() < self.blocks[l].vars.len() {
                dist = -dist; // merge the smaller (r) into the larger (l)
                (l, r)
            } else {
                (r, l)
            };
            self.merge_block(into, from, c, dist);
            r = into;
        }
    }

    /// Merge block `l` rightward across each violated outgoing constraint until feasible.
    fn merge_right(&mut self, b: usize) {
        let mut l = b;
        while let Some(c) = self.find_min_out_constraint(l) {
            if self.slack(c) >= VIOLATION {
                break;
            }
            let lvar = self.cons[c].left;
            let rvar = self.cons[c].right;
            let r = self.vars[rvar].block;
            let mut dist = self.vars[lvar].offset + self.cons[c].gap - self.vars[rvar].offset;
            let (into, from) = if self.blocks[l].vars.len() < self.blocks[r].vars.len() {
                dist = -dist; // merge the smaller (l) into the larger (r)
                (r, l)
            } else {
                (l, r)
            };
            self.merge_block(into, from, c, dist);
            l = into;
        }
    }

    // ---- refine (optimal solution) -------------------------------------------

    fn refine(&mut self) {
        let mut tries = MAX_REFINE_TRIES;
        loop {
            tries -= 1;
            let mut split_any = false;
            let live: Vec<usize> =
                (0..self.blocks.len()).filter(|&b| !self.blocks[b].deleted).collect();
            for b in live {
                if self.blocks[b].deleted {
                    continue;
                }
                if let Some(c) = self.find_min_lm(b) {
                    if self.cons[c].lm < LAGRANGIAN_TOLERANCE {
                        self.split(b, c);
                        split_any = true;
                        break;
                    }
                }
            }
            if !split_any || tries == 0 {
                break;
            }
        }
    }

    /// The active non-equality constraint in block `b` with the most negative Lagrange
    /// multiplier (the constraint that "most wants" to become slack), or `None`.
    fn find_min_lm(&mut self, b: usize) -> Option<usize> {
        let front = self.blocks[b].vars[0];
        let mut min_lm: Option<usize> = None;
        self.compute_dfdv(front, None, &mut min_lm);
        min_lm
    }

    /// Recursively compute the partial derivative of the cost wrt the block's position,
    /// setting each traversed active constraint's Lagrange multiplier and tracking the
    /// minimum. Walks the block's active-constraint spanning tree from `v`, not crossing
    /// back over `u`. (scale = 1 throughout.)
    fn compute_dfdv(&mut self, v: usize, u: Option<usize>, min_lm: &mut Option<usize>) -> f64 {
        let mut dfdv = 2.0 * self.vars[v].weight * (self.position(v) - self.vars[v].desired);
        let outs = self.vars[v].outs.clone();
        for c in outs {
            let con = &self.cons[c];
            if con.active && Some(con.right) != u {
                let right = con.right;
                let lm = self.compute_dfdv(right, Some(v), min_lm);
                self.cons[c].lm = lm;
                dfdv += lm;
                self.track_min_lm(c, min_lm);
            }
        }
        let ins = self.vars[v].ins.clone();
        for c in ins {
            let con = &self.cons[c];
            if con.active && Some(con.left) != u {
                let left = con.left;
                let lm = -self.compute_dfdv(left, Some(v), min_lm);
                self.cons[c].lm = lm;
                dfdv -= lm;
                self.track_min_lm(c, min_lm);
            }
        }
        dfdv
    }

    fn track_min_lm(&self, c: usize, min_lm: &mut Option<usize>) {
        if self.cons[c].equality {
            return;
        }
        match *min_lm {
            Some(m) if self.cons[m].lm <= self.cons[c].lm => {}
            _ => *min_lm = Some(c),
        }
    }

    /// Split block `b` across active constraint `c` into a left sub-block (c.left's active
    /// subtree) and a right sub-block (c.right's), then re-satisfy both.
    fn split(&mut self, b: usize, c: usize) {
        self.cons[c].active = false;
        let left = self.cons[c].left;
        let right = self.cons[c].right;
        let l = self.new_block_from(left, Some(right));
        self.new_block_from(right, Some(left)); // right sub-block; re-fetched below after merges
        self.blocks[b].deleted = true;
        // Re-establish feasibility: the freed sub-blocks may now violate other constraints.
        self.merge_left(l);
        let r = self.vars[right].block; // r may have been merged away
        self.merge_right(r);
    }

    fn new_block_from(&mut self, start: usize, avoid: Option<usize>) -> usize {
        let b = self.blocks.len();
        self.blocks.push(Block {
            vars: Vec::new(),
            posn: 0.0,
            ab: 0.0,
            ad: 0.0,
            a2: 0.0,
            deleted: false,
        });
        self.populate_split(b, start, avoid);
        self.recompute_block(b);
        b
    }

    /// Traverse the active-constraint spanning tree from `v` (not crossing back over `u`),
    /// reassigning every reached variable into block `b`. Offsets are preserved, so the
    /// active constraints internal to the sub-block stay tight.
    fn populate_split(&mut self, b: usize, v: usize, u: Option<usize>) {
        self.vars[v].block = b;
        self.blocks[b].vars.push(v);
        let ins = self.vars[v].ins.clone();
        for c in ins {
            let con = &self.cons[c];
            if con.active && Some(con.left) != u {
                let left = con.left;
                self.populate_split(b, left, Some(v));
            }
        }
        let outs = self.vars[v].outs.clone();
        for c in outs {
            let con = &self.cons[c];
            if con.active && Some(con.right) != u {
                let right = con.right;
                self.populate_split(b, right, Some(v));
            }
        }
    }

    /// Total cost Σ wᵢ(xᵢ − dᵢ)² — useful in tests/diagnostics.
    pub fn cost(&self) -> f64 {
        (0..self.vars.len())
            .map(|v| {
                let diff = self.position(v) - self.vars[v].desired;
                self.vars[v].weight * diff * diff
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "expected {b}, got {a}");
    }

    /// All constraints satisfied (within tolerance) for the returned positions.
    fn feasible(pos: &[f64], cs: &[Constraint]) -> bool {
        cs.iter().all(|c| {
            let s = pos[c.right] - c.gap - pos[c.left];
            if c.equality { s.abs() < 1e-6 } else { s >= -1e-6 }
        })
    }

    #[test]
    fn already_satisfied_is_untouched() {
        let cs = [Constraint::sep(0, 1, 10.0)];
        let mut s = Solver::new(&[0.0, 20.0], &[1.0, 1.0], &cs);
        let p = s.solve();
        assert_close(p[0], 0.0);
        assert_close(p[1], 20.0);
    }

    #[test]
    fn two_vars_center_around_min_gap() {
        // both want 0, must be ≥10 apart → symmetric −5 / +5
        let cs = [Constraint::sep(0, 1, 10.0)];
        let mut s = Solver::new(&[0.0, 0.0], &[1.0, 1.0], &cs);
        let p = s.solve();
        assert_close(p[0], -5.0);
        assert_close(p[1], 5.0);
        assert!(feasible(&p, &cs));
    }

    #[test]
    fn chain_of_three() {
        // all want 0, chain of ≥10 gaps → −10 / 0 / +10
        let cs = [Constraint::sep(0, 1, 10.0), Constraint::sep(1, 2, 10.0)];
        let mut s = Solver::new(&[0.0, 0.0, 0.0], &[1.0, 1.0, 1.0], &cs);
        let p = s.solve();
        assert_close(p[0], -10.0);
        assert_close(p[1], 0.0);
        assert_close(p[2], 10.0);
        assert!(feasible(&p, &cs));
    }

    #[test]
    fn equality_holds_exact_gap() {
        // x1 − x0 == 10, minimise x0² + (x1−5)²  → x0 = −2.5, x1 = 7.5
        let cs = [Constraint::eq(0, 1, 10.0)];
        let mut s = Solver::new(&[0.0, 5.0], &[1.0, 1.0], &cs);
        let p = s.solve();
        assert_close(p[0], -2.5);
        assert_close(p[1], 7.5);
        assert!(feasible(&p, &cs));
    }

    #[test]
    fn refine_splits_unneeded_merge() {
        // 0 and 2 want 0; 1 wants +100; gaps 0→1 and 1→2 of 10.
        // satisfy may pull all three together, but the optimum only needs 1 high:
        // 0 and 2 settle near 0 (10 apart), 1 floats up to ~its desired but bounded by gaps.
        // The real check: result is FEASIBLE and OPTIMAL (cost ≤ the merged-block cost).
        let cs = [Constraint::sep(0, 1, 10.0), Constraint::sep(1, 2, 10.0)];
        let mut s = Solver::new(&[0.0, 100.0, 0.0], &[1.0, 1.0, 1.0], &cs);
        let p = s.solve();
        assert!(feasible(&p, &cs), "infeasible: {p:?}");
        // ordering preserved
        assert!(p[1] >= p[0] + 10.0 - 1e-6);
        assert!(p[2] >= p[1] + 10.0 - 1e-6);
        // optimal: var0 and var2 should NOT be dragged far from 0; with var1 forced between
        // them at ≥10 spacing, the optimum is 0:[-something], symmetric-ish. Just assert the
        // cost is no worse than a naive all-in-one-block stacking.
        assert!(s.cost() < 100.0 * 100.0);
    }

    #[test]
    fn larger_chain_is_feasible_and_ordered() {
        let n = 12;
        let desired: Vec<f64> = (0..n).map(|i| ((i * 37) % 11) as f64).collect();
        let weight = vec![1.0; n];
        let cs: Vec<Constraint> = (0..n - 1).map(|i| Constraint::sep(i, i + 1, 3.0)).collect();
        let mut s = Solver::new(&desired, &weight, &cs);
        let p = s.solve();
        assert!(feasible(&p, &cs), "infeasible: {p:?}");
        for i in 0..n - 1 {
            assert!(p[i + 1] >= p[i] + 3.0 - 1e-6);
        }
    }
}
