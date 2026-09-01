//! `place::emit` — IR → millimetre orchestration: `gather` the parts, seed the
//! coarse grid into mm (`assign_cells`/`apply_cells`), drive the placement engine
//! (`emit_strategy`/`prepare_writer`), and assemble the routed `SchematicWriter`
//! (`build_writer`).

#![allow(clippy::items_after_test_module)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use kicad_symbol::geometry::SymbolGeometry;
use sch_check::model::{Component, Design, PinTarget};
use sch_check::{PinType, SymbolMeta, find_pin};

use crate::write::SchematicWriter;
use geom::Dir;
use sch_place::result::EmitOutput;

use super::*;
use sch_place::item::{Incidence, Item};

// The disjoint-set forest (over a caller-owned `parent` slice) lives in
// `geom::union_find`, shared with the desugar pin reconciler.
use sch_place::ir::{Cell, LayoutIr, Orient};

/// Compose every block's per-block `layout:` grid into one global relative seed:
/// refdes → (grid col, grid row). Each gridded block occupies its own column band
/// (declaration order, laid left→right); within a band a cell maps its refdes to
/// `(band_base + local_col, local_row)`. `None` (`~`) holes are skipped; a refdes
/// repeated in a column takes its FIRST occurrence (the seed — the search then
/// floats/spans it, since the grid is RELATIVE positioning, never an absolute
/// pin). Blocks with no grid contribute nothing here — the engine infers their
/// internal arrangement. Empty when no block carries a `layout:`.
pub(crate) fn grid_from_layout(design: &Design) -> BTreeMap<String, [i32; 4]> {
    let mut out: BTreeMap<String, [i32; 4]> = BTreeMap::new();
    let mut col_base = 0i32;
    for block in design.blocks.values() {
        if block.layout.is_empty() {
            continue;
        }
        let mut width = 0i32;
        for (r, row) in block.layout.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let Some(name) = cell else { continue };
                let (gc, gr) = (col_base + c as i32, r as i32);
                // Bounding box: a refdes in several cells (a column span) grows its
                // box; the seed uses the top-left, the order constraint the whole box.
                let e = out.entry(name.clone()).or_insert([gc, gr, gc, gr]);
                e[0] = e[0].min(gc);
                e[1] = e[1].min(gr);
                e[2] = e[2].max(gc);
                e[3] = e[3].max(gr);
                width = width.max(c as i32 + 1);
            }
        }
        // Next gridded block starts past this one's columns, so bands never overlap.
        col_base += width.max(1);
    }
    out
}

/// Every authored occurrence of a refdes, in row-major order. Repeated cells are
/// meaningful for multi-unit symbols: occurrence 1 seeds unit 1, occurrence 2
/// seeds unit 2, and so on. `grid_from_layout` deliberately keeps only their
/// bounding box for ordering; this companion view preserves the individual cells.
pub(crate) fn grid_occurrences(design: &Design) -> BTreeMap<String, Vec<(i32, i32)>> {
    let mut out: BTreeMap<String, Vec<(i32, i32)>> = BTreeMap::new();
    let mut col_base = 0i32;
    for block in design.blocks.values() {
        if block.layout.is_empty() {
            continue;
        }
        let mut width = 0i32;
        for (r, row) in block.layout.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let Some(name) = cell else { continue };
                out.entry(name.clone())
                    .or_default()
                    .push((col_base + c as i32, r as i32));
                width = width.max(c as i32 + 1);
            }
        }
        col_base += width.max(1);
    }
    out
}

pub(crate) fn unit_place_key(refdes: &str, unit: u8) -> String {
    format!("{refdes}#unit{unit}")
}

// ---------------------------------------------------------------------------
// Compiler internal model.
// ---------------------------------------------------------------------------

/// Spacing constants (mm). All on the 1.27 grid. Kept tight: the real minimum
/// spacing is now content-driven by the body+text overlap rect (`item_rect`)
/// that the refine wall and `decongest` enforce, so these are just the initial
/// table's slack — small, with the overlap model spreading parts only as far as
/// their bodies and side-mounted text actually need.
pub const COL_GAP: f64 = 6.35; // 5 grid — column channel (clears a wide IC's pin text)
pub const ROW_GAP: f64 = 5.08; // 4 grid — vertical stack; tighter lets the rotation
// move flip a clean vertical divider leg horizontal (lower wire cost, but
// unconventional), so keep the conventional spacing here.
pub(crate) const MARGIN: f64 = 12.7;

