//! Force-directed 2-D placement: a Fruchterman-Reingold spring model with
//! rectangle-aware separation, producing a COMPACT, connectivity-aware layout.
//!
//! Connected nodes attract (springs along edges), all nodes repel (so the layout
//! spreads enough to be legible), and overlapping rectangles get an extra strong
//! separation push (so the result is collision-free). The equilibrium is a tight
//! cluster where connected parts sit adjacent — the compact layout a schematic
//! critic rewards (sprawl is penalised more than the few crossings compaction costs),
//! and one the incremental placement search cannot reach from a sprawled grid seed.
//!
//! Pure and deterministic (initial positions are a fixed phyllotaxis spiral by index;
//! no RNG), so it unit-tests in isolation and reproduces exactly run-to-run.

/// A node to place, by its rectangle size (width, height) in the same units as edges.
#[derive(Debug, Clone, Copy)]
pub struct Node {
    pub w: f64,
    pub h: f64,
}

/// The placement problem: node sizes + weighted undirected connectivity edges.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    /// `(a, b, weight)` — a connectivity link pulling nodes `a` and `b` together;
    /// higher weight = stronger pull (e.g. count of shared nets).
    pub edges: Vec<(usize, usize, f64)>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn node(&mut self, w: f64, h: f64) -> usize {
        self.nodes.push(Node { w, h });
        self.nodes.len() - 1
    }
    pub fn link(&mut self, a: usize, b: usize, weight: f64) {
        if a != b {
            self.edges.push((a, b, weight));
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Opts {
    /// Number of relaxation sweeps. ~300 converges typical small graphs.
    pub iters: usize,
    /// Minimum clear gap to keep between node rectangles.
    pub gap: f64,
    /// Weight on the all-pairs repulsion. 1.0 = classic FR (SPREADS nodes for a legible
    /// graph drawing); near 0 = COMPACTION (springs pull connected nodes together and only
    /// the overlap-separation keeps them clear → minimal bounding box). For seeding a
    /// schematic placement, use a small value (~0.1): tight, but not collapsed to a line.
    pub repulsion: f64,
}

impl Default for Opts {
    fn default() -> Self {
        Self { iters: 400, gap: 2.54, repulsion: 1.0 }
    }
}

/// Place the graph; returns each node's CENTER position. Positions are normalised so the
/// minimum corner sits at the origin.
pub fn layout(g: &Graph, opts: &Opts) -> Vec<[f64; 2]> {
    let n = g.nodes.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![[g.nodes[0].w / 2.0, g.nodes[0].h / 2.0]];
    }

    // Ideal spring length: nodes settle roughly a node-diagonal + gap apart, so a linked
    // pair ends adjacent. Based on the mean node size so it scales with the parts.
    let mean = {
        let s: f64 = g.nodes.iter().map(|nd| (nd.w + nd.h) * 0.5).sum();
        (s / n as f64).max(1.0)
    };
    let k = mean + opts.gap;

    // Deterministic initial spread: a phyllotaxis spiral (even, no clumping, no RNG).
    const GOLDEN: f64 = 2.399_963_229_728_653; // π(3−√5)
    let mut pos: Vec<[f64; 2]> = (0..n)
        .map(|i| {
            let r = k * (i as f64).sqrt();
            let a = i as f64 * GOLDEN;
            [r * a.cos(), r * a.sin()]
        })
        .collect();

    let mut temp = k * 2.0;
    let cool = temp / (opts.iters as f64 + 1.0);

    for _ in 0..opts.iters {
        let mut disp = vec![[0.0f64; 2]; n];

        // Repulsion between every pair (FR: k²/d), plus a strong extra push when the two
        // rectangles (inflated by the gap) overlap, so the result is collision-free.
        for i in 0..n {
            for j in (i + 1)..n {
                let mut dx = pos[i][0] - pos[j][0];
                let mut dy = pos[i][1] - pos[j][1];
                let mut d2 = dx * dx + dy * dy;
                if d2 < 1e-9 {
                    // Coincident: nudge deterministically by index so they separate.
                    dx = ((i + 1) as f64).cos() * 0.01;
                    dy = ((j + 1) as f64).sin() * 0.01;
                    d2 = dx * dx + dy * dy;
                }
                let d = d2.sqrt();
                let mut f = opts.repulsion * k * k / d;

                // Rectangle overlap (gap-inflated): add a separation force ∝ penetration.
                let ox = (g.nodes[i].w + g.nodes[j].w) * 0.5 + opts.gap - dx.abs();
                let oy = (g.nodes[i].h + g.nodes[j].h) * 0.5 + opts.gap - dy.abs();
                if ox > 0.0 && oy > 0.0 {
                    f += k * ox.min(oy);
                }

                let (ux, uy) = (dx / d, dy / d);
                disp[i][0] += ux * f;
                disp[i][1] += uy * f;
                disp[j][0] -= ux * f;
                disp[j][1] -= uy * f;
            }
        }

        // Attraction along edges (FR: d²/k, scaled by weight).
        for &(a, b, w) in &g.edges {
            let dx = pos[a][0] - pos[b][0];
            let dy = pos[a][1] - pos[b][1];
            let d = (dx * dx + dy * dy).sqrt().max(1e-6);
            let f = (d * d / k) * w;
            let (ux, uy) = (dx / d, dy / d);
            disp[a][0] -= ux * f;
            disp[a][1] -= uy * f;
            disp[b][0] += ux * f;
            disp[b][1] += uy * f;
        }

        // Step, capped by the cooling temperature.
        for i in 0..n {
            let dl = (disp[i][0] * disp[i][0] + disp[i][1] * disp[i][1]).sqrt();
            if dl > 1e-9 {
                let s = dl.min(temp) / dl;
                pos[i][0] += disp[i][0] * s;
                pos[i][1] += disp[i][1] * s;
            }
        }
        temp = (temp - cool).max(0.0);
    }

    // Final HARD separation: the spring/repulsion equilibrium can leave a small residual
    // overlap (separation balances attraction before reaching zero). Push any overlapping
    // pair apart along its least-penetration axis until the layout is collision-free —
    // deterministic, bounded, and only nudges (the FR result is already nearly clean).
    for _ in 0..200 {
        let mut moved = false;
        for i in 0..n {
            for j in (i + 1)..n {
                let ddx = pos[i][0] - pos[j][0];
                let ddy = pos[i][1] - pos[j][1];
                let ox = (g.nodes[i].w + g.nodes[j].w) * 0.5 + opts.gap - ddx.abs();
                let oy = (g.nodes[i].h + g.nodes[j].h) * 0.5 + opts.gap - ddy.abs();
                if ox > 1e-9 && oy > 1e-9 {
                    moved = true;
                    if ox <= oy {
                        let push = ox / 2.0 + 1e-6;
                        let s = if ddx >= 0.0 { push } else { -push };
                        pos[i][0] += s;
                        pos[j][0] -= s;
                    } else {
                        let push = oy / 2.0 + 1e-6;
                        let s = if ddy >= 0.0 { push } else { -push };
                        pos[i][1] += s;
                        pos[j][1] -= s;
                    }
                }
            }
        }
        if !moved {
            break;
        }
    }

    // Normalise so the minimum rectangle corner is at the origin.
    let (mut minx, mut miny) = (f64::MAX, f64::MAX);
    for i in 0..n {
        minx = minx.min(pos[i][0] - g.nodes[i].w / 2.0);
        miny = miny.min(pos[i][1] - g.nodes[i].h / 2.0);
    }
    for p in &mut pos {
        p[0] -= minx;
        p[1] -= miny;
    }
    pos
}

/// Bounding-box span (w + h) of a placement — a compactness measure for tests/tuning.
pub fn span(g: &Graph, pos: &[[f64; 2]]) -> f64 {
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for (i, p) in pos.iter().enumerate() {
        lo[0] = lo[0].min(p[0] - g.nodes[i].w / 2.0);
        lo[1] = lo[1].min(p[1] - g.nodes[i].h / 2.0);
        hi[0] = hi[0].max(p[0] + g.nodes[i].w / 2.0);
        hi[1] = hi[1].max(p[1] + g.nodes[i].h / 2.0);
    }
    (hi[0] - lo[0]) + (hi[1] - lo[1])
}

/// Whether any two node rectangles overlap (gap not required) — for the collision test.
pub fn any_overlap(g: &Graph, pos: &[[f64; 2]]) -> bool {
    let n = g.nodes.len();
    for i in 0..n {
        for j in (i + 1)..n {
            let ox = (g.nodes[i].w + g.nodes[j].w) * 0.5 - (pos[i][0] - pos[j][0]).abs();
            let oy = (g.nodes[i].h + g.nodes[j].h) * 0.5 - (pos[i][1] - pos[j][1]).abs();
            if ox > 1e-6 && oy > 1e-6 {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
    }

    #[test]
    fn empty_is_empty() {
        assert!(layout(&Graph::new(), &Opts::default()).is_empty());
    }

    #[test]
    fn linked_pair_ends_adjacent() {
        let mut g = Graph::new();
        let a = g.node(10.0, 10.0);
        let b = g.node(10.0, 10.0);
        g.link(a, b, 1.0);
        let p = layout(&g, &Opts::default());
        // Centres settle near the ideal spring length (a node size + gap), i.e. touching.
        assert!(dist(p[a], p[b]) < 30.0, "linked pair too far: {}", dist(p[a], p[b]));
        assert!(!any_overlap(&g, &p), "linked pair overlaps");
    }

    #[test]
    fn overlap_is_resolved() {
        // Three identical nodes, all mutually linked (would collapse without separation).
        let mut g = Graph::new();
        let a = g.node(10.0, 10.0);
        let b = g.node(10.0, 10.0);
        let c = g.node(10.0, 10.0);
        g.link(a, b, 5.0);
        g.link(b, c, 5.0);
        g.link(a, c, 5.0);
        let p = layout(&g, &Opts::default());
        assert!(!any_overlap(&g, &p), "triangle overlaps");
    }

    #[test]
    fn star_is_compact_and_clear() {
        // A hub with 6 leaves: leaves cluster around the hub, none overlapping.
        let mut g = Graph::new();
        let hub = g.node(20.0, 20.0);
        for _ in 0..6 {
            let l = g.node(8.0, 8.0);
            g.link(hub, l, 1.0);
        }
        let p = layout(&g, &Opts::default());
        assert!(!any_overlap(&g, &p));
        // Compact: the span is far below the spiral-init spread.
        assert!(span(&g, &p) < 200.0, "star not compact: span {}", span(&g, &p));
    }

    #[test]
    fn connected_beats_disconnected_distance() {
        // A--B linked, C unlinked: A and B end closer than A and C.
        let mut g = Graph::new();
        let a = g.node(10.0, 10.0);
        let b = g.node(10.0, 10.0);
        let _c = g.node(10.0, 10.0);
        g.link(a, b, 2.0);
        let p = layout(&g, &Opts::default());
        assert!(dist(p[a], p[b]) <= dist(p[a], p[_c]), "linked not closer than unlinked");
    }

    #[test]
    fn deterministic() {
        let mut g = Graph::new();
        for _ in 0..8 {
            g.node(10.0, 10.0);
        }
        for i in 0..7 {
            g.link(i, i + 1, 1.0);
        }
        let p1 = layout(&g, &Opts::default());
        let p2 = layout(&g, &Opts::default());
        assert_eq!(p1, p2);
    }
}
