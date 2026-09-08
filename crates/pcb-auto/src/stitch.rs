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
const PROBE_STEP: f64 = 0.5;
/// How much of the via's ring has to sit inside the island, sampled around its rim.
const RIM_SAMPLES: usize = 8;
/// A board only needs so many stitches; past this the pour is not the problem.
const MAX_VIAS: usize = 60;

/// One filled piece of a pour.
struct Island {
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

    // what is already tied: a through-hole pad or an existing via of the net bridges both layers
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

    fn clear(obstacles: &[Obstacle], p: Point, need: f64) -> bool {
        obstacles.iter().all(|o| o.clears(p, need))
    }
    let well_inside = |p: Point, isl: &Island| {
        point_in_polygon(p, &isl.poly)
            && (0..RIM_SAMPLES).all(|k| {
                let a = std::f64::consts::TAU * k as f64 / RIM_SAMPLES as f64;
                point_in_polygon((p.0 + need * a.cos(), p.1 + need * a.sin()), &isl.poly)
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
            if ties.iter().any(|t| point_in_polygon(*t, &isl.poly)) {
                continue; // already bridged to the other layer
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
                        if well_inside(p, isl) && well_inside(p, target) && clear(&obstacles, p, need) {
                            placed = Some(p);
                            break 'search;
                        }
                    }
                }
            }
            if let Some(p) = placed {
                board.add_via(p, via_size, via_drill, gnd.id, (top, bottom));
                obstacles.push(Obstacle::Disc(p, r));
                ties.push(p);
                added += 1;
            }
        }
    }
    Ok(added)
}
