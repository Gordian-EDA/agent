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

/// An item's SYMBOL BODY at position `at`, rotation-aware — the space the drawing
/// physically occupies, with no allowance for text.
///
/// This is the clearance geometry: what a legaliser, a packer or an ownership test means
/// by "something else is already here". It is exactly the box the writer's text solver
/// treats as solid (`write/build.rs` seeds `half_extents` from the same
/// [`SymbolGeometry::approx_size`]), so what the placer packs to and what the typesetter
/// routes text around are one rectangle. `sch_doc::body_rect` is the same idea read off a
/// placed document's embedded graphics.
pub fn body_rect(it: &Item, at: impl Into<::geom::Point2>) -> ::geom::Rect {
    let at = at.into();
    let s = it.geom.approx_size();
    let quarter = ((it.angle / 90.0).round() as i64).rem_euclid(2) == 1;
    let (w, h) = if quarter { (s[1], s[0]) } else { (s[0], s[1]) };
    let (hw, hh) = (
        (w / 2.0).max(geom::GRID_50_MIL.pitch()),
        (h / 2.0).max(geom::GRID_50_MIL.pitch()),
    );
    ::geom::Rect::new(at[0] - hw, at[1] - hh, at[0] + hw, at[1] + hh)
}

/// An item's body PLUS the room its emitted `Reference`/`Value` text needs
/// ([`field_pad`]) — the placement-search geometry, so a layout the climb accepts is one
/// the readability lint passes.
///
/// Distinct from [`body_rect`] on purpose: text is a real claim on the sheet when the
/// search is choosing a layout, and no claim at all when something else is only asking
/// whether it may stand here.
pub fn item_rect(it: &Item, at: impl Into<::geom::Point2>) -> ::geom::Rect {
    let at = at.into();
    let mut r = body_rect(it, at);
    let [l, rt, t, b] = field_pad(it, r.width(), r.height());
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
/// Only the band an IC or a wide body stacks its field pair in is reserved. That band IS
/// where the solver puts them: for a ≥3-pin part every candidate it offers is a band
/// above/below or a corner (`write/textsolve.rs`), so reserving it is the search agreeing
/// with the typesetter rather than double-booking the sheet.
///
/// A tall 2-pin passive gets NOTHING. Its fields have ten candidate spots, of which the
/// right-hand one this used to reserve is one; paying `text_width + a grid step` of
/// column width for a claim the solver abandons the moment it is inconvenient made every
/// passive column 4.6-6.8 mm wider than the drawing in it — a third of the gap between
/// our part spacing and a human's.
pub fn field_pad(it: &Item, w: f64, h: f64) -> [f64; 4] {
    const BAND: f64 = 2.24;
    if it.geom.pins.len() < 3 && w <= h {
        return [0.0; 4];
    }
    let text = text_width(&it.value).max(text_width(&it.refdes));
    let spill = (text / 2.0 - w / 2.0).max(0.0);
    [spill, spill, BAND, BAND]
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

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_symbol::geometry::{PinGeom, SymbolGeometry};

    fn part(refdes: &str, value: &str, pins: usize) -> Item {
        let pin = |n: usize| PinGeom {
            number: (n + 1).to_string(),
            name: "~".into(),
            at: geom::Point2::new(0.0, if n == 0 { 3.81 } else { -3.81 }),
            angle: 0.0,
            length: 2.54,
            unit: 1,
        };
        Item {
            refdes: refdes.into(),
            block: String::new(),
            part: "Device:R".into(),
            value: value.into(),
            footprint: None,
            geom: SymbolGeometry {
                lib_id: "Device:R".into(),
                pins: (0..pins).map(pin).collect(),
                raw_definition: String::new(),
            },
            pins: Vec::new(),
            at: [0.0, 0.0].into(),
            angle: 0.0,
            unit: 1,
            mirror: false,
            frozen: false,
            preseeded: false,
        }
    }

    #[test]
    fn a_tall_passive_claims_no_text_column() {
        let r = part("R1", "100nF", 2);
        assert_eq!(item_rect(&r, r.at), body_rect(&r, r.at));
    }

    #[test]
    fn an_ic_keeps_the_band_its_fields_land_in() {
        let u = part("U1", "MCP1703", 3);
        let (body, full) = (body_rect(&u, u.at), item_rect(&u, u.at));
        assert!(full.min_y < body.min_y && full.max_y > body.max_y);
    }
}