/// Resolve a component's pins to (number, name, net) using the canonical
/// symbol-wide number-first resolver.
pub(crate) fn resolve_pins(
    comp: &Component,
    geom: &SymbolGeometry,
    meta: &SymbolMeta,
) -> Vec<(String, String, Option<String>)> {
    let targets = resolve_pin_targets(comp, meta);
    geom.pins
        .iter()
        .map(|pg| {
            let net = match targets.get(&pg.number) {
                Some(PinTarget::Net(n)) => Some(n.clone()),
                _ => None,
            };
            (pg.number.clone(), pg.name.clone(), net)
        })
        .collect()
}

fn resolve_pin_targets<'a>(
    comp: &'a Component,
    meta: &SymbolMeta,
) -> BTreeMap<String, &'a PinTarget> {
    let mut targets = BTreeMap::new();
    for (key, target) in &comp.pins {
        for pin in sch_check::pins::resolve(meta, key) {
            targets.insert(pin.number.clone(), target);
        }
    }
    for (unit, pins) in &comp.units {
        let Some(unit) = authored_unit_number(unit) else {
            continue;
        };
        for (key, target) in pins {
            for pin in sch_check::pins::resolve(meta, key)
                .into_iter()
                .filter(|pin| pin.unit == unit)
            {
                targets.entry(pin.number.clone()).or_insert(target);
            }
        }
    }
    targets
}

/// Circuit YAML names symbol units `A`, `B`, ... while KiCad geometry numbers
/// them 1, 2, ... . Numeric unit keys are accepted too for generated designs.
fn authored_unit_number(name: &str) -> Option<u8> {
    let name = name.trim();
    if let Ok(unit) = name.parse::<u8>() {
        return (unit > 0).then_some(unit);
    }
    let mut chars = name.chars();
    let unit = chars.next()?.to_ascii_uppercase();
    if chars.next().is_none() && unit.is_ascii_uppercase() {
        Some(unit as u8 - b'A' + 1)
    } else {
        None
    }
}

fn used_symbol_units(comp: &Component, geom: &SymbolGeometry, meta: &SymbolMeta) -> Vec<u8> {
    let targets = resolve_pin_targets(comp, meta);
    let mut units: Vec<u8> = geom
        .pins
        .iter()
        .filter(|pin| targets.contains_key(&pin.number))
        .map(|pin| pin.unit.max(1))
        .collect();
    units.sort_unstable();
    units.dedup();
    if units.is_empty() {
        units.push(1);
    }
    units
}

#[cfg(test)]
mod resolve_pin_tests {
    use super::*;
    use kicad_symbol::geometry::PinGeom;

    fn pin(number: &str, name: &str, unit: u8) -> PinGeom {
        PinGeom {
            number: number.to_owned(),
            name: name.to_owned(),
            at: [0.0, 0.0].into(),
            angle: 0.0,
            length: 2.54,
            unit,
        }
    }

    #[test]
    fn resolves_authored_multi_unit_pins_on_their_kicad_units() {
        let mut comp = Component {
            part: "Amplifier_Operational:LM324".to_owned(),
            ..Component::default()
        };
        comp.pins
            .insert("4".to_owned(), PinTarget::Net("VCC".to_owned()));
        comp.units
            .entry("A".to_owned())
            .or_default()
            .insert("OUT".to_owned(), PinTarget::Net("OUT_A".to_owned()));
        comp.units
            .entry("B".to_owned())
            .or_default()
            .insert("OUT".to_owned(), PinTarget::Net("OUT_B".to_owned()));
        comp.units
            .entry("B".to_owned())
            .or_default()
            .insert("7".to_owned(), PinTarget::NoConnect);
        let geom = SymbolGeometry {
            lib_id: comp.part.clone(),
            pins: vec![
                pin("1", "OUT", 1),
                pin("5", "OUT", 2),
                pin("7", "-", 2),
                pin("4", "V+", 5),
            ],
            raw_definition: String::new(),
        };
        let mut provider = SymbolTable::mock();
        provider.mock_add(
            &comp.part,
            vec![
                ("1", "OUT", PinType::Other, 1),
                ("5", "OUT", PinType::Other, 2),
                ("7", "-", PinType::Other, 2),
                ("4", "V+", PinType::Other, 5),
            ],
        );
        let meta = provider.symbol(&comp.part).unwrap();

        assert_eq!(
            resolve_pins(&comp, &geom, &meta),
            vec![
                ("1".to_owned(), "OUT".to_owned(), Some("OUT_A".to_owned())),
                ("5".to_owned(), "OUT".to_owned(), Some("OUT_B".to_owned())),
                ("7".to_owned(), "-".to_owned(), None),
                ("4".to_owned(), "V+".to_owned(), Some("VCC".to_owned())),
            ]
        );
        assert_eq!(
            used_symbol_units(&comp, &geom, &meta),
            [1, 2, 5],
            "an explicitly no-connected unit must still be placed"
        );
    }

