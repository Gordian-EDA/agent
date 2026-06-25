//! Disjoint-set forests.

/// Union-find view over a caller-owned parent slice.
///
/// `union_to(a, b)` keeps `b`'s root as the representative; schematic callers
/// rely on that stable survivor when they key maps by root.
pub struct ParentForest<'a> {
    parent: &'a mut [usize],
}

impl<'a> ParentForest<'a> {
    pub fn new(parent: &'a mut [usize]) -> Self {
        Self { parent }
    }

    /// Root of `x`, compressing the path to it.
    pub fn find(&mut self, x: usize) -> usize {
        let mut r = x;
        while self.parent[r] != r {
            r = self.parent[r];
        }
        let mut c = x;
        while self.parent[c] != r {
            let next = self.parent[c];
            self.parent[c] = r;
            c = next;
        }
        r
    }

    /// Merge the set containing `a` into the set containing `b`.
    pub fn union_to(&mut self, a: usize, b: usize) -> usize {
        let ra = self.find(a);
        let rb = self.find(b);
        self.parent[ra] = rb;
        rb
    }
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
