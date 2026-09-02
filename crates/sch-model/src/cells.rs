//! The coarse `(col, row, orient)` cell grid: assign cells from the IR, then render
//! them into millimetres on text-aware tracks.

use std::collections::BTreeMap;

use crate::geometry::{COL_GAP, ROW_GAP, field_pad};
use crate::idiom::orient_angle;
use crate::ir::{Cell, LayoutIr, Orient};
use crate::item::Item;

/// The `ir.place` key of one UNIT of a multi-unit part.
pub fn unit_place_key(refdes: &str, unit: u8) -> String {
    format!("{refdes}#unit{unit}")
}

/// Pack sized tracks (column widths or row heights) in ascending index order
/// with `gap` between successive tracks, returning each index's centre. The
/// grid is ordinal: a skipped index reserves no space (the LLM uses col/row for
/// order and alignment, not metric spacing).
pub fn track_centres(sizes: &BTreeMap<i32, f64>, gap: f64) -> BTreeMap<i32, f64> {
    let mut out = BTreeMap::new();
    let mut edge = 0.0;
    for (&idx, &size) in sizes {
        out.insert(idx, edge + size / 2.0);
        edge += size + gap;
    }
    out
}

/// The coarse cell each item occupies. `assign_cells` reads the IR (unplaced
/// parts flow into spare columns on the right); the refinement loop perturbs
/// these; then [`apply_cells`] turns them into mm.
///
/// An [`Item`] marked `preseeded` carries a LIVE pose the caller owns and
/// [`apply_cells`] leaves it alone. `frozen` is NOT that signal — it only forbids the
/// search from moving an item, and a frozen item still gets its seed here.
pub fn assign_cells(items: &[Item], ir: &LayoutIr) -> Vec<Cell> {
    let max_col = ir.place.values().map(|c| c.col).max().unwrap_or(-1);
    let mut spare = max_col + 1;
    // `place` is keyed by refdes, so a MULTI-UNIT part's units (op-amp A/B + power unit)
    // all resolve to ONE cell — they'd seed coincident, then decongest scatters them in
    // arbitrary directions. Offset each successive same-refdes unit by one ordinal row so
    // they seed ADJACENT (a vertical stack); the sibling-cohesion term then holds them
    // clustered. Single-unit parts (one item/refdes) get offset 0 → byte-identical seed.
    let mut unit_seen: BTreeMap<&str, i32> = BTreeMap::new();
    items
        .iter()
        .map(|it| {
            let k = {
                let e = unit_seen.entry(it.refdes.as_str()).or_insert(0);
                let v = *e;
                *e += 1;
                v
            };
            if let Some(c) = ir.place.get(&unit_place_key(&it.refdes, it.unit)) {
                *c
            } else {
                match ir.place.get(&it.refdes) {
                    Some(c) => Cell {
                        col: c.col,
                        row: c.row + k,
                        orient: c.orient,
                    },
                    None => {
                        let c = spare;
                        spare += 1;
                        Cell {
                            col: c,
                            row: k,
                            orient: Orient::Down,
                        }
                    }
                }
            }
        })
        .collect()
}

pub fn apply_cells(items: &mut [Item], cells: &[Cell]) {
    apply_cells_with_gaps(items, cells, COL_GAP, ROW_GAP);
}

