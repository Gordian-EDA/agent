//! Footprint → placement bridge: turn a parsed [`Footprint`] into a
//! [`pcb_engine::placement::Part`], and move a template board's footprints to an
//! engine placement. The placement-side companion to [`crate::pcb`]'s
//! board↔[`RouteProblem`] translation.
//!
//! ## The courtyard-enclosing rule (the load-bearing invariant)
//!
//! A placement [`Part`]'s courtyard is `courtyard_w`/`courtyard_h` **centered on
//! the part origin** — a rectangle symmetric about (0,0). The engine legalizes on
//! courtyards alone, so for courtyard clearance to *imply* pad clearance the
//! courtyard MUST enclose every pad (otherwise two gap-legal courtyards could
//! still short foreign copper — the finding recorded in `placement.rs`'s
//! `r0603` test helper).
//!
//! A KiCAD `F.CrtYd` outline is **not** generally symmetric about the footprint
//! origin (e.g. the vendored `PinHeader_1x02` courtyard spans y `-1.77..4.32`),
//! and may even under-cover the pads. We therefore build an **origin-symmetric**
//! courtyard whose half-extent on each axis is the *maximum absolute coordinate*
//! over BOTH the footprint courtyard corners AND the pad copper bbox:
//!
//! ```text
//! hw = max(|crtyd.min_x|, |crtyd.max_x|, |pad_bbox.min_x|, |pad_bbox.max_x|)
//! hh = max(|crtyd.min_y|, |crtyd.max_y|, |pad_bbox.min_y|, |pad_bbox.max_y|)
//! courtyard_w = 2·hw,  courtyard_h = 2·hh
//! ```
//!
//! This is conservative (≥ KiCAD's asymmetric courtyard) but **guarantees** the
//! invariant: the courtyard encloses the pads on every side. Folding in the
//! footprint courtyard too means a tight body-hugging `F.CrtYd` never *shrinks*
//! the keep-out below the real one. Pad rotation is folded into each pad's AABB
//! exactly as [`crate::pcb`] does for board pads.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use kiutils_kicad::PcbFile;
use pcb_engine::placement::{Part, Placement, PartPad};
use pcb_engine::problem::{LayerRef, Point2};

use crate::footlib::{BBox, Footprint, FootprintPad, PadTechnology};

/// Build a placement [`Part`] from a parsed [`Footprint`].
///
/// `reference` is the schematic designator the part will carry on the board.
/// `net_map` maps a pad *number* (`"1"`, `"2"`, `"A1"`) to the net name that pad
/// belongs to; a pad whose number is absent from the map is left unconnected
/// (`net: None`). Pad offsets, sizes and layers are carried over; layers map
/// `F.Cu`→top, `B.Cu`→bottom, and a `*.Cu` / through-hole pad to both copper
/// faces (consistent with how [`crate::pcb`] reads board pads).
///
/// The courtyard follows the origin-symmetric enclosing rule documented on this
/// module: it encloses both the footprint's `F.CrtYd` and every pad.
pub fn part_from_footprint(
    footprint: &Footprint,
    reference: &str,
    net_map: &BTreeMap<String, String>,
) -> Part {
    part_from_footprint_layers(footprint, reference, net_map, 2)
}

/// [`part_from_footprint`] for a board of `layer_count` copper layers. A
/// through-hole pad spans EVERY copper layer (its plated barrel passes through
/// all of them), so on a 4-layer board its inner layers are correctly occupied —
/// otherwise the router would run an inner-layer trace straight through a header
/// pin and short it (a defect KiCAD's DRC catches but the 2-layer pad model hid).
pub fn part_from_footprint_layers(
    footprint: &Footprint,
    reference: &str,
    net_map: &BTreeMap<String, String>,
    layer_count: u32,
) -> Part {
    let pads: Vec<PartPad> = footprint
        .pads
        .iter()
        .map(|p| part_pad(p, net_map, layer_count))
        .collect();

    let (courtyard_w, courtyard_h) = enclosing_courtyard(footprint);

    Part {
        reference: reference.to_owned(),
        courtyard_w,
        courtyard_h,
        pads,
        locked: None,
    }
}

/// Translate one library [`FootprintPad`] into a placement [`PartPad`].
fn part_pad(pad: &FootprintPad, net_map: &BTreeMap<String, String>, layer_count: u32) -> PartPad {
    PartPad {
        number: pad.number.clone(),
        offset: Point2 {
            x: pad.at[0],
            y: pad.at[1],
        },
        width: pad.size[0],
        height: pad.size[1],
        layers: pad_layers(pad, layer_count),
        net: net_map.get(&pad.number).cloned(),
    }
}

