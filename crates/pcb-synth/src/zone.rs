//! Copper **zones**: the pour/plane fill geometry ([`plane_fill_rects`]) and the
//! KiCAD `(zone …)` emit (copper planes, signal pours, and routing keep-out rule
//! areas). Co-located so an emitter has the whole zone story in one module.
//! Deterministic; never panics.

use std::fmt::Write as _;

use kicad_sexpr::fmt_num;
use pcb_model::{Bounds, Point2};

use crate::ids::synth_uuid;

/// A copper-plane zone to emit: the net it belongs to, the copper layer name
/// (e.g. `"In1.Cu"`), and the precomputed fill rectangles ([`plane_fill_rects`]).
#[derive(Debug, Clone)]
pub struct ZoneSpec {
    pub net_name: String,
    pub layer_name: String,
    pub fill_rects: Vec<[f64; 4]>,
    /// Zone copper-to-foreign clearance (mm) — the board's design clearance, so the
    /// zone is checked against the SAME rule the router used (not KiCAD's 0.2 default,
    /// which false-flags a finer-pitch board's plane).
    pub clearance: f64,
    /// Minimum zone copper width (mm) — the board's min trace width.
    pub min_thickness: f64,
}

/// A routing keep-out exported as a KiCAD rule area: tracks + vias are not allowed inside
/// `[min, max]` on the listed copper `layers`. Lets the finished board carry the design
/// intent the engine routed around, and gives KiCAD an independent check that it did.
#[derive(Debug, Clone)]
pub struct KeepoutZone {
    pub layers: Vec<String>,
    pub min: [f64; 2],
    pub max: [f64; 2],
}

/// Emit one routing keep-out as a KiCAD rule area: `(tracks not_allowed) (vias not_allowed)`
/// over the keep-out rectangle. Pads/copperpour are left ALLOWED so this never false-flags a
/// pad the placer legitimately kept clear or a plane already carved around the keep-out — it
/// only enforces (and documents) the track/via routing keep-out the engine honoured.
pub fn push_keepout_zone(out: &mut String, k: &KeepoutZone, idx: usize) {
    let uuid = synth_uuid(&format!("keepout:{idx}:{}:{}", k.min[0], k.min[1]));
    let layers = k
        .layers
        .iter()
        .map(|l| format!("\"{l}\""))
        .collect::<Vec<_>>()
        .join(" ");
    let _ = writeln!(
        out,
        "\t(zone\n\t\t(net 0)\n\t\t(net_name \"\")\n\t\t(layers {layers})\n\t\t(uuid \"{uuid}\")\n\
         \t\t(name \"keepout\")\n\t\t(hatch edge 0.5)\n\
         \t\t(keepout (tracks not_allowed) (vias not_allowed) (pads allowed) (copperpour allowed) (footprints allowed))\n\
         \t\t(polygon (pts (xy {} {}) (xy {} {}) (xy {} {}) (xy {} {})))\n\t)",
        fmt_num(k.min[0]), fmt_num(k.min[1]), fmt_num(k.max[0]), fmt_num(k.min[1]),
        fmt_num(k.max[0]), fmt_num(k.max[1]), fmt_num(k.min[0]), fmt_num(k.max[1])
    );
}