/// Apply coarse cells with caller-selected track gaps.
pub fn apply_cells_with_gaps(items: &mut [Item], cells: &[Cell], column_gap: f64, row_gap: f64) {
    let angles: Vec<f64> = items
        .iter()
        .zip(cells)
        .map(|(it, c)| orient_angle(&it.geom, c.orient))
        .collect();

    // Rotation-aware footprint (a quarter-turn swaps width and height), grown by the
    // room the item's emitted Reference/Value text will need. Without the text a
    // long-MPN IC gets a column exactly as wide as its body and its value smears onto
    // the neighbouring columns (the `MCP1703Ax-330xxTT` overlap). `pads` is asymmetric —
    // a tall passive stacks its fields to the RIGHT — so the item is seeded off the
    // track centre by half the imbalance, leaving the text's side of the track free.
    let (pads, dims): (Vec<[f64; 4]>, Vec<(f64, f64)>) = items
        .iter()
        .zip(&angles)
        .map(|(it, &angle)| {
            let s = it.geom.approx_size();
            let (w, h) = if (angle / 90.0).round() as i64 % 2 == 1 {
                (s[1], s[0])
            } else {
                (s[0], s[1])
            };
            let p = field_pad(it, w, h);
            (p, (w + p[0] + p[1], h + p[2] + p[3]))
        })
        .unzip();

    // Track sizes: a column is as wide as its widest part, a row as tall as its
    // tallest.
    let mut col_w: BTreeMap<i32, f64> = BTreeMap::new();
    let mut row_h: BTreeMap<i32, f64> = BTreeMap::new();
    for (c, &(w, h)) in cells.iter().zip(&dims) {
        let e = col_w.entry(c.col).or_insert(0.0);
        *e = e.max(w);
        let e = row_h.entry(c.row).or_insert(0.0);
        *e = e.max(h);
    }
    let col_x = track_centres(&col_w, column_gap);
    let row_y = track_centres(&row_h, row_gap);

    for (((it, c), &angle), p) in items.iter_mut().zip(cells).zip(&angles).zip(&pads) {
        // A preseeded item holds a live pose its caller owns (the region adapter's fixed
        // neighbours). Everything else — frozen idiom members included — is seeded here.
        if it.preseeded {
            continue;
        }
        it.at = [
            geom::GRID_50_MIL.snap(col_x[&c.col] + (p[0] - p[1]) / 2.0),
            geom::GRID_50_MIL.snap(row_y[&c.row] + (p[2] - p[3]) / 2.0),
        ]
        .into();
        it.angle = angle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_symbol::geometry::{PinGeom, SymbolGeometry};

    fn resistor(refdes: &str) -> Item {
        let pin = |number: &str, y: f64| PinGeom {
            number: number.to_string(),
            name: "~".to_string(),
            at: geom::Point2::new(0.0, y),
            angle: 0.0,
            length: 2.54,
            unit: 1,
        };
        Item {
            refdes: refdes.to_string(),
            part: "Device:R".to_string(),
            value: "1k".to_string(),
            footprint: None,
            geom: SymbolGeometry {
                lib_id: "Device:R".to_string(),
                pins: vec![pin("1", 3.81), pin("2", -3.81)],
                raw_definition: String::new(),
            },
            pins: vec![
                ("1".into(), "~".into(), None),
                ("2".into(), "~".into(), None),
            ],
            at: [0.0, 0.0].into(),
            angle: 0.0,
            unit: 1,
            mirror: false,
            frozen: false,
            preseeded: false,
        }
    }

    /// `frozen` forbids the search from moving an item; it never means the item already
    /// has a pose. Only a `preseeded` item (the region adapter's live neighbours) keeps
    /// the pose it arrived with.
    #[test]
    fn seeding_skips_preseeded_not_frozen() {
        let cell = |col, row| Cell {
            col,
            row,
            orient: Orient::Down,
        };
        let mut items = vec![resistor("R1"), resistor("R2"), resistor("R3")];
        items[0].frozen = true;
        items[1].preseeded = true;
        items[1].at = [80.0, 40.0].into();
        let ir = LayoutIr {
            place: [
                ("R1".to_string(), cell(2, 1)),
                ("R2".to_string(), cell(1, 0)),
                ("R3".to_string(), cell(0, 0)),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let cells = assign_cells(&items, &ir);
        apply_cells(&mut items, &cells);

        assert!(
            items[0].at[0] > 0.0 && items[0].at[1] > 0.0,
            "a frozen item on an empty sheet must still be seeded: {:?}",
            items[0].at
        );
        assert_eq!(items[1].at, [80.0, 40.0].into(), "preseeded pose was moved");
        assert!(
            items[2].at[0] < items[0].at[0],
            "cell columns must still order"
        );
    }
}
