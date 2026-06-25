//! Disjoint-set forests, in two shapes for two call styles:
//!
//! - The free [`uf_find`] / [`uf_union`] functions over a **caller-owned**
//!   `parent` slice, for callers that grow the set dynamically or already hold a
//!   `Vec<usize>` (the schematic pin reconciler, block splitting). The union rule
//!   is fixed: `union(a, b)` always makes `find(b)`'s root the survivor, because
//!   callers key maps by the specific representative a set collapses to.
//! - The owning [`UnionFind`] struct (union by rank, path-halving) for callers
//!   that want a self-contained forest of a known size (the DRC copper-net oracle).

/// Root of `x` in the disjoint-set forest, compressing the path to it.
pub fn uf_find(parent: &mut [usize], x: usize) -> usize {
    let mut r = x;
    while parent[r] != r {
        r = parent[r];
    }
    let mut c = x;
    while parent[c] != r {
        let next = parent[c];
        parent[c] = r;
        c = next;
    }
    r
}

/// Union the sets containing `a` and `b`; returns the surviving root (`b`'s).
pub fn uf_union(parent: &mut [usize], a: usize, b: usize) -> usize {
    let (ra, rb) = (uf_find(parent, a), uf_find(parent, b));
    parent[ra] = rb;
    rb
}

/// Union-find over `0..n`, union by rank with path-halving on `find`.
pub struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    /// A forest of `n` singletons.
    pub fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    /// Root of `i`'s set, compressing the path (halving) as it climbs.
    pub fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]]; // path halving
            i = self.parent[i];
        }
        i
    }

    /// Merge the sets containing `a` and `b` (union by rank).
    pub fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}
