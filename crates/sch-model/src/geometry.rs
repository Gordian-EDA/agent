//! Placement geometry: an item's body+text rect, overlap counts, pin endpoints,
//! and the sheet spacing constants every seeding pass shares.

use std::collections::BTreeMap;

use geom::{EPS, GRID_50_MIL, Point2};
use kicad_symbol::geometry::PinGeom;

use crate::ir::LayoutIr;
use crate::item::Item;
use crate::text::text_width;

/// Column channel width (mm) — clears a wide IC's pin text.
pub const COL_GAP: f64 = 6.35;
/// Row pitch (mm) for a vertical stack. Tighter spacing lets a rotation move flip a
/// clean vertical divider leg horizontal, so the conventional spacing is kept here.
pub const ROW_GAP: f64 = 5.08;
/// Sheet margin (mm) the normalized placement starts at.
pub const MARGIN: f64 = 12.7;
/// Quantization for comparing coordinates by grid cell.
pub const GRID_KEY: f64 = GRID_50_MIL.pitch();

/// An item's body rect at position `at`. Uses the FULL `approx_size` (which
/// already pads 2.54 mm/side) so the placement overlap check reserves room for
/// the symbol body *and* its value/refdes text — matching what the
/// readability lint flags as an overlap, so a layout the climb accepts is one
/// the lint passes. (The router uses its own, tighter solid extent in
/// `emit::route_scene`; this looser one is only for symbol-vs-symbol spacing.)
///
/// The text reservation follows the ACTUAL emitted `Reference`/`Value` strings and
/// the spot the writer's field solver will choose for them, for EVERY part kind —
/// an IC's long MPN value (`emit::gather` gives a ≥3-pin part its part name when the
/// author wrote none) needs the widest reservation of all, and reserving nothing for
/// it is what let a neighbour land on top of the `MCP1703` label.
pub fn item_rect(it: &Item, at: impl Into<::geom::Point2>) -> ::geom::Rect {
    let at = at.into();
    let s = it.geom.approx_size();
    let quarter = ((it.angle / 90.0).round() as i64).rem_euclid(2) == 1;
    let (w, h) = if quarter { (s[1], s[0]) } else { (s[0], s[1]) };
    let (hw, hh) = (
        (w / 2.0).max(geom::GRID_50_MIL.pitch()),
        (h / 2.0).max(geom::GRID_50_MIL.pitch()),
    );
    let mut r = ::geom::Rect::new(at[0] - hw, at[1] - hh, at[0] + hw, at[1] + hh);
    let [l, rt, t, b] = field_pad(it, hw * 2.0, hh * 2.0);
    r.min_x -= l;
    r.max_x += rt;
    r.min_y -= t;
    r.max_y += b;
    r
}

/// The room the writer's field solver will need for an item's emitted
/// `Reference`/`Value` text, as a pad per side of its body: `[left, right, top, bottom]`
/// in the PLACED frame, given the body's rotated size.
///
/// The text solver bands an IC's (≥3-pin) or a wide body's field pair
/// ABOVE/BELOW the body, centred, and stacks a tall 2-pin part's to the RIGHT. The band's
/// two 1.6 mm lines reach ~4.8 mm past the SOLID body, of which `approx_size` already
/// pads 2.54 mm.
///
/// [`item_rect`] grows an item's rect by this so the placement search keeps neighbours
/// out of the text's spot, and [`crate::cells::apply_cells`] sizes its grid tracks by it
/// so an IC's long MPN gets a column wide enough to hold it in the first place.
pub fn field_pad(it: &Item, w: f64, h: f64) -> [f64; 4] {
    const BAND: f64 = 2.24;
    let text = text_width(&it.value).max(text_width(&it.refdes));
    if it.geom.pins.len() >= 3 || w > h {
        let spill = (text / 2.0 - w / 2.0).max(0.0);
        [spill, spill, BAND, BAND]
    } else {
        [0.0, text + geom::GRID_50_MIL.pitch(), 0.0, 0.0]
    }
}

/// Count pairs of items whose bodies overlap — the hard "never let two symbols
/// collide" wall. Catches adjacent-cell collisions the same-cell check misses.
pub fn body_overlap_count(items: &[Item]) -> usize {
    let mut n = 0;
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            if item_rect(&items[i], items[i].at).overlaps(&item_rect(&items[j], items[j].at)) {
                n += 1;
            }
        }
    }
    n
}

