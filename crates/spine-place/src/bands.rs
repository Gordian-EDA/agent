//! Type banding: humans align same-KIND things on one axis — headers in one
//! left-flush column at even pitch, mounting holes in a corner block, repeated
//! ICs stamped in a grid. The discriminator for what may move: a module wired
//! into the signal flow (an inter-module chain with both ends placed) must hold
//! its flow position; a LABEL-ISLAND — connected only through rails, labels, or
//! fanout nets — is free to band. Deterministic: members sort by refdes with
//! numeric awareness (J2 before J10).

use std::collections::BTreeMap;

use geom::Rect;
use sch_place::item::Item;
use sch_place::netclass::is_connector_like;

use crate::scene::Scene;

const GRID: f64 = 1.27;
const PITCH_GAP: f64 = 7.62;

fn snap(v: f64) -> f64 {
    (v / GRID).round() * GRID
}

/// Refdes sort key: alpha prefix + numeric suffix, so J2 < J10.
pub(crate) fn refdes_key(r: &str) -> (String, u64) {
    let split = r.find(|c: char| c.is_ascii_digit()).unwrap_or(r.len());
    let (alpha, num) = r.split_at(split);
    (alpha.to_string(), num.parse().unwrap_or(0))
}

/// The band a module belongs to, from its anchor's part.
fn band_key(part: &str, same_part_counts: &BTreeMap<&str, usize>) -> Option<String> {
    if part.contains("Mounting") || part.contains("Fiducial") {
        return Some("mount".into());
    }
    if part.contains("TestPoint") {
        return Some("tp".into());
    }
    if is_connector_like(part) {
        return Some("conn".into());
    }
    if same_part_counts
        .get(part_family(part).as_str())
        .copied()
        .unwrap_or(0)
        >= 3
    {
        return Some(format!("part:{}", part_family(part)));
    }
    None
}

/// Symbol family: the part name with its variant suffix stripped —
/// `AMS1117-3.3` and `AMS1117-5.0` are one visual motif.
fn part_family(part: &str) -> String {
    let name = part.rsplit(':').next().unwrap_or(part);
    name.split('-').next().unwrap_or(name).to_string()
}

/// One plannable band: the scene nodes to align, in refdes order.
pub struct BandPlan {
    pub key: String,
    pub members: Vec<usize>,
}

/// Plan the bands (see [`band_key`] for kinds); application and gating are the
/// caller's, one band at a time — an all-or-nothing pass lets one colliding
/// band veto every good one.
pub fn plan(items: &[Item], scene: &Scene) -> Vec<BandPlan> {
    // Wired = participates in an inter-module chain with BOTH ends placed.
    let mut wired = vec![false; scene.nodes.len()];
    for (a, b) in scene.ends.values() {
        if let (Some(a), Some(b)) = (a, b) {
            wired[*a] = true;
            wired[*b] = true;
        }
    }

    let mut family_counts: BTreeMap<String, usize> = BTreeMap::new();
    for node in &scene.nodes {
        if let Some(a) = node.anchor {
            *family_counts
                .entry(part_family(&items[a].part))
                .or_default() += 1;
        }
    }
    let same_part_counts: BTreeMap<&str, usize> = family_counts
        .iter()
        .map(|(k, &v)| (k.as_str(), v))
        .collect();

    // band key → members (scene node, anchor item). Label-islands band freely;
    // WIRED modules join only same-part MOTIF bands (repeated channels
    // interconnect in parallel, so a grid preserves their wiring — and the
    // wire-first gate arbitrates anyway).
    let mut bands: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    for (sn, node) in scene.nodes.iter().enumerate() {
        let Some(a) = node.anchor else { continue };
        // An inferred idiom may share a scene node with a connector or other
        // grammar anchor. Moving that node would silently break the frozen
        // cell contract after Spine has re-seated it.
        if node.places.iter().any(|place| items[place.item].frozen) {
            continue;
        }
        if let Some(key) = band_key(&items[a].part, &same_part_counts) {
            if wired[sn] && !key.starts_with("part:") {
                continue;
            }
            bands.entry(key).or_default().push((sn, a));
        }
    }

    let mut out: Vec<BandPlan> = Vec::new();
    for (key, mut members) in bands {
        if members.len() < 2 {
            continue;
        }
        members.sort_by_key(|&(_, a)| refdes_key(&items[a].refdes));
        out.push(BandPlan {
            key,
            members: members.into_iter().map(|(sn, _)| sn).collect(),
        });
    }
    out
}

