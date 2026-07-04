//! `place::emit` — IR → millimetre orchestration: `gather` the parts, seed the
//! coarse grid into mm (`assign_cells`/`apply_cells`), drive the placement engine
//! (`emit_strategy`/`prepare_writer`), and assemble the routed `SchematicWriter`
//! (`build_writer`, `compose_writers`).

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use circuit_lang::model::{Component, Design, PinTarget};
use circuit_lang::{PinType, find_pin};
use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;
use kicad_symbol::geometry::SymbolGeometry;

use crate::write::SchematicWriter;
use geom::Dir;
use sch_place::result::EmitOutput;

use super::*;
use sch_place::item::{Incidence, Item};

// The disjoint-set forest (over a caller-owned `parent` slice) lives in
// `geom::union_find`, shared with circuit-lang's pin reconciler.
use sch_place::ir::{Cell, LayoutIr, Orient};

/// Read the engine [`PlaceOptions`] from the environment at problem construction —
/// the env coupling stays here at the composition root, so the engines themselves
/// never touch `std::env`. (Other `MULTISHEET_REFINE` reads scattered through this
/// crate steer non-engine layout passes and stay as direct env reads.)
pub(crate) fn place_options_from_env() -> sch_place::place::PlaceOptions {
    sch_place::place::PlaceOptions {
        debug_timing: std::env::var("DEBUG_SA_TIME").is_ok(),
        force_fast: std::env::var("MULTISHEET_REFINE").is_ok(),
        motif_tile: std::env::var("MOTIF_TILE").is_ok(),
    }
}

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

