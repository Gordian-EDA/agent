//! Disjoint-set forest over a caller-owned `parent` slice.
//!
//! The schematic side keys maps by the *specific* representative a set collapses
//! to (net naming, cluster roots), so the union rule is fixed: `union(a, b)`
//! always makes `find(b)`'s root the survivor. Compression strategy is internal
//! and never changes which root `find` returns.

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
