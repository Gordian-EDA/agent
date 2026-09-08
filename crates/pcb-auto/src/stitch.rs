//! Tie a ground pour back together after routing.
//!
//! Tracks cut a poured plane into islands. Each island still holds its own pads, so KiCad does not
//! delete it, but it reports every pair of islands as a missing connection — on a Blue Pill that is
//! most of what is left open after a clean route. A human answers this with stitching vias, and so
//! does this: every island that is not already tied to the copper on the other layer gets one via,
//! placed where both layers' fill overlaps and nothing else's copper is near.

use std::collections::HashMap;
use std::path::Path;

use kicad::KicadInstallation;

use crate::geom::{dist, point_in_polygon, point_rect_dist, polygon_area, seg_point_dist, BBox, Point};
use crate::model::Board;

/// Step of the lattice a stitch point is searched on.
const PROBE_STEP: f64 = 0.25;
/// How much of the via's ring has to sit inside the island, sampled around its rim.
const RIM_SAMPLES: usize = 8;
/// A board only needs so many stitches; past this the pour is not the problem.
const MAX_VIAS: usize = 60;

/// One filled piece of a pour.
struct Island {
    index: usize,
    layer: String,
    poly: Vec<Point>,
    bbox: BBox,
}

/// Copper a stitch via has to keep clear of, with the radius it has to keep clear by.
enum Obstacle {
    Seg(Point, Point, f64),
    Box(BBox, f64),
    Disc(Point, f64),
}

impl Obstacle {
    fn clears(&self, p: Point, need: f64) -> bool {
        match self {
            Obstacle::Seg(a, b, r) => seg_point_dist(*a, *b, p) > need + r,
            Obstacle::Box(bb, r) => point_rect_dist(p, bb) > need + r,
            Obstacle::Disc(c, r) => dist(*c, p) > need + r,
        }
    }
}

/// Refill `pcb`'s zones on a scratch copy and read back the ground pour's filled islands.
fn filled_islands(
    kicad: &KicadInstallation,
    pcb: &Path,
    net: &str,
) -> anyhow::Result<Vec<Island>> {
    let dir = tempfile::tempdir()?;
    let copy = dir.path().join("fill.kicad_pcb");
    std::fs::copy(pcb, &copy)?;
    let pro = pcb.with_extension("kicad_pro");
    if pro.is_file() {
        let _ = std::fs::copy(&pro, copy.with_extension("kicad_pro"));
    }
    kicad.refill_zones(&copy, true)?;
    let filled = Board::load(&copy)?;
    let mut out = Vec::new();
    for z in filled.zones() {
        if z.keepout.is_some() || z.net_name != net {
            continue;
        }
        let Some(layer) = z.layers.first().cloned() else {
            continue;
        };
        for poly in z.filled {
            if poly.len() >= 3 {
                out.push(Island {
                    index: out.len(),
                    bbox: BBox::of_points(poly.iter().copied()),
                    layer: layer.clone(),
                    poly,
                });
            }
        }
    }
    Ok(out)
}