/// Every copper layer of a `layer_count`-layer board as a [`LayerRef`]:
/// `top, inner1, …, inner(layer_count-2), bottom`.
fn all_copper_layers(layer_count: u32) -> Vec<LayerRef> {
    let n = layer_count.max(2);
    let mut v = vec![LayerRef::top()];
    for i in 1..=(n.saturating_sub(2)) {
        v.push(LayerRef(format!("inner{i}")));
    }
    v.push(LayerRef::bottom());
    v
}

/// The engine [`LayerRef`]s a pad sits on. A surface-mount pad on a single face
/// maps to that face; a through-hole / `*.Cu` pad spans EVERY copper layer of the
/// board (its barrel is through-plated), so the router treats the inner layers
/// under it as occupied too.
fn pad_layers(pad: &FootprintPad, layer_count: u32) -> Vec<LayerRef> {
    let spans_all = matches!(pad.technology, PadTechnology::ThruHole | PadTechnology::NpThruHole)
        || pad.layers.iter().any(|l| l == "*.Cu");
    if spans_all {
        return all_copper_layers(layer_count);
    }
    let on_front = pad.layers.iter().any(|l| l == "F.Cu");
    let on_back = pad.layers.iter().any(|l| l == "B.Cu");
    match (on_front, on_back) {
        (true, true) => all_copper_layers(layer_count),
        (false, true) => vec![LayerRef::bottom()],
        // Default (front-only, or no copper layer named) to the top face.
        _ => vec![LayerRef::top()],
    }
}

/// Origin-symmetric courtyard `(width, height)` enclosing both the footprint
/// courtyard and the pad copper bbox — see the module docs for the rule.
fn enclosing_courtyard(footprint: &Footprint) -> (f64, f64) {
    let mut hw = abs_half(&footprint.courtyard).0;
    let mut hh = abs_half(&footprint.courtyard).1;
    if let Some(pad_bbox) = pad_bbox(&footprint.pads) {
        let (pw, ph) = abs_half(&pad_bbox);
        hw = hw.max(pw);
        hh = hh.max(ph);
    }
    (hw * 2.0, hh * 2.0)
}

/// Max absolute coordinate of a bbox on each axis: the half-extent of the
/// smallest origin-centered rectangle enclosing the bbox.
fn abs_half(b: &BBox) -> (f64, f64) {
    (
        b.min_x.abs().max(b.max_x.abs()),
        b.min_y.abs().max(b.max_y.abs()),
    )
}

/// Bounding box of every pad's copper rectangle (rotation folded into an AABB,
/// matching `footlib`'s pad-corner conservatism). `None` if there are no pads.
fn pad_bbox(pads: &[FootprintPad]) -> Option<BBox> {
    let mut it = pads.iter();
    let first = it.next()?;
    let mut b = pad_aabb(first);
    for p in it {
        let pb = pad_aabb(p);
        b.min_x = b.min_x.min(pb.min_x);
        b.min_y = b.min_y.min(pb.min_y);
        b.max_x = b.max_x.max(pb.max_x);
        b.max_y = b.max_y.max(pb.max_y);
    }
    Some(b)
}

/// One pad's axis-aligned copper bbox in the footprint frame (local pad rotation
/// folded into the enclosing AABB).
fn pad_aabb(pad: &FootprintPad) -> BBox {
    let [cx, cy] = pad.at;
    let (hw, hh) = rotated_aabb_half(pad.size[0], pad.size[1], pad.rotation);
    BBox {
        min_x: cx - hw,
        min_y: cy - hh,
        max_x: cx + hw,
        max_y: cy + hh,
    }
}

/// Axis-aligned half-extents of a `w × h` rectangle rotated `deg` degrees —
/// identical formula to `footlib`/`pcb`.
fn rotated_aabb_half(w: f64, h: f64, deg: f64) -> (f64, f64) {
    let theta = deg.to_radians();
    let (s, c) = theta.sin_cos();
    let hw = (w / 2.0 * c).abs() + (h / 2.0 * s).abs();
    let hh = (w / 2.0 * s).abs() + (h / 2.0 * c).abs();
    (hw, hh)
}