    #[test]
    fn authored_unit_letters_and_numbers_map_to_kicad_units() {
        assert_eq!(authored_unit_number("A"), Some(1));
        assert_eq!(authored_unit_number("d"), Some(4));
        assert_eq!(authored_unit_number("5"), Some(5));
        assert_eq!(authored_unit_number("0"), None);
        assert_eq!(authored_unit_number("Unit A"), None);
    }
}

// ---------------------------------------------------------------------------
// Public entry.
// ---------------------------------------------------------------------------

/// Emit a complete `.kicad_sch`. Pass `ir: None` for connectivity inference (production);
/// `Some(ir)` for a hand-tuned / sidecar frame (validation fixtures).
pub fn emit_strategy(
    env: &KicadInstallation,
    design: &Design,
    engine: Box<dyn PlacementEngine>,
    ir: Option<LayoutIr>,
) -> io::Result<EmitOutput> {
    let (w, mut out) = prepare_writer(env, design, ir, engine)?;
    out.sch = w.finish();
    Ok(out)
}

/// Lay out `design` under `strategy` and build its FINALIZED writer (placed,
/// routed, text-solved, reframed) WITHOUT rendering it. Returns the prepared
/// writer plus the readability metadata; `EmitOutput.sch` is left empty (the
/// caller either `finish`es this single writer or composes several into one).
/// This is the shared body of `emit_strategy` and the multi-block
/// `emit_anneal_writer` compose entry, so both judge the same geometry.
#[tracing::instrument(
    skip_all,
    fields(
        engine = engine.name(),
        design = design.name.as_deref().unwrap_or("<unnamed>")
    )
)]
pub(crate) fn prepare_writer(
    env: &KicadInstallation,
    design: &Design,
    ir: Option<LayoutIr>,
    engine: Box<dyn PlacementEngine>,
) -> io::Result<(SchematicWriter, EmitOutput)> {
    let mut problem = SchematicPlaceProblem::from_design(env, design)?;
    if problem.options.debug_timing {
        tracing::debug!("[place] engine = {}", engine.name());
    }
    let placement = engine.place(env, design, &mut problem, ir);
    let ir = placement.ir;

    let detected_idioms = ir.idioms.clone();
    let realizer = RoutedSheetRealizer::new(env, &problem.inc, &ir);
    let evaluator = RoutedEvaluator::new(&realizer);
    let mut w = realizer.realize_writer(
        design.name.as_deref(),
        &problem.items,
        RouteRealization::ShippedSheet,
    )?;
    if problem.options.debug_timing {
        diagnose_shorts(env, &w, &problem.items, &problem.inc, design);
    }
    add_orphan_label_columns(&mut w, design, &problem.inc);
    w.set_frame(true);
    w.prepare();
    let warnings = w.layout_warnings();
    let crossings = evaluator.crossings(&problem.items);
    Ok((
        w,
        EmitOutput {
            sch: String::new(),
            layout_warnings: warnings,
            crossings,
            detected_idioms,
        },
    ))
}