/// Add ground stitching vias so every pour island is tied to the copper on the other layer.
///
/// `pcb` must be the file `board` was saved to; it is refilled on a scratch copy to read the fill.
/// Returns the number of vias added; the board is left for the caller to save.
pub fn stitch_pours(
    kicad: &KicadInstallation,
    board: &mut Board,
    pcb: &Path,
    net: &str,
    clearance: f64,
) -> anyhow::Result<usize> {
    let copper = board.copper_layers();
    if copper.len() < 2 {
        return Ok(0);
    }
    let islands = filled_islands(kicad, pcb, net)?;
    if islands.len() < 2 {
        return Ok(0);
    }
    // Where the pour is ALLOWED to be on each layer, as opposed to where it currently fills. A
    // via dropped on bare copper-free space inside the far layer's zone is absorbed by that pour
    // on the next refill, so an island with no fill under it is still stitchable.
    let zone_outline: HashMap<String, Vec<Point>> = board
        .zones()
        .into_iter()
        .filter(|z| z.keepout.is_none() && z.net_name == net && z.polygon.len() >= 3)
        .filter_map(|z| z.layers.first().cloned().map(|l| (l, z.polygon)))
        .collect();
    let Some(gnd) = board.net_by_name(net) else {
        return Ok(0);
    };
    let rules = board.design_rules();
    let (via_size, via_drill) = (rules.via_size, rules.via_drill);
    let r = via_size / 2.0;

    // per layer, the islands to tie and the fill on the other side to tie them to
    let mut by_layer: HashMap<&str, Vec<&Island>> = HashMap::new();
    for i in &islands {
        by_layer.entry(i.layer.as_str()).or_default().push(i);
    }
    let (top, bottom) = (copper[0].as_str(), copper[copper.len() - 1].as_str());

    // What the copper already joins. A tie point -- a ground via or a plated through-hole ground
    // pad -- bridges every island it lands in; a ground track joins the islands its two ends sit
    // in. Union-find over those says which islands are one piece of copper and which are adrift.
    let mut ties: Vec<Point> = board
        .vias()
        .iter()
        .filter(|v| v.net_id == gnd.id)
        .map(|v| v.pos)
        .collect();
    for f in board.footprints() {
        for p in &f.pads {
            if p.net_id == gnd.id && p.is_through() {
                ties.push(p.pos);
            }
        }
    }
    let mut uf = crate::geom::UnionFind::new(islands.len());
    // `layer: None` means "any layer": only a via or a plated through-hole bridges the stack.
    let hits = |p: Point, layer: Option<&str>| -> Vec<usize> {
        islands
            .iter()
            .enumerate()
            .filter(|(_, i)| layer.is_none_or(|l| i.layer == l) && point_in_polygon(p, &i.poly))
            .map(|(k, _)| k)
            .collect()
    };
    for t in &ties {
        let h = hits(*t, None);
        for w in h.windows(2) {
            uf.join(w[0], w[1]);
        }
    }
    for t in board.tracks() {
        if t.net_id != gnd.id {
            continue;
        }
        // ON ITS OWN LAYER: a back-side track running over a front-side island joins nothing,
        // and treating it as a join is what left islands looking connected and unstitched.
        let mut h = hits(t.start, Some(&t.layer));
        h.extend(hits(t.end, Some(&t.layer)));
        for w in h.windows(2) {
            uf.join(w[0], w[1]);
        }
    }
    // the component holding the largest island is the plane; everything else has to reach it
    let main = (0..islands.len())
        .max_by(|a, b| {
            polygon_area(&islands[*a].poly)
                .abs()
                .partial_cmp(&polygon_area(&islands[*b].poly).abs())
                .unwrap()
        })
        .map(|k| uf.find(k));

    let mut obstacles: Vec<Obstacle> = Vec::new();
    for t in board.tracks() {
        if t.net_id != gnd.id {
            obstacles.push(Obstacle::Seg(t.start, t.end, t.width / 2.0));
        }
    }
    for v in board.vias() {
        // even a same-net via is somewhere not to drill a second hole
        obstacles.push(Obstacle::Disc(v.pos, v.size / 2.0));
    }
    for f in board.footprints() {
        for p in &f.pads {
            if p.net_id != gnd.id || p.is_through() {
                obstacles.push(Obstacle::Box(p.bbox(), 0.0));
            }
        }
    }
    let need = r + clearance;
    // a stitch via is copper like any other: it owes the board edge its edge clearance
    let edge_keep = r + rules.edge_clearance;
    let outline = board.outline_polygon().unwrap_or_default();
    let inside_edge = |p: Point| -> bool {
        outline.is_empty()
            || (point_in_polygon(p, &outline)
                && (0..RIM_SAMPLES).all(|k| {
                    let a = std::f64::consts::TAU * k as f64 / RIM_SAMPLES as f64;
                    point_in_polygon(
                        (p.0 + edge_keep * a.cos(), p.1 + edge_keep * a.sin()),
                        &outline,
                    )
                }))
    };

    fn clear(obstacles: &[Obstacle], p: Point, need: f64) -> bool {
        obstacles.iter().all(|o| o.clears(p, need))
    }
    // The via only has to LAND on the island for the fill to take it -- the pour refills around
    // the barrel either way -- so the rim test asks for the drill, not the whole clearance ring.
    // Demanding the full ring is what left a sliver of pour stranded with nowhere to stitch it.
    let rim = via_drill / 2.0;
    let well_inside = |p: Point, isl: &Island| {
        point_in_polygon(p, &isl.poly)
            && (0..RIM_SAMPLES).all(|k| {
                let a = std::f64::consts::TAU * k as f64 / RIM_SAMPLES as f64;
                point_in_polygon((p.0 + rim * a.cos(), p.1 + rim * a.sin()), &isl.poly)
            })
    };

    let mut added = 0usize;
    for (layer, other) in [(top, bottom), (bottom, top)] {
        let Some(here) = by_layer.get(layer) else {
            continue;
        };
        let empty: Vec<&Island> = Vec::new();
        let there = by_layer.get(other).unwrap_or(&empty);
        for isl in here {
            if added >= MAX_VIAS {
                break;
            }
            if main == Some(uf.find(isl.index)) {
                continue; // already part of the plane
            }
            // The largest island on the far side is the one worth reaching; try them all, biggest
            // first, so a stitch lands on the main plane rather than on another crumb.
            let mut targets: Vec<&&Island> = there
                .iter()
                .filter(|o| o.bbox.overlaps(&isl.bbox))
                .collect();
            targets.sort_by(|a, b| {
                polygon_area(&b.poly)
                    .abs()
                    .partial_cmp(&polygon_area(&a.poly).abs())
                    .unwrap()
            });
            let mut placed = None;
            let far_zone = zone_outline.get(other);
            'search: for target in targets {
                let b = &isl.bbox;
                let nx = ((b.w() / PROBE_STEP) as usize).max(1);
                let ny = ((b.h() / PROBE_STEP) as usize).max(1);
                for j in 0..=ny {
                    for i in 0..=nx {
                        let p = (
                            b.x0 + i as f64 * PROBE_STEP,
                            b.y0 + j as f64 * PROBE_STEP,
                        );
                        if well_inside(p, isl) && well_inside(p, target) && clear(&obstacles, p, need)
                            && inside_edge(p)
                        {
                            placed = Some(p);
                            break 'search;
                        }
                    }
                }
            }
            if placed.is_none()
                && let Some(poly) = far_zone {
                    let b = &isl.bbox;
                    let nx = ((b.w() / PROBE_STEP) as usize).max(1);
                    let ny = ((b.h() / PROBE_STEP) as usize).max(1);
                    'fallback: for j in 0..=ny {
                        for i in 0..=nx {
                            let p = (b.x0 + i as f64 * PROBE_STEP, b.y0 + j as f64 * PROBE_STEP);
                            if well_inside(p, isl)
                                && point_in_polygon(p, poly)
                                && clear(&obstacles, p, need)
                                && inside_edge(p)
                            {
                                placed = Some(p);
                                break 'fallback;
                            }
                        }
                    }
                }
            if let Some(p) = placed {
                board.add_via(p, via_size, via_drill, gnd.id, (top, bottom));
                obstacles.push(Obstacle::Disc(p, r));
                added += 1;
            }
        }
    }
    Ok(added)
}