// ── move_footprints: re-seat a template board to an engine placement ─────────
//
// Write path: **template + move** (not from-scratch construction). A template
// `.kicad_pcb` already carries the footprints with their pads, nets and the net
// table; the engine only decides *where* each one goes. So we relocate each
// footprint's origin to its [`Placement`] rather than synthesising footprint
// bodies (kiutils can read pads/nets but its `ast_mut` footprint edits do not
// round-trip through `write()`, the same limitation `pcb::write_solution`
// documents). We therefore edit the source text directly, exactly as
// `write_solution` splices copper: locate each footprint block by its
// `Reference` property and rewrite that block's footprint-level `(at …)` line.
// Every other byte is preserved, and the result is re-parsed to validate that
// each footprint landed where the placement asked before promoting the file.

/// Move each footprint in the template board at `pcb_path` to its position (and
/// rotation) in `placements`, writing the result to `out_path`.
///
/// A footprint is matched to a [`Placement`] by its `Reference` property. Only
/// the footprint-level `(at x y [rot])` is rewritten — pad offsets stay in the
/// footprint's own frame, so KiCAD re-derives every pad's board position from
/// the new origin/rotation (the same model `pcb::pad_center` reads back).
/// Placements with no matching footprint, and footprints with no matching
/// placement, are left untouched (the caller's coherence check, not ours to
/// silently invent geometry).
///
/// Returns an [`io::Error`] if the board cannot be read/parsed, if a footprint
/// has no rewritable `(at …)`, or if the rewritten board fails to re-parse with
/// every targeted footprint at its requested position.
pub fn move_footprints(
    pcb_path: &Path,
    placements: &[Placement],
    out_path: &Path,
) -> io::Result<()> {
    // Validate the input parses up front (same baseline discipline as
    // write_solution); the parsed footprints also tell us which refs exist.
    let _ = PcbFile::read(pcb_path).map_err(map_kiutils_err)?;
    let source = std::fs::read_to_string(pcb_path)?;

    let by_ref: BTreeMap<&str, &Placement> =
        placements.iter().map(|p| (p.reference.as_str(), p)).collect();

    let rewritten = rewrite_footprint_positions(&source, &by_ref)?;

    // Re-parse and verify every targeted footprint landed where we asked before
    // the file is promoted — a board that fails verification is never written.
    std::fs::write(out_path, rewritten.as_bytes())?;
    let reread = PcbFile::read(out_path).map_err(map_kiutils_err)?;
    for fp in &reread.ast().footprints {
        let Some(reference) = fp.reference.as_deref() else {
            continue;
        };
        let Some(pl) = by_ref.get(reference) else {
            continue;
        };
        let [x, y] = fp.at.unwrap_or([0.0, 0.0]);
        if (x - pl.at.x).abs() > 1e-6 || (y - pl.at.y).abs() > 1e-6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "footprint {reference} did not move: wanted ({:.3},{:.3}), board has ({x:.3},{y:.3})",
                    pl.at.x, pl.at.y
                ),
            ));
        }
    }
    Ok(())
}

/// Rewrite the footprint-level `(at …)` of each footprint whose `Reference`
/// matches a placement. Operates on the raw text so every other byte is
/// preserved. A footprint matched to a placement but lacking an `(at …)` to
/// rewrite is an error (the template is malformed for our purposes).
fn rewrite_footprint_positions(
    source: &str,
    by_ref: &BTreeMap<&str, &Placement>,
) -> io::Result<String> {
    // Split the file into footprint blocks. Each block begins at a top-level
    // "(footprint " token (indented one tab in canonical KiCAD output). We scan
    // for the marker, carve the block up to the next marker, and rewrite within.
    const MARKER: &str = "\n\t(footprint ";
    let mut out = String::with_capacity(source.len() + 64);
    let mut rest = source;

    while let Some(rel) = rest.find(MARKER) {
        // Everything up to and including the marker's leading newline+tab stays.
        let block_start = rel + 1; // keep the '\n', start block at the '\t'
        out.push_str(&rest[..block_start]);
        let after = &rest[block_start..];

        // The block runs until the next footprint marker (or end of file).
        let block_len = after[MARKER.len() - 1..]
            .find(MARKER)
            .map(|i| i + (MARKER.len() - 1))
            .unwrap_or(after.len());
        let block = &after[..block_len];

        out.push_str(&rewrite_one_block(block, by_ref)?);
        rest = &after[block_len..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Rewrite a single footprint block's `(at …)` if its reference is targeted.
fn rewrite_one_block(block: &str, by_ref: &BTreeMap<&str, &Placement>) -> io::Result<String> {
    let Some(reference) = footprint_reference(block) else {
        return Ok(block.to_owned());
    };
    let Some(pl) = by_ref.get(reference.as_str()) else {
        return Ok(block.to_owned());
    };

    // The footprint-level (at …) is the FIRST "(at " on its own indented line
    // that is NOT inside a (property …)/(pad …)/(fp_…) child. In canonical KiCAD
    // output it appears at indentation "\n\t\t(at " before any child node. We
    // find the first such line and replace it wholesale.
    const AT_MARKER: &str = "\n\t\t(at ";
    let at_rel = block.find(AT_MARKER).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("footprint {reference} has no rewritable (at …) line"),
        )
    })?;
    let line_start = at_rel + 1; // after the '\n'
    let line_end = line_start
        + block[line_start..]
            .find('\n')
            .unwrap_or(block.len() - line_start);

    let rot = pl.rotation.rem_euclid(360);
    let at_line = if rot == 0 {
        format!("\t\t(at {} {})", fmt_num(pl.at.x), fmt_num(pl.at.y))
    } else {
        format!("\t\t(at {} {} {})", fmt_num(pl.at.x), fmt_num(pl.at.y), rot)
    };

    let mut new_block = String::with_capacity(block.len() + 8);
    new_block.push_str(&block[..line_start]);
    new_block.push_str(&at_line);
    new_block.push_str(&block[line_end..]);
    Ok(new_block)
}