/// Resolve a component's pins to (number, name, net) using geometry + the
/// authored pin map (number first, then name — matching the emitter).
pub(crate) fn resolve_pins(
    comp: &Component,
    geom: &SymbolGeometry,
) -> Vec<(String, String, Option<String>)> {
    geom.pins
        .iter()
        .map(|pg| {
            let target = comp
                .pins
                .get(&pg.number)
                .or_else(|| comp.pins.get(&pg.name));
            let net = match target {
                Some(PinTarget::Net(n)) => Some(n.clone()),
                _ => None,
            };
            (pg.number.clone(), pg.name.clone(), net)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Public entry.
// ---------------------------------------------------------------------------

/// Emit a complete `.kicad_sch`. Pass `ir: None` for connectivity inference (production);
/// `Some(ir)` for a hand-tuned / sidecar frame (validation fixtures).
pub fn emit_strategy(
    env: &KicadEnv,
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
pub(crate) fn prepare_writer(
    env: &KicadEnv,
    design: &Design,
    ir: Option<LayoutIr>,
    engine: Box<dyn PlacementEngine>,
) -> io::Result<(SchematicWriter, EmitOutput)> {
    let mut problem = SchematicPlaceProblem::from_design(env, design)?;
    if std::env::var("DEBUG_PLACE").is_ok() {
        eprintln!("[place] engine = {}", engine.name());
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
    if std::env::var_os("FLOORPLAN_SHORT_DIAG").is_some() {
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

/// Lay out one block group and return its finalized writer plus readability metadata.
pub fn emit_group(
    env: &KicadEnv,
    design: &Design,
    engine: Box<dyn PlacementEngine>,
) -> io::Result<(SchematicWriter, EmitOutput)> {
    prepare_writer(env, design, None, engine)
}

/// Lay out one block GROUP and return its FINALIZED-but-unrendered writer (see
/// [`prepare_writer`]), for the multi-block single-sheet composer. Each group is
/// laid out INDEPENDENTLY in its own coordinate space (min corner at the page
/// margin), exactly as a standalone `emit_anneal`; the composer then translates
/// each writer to its tile and folds them into one. Forces the premium anneal so
/// composed groups match the agent's single-block quality.
pub fn emit_writer(
    env: &KicadEnv,
    design: &Design,
    engine: Box<dyn PlacementEngine>,
) -> io::Result<SchematicWriter> {
    Ok(prepare_writer(env, design, None, engine)?.0)
}

/// Bin-pack tile sizes into the column count whose packed sheet aspect is closest to
/// `target_aspect`. For each candidate column count `1..=n` the blocks are placed
/// first-fit-decreasing by height (tallest first, into the currently-shortest column),
/// the resulting sheet width/height is measured, and the column count minimising
/// `|width/height - target_aspect|` wins. Returns the per-input tile origin `(x, y)`,
/// in original input order. A single block (or empty) trivially packs to one column.
pub(crate) fn pack_columns(sizes: &[[f64; 2]], margin: f64, target_aspect: f64) -> Vec<[f64; 2]> {
    let n = sizes.len();
    if n == 0 {
        return Vec::new();
    }
    // Tallest-first order; ties broken by input index for determinism.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| sizes[b][1].total_cmp(&sizes[a][1]).then(a.cmp(&b)));

    let pack = |ncol: usize| -> (Vec<[f64; 2]>, f64) {
        // Per-column running height (next free y) and accumulated max width.
        let mut col_y = vec![0.0_f64; ncol];
        let mut col_w = vec![0.0_f64; ncol];
        let mut col_of = vec![0usize; n];
        let mut yof = vec![0.0_f64; n];
        for &i in &order {
            // Shortest column (lowest running y), ties to the leftmost.
            let c = (0..ncol)
                .min_by(|&a, &b| col_y[a].total_cmp(&col_y[b]))
                .unwrap();
            yof[i] = col_y[c];
            col_of[i] = c;
            col_y[c] += sizes[i][1] + margin;
            col_w[c] = col_w[c].max(sizes[i][0]);
        }
        // Column x origins from the cumulative max widths.
        let mut col_x = vec![0.0_f64; ncol];
        let mut acc = 0.0_f64;
        for c in 0..ncol {
            col_x[c] = acc;
            acc += col_w[c] + margin;
        }
        let width = col_x[ncol - 1] + col_w[ncol - 1];
        let height = col_y.iter().cloned().fold(0.0_f64, f64::max);
        let tiles: Vec<[f64; 2]> = (0..n).map(|i| [col_x[col_of[i]], yof[i]]).collect();
        ((tiles), if height > 0.0 { width / height } else { 1.0 })
    };

    // ROW packing (transpose): widest-first into the currently-narrowest row.
    // A BANNER tile (one very wide block among small ones) forces column
    // packing into a portrait strip; rows lay the smalls in a band under it.
    let mut worder: Vec<usize> = (0..n).collect();
    worder.sort_by(|&a, &b| sizes[b][0].total_cmp(&sizes[a][0]).then(a.cmp(&b)));
    let pack_rows = |nrow: usize| -> (Vec<[f64; 2]>, f64) {
        let mut row_x = vec![0.0_f64; nrow];
        let mut row_h = vec![0.0_f64; nrow];
        let mut row_of = vec![0usize; n];
        let mut xof = vec![0.0_f64; n];
        for &i in &worder {
            let r = (0..nrow)
                .min_by(|&a, &b| row_x[a].total_cmp(&row_x[b]))
                .unwrap();
            xof[i] = row_x[r];
            row_of[i] = r;
            row_x[r] += sizes[i][0] + margin;
            row_h[r] = row_h[r].max(sizes[i][1]);
        }
        let mut row_y = vec![0.0_f64; nrow];
        let mut acc = 0.0_f64;
        for r in 0..nrow {
            row_y[r] = acc;
            acc += row_h[r] + margin;
        }
        let width = row_x.iter().cloned().fold(0.0_f64, f64::max);
        let height = row_y[nrow - 1] + row_h[nrow - 1];
        let tiles: Vec<[f64; 2]> = (0..n).map(|i| [xof[i], row_y[row_of[i]]]).collect();
        (tiles, if height > 0.0 { width / height } else { 1.0 })
    };

    (1..=n)
        .map(|ncol| {
            let (tiles, aspect) = pack(ncol);
            (tiles, (aspect - target_aspect).abs())
        })
        .chain((1..=n).map(|nrow| {
            let (tiles, aspect) = pack_rows(nrow);
            (tiles, (aspect - target_aspect).abs())
        }))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(tiles, _)| tiles)
        .unwrap()
}

/// Compose independently-laid-out block-GROUP writers into ONE `.kicad_sch`. Each
/// group writer arrives finalized in its own coordinate space (min corner at the
/// page margin); this column bin-packs the groups (via `pack_columns`) toward a
/// landscape sheet aspect, translates each to its tile (typed mm math — no string
/// geometry), frames it
/// with a dashed rectangle + a bold name label, dedups cross-group `PWR_FLAG`s,
/// folds every group into one writer, and renders it via a single `finish`.
/// Cross-group nets are already global labels (same name ⇒ KiCAD joins them on the
/// one sheet), so no wire crosses a tile border and enlarging the page is free.
pub fn compose_writers(groups: Vec<(String, SchematicWriter)>, title: Option<&str>) -> String {
    /// Clear space around each group's content so two frames never touch.
    const TILE_MARGIN: f64 = 22.0;
    /// The page margin each group writer is reframed to (its min corner sits here).
    const M: f64 = 12.7;

    // ── PWR_FLAG dedup across groups (KiCAD ERCs "power output ↔ power output" when
    // the same rail is flagged twice). DRIVEN = a group references the net but flags
    // no flag for it (a regulator drives it) ⇒ drop ALL its flags; UNDRIVEN raw rail
    // ⇒ keep exactly one flag globally.
    let mut groups = groups;
    let flag_nets: std::collections::HashSet<String> = groups
        .iter()
        .flat_map(|(_, w)| w.pwr_flag_nets())
        .map(|(n, _)| n)
        .collect();
    // A flagged net is driven iff some group references it WITHOUT flagging it.
    let driven: std::collections::HashSet<String> = flag_nets
        .into_iter()
        .filter(|net| {
            groups.iter().any(|(_, w)| {
                w.referenced_nets().contains(net)
                    && !w.pwr_flag_nets().iter().any(|(n, _)| n == net)
            })
        })
        .collect();
    let mut kept: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (_, w) in &mut groups {
        let drop: Vec<usize> = w
            .pwr_flag_nets()
            .into_iter()
            .filter(|(net, _)| driven.contains(net) || !kept.insert(net.clone()))
            .map(|(_, i)| i)
            .collect();
        if !drop.is_empty() {
            w.remove_instances(drop);
        }
    }

    // ── Column bin-pack onto a roughly-square sheet. A width-only shelf target degenerates
    // into a tall ribbon when blocks vary in height (one wide-but-short block forces a narrow
    // row width, stacking the rest). Instead pick the column count in 1..=n whose packed sheet
    // is closest to TARGET_ASPECT — first-fit-decreasing by height, each block dropped into the
    // currently-shortest column — and keep the column assignment that minimises |aspect - 1.4|.
    const TARGET_ASPECT: f64 = 1.4; // landscape sheets read better than square or portrait
    let sizes: Vec<[f64; 2]> = groups
        .iter()
        .map(|(_, w)| w.content_size().unwrap_or([1.0, 1.0]))
        .collect();
    let tiles = pack_columns(&sizes, TILE_MARGIN, TARGET_ASPECT);

    // ── Translate each group to its tile, frame it, and fold into one writer. The
    // group's content min corner sits at M; map it to (tile + TILE_MARGIN).
    let mut out = SchematicWriter::new();
    if let Some(t) = title {
        out.set_title(t);
    }
    for (i, (name, mut w)) in groups.into_iter().enumerate() {
        let [tx, ty] = tiles[i];
        let [tw, th] = sizes[i];
        let (dx, dy) = (
            geom::GRID_50_MIL.snap(tx + TILE_MARGIN - M),
            geom::GRID_50_MIL.snap(ty + TILE_MARGIN - M),
        );
        w.translate(dx, dy);
        // Frame: a dashed box hugging the tile's content + a bold name above it.
        let (rx0, ry0) = (tx + TILE_MARGIN - 6.0, ty + TILE_MARGIN - 6.0);
        let (rx1, ry1) = (tx + TILE_MARGIN + tw + 1.0, ty + TILE_MARGIN + th + 1.0);
        out.add_rect([rx0, ry0], [rx1, ry1], &format!("frame:{name}"));
        out.add_text(
            &name,
            [rx0 + 1.0, ry0 - 1.5],
            3.0,
            true,
            &format!("label:{name}"),
        );
        out.absorb(w);
    }
    out.promote_local_labels_for_global_nets();
    // Already laid out per group + tiled here; a global reframe would only re-snap.
    out.set_frame(false);
    out.finish()
}

/// Build the complete schematic writer for a placed `items`: symbols (+mirror),
/// no-connects on unconnected pins, all wiring (rails + routed signals), and ERC
/// flags. Shared by the final emission and the refinement scorer so both judge
/// the same geometry — except `fan_risers`, a finalize-only correctness repair
/// (like `prepare`'s wire-split): two rails whose risers are collinear short, so
/// the shipped sheet fans them apart, but the per-move scorer skips it (the fan
/// is a transient mid-search artifact that would churn the placement otherwise).
pub fn build_writer(
    env: &KicadEnv,
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
    env: &KicadEnv,
    items: &[Item],
    ir: &LayoutIr,
) -> BTreeSet<String> {
    let provider = SymbolTable::from_env(env);
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

pub(crate) fn gather(env: &KicadEnv, design: &Design) -> io::Result<Vec<Item>> {
    let mut items = Vec::new();
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
            let geom = SymbolGeometry::load(env, &comp.part)?;
            let pins = resolve_pins(comp, &geom); // same order/len as geom.pins
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
            let mut units: Vec<u8> = pin_unit
                .iter()
                .zip(pins.iter())
                .filter(|pair| pair.1.2.is_some())
                .map(|pair| *pair.0)
                .collect();
            units.sort_unstable();
            units.dedup();
            if units.is_empty() {
                units.push(1);
            }
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
/// these; then `apply_cells` turns them into mm.
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
        })
        .collect()
}

pub fn apply_cells(items: &mut [Item], cells: &[Cell]) {
    let angles: Vec<f64> = items
        .iter()
        .zip(cells)
        .map(|(it, c)| orient_angle(&it.geom, c.orient))
        .collect();

    // Rotation-aware footprint (a quarter-turn swaps width and height).
    let dims: Vec<(f64, f64)> = items
        .iter()
        .zip(&angles)
        .map(|(it, &angle)| {
            let s = it.geom.approx_size();
            if (angle / 90.0).round() as i64 % 2 == 1 {
                (s[1], s[0])
            } else {
                (s[0], s[1])
            }
        })
        .collect();

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
    let col_x = track_centres(&col_w, COL_GAP);
    let row_y = track_centres(&row_h, ROW_GAP);

    for ((it, c), &angle) in items.iter_mut().zip(cells).zip(&angles) {
        it.at = [
            geom::GRID_50_MIL.snap(col_x[&c.col]),
            geom::GRID_50_MIL.snap(row_y[&c.row]),
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