/// Build the complete schematic writer for a placed `items`: symbols (+mirror),
/// no-connects on unconnected pins, all wiring (rails + routed signals), and ERC
/// flags. Shared by the final emission and the refinement scorer so both judge
/// the same geometry — except `fan_risers`, a finalize-only correctness repair
/// (like `prepare`'s wire-split): two rails whose risers are collinear short, so
/// the shipped sheet fans them apart, but the per-move scorer skips it (the fan
/// is a transient mid-search artifact that would churn the placement otherwise).
#[allow(clippy::too_many_arguments)]
pub fn build_writer(
    env: &KicadInstallation,
    title: Option<&str>,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    fan_risers: bool,
) -> io::Result<SchematicWriter> {
    let mut w = SchematicWriter::new();
    if let Some(name) = title {
        w.set_title(name);
    }
    for it in items {
        // The item already carries its geometry — including for a part whose library
        // only this sheet has — so the writer never needs to look one up.
        w.register(&it.geom);
        w.add_symbol_full(
            env,
            &it.part,
            &it.refdes,
            &it.value,
            it.at,
            it.angle,
            it.footprint.as_deref(),
            &[],
            None,
        )?;
        if it.unit != 1 {
            w.set_unit_last(it.unit);
        }
        if it.mirror {
            w.set_mirror_last();
        }
    }
    for it in items {
        for (num, _name, net) in &it.pins {
            if net.is_none() {
                w.add_no_connect(env, &it.refdes, num)?;
            }
        }
    }
    let mut flag_points: BTreeMap<String, ([f64; 2], f64)> = BTreeMap::new();
    wire(
        env,
        &mut w,
        items,
        inc,
        ir,
        needs_flag,
        &mut flag_points,
        fan_risers,
    )?;
    for net in needs_flag {
        if let Some((at, angle)) = flag_points.get(net) {
            w.add_power_flag_at(env, &format!("#FLG_{net}"), *at, *angle)?;
        }
    }
    Ok(w)
}

/// Power nets needing a PWR_FLAG: every power-input pin's net and every declared
/// rail, minus any net already driven by a power-output pin (a regulator output,
/// say). KiCAD flags an undriven power-input pin as an error, so each such net
/// gets exactly one flag.
pub(crate) fn compute_needs_flag(
    env: &KicadInstallation,
    items: &[Item],
    ir: &LayoutIr,
) -> BTreeSet<String> {
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let (mut driven, mut power_input) = (BTreeSet::new(), BTreeSet::new());
    for it in items {
        let Some(meta) = provider.symbol(&it.part) else {
            continue;
        };
        for (num, _name, net) in &it.pins {
            if let (Some(net), Some(pm)) = (net, find_pin(&meta.pins, num)) {
                match pm.etype {
                    PinType::PowerOutput => {
                        driven.insert(net.clone());
                    }
                    PinType::PowerInput => {
                        power_input.insert(net.clone());
                    }
                    _ => {}
                }
            }
        }
    }
    let mut needs_flag: BTreeSet<String> = power_input;
    needs_flag.extend(ir.rails.keys().cloned());
    for net in &driven {
        needs_flag.remove(net);
    }
    needs_flag
}

// ---------------------------------------------------------------------------
// Gather + incidence.
// ---------------------------------------------------------------------------

pub(crate) fn gather(env: &KicadInstallation, design: &Design) -> io::Result<Vec<Item>> {
    let mut items = Vec::new();
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            if comp.dnp {
                continue;
            }
            // A power-symbol component (`power:GND`, `power:+5V`, …) is a power-net
            // DECLARATION, not a placed part: it tells the engine its net is a power
            // rail (via `NetAttrs.power`), and the rail/terminal drawing is emitted by
            // the power path (`emit_rail`), not as a gathered symbol. Skip it here.
            // A `label:*` component is likewise a net-LABEL declaration (marks a net a
            // port / draws its name), not a placed symbol — and has no geometry. Skip.
            if comp.part.starts_with("power:") || comp.part.starts_with("label:") {
                continue;
            }
            let geom = SymbolGeometry::load(env.symbol_dir(), &comp.part)?;
            let meta = provider.symbol(&comp.part).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("symbol metadata unavailable for {}", comp.part),
                )
            })?;
            let pins = resolve_pins(comp, &geom, &meta); // same order/len as geom.pins
            // An IC/connector (>=3 pins) with no authored value shows its part
            // name (the MPN) so the part is identifiable on the sheet — the
            // reference's "MCP1703A-3302" etc. Passives keep their authored value.
            let value = match comp.value.clone() {
                Some(v) if !v.is_empty() => v,
                _ if geom.pins.len() >= 3 => comp
                    .part
                    .rsplit(':')
                    .next()
                    .unwrap_or(&comp.part)
                    .to_string(),
                _ => String::new(),
            };
            // MULTI-UNIT SPLIT. A part's pins are spread across symbol units
            // (op-amp: A=1/2/3, B=5/6/7, power=4/8). KiCAD draws one unit per
            // placed instance, so we emit one Item per USED unit (a unit carrying
            // at least one assigned pin), each holding only that unit's pins. A
            // single-unit part collapses to exactly one Item (unit 1) — identical
            // to the old behaviour. Without this, only unit 1's pins ever reach the
            // netlist (the power pins and unit B silently vanish).
            let pin_unit: Vec<u8> = geom.pins.iter().map(|p| p.unit.max(1)).collect();
            let units = used_symbol_units(comp, &geom, &meta);
            for (k, &u) in units.iter().enumerate() {
                let unit_pins: Vec<(String, String, Option<String>)> = pins
                    .iter()
                    .zip(pin_unit.iter())
                    .filter(|pair| *pair.1 == u)
                    .map(|pair| pair.0.clone())
                    .collect();
                items.push(Item {
                    refdes: refdes.clone(),
                    part: comp.part.clone(),
                    // Show the MPN/value on the FIRST placed unit only — N copies
                    // of "MCP6002" across the units would just be clutter.
                    value: if k == 0 { value.clone() } else { String::new() },
                    footprint: if k == 0 { comp.footprint.clone() } else { None },
                    geom: geom.clone(),
                    pins: unit_pins,
                    at: [0.0, 0.0].into(),
                    angle: 0.0,
                    unit: u,
                    mirror: false,
                    frozen: false,
                    preseeded: false,
                });
            }
        }
    }
    Ok(items)
}