/// Violations of the author's per-block `layout:` relative ordering (`ir.grid`).
/// For each pair of gridded parts whose grid boxes are DISJOINT on an axis, the
/// search must hold that order: A strictly left of B (`A.col_max < B.col_min`)
/// requires A's body centre left of B's; A strictly above B (`A.row_max <
/// B.row_min`) requires A above B (smaller y). Boxes that OVERLAP on an axis — a
/// column-span float like a tall IC — impose no constraint on that axis, so the
/// part floats within its span. Empty grid ⇒ 0 (no `layout:` / sidecar path).
pub fn grid_order_viol(items: &[Item], ir: &LayoutIr) -> usize {
    if ir.grid.is_empty() {
        return 0;
    }
    let pos: BTreeMap<&str, [f64; 2]> = items
        .iter()
        .map(|it| (it.refdes.as_str(), it.at.into()))
        .collect();
    let g: Vec<(&String, &[i32; 4])> = ir.grid.iter().collect();
    let mut viol = 0;
    for i in 0..g.len() {
        for j in (i + 1)..g.len() {
            let (ra, ba) = g[i];
            let (rb, bb) = g[j];
            let (Some(pa), Some(pb)) = (pos.get(ra.as_str()), pos.get(rb.as_str())) else {
                continue;
            };
            // Columns → left/right, only when the two boxes share no column.
            if (ba[2] < bb[0] && pa[0] >= pb[0] - EPS) || (bb[2] < ba[0] && pb[0] >= pa[0] - EPS) {
                viol += 1;
            }
            // Rows → above/below (smaller y is higher), only when row-disjoint.
            if (ba[3] < bb[1] && pa[1] >= pb[1] - EPS) || (bb[3] < ba[1] && pb[1] >= pa[1] - EPS) {
                viol += 1;
            }
        }
    }
    viol
}

/// Compute the sheet-space connection endpoint of a pin on a placed instance.
///
/// ## What "connection endpoint" means
///
/// In a `.kicad_sym`, a pin's `(at x y angle)` is the pin's **connection point**
/// — the tip where wires/labels attach — and the pin line extends `length` mm
/// *into the symbol body* along `angle`. So the connection point is exactly the
/// pin's local `at`; no `length` projection is applied (projecting by `length`
/// would land inside the body, off the connection). E.g. Device:R pin 1 at local
/// `(0, 3.81)` maps to sheet `(inst_x, inst_y - 3.81)`.
///
/// ## The transform (symbol space → sheet space)
///
/// KiCAD symbol Y grows **upward**; the schematic sheet Y grows **downward**. A
/// placed instance applies, in order: a rotation by the instance `angle`, the
/// Y-flip into sheet space, the optional `(mirror y)`, then a translation to the
/// instance position. Concretely, for a local point `(lx, ly)`:
///
/// 1. **Rotate** by the instance angle θ (KiCAD rotates counter-clockwise in
///    symbol space): `(lx·cosθ − ly·sinθ, lx·sinθ + ly·cosθ)`.
/// 2. **Y-flip**: `(rx, −ry)` — the sheet-space offset.
/// 3. **Mirror** (`(mirror y)`): negate the SHEET x. It comes after the rotation,
///    not before it: negating the local x instead agrees at 0°/180° but is the
///    point-reflection of the truth at 90°/270°, which transposes a 2-pin part's
///    two pins and wires each to the other's net.
/// 4. **Translate**: `sheet = (inst_x + ox, inst_y + oy)`.
///
/// At θ = 0 with no mirror this reduces to `(inst_x + lx, inst_y − ly)`, the
/// spike's proven form. Angles are restricted to 0/90/180/270 in practice, so
/// the sin/cos are exact (±1, 0) and the result stays on the grid; we still snap
/// to absorb floating-point dust.
pub fn pin_endpoint(
    pin: &PinGeom,
    inst_at: impl Into<Point2>,
    inst_angle: f64,
    mirror: bool,
) -> [f64; 2] {
    let inst_at = inst_at.into();
    let off = pin.at.transform_offset(inst_angle, mirror);
    GRID_50_MIL
        .snap_point(Point2::new(inst_at.x + off[0], inst_at.y + off[1]))
        .into()
}