/// Emit one copper-plane `(zone …)` with the precomputed fill rectangles as
/// edge-sharing `filled_polygon` islands (KiCAD treats them as one connected
/// pour — see [`plane_fill_rects`]).
pub fn push_zone(out: &mut String, net_code: i32, z: &ZoneSpec, idx: usize) {
    let uuid = synth_uuid(&format!("zone:{}:{}", z.net_name, z.layer_name));
    let _ = idx;
    let _ = writeln!(
        out,
        // SOLID pad connection (`connect_pads yes`): a power/ground plane should
        // tie to its same-net pads with full copper, not thermal-relief spokes —
        // the spokes starve on large through-hole pads (mounting holes, TH power),
        // which KiCAD flags as `starved_thermal`. Solid is the standard plane
        // connection and is low-impedance. Foreign pads are still carved out by the
        // anti-pad keepouts baked into `fill_rects`, so this only ties same-net copper.
        "\t(zone\n\t\t(net {net_code})\n\t\t(net_name \"{}\")\n\t\t(layer \"{}\")\n\
         \t\t(uuid \"{uuid}\")\n\t\t(hatch edge 0.5)\n\t\t(connect_pads yes (clearance {clr}))\n\
         \t\t(min_thickness {mt})\n\t\t(fill yes (thermal_gap 0.3) (thermal_bridge_width 0.5))",
        z.net_name, z.layer_name, clr = fmt_num(z.clearance), mt = fmt_num(z.min_thickness)
    );
    // Zone outline = the board's fill bounding box (KiCAD requires a polygon; the
    // filled_polygon islands below are the authoritative copper).
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for r in &z.fill_rects {
        x0 = x0.min(r[0]);
        y0 = y0.min(r[1]);
        x1 = x1.max(r[2]);
        y1 = y1.max(r[3]);
    }
    if x0 <= x1 {
        let _ = writeln!(
            out,
            "\t\t(polygon (pts (xy {} {}) (xy {} {}) (xy {} {}) (xy {} {})))",
            fmt_num(x0), fmt_num(y0), fmt_num(x1), fmt_num(y0),
            fmt_num(x1), fmt_num(y1), fmt_num(x0), fmt_num(y1)
        );
    }
    for r in &z.fill_rects {
        let _ = writeln!(
            out,
            "\t\t(filled_polygon (layer \"{}\") (pts (xy {} {}) (xy {} {}) (xy {} {}) (xy {} {})))",
            z.layer_name,
            fmt_num(r[0]), fmt_num(r[1]), fmt_num(r[2]), fmt_num(r[1]),
            fmt_num(r[2]), fmt_num(r[3]), fmt_num(r[0]), fmt_num(r[3])
        );
    }
    out.push_str("\t)\n");
}