/// World rect of a scene node's placed bodies.
fn node_rect(items: &[Item], scene: &Scene, sn: usize) -> Rect {
    scene.nodes[sn]
        .places
        .iter()
        .map(|p| sch_floorplan::contract::item_rect(&items[p.item], items[p.item].at))
        .reduce(|acc, r| {
            Rect::new(
                acc.min_x.min(r.min_x),
                acc.min_y.min(r.min_y),
                acc.max_x.max(r.max_x),
                acc.max_y.max(r.max_y),
            )
        })
        .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0))
}

/// Apply one band: members re-form as a refdes-ordered column (or near-square
/// grid for 5+), anchored at the members' median position — slid right/down in
/// cell steps until the formation's box is clear of every NON-member body.
pub fn apply(items: &mut [Item], scene: &Scene, band: &BandPlan) {
    let rects: Vec<Rect> = band
        .members
        .iter()
        .map(|&sn| node_rect(items, scene, sn))
        .collect();

    let member_items: std::collections::BTreeSet<usize> = band
        .members
        .iter()
        .flat_map(|&sn| scene.nodes[sn].places.iter().map(|p| p.item))
        .collect();
    let others: Vec<Rect> = (0..items.len())
        .filter(|i| !member_items.contains(i))
        .map(|i| sch_floorplan::contract::item_rect(&items[i], items[i].at))
        .collect();

    let cols = if band.members.len() >= 5 {
        (band.members.len() as f64).sqrt().ceil() as usize
    } else {
        1
    };
    let rows = band.members.len().div_ceil(cols);
    let cell_w = rects.iter().map(|r| r.width()).fold(0.0, f64::max) + PITCH_GAP;
    let cell_h = rects.iter().map(|r| r.height()).fold(0.0, f64::max) + PITCH_GAP;

    // Mechanical bands live in the sheet's bottom-right CORNER (the human
    // convention for fiducials and mounting holes); everything else anchors at
    // its members' median so it stays in its neighbourhood.
    let (mut bx, mut by) = if band.key == "mount" {
        let (mut mx, mut my) = (f64::MIN, f64::MIN);
        for o in &others {
            mx = mx.max(o.max_x);
            my = my.max(o.max_y);
        }
        (
            snap(mx - cols as f64 * cell_w + PITCH_GAP),
            snap(my - rows as f64 * cell_h + PITCH_GAP),
        )
    } else {
        let mut xs: Vec<f64> = rects.iter().map(|r| r.min_x).collect();
        let mut ys: Vec<f64> = rects.iter().map(|r| r.min_y).collect();
        xs.sort_by(|a, b| a.total_cmp(b));
        ys.sort_by(|a, b| a.total_cmp(b));
        (snap(xs[xs.len() / 2]), snap(ys[ys.len() / 2]))
    };

    // Slide the whole formation in half-cell steps until its box clears every
    // non-member body (right, then down, alternating outward).
    let fits = |x: f64, y: f64| -> bool {
        let bbox = Rect::new(
            x - 1.27,
            y - 1.27,
            x + cols as f64 * cell_w + 1.27,
            y + rows as f64 * cell_h + 1.27,
        );
        !others.iter().any(|o| o.overlaps(&bbox))
    };
    'search: for ring in 0..24 {
        let step = ring as f64 * 0.5;
        for (ddx, ddy) in [
            (step, 0.0),
            (0.0, step),
            (step, step),
            (-step, 0.0),
            (0.0, -step),
        ] {
            let (cx, cy) = (snap(bx + ddx * cell_w), snap(by + ddy * cell_h));
            if fits(cx, cy) {
                (bx, by) = (cx, cy);
                break 'search;
            }
        }
    }

    for (k, (&sn, r)) in band.members.iter().zip(&rects).enumerate() {
        let tx = snap(bx + (k % cols) as f64 * cell_w);
        let ty = snap(by + (k / cols) as f64 * cell_h);
        let (dx, dy) = (tx - r.min_x, ty - r.min_y);
        if dx == 0.0 && dy == 0.0 {
            continue;
        }
        for p in &scene.nodes[sn].places {
            items[p.item].at[0] = snap(items[p.item].at[0] + dx);
            items[p.item].at[1] = snap(items[p.item].at[1] + dy);
        }
    }
}