/// Draw every ORPHANED `label:global` net — one carried by label parts but by no
/// placed symbol on this sheet (so it never reaches `inc`) — as a clean, evenly
/// spaced vertical COLUMN of global labels. This rescues a pure/mostly-label
/// block (an external-I/O / pinout breakout sheet) from rendering blank: each
/// label part is geometry-free and skipped by `gather`, and the engine's port
/// pennants attach only to real pins, so without this such a sheet emits nothing.
///
/// The pennant text is the NET name — cross-sheet connectivity binds by net, so
/// the same name's global labels on this net's target sheets join to it, exactly
/// as a wired pin's port pennant would. (The label part's `value`, e.g. `CH1P`,
/// is a shorter human alias of the same net and carries no extra connectivity, so
/// the pennant's own net text is the clearer, complete annotation.) Labels are
/// ordered by the design's component order and deduplicated by net, so the
/// emission is deterministic. Coordinates are nominal: `reframe` shifts the whole
/// column to the page margin. No-op (byte-identical) when no net is orphaned.
pub(crate) fn add_orphan_label_columns(w: &mut SchematicWriter, design: &Design, inc: &Incidence) {
    // Orphaned nets in component order, deduplicated by net (first occurrence wins).
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut orphans: Vec<String> = Vec::new();
    for block in design.blocks.values() {
        for comp in block.components.values() {
            if comp.dnp || comp.part != "label:global" {
                continue;
            }
            for target in comp.pins.values() {
                if let PinTarget::Net(net) = target {
                    if inc.contains_key(net) || !seen.insert(net.clone()) {
                        continue; // already drawn (real pin) or already in the column
                    }
                    orphans.push(net.clone());
                }
            }
        }
    }
    if orphans.is_empty() {
        return;
    }
    // A readable single column: even vertical pitch, each pennant pointing right
    // (text reads outward). PITCH leaves a clear gap between the 1.27 mm-tall rows.
    const X: f64 = 25.4;
    const Y0: f64 = 25.4;
    const PITCH: f64 = 7.62;
    for (i, net) in orphans.iter().enumerate() {
        let y = Y0 + i as f64 * PITCH;
        w.add_cluster_label(net, [X, y], Dir::East, true);
    }
}

/// net -> list of (item index, pin number).
pub(crate) fn incidence(items: &[Item]) -> Incidence {
    let mut inc: Incidence = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        for (num, _name, net) in &it.pins {
            if let Some(net) = net {
                inc.entry(net.clone()).or_default().push((i, num.clone()));
            }
        }
    }
    inc
}

// ---------------------------------------------------------------------------
// Placement — the coarse (col,row,orient) grid rendered as a table.
// ---------------------------------------------------------------------------

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

/// Pack sized tracks (column widths or row heights) in ascending index order
/// with `gap` between successive tracks, returning each index's centre. The
/// grid is ordinal: a skipped index reserves no space (the LLM uses col/row for
/// order and alignment, not metric spacing).
pub(crate) fn track_centres(sizes: &BTreeMap<i32, f64>, gap: f64) -> BTreeMap<i32, f64> {
    let mut out = BTreeMap::new();
    let mut edge = 0.0;
    for (&idx, &size) in sizes {
        out.insert(idx, edge + size / 2.0);
        edge += size + gap;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_symbol::geometry::PinGeom;

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