/// The `Reference` property value of a footprint block, if present.
fn footprint_reference(block: &str) -> Option<String> {
    let key = "(property \"Reference\" \"";
    let i = block.find(key)? + key.len();
    let j = block[i..].find('"')?;
    Some(block[i..i + j].to_owned())
}

/// Format an `f64` the way KiCAD writes coordinates (shortest round-tripping
/// decimal, `-0.0` collapsed to `0`). Mirrors `pcb::fmt_num`.
fn fmt_num(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v}")
}

fn map_kiutils_err(e: kiutils_kicad::Error) -> io::Error {
    match e {
        kiutils_kicad::Error::Io(io) => io,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/footprints")
            .join(name)
    }

    fn net(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Every pad of the produced part must sit inside the origin-symmetric
    /// courtyard — the load-bearing invariant the engine relies on.
    fn assert_courtyard_encloses_pads(part: &Part) {
        let hw = part.courtyard_w / 2.0;
        let hh = part.courtyard_h / 2.0;
        for pad in &part.pads {
            let (phw, phh) = rotated_aabb_half(pad.width, pad.height, 0.0);
            assert!(
                pad.offset.x.abs() + phw <= hw + 1e-9 && pad.offset.y.abs() + phh <= hh + 1e-9,
                "pad {} at {:?} (±{phw:.3},±{phh:.3}) escapes courtyard ±({hw:.3},{hh:.3})",
                pad.number,
                pad.offset,
            );
        }
    }

    #[test]
    fn r0603_part_has_nets_and_enclosing_courtyard() {
        let fp = Footprint::load(&fixture("R_0603_1608Metric.kicad_mod")).unwrap();
        let part = part_from_footprint(&fp, "R1", &net(&[("1", "VOUT"), ("2", "GND")]));
        assert_eq!(part.reference, "R1");
        assert_eq!(part.pads.len(), 2);
        assert_eq!(part.pads[0].net.as_deref(), Some("VOUT"));
        assert_eq!(part.pads[1].net.as_deref(), Some("GND"));
        // SMD front pads → top only.
        assert_eq!(part.pads[0].layers, vec![LayerRef::top()]);
        assert_courtyard_encloses_pads(&part);
    }

    #[test]
    fn pinheader_thru_hole_pads_span_both_faces() {
        let fp = Footprint::load(&fixture("PinHeader_1x02_P2.54mm_Vertical.kicad_mod")).unwrap();
        let part = part_from_footprint(&fp, "J1", &net(&[("1", "VIN"), ("2", "GND")]));
        // Through-hole → both copper faces.
        assert_eq!(part.pads[0].layers, vec![LayerRef::top(), LayerRef::bottom()]);
        // The vendored courtyard is asymmetric about the origin (y -1.77..4.32),
        // and the pads run to y≈3.39 — the symmetric courtyard must still enclose
        // them.
        assert_courtyard_encloses_pads(&part);
        // A pad with no entry in the map is unconnected.
        let part2 = part_from_footprint(&fp, "J2", &net(&[("1", "VIN")]));
        assert_eq!(part2.pads[1].net, None);
    }

    #[test]
    fn sot23_three_pads_enclosed() {
        let fp = Footprint::load(&fixture("SOT-23.kicad_mod")).unwrap();
        let part =
            part_from_footprint(&fp, "U1", &net(&[("1", "VIN"), ("2", "GND"), ("3", "VOUT")]));
        assert_eq!(part.pads.len(), 3);
        assert_courtyard_encloses_pads(&part);
    }
}