/// A copper-plane (power-pour) fill as axis-aligned rectangles tiling `bounds`
/// inset by `edge_margin`, MINUS a rectangular keep-out around each item in
/// `keepouts` (`(center, half_x, half_y)` — the halves already include the
/// required clearance; a via/pad passes equal halves, a keep-out region its rect
/// halves). Returns rects as `[min_x, min_y, max_x, max_y]`.
///
/// KiCAD treats edge-sharing `filled_polygon` islands as one connected plane
/// (verified against kicad-cli), so a horizontal-band sweep produces a valid,
/// DRC-clean fill with NO clipping / keyhole geometry: cut the board into y-bands
/// at every keep-out edge, and in each band emit the x-segments left free by the
/// keep-outs active there. This is how a power net's many pins are joined without
/// routing each one — the lever a BGA's power balls need.
pub fn plane_fill_rects(
    bounds: &Bounds,
    edge_margin: f64,
    keepouts: &[(Point2, f64, f64)],
    outline: Option<&[Point2]>,
) -> Vec<[f64; 4]> {
    let (bx0, bx1) = (bounds.min_x + edge_margin, bounds.max_x - edge_margin);
    let (by0, by1) = (bounds.min_y + edge_margin, bounds.max_y - edge_margin);
    if bx1 <= bx0 || by1 <= by0 {
        return Vec::new();
    }
    // y-band boundaries: the board edges plus each keep-out's top/bottom (clamped).
    let mut ycuts: Vec<f64> = vec![by0, by1];
    for (c, _hx, hy) in keepouts {
        ycuts.push((c.y - hy).clamp(by0, by1));
        ycuts.push((c.y + hy).clamp(by0, by1));
    }
    // For a custom OUTLINE, add fine y-bands so the per-band polygon scanline clip
    // (taken at the band mid-y) follows the true edge smoothly instead of overhanging.
    if outline.is_some() {
        let mut y = by0;
        while y < by1 {
            ycuts.push(y);
            y += 0.5;
        }
    }
    ycuts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ycuts.dedup_by(|a, b| (*a - *b).abs() < 1e-6);

    let mut rects = Vec::new();
    for w in ycuts.windows(2) {
        let (y0, y1) = (w[0], w[1]);
        if y1 - y0 < 1e-6 {
            continue;
        }
        let ymid = (y0 + y1) / 2.0;
        // x-intervals blocked by keep-outs straddling this band, merged.
        let mut blocked: Vec<(f64, f64)> = keepouts
            .iter()
            .filter(|(c, _hx, hy)| c.y - hy < ymid && ymid < c.y + hy)
            .map(|(c, hx, _hy)| ((c.x - hx).max(bx0), (c.x + hx).min(bx1)))
            .filter(|(a, b)| b > a)
            .collect();
        blocked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (a, b) in blocked {
            match merged.last_mut() {
                Some(last) if a <= last.1 + 1e-9 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        // Free x-segments = [bx0, bx1] minus the merged blocked intervals.
        let mut free: Vec<(f64, f64)> = Vec::new();
        let mut x = bx0;
        for (a, b) in &merged {
            if a - x > 1e-6 {
                free.push((x, *a));
            }
            x = x.max(*b);
        }
        if bx1 - x > 1e-6 {
            free.push((x, bx1));
        }
        // Custom outline: clip each free segment to the polygon's interior at this band
        // (scanline x-spans at mid-y, inset by the edge margin), so copper never reaches
        // past the true edge — a pour/plane on a non-rectangular board.
        if let Some(poly) = outline {
            let spans: Vec<(f64, f64)> = polygon_x_spans(poly, ymid)
                .into_iter()
                .map(|(a, b)| (a + edge_margin, b - edge_margin))
                .filter(|(a, b)| b - a > 1e-6)
                .collect();
            let mut clipped = Vec::new();
            for (fa, fb) in &free {
                for (pa, pb) in &spans {
                    let (lo, hi) = (fa.max(*pa), fb.min(*pb));
                    if hi - lo > 1e-6 {
                        clipped.push((lo, hi));
                    }
                }
            }
            free = clipped;
        }
        for (a, b) in free {
            rects.push([a, y0, b, y1]);
        }
    }
    rects
}

/// The x-intervals where the horizontal line `y` is INSIDE polygon `poly` (scanline,
/// even-odd): the sorted edge crossings paired up. A convex shape gives one interval; a
/// concave one (a star) gives several. Used to clip a plane/pour fill to a custom outline.
fn polygon_x_spans(poly: &[Point2], y: f64) -> Vec<(f64, f64)> {
    let n = poly.len();
    if n < 3 {
        return Vec::new();
    }
    let mut xs: Vec<f64> = Vec::new();
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (&poly[j], &poly[i]);
        if (a.y > y) != (b.y > y) {
            xs.push(a.x + (y - a.y) / (b.y - a.y) * (b.x - a.x));
        }
        j = i;
    }
    xs.sort_by(|p, q| p.partial_cmp(q).unwrap_or(std::cmp::Ordering::Equal));
    xs.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0], c[1])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_fill_empty_is_single_inset_rect() {
        let b = Bounds { min_x: 0.0, max_x: 20.0, min_y: 0.0, max_y: 10.0 };
        let rects = plane_fill_rects(&b, 0.5, &[], None);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], [0.5, 0.5, 19.5, 9.5]);
    }

    #[test]
    fn plane_fill_carves_keepouts_and_stays_in_bounds() {
        let b = Bounds { min_x: 0.0, max_x: 20.0, min_y: 0.0, max_y: 20.0 };
        let ko = (Point2 { x: 10.0, y: 10.0 }, 0.65, 0.65);
        let rects = plane_fill_rects(&b, 0.5, &[ko], None);
        assert!(rects.len() > 1, "a central keep-out must split the fill");
        // The keep-out square [9.35,10.65]^2 must contain NO fill rect interior.
        let (kx0, kx1, ky0, ky1) = (9.35, 10.65, 9.35, 10.65);
        for r in &rects {
            // every rect within the inset board
            assert!(r[0] >= 0.5 - 1e-9 && r[2] <= 19.5 + 1e-9);
            assert!(r[1] >= 0.5 - 1e-9 && r[3] <= 19.5 + 1e-9);
            // and not overlapping the keep-out interior
            let overlap = r[0] < kx1 - 1e-6 && r[2] > kx0 + 1e-6 && r[1] < ky1 - 1e-6 && r[3] > ky0 + 1e-6;
            assert!(!overlap, "rect {r:?} overlaps the keep-out");
        }
    }
}
