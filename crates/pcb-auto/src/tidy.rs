//! Clean up the copper a route just laid.
//!
//! Freerouting occasionally leaves a segment a hair short of the pad it was aiming at, and
//! occasionally leaves a stub attached to nothing at all. KiCad reports both as missing
//! connections, so both are worth a pass: [`attach_pads`] pulls a near-miss end onto its pad,
//! [`prune_fragments`] deletes copper that connects no pad of its net.

use std::collections::BTreeMap;

use crate::geom::{dist, seg_point_dist, BBox, Point, UnionFind};
use crate::model::{Board, Track, Via};
use crate::sexp::Node;

/// How far a track end may sit from the pad it was aiming at and still count as a near miss.
const ATTACH_GAP_MM: f64 = 0.6;
/// Slop for "these two pieces of copper touch".
const TOUCH_MM: f64 = 0.01;

/// One piece of copper: a pad, a track or a via, reduced to what connectivity needs.
struct Item {
    net: i64,
    layers: Vec<String>,
    /// Two points for a track, one for a via or a pad.
    pts: Vec<Point>,
    radius: f64,
    bbox: Option<BBox>,
    is_pad: bool,
}

impl Item {
    fn touches(&self, o: &Item) -> bool {
        if self.net != o.net || !self.layers.iter().any(|l| o.layers.contains(l)) {
            return false;
        }
        // pads and vias are discs/boxes; tracks are capsules
        let near = |p: Point, q: &Item| -> bool {
            match (&q.bbox, q.pts.len()) {
                (Some(b), _) => b.inflate(self.radius + TOUCH_MM).contains(p),
                (None, 2) => seg_point_dist(q.pts[0], q.pts[1], p) <= q.radius + self.radius + TOUCH_MM,
                (None, _) => dist(p, q.pts[0]) <= q.radius + self.radius + TOUCH_MM,
            }
        };
        self.pts.iter().any(|p| near(*p, o)) || o.pts.iter().any(|p| near(*p, self))
    }
}

/// Every netted piece of copper on the board: pads first, then tracks, then vias.
fn items(board: &Board, tracks: &[Track], vias: &[Via]) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    let copper = board.copper_layers();
    for f in board.footprints() {
        for p in &f.pads {
            if p.net_id == 0 {
                continue;
            }
            let layers = if p.is_through() {
                copper.clone()
            } else {
                p.copper_layers()
            };
            out.push(Item {
                net: p.net_id,
                layers,
                pts: vec![p.pos],
                radius: 0.0,
                bbox: Some(p.bbox()),
                is_pad: true,
            });
        }
    }
    for t in tracks {
        out.push(Item {
            net: t.net_id,
            layers: vec![t.layer.clone()],
            pts: vec![t.start, t.end],
            radius: t.width / 2.0,
            bbox: None,
            is_pad: false,
        });
    }
    for v in vias {
        let (a, b) = (&v.layers.0, &v.layers.1);
        let span: Vec<String> = match (
            copper.iter().position(|l| l == a),
            copper.iter().position(|l| l == b),
        ) {
            (Some(i), Some(j)) => copper[i.min(j)..=i.max(j)].to_vec(),
            _ => copper.clone(),
        };
        out.push(Item {
            net: v.net_id,
            layers: span,
            pts: vec![v.pos],
            radius: v.size / 2.0,
            bbox: None,
            is_pad: false,
        });
    }
    out
}

/// Connected components of every net's copper, as a component id per item.
fn components(items: &[Item]) -> Vec<usize> {
    let mut uf = UnionFind::new(items.len());
    // grouped by net so the quadratic pass is per net, not per board
    let mut by_net: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        by_net.entry(it.net).or_default().push(i);
    }
    for idx in by_net.values() {
        for (a, &i) in idx.iter().enumerate() {
            for &j in &idx[a + 1..] {
                if items[i].touches(&items[j]) {
                    uf.join(i, j);
                }
            }
        }
    }
    (0..items.len()).map(|i| uf.find(i)).collect()
}

/// Pull a *dangling* track end onto the pad it was aiming at.
///
/// Only an end whose whole component holds no pad qualifies: a track that already reaches the net
/// somewhere is finished copper, and snapping it to the nearest same-net pad — every ground track
/// passes one — moves a legal route onto a short. Returns the number of ends moved.
pub fn attach_pads(board: &mut Board) -> usize {
    let tracks = board.tracks();
    let vias = board.vias();
    let its = items(board, &tracks, &vias);
    let comp = components(&its);
    let n_pads = its.iter().filter(|i| i.is_pad).count();
    let mut has_pad: BTreeMap<usize, bool> = BTreeMap::new();
    for (i, it) in its.iter().enumerate() {
        *has_pad.entry(comp[i]).or_insert(false) |= it.is_pad;
    }
    let copper = board.copper_layers();
    let pads: Vec<(i64, Vec<String>, Point, BBox)> = board
        .footprints()
        .into_iter()
        .flat_map(|f| {
            f.pads
                .iter()
                .filter(|p| p.net_id != 0)
                .map(|p| {
                    let layers = if p.is_through() {
                        copper.clone()
                    } else {
                        p.copper_layers()
                    };
                    (p.net_id, layers, p.pos, p.bbox())
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let mut fixes: Vec<(usize, bool, Point)> = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        if has_pad[&comp[n_pads + i]] {
            continue;
        }
        for (end, p) in [(false, t.start), (true, t.end)] {
            let hit = pads.iter().find(|(net, layers, _, bb)| {
                *net == t.net_id
                    && layers.contains(&t.layer)
                    && bb.inflate(t.width / 2.0 + ATTACH_GAP_MM).contains(p)
            });
            if let Some((_, _, centre, _)) = hit {
                fixes.push((i, end, *centre));
            }
        }
    }
    if fixes.is_empty() {
        return 0;
    }
    let mut moved = 0usize;
    let mut seen = 0usize;
    for item in board.tree.items.iter_mut() {
        let Node::List(s) = item else { continue };
        if !s.is("segment") {
            continue;
        }
        for (i, end, centre) in &fixes {
            if *i == seen
                && let Some(node) = s.find_mut(if *end { "end" } else { "start" }) {
                    node.set_args(vec![Node::num(centre.0), Node::num(centre.1)]);
                    moved += 1;
                }
        }
        seen += 1;
    }
    moved
}

/// Delete copper that connects no pad of its own net.
///
/// A stub the router abandoned is reported by KiCad as a missing connection between it and the
/// rest of the net, so removing it is what closes the connection rather than a cosmetic tidy.
/// Copper that overlaps a pour of its own net is kept: the pour is what connects it.
pub fn prune_fragments(board: &mut Board) -> usize {
    let tracks = board.tracks();
    let vias = board.vias();
    let its = items(board, &tracks, &vias);
    let comp = components(&its);
    let mut has_pad: BTreeMap<usize, bool> = BTreeMap::new();
    for (i, it) in its.iter().enumerate() {
        let e = has_pad.entry(comp[i]).or_insert(false);
        *e |= it.is_pad;
    }
    // a fragment sitting on its own net's pour is connected by that pour, not adrift
    let pours: Vec<(i64, Vec<String>, Vec<Point>)> = board
        .zones()
        .into_iter()
        .filter(|z| z.keepout.is_none() && z.net_id != 0)
        .map(|z| (z.net_id, z.layers.clone(), z.polygon.clone()))
        .collect();
    let on_pour = |it: &Item| {
        pours.iter().any(|(net, layers, poly)| {
            *net == it.net
                && layers.iter().any(|l| it.layers.contains(l))
                && it
                    .pts
                    .iter()
                    .any(|p| crate::geom::point_in_polygon(*p, poly))
        })
    };

    // indices into tracks then vias, in the order `items` built them
    let n_pads = its.iter().filter(|i| i.is_pad).count();
    let doomed: std::collections::HashSet<usize> = its
        .iter()
        .enumerate()
        .filter(|(i, it)| !it.is_pad && !has_pad[&comp[*i]] && !on_pour(it))
        .map(|(i, _)| i - n_pads)
        .collect();
    if doomed.is_empty() {
        return 0;
    }
    let n_tracks = tracks.len();
    let mut seen_t = 0usize;
    let mut seen_v = 0usize;
    let before = board.tree.items.len();
    let items_out: Vec<Node> = std::mem::take(&mut board.tree.items)
        .into_iter()
        .filter(|c| {
            let Some(s) = c.as_list() else { return true };
            if s.is("segment") {
                let keep = !doomed.contains(&seen_t);
                seen_t += 1;
                return keep;
            }
            if s.is("via") {
                let keep = !doomed.contains(&(n_tracks + seen_v));
                seen_v += 1;
                return keep;
            }
            true
        })
        .collect();
    board.tree.items = items_out;
    before - board.tree.items.len()
}

/// Both cleanups, in the order that matters: attach first (an attached stub is no longer a
/// fragment), then prune whatever is still connected to nothing.
pub fn tidy(board: &mut Board) -> (usize, usize) {
    let attached = attach_pads(board);
    let pruned = prune_fragments(board);
    (attached, pruned)
}
