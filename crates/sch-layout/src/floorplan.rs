//! Floorplan engine — human-style schematic layout from a minimal Layout IR.
//!
//! Layer 2 of the two-layer design. Given a
//! [`Design`] (connectivity only) plus a geometry-free [`LayoutIr`] (the
//! *frame*: which nets are rails and their band, where the ICs go, which nets
//! exit as ports, the global flow), it produces a complete `.kicad_sch` with:
//!
//! * power distributed via **rails** (shared horizontal wires) instead of one
//!   power symbol per pin,
//! * passives placed by a small fixed rule set derived from connectivity,
//! * orthogonal routed wires (the elbow router) with label fallback,
//! * the sheet sized to its content so the drawing fills the view.
//!
//! All exact geometry is decided here; the LLM that emits the IR never sees a
//! millimetre.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use circuit_lang::model::{Component, Design, PinTarget};
use circuit_lang::{find_pin, PinDir, PinType, SymbolProvider};
use kicad_bridge::env::KicadEnv;
use kicad_bridge::geometry::SymbolGeometry;
use kicad_bridge::provider::RealSymbolProvider;
use serde::{Deserialize, Serialize};

use crate::emit::{Dir, SchematicWriter};
use crate::output::EmitOutput;

// ---------------------------------------------------------------------------
// The Layout IR — the four-key language the subagent emits.
// ---------------------------------------------------------------------------

/// Global signal-flow direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Flow {
    /// Left → right (signals flow horizontally). The common case.
    #[default]
    Lr,
    /// Top → bottom.
    Tb,
}

/// Which horizontal band a rail net occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Band {
    Top,
    Bottom,
}

/// Which sheet edge a port net exits toward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// Orientation of a 2-pin part, stated as the direction its pins run — from its
/// first connected net (pin 1) toward its second (pin 2). The engine works out
/// the exact rotation from the symbol's own pin geometry, so the LLM never
/// reasons about a symbol's native axis or KiCAD angles; it just says which way
/// the part points. `down` (pin 1 on top, e.g. a divider leg from VCC down to
/// GND) is the common default. ICs/connectors ignore this (they stay at 0°; use
/// `mirror` to flip them left-to-right).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Orient {
    /// Pin 1 at the bottom, pin 2 at the top.
    Up,
    /// Pin 1 at the top, pin 2 at the bottom (the usual passive orientation).
    #[default]
    Down,
    /// Pin 1 on the right, pin 2 on the left.
    Left,
    /// Pin 1 on the left, pin 2 on the right (a series element along the flow).
    Right,
}

/// A coarse, unitless placement cell + orientation. The engine maps the
/// (col,row) grid to mm — each column sized to its widest part, each row to its
/// tallest — and places the symbol at the cell centre. `col` grows right, `row`
/// grows down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub col: i32,
    pub row: i32,
    #[serde(default)]
    pub orient: Orient,
}

/// The geometry-free floorplan. Four keys; everything else is inferred from
/// connectivity by the compiler's fixed rule set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutIr {
    #[serde(default)]
    pub flow: Flow,
    /// Net → band. Nets drawn as spanning rails.
    #[serde(default)]
    pub rails: BTreeMap<String, Band>,
    /// Refdes → coarse cell. Usually only ICs; any refdes may be pinned.
    #[serde(default)]
    pub place: BTreeMap<String, Cell>,
    /// Net → edge side. Nets that exit as labelled ports.
    #[serde(default)]
    pub ports: BTreeMap<String, Side>,
    /// Anchors (ICs) to flip left-to-right, so the pins facing their neighbours
    /// point the right way (e.g. a level translator's B-side toward a connector).
    #[serde(default)]
    pub mirror: BTreeSet<String>,
    /// Refdes → authored grid bounding box `[col_min, row_min, col_max, row_max]`
    /// in composed grid-ordinal coords (from the per-block `layout:`). The search
    /// holds gridded parts in this RELATIVE order — left/right by column, top/bottom
    /// by row — so the author's arrangement is "relatively rigid"; a part spanning a
    /// column range floats within it. Empty on the sidecar/baseline paths (no
    /// authored grid ⇒ no ordering constraint, so tuned references are unaffected).
    #[serde(default)]
    pub grid: BTreeMap<String, [i32; 4]>,
    /// Idioms the engine recognized from connectivity and co-placed as cohesive
    /// clusters (crystal+load-caps, decoupling bank, op-amp feedback). Surfaced to
    /// the agent via `EmitOutput.detected_idioms`. `#[serde(default)]` so existing
    /// sidecar `layout.json` files (which never carry it) still deserialize.
    #[serde(default)]
    pub idioms: Vec<crate::output::IdiomReport>,
    /// Refdes the placement search must NOT move — an idiom cluster's members,
    /// pinned so their recognized arrangement ships intact.
    #[serde(default)]
    pub frozen: BTreeSet<String>,
    /// Power nets the author wants drawn as DISTRIBUTED LOCAL grounds/supplies — one
    /// power symbol per pin (the professional "drop a GND triangle at each pin" style)
    /// — instead of one sheet-spanning rail. The signal is the author declaring ≥2
    /// power symbols for the net (`GND1`, `GND2`, …); a board with one keeps the rail.
    /// Tames the long-rail sprawl of a dense MCU. `#[serde(default)]` so sidecars (one
    /// symbol per rail) deserialize empty and the tuned references stay rails.
    #[serde(default)]
    pub rail_locals: BTreeSet<String>,
    /// HYBRID VLM placement: refdes → a COARSE target position as a fraction of the
    /// board bbox, `[fx, fy]` in 0..1 (fx: 0=left,1=right; fy: 0=top,1=bottom). A vision
    /// LLM is good at rough DIRECTION ("power left, MCU centre") but not millimetre
    /// positions, so this is applied as a SOFT bias in the placement cost (`zone_bias`),
    /// NOT a forced cell — the engine still does the precise placement, just nudged
    /// toward the LLM's zones. Empty on every existing path ⇒ no bias ⇒ unchanged.
    #[serde(default)]
    pub zone: BTreeMap<String, [f64; 2]>,
}

impl LayoutIr {
    /// Deserialize an IR from JSON (the subagent's structured output / a test
    /// fixture sidecar).
    pub fn from_json(s: &str) -> serde_json::Result<LayoutIr> {
        serde_json::from_str(s)
    }
}

/// A deterministic baseline IR for designs without an LLM-produced one: rails
/// from the design's power nets (ground-like → bottom, else top), no explicit
/// anchor cells, no ports. Good enough to render; not tuned for aesthetics.
pub fn baseline_ir(design: &Design) -> LayoutIr {
    let mut rails = BTreeMap::new();
    for (net, attrs) in &design.nets {
        if attrs.power {
            let band = if is_ground(net) { Band::Bottom } else { Band::Top };
            rails.insert(net.clone(), band);
        }
    }
    LayoutIr {
        flow: Flow::Lr,
        rails,
        place: BTreeMap::new(),
        ports: BTreeMap::new(),
        mirror: BTreeSet::new(),
        grid: BTreeMap::new(),
        idioms: Vec::new(),
        frozen: BTreeSet::new(),
        rail_locals: local_rail_nets(design),
        zone: BTreeMap::new(),
    }
}

/// Power nets the author requested as DISTRIBUTED local grounds/supplies: those with
/// ≥2 declared `power:` symbols (`GND1`, `GND2`, …). Counting the symbols across all
/// blocks lets a designer opt a dense board out of one huge spanning rail.
fn local_rail_nets(design: &Design) -> BTreeSet<String> {
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for block in design.blocks.values() {
        for comp in block.components.values() {
            if comp.part.starts_with("power:") {
                for target in comp.pins.values() {
                    if let circuit_lang::model::PinTarget::Net(net) = target {
                        *count.entry(net.clone()).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    // Distribute only GROUND nets (the original intent: the many ground RETURNS are what
    // tangle a dense board into long rails). Keep POSITIVE supplies as a single rail so their
    // decoupling caps hang off it in a tidy ROW (as on the 9-scoring idiom-stm32) instead of
    // every cap getting its own local symbol and SCATTERING (the #1 critic defect —
    // "decoupling caps parked in empty space"). Env-gated to restore the old all-rails behaviour.
    let all = std::env::var("DISTRIBUTE_ALL_RAILS").is_ok();
    count
        .into_iter()
        .filter(|(net, n)| *n >= 2 && (all || is_ground(net)))
        .map(|(net, _)| net)
        .collect()
}

/// Ground-like net name heuristic.
fn is_ground(net: &str) -> bool {
    let u = net.to_ascii_uppercase();
    u == "GND" || u == "GNDD" || u == "AGND" || u == "DGND" || u == "VSS" || u.starts_with("GND")
}

/// A voltage-rail token: optional `+`/`-`, then a number with `V` as the decimal/unit
/// marker (`3V3`, `+5V`, `1V8`, `12V`, `3.3V`, `-5V`). Conservative — must start with
/// a digit and contain only digits / `.` / a single `V`, so signal names like
/// `5V_SENSE` or `VIN_FB` are NOT matched.
fn is_voltage_token(u: &str) -> bool {
    let s = u.strip_prefix('+').or_else(|| u.strip_prefix('-')).unwrap_or(u);
    if !s.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    if s.chars().filter(|&c| c == 'V').count() != 1 {
        return false;
    }
    s.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 'V')
}

/// Whether a net NAME is conventionally a power/ground rail. Used to infer rails on
/// agent-authored boards that name nets `GND`/`3V3`/`VBUS` but place no `power:`
/// symbols (so `attrs.power` is unset and the engine would otherwise route the supply
/// as a long signal wire — the #1 source of the central rail knot + sprawl). Covers
/// grounds, common named supplies (VCC/VDD/VBAT/VBUS/…), and voltage tokens (3V3,
/// +5V). Applied only when the design declares NO power symbols, so every reference/
/// oracle fixture (all of which declare `power:` symbols) is untouched.
fn is_power_net(net: &str) -> bool {
    if is_ground(net) {
        return true;
    }
    let u = net.to_ascii_uppercase();
    if matches!(
        u.as_str(),
        "VCC" | "VDD" | "VDDA" | "VCCA" | "VCCD" | "AVCC" | "AVDD" | "DVDD"
            | "VBAT" | "VBUS" | "VIN" | "VOUT" | "VEE" | "VPP" | "VDDIO" | "VSYS"
            | "V+" | "V-" | "VS" | "VMOT"
    ) {
        return true;
    }
    if u.starts_with("VCC") || u.starts_with("VDD") || u.starts_with("VBUS") || u.starts_with("VBAT")
    {
        return true;
    }
    is_voltage_token(&u)
}

/// A NEGATIVE supply rail (`VEE`, `V-`, `-12V`, `-5V`). A decoupling/bulk cap with one
/// pin on a negative supply is a VERTICAL rail tap (like a V+ or GND tap), NOT a
/// horizontal series element. Without this, the satellite role classifier sees the cap's
/// other net (e.g. VEE) as neither V+ nor ground and mis-orients it horizontal — the
/// recurring defect on split-supply audio/analog power-entry sheets (e.g. a VEE↔GND bulk
/// cap drawn sideways while its VCC↔GND twin is correctly vertical).
fn is_neg_supply(net: &str) -> bool {
    let u = net.to_ascii_uppercase();
    matches!(u.as_str(), "VEE" | "V-") || (u.starts_with('-') && is_voltage_token(&u))
}

/// The side of the symbol body a pin sits on, from its local geometry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PinSide {
    East,
    West,
    North,
    South,
}

fn pin_side(at: [f64; 2]) -> PinSide {
    if at[0].abs() >= at[1].abs() {
        if at[0] >= 0.0 { PinSide::East } else { PinSide::West }
    } else if at[1] >= 0.0 {
        PinSide::North // symbol-local +y is up; the pin points up = top side
    } else {
        PinSide::South
    }
}

/// Connectivity-driven frame inference: derive a full Layout IR — rails, anchor
/// columns, satellite cells/orientation by the spec's inference rules, and edge
/// ports — straight from the netlist + symbol pin geometry, so the engine owns the
/// whole layout and needs no LLM `place`. The coarse cells it emits are polished
/// by the same refine/align/decongest passes the LLM-frame path uses.
pub fn infer_ir(env: &KicadEnv, design: &Design) -> LayoutIr {
    let Ok(items) = gather(env, design) else { return baseline_ir(design) };
    let inc = incidence(&items);

    // Rails: declared power nets, V+ on top, ground on bottom.
    let mut rails = BTreeMap::new();
    for (net, attrs) in &design.nets {
        if attrs.power {
            rails.insert(net.clone(), if is_ground(net) { Band::Bottom } else { Band::Top });
        }
    }
    // Agent boards routinely NAME nets `GND`/`3V3`/`VBUS` but place NO `power:`
    // symbols, so `attrs.power` is unset and every supply net would route as a long
    // cross-sheet SIGNAL wire (no power symbols, no rail) — the dominant source of the
    // central rail knot, scattered decoupling, and sprawl the critic flags. When the
    // design declares NO power symbols at all, infer the rails from net NAMES instead.
    // Gated on "no declared power" so every reference / oracle fixture (all of which
    // DO declare `power:` symbols) is bit-for-bit untouched.
    let has_power_syms = design
        .blocks
        .values()
        .any(|b| b.components.values().any(|c| c.part.starts_with("power:")));
    if !has_power_syms {
        for net in inc.keys() {
            if is_power_net(net) {
                rails.entry(net.clone()).or_insert(if is_ground(net) {
                    Band::Bottom
                } else {
                    Band::Top
                });
            }
        }
    }
    let is_rail = |n: &str| rails.contains_key(n);
    let is_vplus = |n: &str| is_rail(n) && !is_ground(n);

    let anchors: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() >= 3).collect();
    let sats: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() == 2).collect();

    // Per-anchor: its pins grouped by side, ordered, so a satellite tapping one
    // pin knows the pin's side (which column) and rank (which row) on that side.
    const MID: i32 = 4;
    // Ordinal row separation between authored grid rows: large enough that a
    // row's tap satellites (anchor row ± 2, ± rank) never collide with the next
    // row's anchor. Rows are ordinal — the gap reserves no metric space, it only
    // orders (apply_cells packs populated rows by ROW_GAP).
    const ROW_BAND: i32 = 10;
    let mut place: BTreeMap<String, Cell> = BTreeMap::new();
    // Authored placement grid (`layout:` 2D array): refdes → (grid col, grid row).
    // A gridded anchor takes its cell; the rest flow left→right in connectivity
    // order after the last grid column. With no grid the map is empty and this is
    // exactly the old col=k*5 / row=MID behaviour.
    let authored = grid_from_layout(design);
    let order = order_anchors(&items, &inc, &anchors);
    let max_gcol = authored.values().map(|b| b[2]).max().unwrap_or(-1);
    let mut anchor_col: BTreeMap<usize, i32> = BTreeMap::new();
    let mut anchor_row: BTreeMap<usize, i32> = BTreeMap::new();

    // The anchors the author did NOT grid are 2-D SHELF-PACKED into a page-shaped block
    // (≈√n per shelf) toward the TOP-LEFT, instead of laid out in one ever-widening row.
    // One flat row is the #1 sprawl source on a multi-module board (4-5 ICs + connectors
    // unroll into a wide strip with an empty vertical middle and long cross-sheet rails —
    // the 20-circuit sweep's dominant defect); shelving folds that strip into a compact
    // block and leaves the bottom-right corner clearer for the title block. Grid rows/cols
    // are ORDINAL — `apply_cells` packs each populated track by its real content size — so
    // a module only needs the right SHELF and order, not a metric footprint; a satellite-
    // heavy anchor still reserves extra columns so its tap fan does not collide a neighbour.
    let inferred: Vec<usize> =
        order.iter().copied().filter(|ai| !authored.contains_key(&items[*ai].refdes)).collect();
    let fwidth = |ai: usize| -> i32 {
        let nsat = sats
            .iter()
            .filter(|&&si| {
                anchor_tap(&items, &inc, &anchors, si, &rails).map(|(a, _, _)| a == ai).unwrap_or(false)
            })
            .count() as i32;
        (1 + nsat / 6).max(1)
    };
    let per_row = (inferred.len() as f64).sqrt().ceil().max(1.0) as i32;
    let x0 = max_gcol + 1;
    let (mut gx, mut shelf, mut in_row, mut max_used) = (x0, 0i32, 0i32, max_gcol);
    let mut packed: BTreeMap<usize, (i32, i32)> = BTreeMap::new();
    for &ai in &inferred {
        if in_row >= per_row {
            shelf += 1;
            gx = x0;
            in_row = 0;
        }
        packed.insert(ai, (gx, shelf));
        let w = fwidth(ai);
        max_used = max_used.max(gx + w - 1);
        gx += w;
        in_row += 1;
    }
    let next_col = max_used + 1;

    // GLOBAL OPTIMISER (opt-in `GLOBAL_OPT`): replace the connectivity-blind shelf-pack
    // with a dataflow-LAYERED, crossing-minimised placement of the inferred anchors. The
    // shelf-pack still ran above (cheap) so `packed` is a safe fallback if crossmin
    // declines (≤1 anchor).
    if std::env::var("GLOBAL_OPT").is_ok() {
        if let Some(cm) = crossmin_anchor_cells(env, &items, &inc, &inferred, &rails) {
            packed = cm;
        }
    }

    for &ai in &order {
        let rd = &items[ai].refdes;
        let (gcol, grow) = match authored.get(rd) {
            Some(b) => (b[0], b[1]), // seed from the box's top-left cell
            None => packed[&ai],
        };
        let col = gcol * 5; // wide gaps leave room for tap satellites either side
        let row = MID + grow * ROW_BAND;
        anchor_col.insert(ai, col);
        anchor_row.insert(ai, row);
        place.insert(rd.clone(), Cell { col, row, orient: Orient::Down });
    }

    // For each anchor, map pin number -> (side, rank-on-side) for row offsets.
    let mut pin_meta: BTreeMap<(usize, String), (PinSide, i32)> = BTreeMap::new();
    for &ai in &anchors {
        let mut by_side: BTreeMap<u8, Vec<(&str, f64)>> = BTreeMap::new();
        for pg in &items[ai].geom.pins {
            let s = pin_side(pg.at);
            by_side.entry(s as u8).or_default().push((pg.number.as_str(), pg.at[1]));
        }
        for (sb, mut v) in by_side {
            // East/West ranked top->down (descending local y); top/bottom by x.
            v.sort_by(|a, b| b.1.total_cmp(&a.1));
            let n = v.len() as i32;
            for (rank, (num, _)) in v.into_iter().enumerate() {
                let side = match sb {
                    0 => PinSide::East,
                    1 => PinSide::West,
                    2 => PinSide::North,
                    _ => PinSide::South,
                };
                // Centre the rank around 0 so taps land beside their actual pin row.
                let r = rank as i32 - (n - 1) / 2;
                pin_meta.insert((ai, num.to_string()), (side, r));
            }
        }
    }

    // Idiom co-placement: recognize circuit idioms (crystal networks, …) from
    // connectivity and place each as a cohesive cluster BEFORE the generic loop,
    // marking the members `placed` so the loop skips them. A board with no idiom is
    // untouched; an author-gridded cluster is left to the grid override below.
    let detected = detect_idioms(
        &items, &inc, &anchors, &sats, &rails, &pin_meta, &anchor_col, &anchor_row,
    );
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut idiom_reports: Vec<crate::output::IdiomReport> = Vec::new();
    for idiom in &detected {
        // If the author gridded the anchor or any member, their grid wins — skip.
        let gridded = std::iter::once(&items[idiom.anchor].refdes)
            .chain(idiom.cells.iter().map(|(rd, _)| rd))
            .any(|rd| authored.contains_key(rd));
        if gridded {
            continue;
        }
        if idiom.freeze {
            for (rd, cell) in &idiom.cells {
                place.insert(rd.clone(), *cell);
                placed.insert(rd.clone());
            }
        }
        idiom_reports.push(crate::output::IdiomReport {
            kind: idiom.kind.to_string(),
            anchor: items[idiom.anchor].refdes.clone(),
            parts: idiom.cells.iter().map(|(rd, _)| rd.clone()).collect(),
        });
    }

    // Spare columns for satellites that don't resolve to an anchor pin. Start
    // past the last placed anchor column (`next_col` covers grid + inferred).
    let mut spare_col = next_col * 5 + 3;
    // Track how many V+/GND-band parts already sit in each column, to spread them.
    let mut band_fill: BTreeMap<(i32, i32), i32> = BTreeMap::new();

    // Grid cells that contain an anchor: a satellite the author gridded into such
    // a cell still FLANKS the anchor (inference); one gridded into an anchor-less
    // cell (a bare 2-pin connector, or an all-passive block) is stacked there.
    let anchor_cells: BTreeSet<(i32, i32)> = anchors
        .iter()
        .filter_map(|&ai| authored.get(&items[ai].refdes).map(|b| (b[0], b[1])))
        .collect();
    let mut stack_row: BTreeMap<(i32, i32), i32> = BTreeMap::new();
    // How many satellites have already tapped a given (anchor, pin), so the next
    // one fans into the adjacent column instead of overlapping.
    let mut same_pin: BTreeMap<(usize, String), i32> = BTreeMap::new();

    for &si in &sats {
        let s = &items[si];
        // Already co-placed by an idiom cluster — skip the generic rules.
        if placed.contains(&s.refdes) {
            continue;
        }
        let (n1, n2) = (s.pins[0].2.clone(), s.pins[1].2.clone());
        let (Some(n1), Some(n2)) = (n1, n2) else { continue };

        // An explicitly-gridded satellite in an anchor-less cell: place it at its
        // cell, stacking successive parts down the column so they don't collide.
        if let Some(b) = authored.get(&s.refdes).copied()
            && !anchor_cells.contains(&(b[0], b[1]))
        {
            let (gc, gr) = (b[0], b[1]);
            let k = stack_row.entry((gc, gr)).or_insert(0);
            let row = MID + gr * ROW_BAND + *k;
            *k += 1;
            place.insert(s.refdes.clone(), Cell { col: gc * 5, row, orient: orient_for(&s.pins, &n1, true) });
            continue;
        }

        // The single anchor pin this satellite taps (if any), with its side/rank.
        let tap = anchor_tap(&items, &inc, &anchors, si, &rails);

        let cell = if let Some((ai, ref pin_num, tap_net)) = tap {
            let acol = anchor_col[&ai];
            let arow = anchor_row[&ai]; // tap satellites sit in their anchor's grid row
            let (side, rank) = *pin_meta.get(&(ai, pin_num.clone())).unwrap_or(&(PinSide::East, 0));
            // The OTHER net (not the tapped pin's) decides the satellite's role.
            let other = if tap_net == n1 { &n2 } else { &n1 };
            // Multiple satellites tapping the SAME anchor pin (e.g. a bias node's
            // bypass caps) fan out into adjacent columns instead of piling onto
            // one cell. First tap → off 0 (unchanged); the rest step outward.
            let off = {
                let e = same_pin.entry((ai, pin_num.clone())).or_insert(0);
                let o = *e;
                *e += 1;
                o
            };
            let col_for_side = |s: PinSide| match s {
                PinSide::East => acol + 1 + off,
                PinSide::West => acol - 1 - off,
                _ => acol + off,
            };
            if is_vplus(other) {
                // Pull-up / supply tap → vertical in the V+ band above its pin.
                let c = col_for_side(side);
                Cell { col: c, row: arow - 2, orient: orient_for(&s.pins, &n1, true) }
            } else if is_ground(other) || is_neg_supply(other) {
                // Pull-down / ground return OR negative-supply tap → vertical in the band
                // below. A cap/part whose OTHER pin is GND (or VEE/V-) is a rail tap, not a
                // series element — drop the old `is_rail(other)` qualifier on ground, which
                // wrongly fell through to horizontal when GND wasn't flagged a rail in a
                // multi-sheet sub-design (the split-supply VEE↔GND cap drawn sideways). The
                // references declare power symbols so is_rail(GND) held there → inert for them.
                let c = col_for_side(side);
                Cell { col: c, row: arow + 2, orient: orient_for(&s.pins, &n1, true) }
            } else {
                // Series element in the signal flow → horizontal beside the pin.
                let c = col_for_side(side);
                let horiz = if side == PinSide::West { Orient::Left } else { Orient::Right };
                Cell { col: c, row: arow + rank, orient: series_orient(&s.pins, &tap_net, horiz) }
            }
        } else if (is_vplus(&n1) || is_neg_supply(&n1)) && is_ground(&n2)
            || is_ground(&n1) && (is_vplus(&n2) || is_neg_supply(&n2))
        {
            // Pure decoupling/bulk cap (supply↔GND, no signal pin — V+ OR a negative rail
            // like VEE/V-, so split-supply analog bypass caps are handled too). Seed it in
            // the supply band of
            // the supply IC it bypasses — the anchor with the most pins on that V+ rail,
            // preferring a real IC — fanning successive caps into adjacent columns. With
            // DISTRIBUTED local power symbols the cap has NO wire pulling it toward the
            // rail, so the old spare-column seed STRANDED it far from the circuit (the #1
            // "stranded decoupling cap" critic defect on agent boards); seating it beside
            // its IC fixes that. Falls back to a spare column if no anchor uses the rail.
            let vp = if is_ground(&n1) { &n2 } else { &n1 };
            // The supply load is found by REFDES across all its units, not just the anchor
            // item's own pins: a multi-unit IC (op-amp, FPGA) carries its V+/V- on a separate
            // 2-pin POWER UNIT that isn't itself an anchor, so a per-item pin check finds no
            // anchor and the dual-supply decoupling scatters. Counting vp pins over every
            // unit sharing the refdes seats the bypass beside the IC's signal-unit anchor.
            let refdes_vp = |rd: &str| -> usize {
                items
                    .iter()
                    .filter(|it| it.refdes == rd)
                    .flat_map(|it| &it.pins)
                    .filter(|(_, _, n)| n.as_deref() == Some(vp.as_str()))
                    .count()
            };
            let sup = anchors
                .iter()
                .copied()
                .filter(|&ai| refdes_vp(&items[ai].refdes) > 0)
                .max_by_key(|&ai| (!is_connector_like(&items[ai].part), refdes_vp(&items[ai].refdes)));
            if let Some(ai) = sup {
                let acol = anchor_col[&ai];
                let arow = anchor_row[&ai];
                let off = {
                    let e = same_pin.entry((ai, vp.clone())).or_insert(0);
                    let o = *e;
                    *e += 1;
                    o
                };
                Cell { col: acol + 1 + off, row: arow - 2, orient: orient_for(&s.pins, &n1, true) }
            } else {
                let c = spare_col;
                spare_col += 1;
                Cell { col: c, row: MID, orient: orient_for(&s.pins, &n1, true) }
            }
        } else {
            // Rail-to-rail / star leg with a signal midpoint (e.g. a divider): a
            // V+→signal leg sits high, a signal→GND leg low, sharing a column.
            let high = is_vplus(&n1) || is_vplus(&n2);
            let col = spare_col;
            // Keep legs of the same signal node in one column: reuse the column of
            // the first leg seen for this signal net.
            let signal = if is_rail(&n1) { &n2 } else { &n1 };
            let c = *band_fill.entry((-1, hash_col(signal))).or_insert_with(|| {
                spare_col += 1;
                col
            });
            let row = if high { MID - 1 } else { MID + 1 };
            Cell { col: c, row, orient: orient_for(&s.pins, &n1, true) }
        };
        place.insert(s.refdes.clone(), cell);
    }

    // Ports: a net the author EXPLICITLY marked (a `label:global` component → the
    // `port` flag) OR — as a convenience — a single-pin signal net that obviously
    // exits the sheet. The explicit mark is what lets a degree-2+ output (a NOT-gate
    // OUT touching the collector R and the transistor) be a port; the degree-1 rule
    // alone can't see it. Heuristic side: input-ish name left, else right.
    let mut ports = BTreeMap::new();
    for (net, pins) in &inc {
        let attrs = design.nets.get(net);
        let power = attrs.map(|a| a.power).unwrap_or(false);
        let marked = attrs.map(|a| a.port).unwrap_or(false);
        // Not a no-connect (NC_*) and not a power rail (rails draw their own symbols).
        let nc = net.to_ascii_uppercase().starts_with("NC");
        if !power && !nc && (marked || pins.len() == 1) {
            let side = if net_is_input(net) { Side::Left } else { Side::Right };
            ports.insert(net.clone(), side);
        }
    }

    // Mirror inference: flip a connector at col 0 so its pins face into the
    // circuit; flip an IC whose connector-facing pins currently point AWAY from
    // the connector (a level translator's B-side toward the upstream connector).
    let mut mirror = BTreeSet::new();
    for &ai in &anchors {
        let is_conn = items[ai].part.contains("Connector");
        if is_conn && anchor_col.get(&ai) == Some(&0) {
            mirror.insert(items[ai].refdes.clone());
            continue;
        }
        if !is_conn && wants_mirror(&items, &inc, &anchors, &pin_meta, ai) {
            mirror.insert(items[ai].refdes.clone());
        }
    }

    // The author's per-block `layout:` grid OVERRIDES inferred placement for every
    // gridded part — anchors AND satellites. A gridded satellite SEEDS where the
    // grid's relative arrangement says, not where its tap would pull it (the grid
    // is the author's explicit intent; inference only fills the rest). The search
    // then holds that relative order via the `grid_order` cost (see `layout_cost`).
    for (rd, b) in &authored {
        if let Some(cell) = place.get_mut(rd) {
            cell.col = b[0] * 5;
            cell.row = MID + b[1] * ROW_BAND;
        }
    }

    // HYBRID VLM placement: a coarse zone map {refdes:[fx,fy]} from the LLM, applied as a
    // SOFT bias (zone_bias in proxy_cost). Loaded from $ZONE_FILE for the A/B loop / tests;
    // absent ⇒ empty ⇒ no bias. (The agent pipeline will pass it directly in future.)
    let zone = std::env::var("ZONE_FILE")
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<BTreeMap<String, [f64; 2]>>(&s).ok())
        .unwrap_or_default();

    LayoutIr {
        flow: Flow::Lr,
        rails,
        place,
        ports,
        mirror,
        grid: authored,
        idioms: idiom_reports,
        frozen: placed,
        rail_locals: local_rail_nets(design),
        zone,
    }
}

/// Cheap stable column key for a net name (group same-node legs in one column).
fn hash_col(net: &str) -> i32 {
    net.bytes().fold(0i32, |a, b| a.wrapping_mul(31).wrapping_add(b as i32)).abs() % 100000
}

/// True if a net name reads like a board input (goes on the left edge).
fn net_is_input(net: &str) -> bool {
    let u = net.to_ascii_uppercase();
    u.contains("IN") || u.contains("VIN") || u.contains("BUS") || u.contains("RX") || u.contains("TX_RAW")
}

/// Orient a 2-pin part vertical with pin1 toward the top when `pin1_high`, derived
/// from which of its nets sits higher; used for divider legs and decoupling caps.
fn orient_for(pins: &[(String, String, Option<String>)], n1: &str, _pin1_high: bool) -> Orient {
    // pin1 is the first authored net; if it is the V+/upper net, pin1 on top → Down.
    let p1 = pins.first().and_then(|p| p.2.as_deref());
    if p1 == Some(n1) {
        // Default vertical with pin1 on top.
        Orient::Down
    } else {
        Orient::Up
    }
}

/// Orient a series 2-pin part horizontal so its tapped pin faces the anchor.
fn series_orient(
    pins: &[(String, String, Option<String>)],
    tap_net: &str,
    side_default: Orient,
) -> Orient {
    let p1 = pins.first().and_then(|p| p.2.as_deref());
    // If pin1 is the tapped (anchor-side) net, the part runs out from the anchor.
    match (p1 == Some(tap_net), side_default) {
        (true, Orient::Right) => Orient::Left, // pin1 (anchor) on right → points left
        (true, Orient::Left) => Orient::Right,
        (false, o) => o,
        (_, o) => o,
    }
}

/// A circuit idiom recognized purely from connectivity + symbol pin geometry.
/// `infer_ir` turns it into an [`crate::output::IdiomReport`] for the LLM. A FROZEN
/// idiom also seeds its cells into `place` and pins its members; a REPORT-ONLY idiom
/// (`!freeze`) lets the members flow through normal placement and is instead tidied by
/// an mm post-pass in `emit` (e.g. a GPIO LED's resistor snapped below it).
struct Idiom {
    kind: &'static str,
    anchor: usize,
    /// (refdes, assigned cell) for every member. Cells seed placement only when frozen;
    /// the refdes list always feeds the report.
    cells: Vec<(String, Cell)>,
    /// Pin the members and seed their cells (true), or just recognize them (false).
    freeze: bool,
}

/// Project the placed parts into the pure [`circuit_graph::CircuitGraph`] the idiom
/// matcher consumes. Net kinds come from the rail table, with a name-based ground
/// fallback so a lifted netlist that dropped the power-net marks still classifies
/// `GND`/`VSS` correctly (the crystal load caps return to ground by name).
fn build_circuit_graph(items: &[Item], rails: &BTreeMap<String, Band>) -> circuit_graph::CircuitGraph {
    let nodes: Vec<circuit_graph::Node> = items
        .iter()
        .map(|it| circuit_graph::Node {
            refdes: it.refdes.clone(),
            lib_id: it.part.clone(),
            value: it.value.clone(),
            pins: it
                .pins
                .iter()
                .map(|(num, name, net)| circuit_graph::Pin {
                    number: num.clone(),
                    name: name.clone(),
                    net: net.clone(),
                })
                .collect(),
        })
        .collect();
    let rails = rails.clone();
    circuit_graph::CircuitGraph::new(nodes, move |net| {
        if rails.contains_key(net) {
            if is_ground(net) { circuit_graph::NetKind::Ground } else { circuit_graph::NetKind::Power }
        } else if is_ground(net) {
            circuit_graph::NetKind::Ground
        } else {
            circuit_graph::NetKind::Signal
        }
    })
}

/// Recognize circuit idioms with the graph-similarity matcher (`circuit-graph`)
/// and co-place each as a cohesive cluster BEFORE the generic satellite loop. The
/// matcher decides *what* is an idiom (declarative, extensible); the per-kind
/// placement helpers decide *where* the cluster lands using grid/pin geometry the
/// pure crate cannot see. A board with no idiom is untouched. A match whose
/// geometry can't be realized (e.g. an osc pin not on the IC) is silently dropped,
/// so the generic rules place it instead — the engine never reports an idiom it
/// did not actually freeze.
fn detect_idioms(
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    sats: &[usize],
    rails: &BTreeMap<String, Band>,
    pin_meta: &BTreeMap<(usize, String), (PinSide, i32)>,
    anchor_col: &BTreeMap<usize, i32>,
    anchor_row: &BTreeMap<usize, i32>,
) -> Vec<Idiom> {
    let _ = (sats, pin_meta);
    let graph = build_circuit_graph(items, rails);
    let matches = circuit_graph::find_all(&graph, &circuit_graph::library::active_library());
    let idx: BTreeMap<&str, usize> =
        items.iter().enumerate().map(|(i, it)| (it.refdes.as_str(), i)).collect();
    let get = |rd: &str| idx.get(rd).copied();

    let mut out: Vec<Idiom> = Vec::new();
    let mut claimed: BTreeSet<usize> = BTreeSet::new();
    for m in &matches {
        let Some(ai) = get(&m.anchor) else { continue };
        match m.pattern {
            "crystal" => {
                let (Some(yi), Some(caps)) = (
                    m.bindings.get("crystal").and_then(|v| v.first()).and_then(|r| get(r)),
                    Some(
                        ["cap_a", "cap_b"]
                            .iter()
                            .filter_map(|k| m.bindings.get(*k))
                            .flatten()
                            .filter_map(|r| get(r))
                            .collect::<Vec<_>>(),
                    ),
                ) else {
                    continue;
                };
                if caps.len() != 2 || caps.iter().chain(std::iter::once(&yi)).any(|c| claimed.contains(c)) {
                    continue;
                }
                if let Some(cells) =
                    place_crystal(items, inc, anchors, anchor_col, anchor_row, ai, yi, &caps)
                {
                    claimed.insert(yi);
                    claimed.extend(&caps);
                    out.push(Idiom { kind: "crystal", anchor: ai, cells, freeze: true });
                }
            }
            "decoupling" => {
                let caps: Vec<usize> = m
                    .bindings
                    .get("cap")
                    .into_iter()
                    .flatten()
                    .filter_map(|r| get(r))
                    .filter(|c| !claimed.contains(c))
                    .collect();
                // The graph matcher's anchor is ANY part bridging V+ and GND — often a
                // jumper or power connector that merely touches the rails, not the IC
                // the caps actually bypass. Re-select the real load: the anchor with the
                // most pins on the bank's V+ rail, preferring a true IC over a
                // connector/jumper. Without this the whole bank freezes beside a stray
                // 3-pin part and floats far from the MCU (the #1 "decoupling bank in the
                // far corner" critic defect).
                let ai = best_decoupling_anchor(items, anchors, rails, &caps).unwrap_or(ai);
                if let Some(cells) =
                    place_decoupling(items, inc, anchors, rails, anchor_col, anchor_row, ai, &caps, &out)
                {
                    claimed.extend(cells.iter().filter_map(|(rd, _)| get(rd)));
                    out.push(Idiom { kind: "decoupling", anchor: ai, cells, freeze: true });
                }
            }
            "led_indicator" => {
                // Report-only: a GPIO LED taps its IC pin so normal placement seats it
                // well; we just recognize the pair so the mm post-pass (`align_led_chain`)
                // can snap the series resistor directly below the LED, clear of the body,
                // rather than letting it drift to a spare column.
                let Some(ri) = m.bindings.get("res").and_then(|v| v.first()).and_then(|r| get(r))
                else {
                    continue;
                };
                if claimed.contains(&ai) || claimed.contains(&ri) {
                    continue;
                }
                out.push(Idiom {
                    kind: "led_indicator",
                    anchor: ai,
                    cells: vec![(items[ri].refdes.clone(), Cell { col: 0, row: 0, orient: Orient::Down })],
                    freeze: false,
                });
            }
            _ => {}
        }
    }
    out
}

/// A part that sits on power rails but is NOT the IC a decoupling bank serves — a
/// connector, jumper, mounting hole, or test point. These trip the graph matcher's
/// "anything bridging V+/GND" anchor pick, so the bank must skip them.
fn is_connector_like(part: &str) -> bool {
    part.contains("Connector")
        || part.contains("Conn_")
        || part.contains("Jumper")
        || part.contains("Mounting")
        || part.contains("TestPoint")
}

/// The IC a decoupling bank actually bypasses: among anchors with a pin on the bank's
/// V+ rail, the real IC (not a connector/jumper) with the most pins on that rail, then
/// the most pins overall. `None` if the caps share no non-ground rail with any anchor.
fn best_decoupling_anchor(
    items: &[Item],
    anchors: &[usize],
    rails: &BTreeMap<String, Band>,
    caps: &[usize],
) -> Option<usize> {
    // The V+ rail the bank bypasses: the most common non-ground rail among the caps.
    let mut vp_count: BTreeMap<String, usize> = BTreeMap::new();
    for &ci in caps {
        for (_, _, n) in &items[ci].pins {
            if let Some(n) = n.as_deref() {
                if rails.contains_key(n) && !is_ground(n) {
                    *vp_count.entry(n.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    let vp = vp_count.into_iter().max_by_key(|(_, c)| *c).map(|(n, _)| n)?;
    anchors
        .iter()
        .copied()
        .filter_map(|ai| {
            let on_rail = items[ai]
                .pins
                .iter()
                .filter(|(_, _, n)| n.as_deref() == Some(vp.as_str()))
                .count();
            (on_rail > 0).then(|| {
                (ai, !is_connector_like(&items[ai].part), on_rail, items[ai].geom.pins.len())
            })
        })
        .max_by(|a, b| (a.1, a.2, a.3).cmp(&(b.1, b.2, b.3)))
        .map(|(ai, _, _, _)| ai)
}

/// Place a matched **decoupling bank**: lay its caps in one evenly-spaced ROW just
/// off the IC's power-pin edge (each cap vertical, V+ up / GND down) rather than
/// scattering them into spare columns where their risers knot near the power pins.
///
/// `caps` are the matcher's V+↔GND caps that reach `ai`; this re-applies the
/// geometric preconditions the pure matcher cannot express — a cap whose nets reach
/// *another* anchor is ambiguous and dropped, and a V+ rail needs ≥3 caps to form a
/// bank — then shifts the whole row PAST one vertical edge (the side the crystal
/// idiom did not claim) so every GND riser drops clear of the package. Returns
/// `None` if nothing qualifies (the caps fall back to the generic loop).
fn place_decoupling(
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    rails: &BTreeMap<String, Band>,
    anchor_col: &BTreeMap<usize, i32>,
    anchor_row: &BTreeMap<usize, i32>,
    ai: usize,
    caps: &[usize],
    out: &[Idiom],
) -> Option<Vec<(String, Cell)>> {
    // Group qualifying caps by their V+ rail; a cap whose own nets reach a *different*
    // IC is not unambiguously this IC's bypass, so drop it. EXCLUDE connectors: a power
    // CONNECTOR (J*, a power source / entry) sits on the same rail+ground as the bypass
    // caps on essentially every real board, so counting it here would drop the whole
    // bank (a dense MCU with a power/SWD header scatters its decoupling) — the bank
    // still decouples THIS IC regardless of where the rail enters the sheet.
    let mut by_rail: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for &ci in caps {
        let cn: Vec<&str> = items[ci].pins.iter().filter_map(|(_, _, n)| n.as_deref()).collect();
        if cn.len() != 2 {
            continue;
        }
        // A shared POWER/GROUND RAIL reaching another IC is NORMAL — on any board with
        // a regulator the V+ rail feeds both the LDO and the MCU it powers, and GND is
        // universal — so a rail net must NOT disqualify a bypass cap (doing so dropped
        // EVERY decoupling cap on a two-IC board, collapsing the bank below 3 and
        // scattering it; the #1 defect on realistic LDO+MCU boards). Only a SIGNAL
        // (non-rail) net reaching a different IC marks a cap as that IC's part.
        let touches_other_ic = cn.iter().any(|n| {
            !rails.contains_key(*n)
                && inc.get(*n).into_iter().flatten().any(|(j, _)| {
                    anchors.contains(j) && *j != ai && !items[*j].part.contains("Connector")
                })
        });
        if touches_other_ic {
            continue;
        }
        let vp = if is_ground(cn[0]) { cn[1] } else { cn[0] };
        by_rail.entry(vp.to_string()).or_default().push(ci);
    }
    let mut bank: Vec<usize> =
        by_rail.into_values().filter(|v| v.len() >= 3).flatten().collect();
    bank.sort_unstable();
    if bank.len() < 3 {
        return None;
    }
    let (acol, arow) = (anchor_col[&ai], anchor_row[&ai]);
    let n = bank.len() as i32;
    let crystal_l = out.iter().any(|id| {
        id.kind == "crystal" && id.anchor == ai && id.cells.iter().any(|(_, c)| c.col < acol)
    });
    // Past the right edge (crystal on the left) or past the left edge.
    let base = if crystal_l { 2 } else { -(n + 1) };
    // One row above the IC's pin band (arow-1), not two: the IC renders tall, so an
    // extra ordinal row leaves a wide empty gap between the bank and the power pins it
    // serves; one row hugs it while still clearing the body.
    Some(
        bank.iter()
            .enumerate()
            .map(|(k, &ci)| {
                let cell = Cell { col: acol + base + k as i32, row: arow - 1, orient: Orient::Down };
                (items[ci].refdes.clone(), cell)
            })
            .collect(),
    )
}

/// Place a matched **crystal network**: the crystal one column out from the IC's
/// oscillator pins (vertical, between them) and each load cap one column further out
/// at its osc pin's row — the textbook compact oscillator block hugging the IC.
///
/// `yi` is the crystal, `caps` its two load caps (in any order). This re-derives the
/// geometry the pure matcher cannot: both osc nets must tap exactly this IC, on a
/// close pin pair; each cap is bound to the osc net it shares with the crystal.
/// Returns `None` if that geometry does not hold (the parts fall back to the loop).
fn place_crystal(
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    anchor_col: &BTreeMap<usize, i32>,
    anchor_row: &BTreeMap<usize, i32>,
    ai: usize,
    yi: usize,
    caps: &[usize],
) -> Option<Vec<(String, Cell)>> {
    // The single anchor a net taps (None if it reaches zero or several anchors).
    let anchor_pin = |net: &str| -> Option<(usize, String)> {
        let hits: Vec<(usize, String)> = inc
            .get(net)
            .into_iter()
            .flatten()
            .filter(|(j, _)| anchors.contains(j))
            .map(|(j, num)| (*j, num.clone()))
            .collect();
        (hits.len() == 1).then(|| hits[0].clone())
    };
    {
        let nets: Vec<String> = items[yi].pins.iter().filter_map(|(_, _, n)| n.clone()).collect();
        if nets.len() != 2 || nets[0] == nets[1] {
            return None;
        }
        let (xa, xb) = (nets[0].clone(), nets[1].clone());
        // Both oscillator nets must tap exactly THIS IC (one anchor pin each).
        let (Some((aa, pa)), Some((ab, pb))) = (anchor_pin(&xa), anchor_pin(&xb)) else {
            return None;
        };
        if aa != ai || ab != ai {
            return None;
        }
        // Bind each load cap to the osc net it shares with the crystal.
        let cap_on = |osc: &str| -> Option<usize> {
            caps.iter().copied().find(|&ci| items[ci].pins.iter().any(|(_, _, n)| n.as_deref() == Some(osc)))
        };
        let (Some(ca), Some(cb)) = (cap_on(&xa), cap_on(&xb)) else {
            return None;
        };
        if ca == cb {
            return None;
        }
        let dbg = std::env::var("IDIOM_DEBUG").is_ok();
        // Raw pin geometry. `pin_side` (|x| vs |y|) mis-buckets a TALL IC's corner
        // pins — a left-edge pin high on the body has |y|>|x| and reads "North" — so
        // classify the osc port by which EDGE of the IC's pin bounding box it hugs.
        let pin_at =
            |num: &str| items[ai].geom.pins.iter().find(|p| p.number == num).map(|p| p.at);
        let (Some(paa), Some(pba)) = (pin_at(&pa), pin_at(&pb)) else {
            return None;
        };
        // The two osc pins must be a close pair (consecutive pins of one port).
        if (paa[0] - pba[0]).hypot(paa[1] - pba[1]) > 12.7 {
            return None;
        }
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for p in &items[ai].geom.pins {
            lo[0] = lo[0].min(p.at[0]);
            lo[1] = lo[1].min(p.at[1]);
            hi[0] = hi[0].max(p.at[0]);
            hi[1] = hi[1].max(p.at[1]);
        }
        let (mx, my) = ((paa[0] + pba[0]) / 2.0, (paa[1] + pba[1]) / 2.0);
        // Nearest bbox edge ⇒ outward grid direction. +x is +col (right); symbol +y
        // is UP while grid row grows DOWN, so the North (top) edge is the -row dir.
        let (dl, dr, db, dt) = (mx - lo[0], hi[0] - mx, my - lo[1], hi[1] - my);
        let m = dl.min(dr).min(db).min(dt);
        let (dcol, drow) = if m == dl {
            (-1, 0)
        } else if m == dr {
            (1, 0)
        } else if m == dt {
            (0, -1)
        } else {
            (0, 1)
        };
        // Per-pin offset ALONG the edge = the pin's centred RANK among the pins on
        // that same edge — the convention the generic tap placement uses (arow+rank),
        // so the cluster aligns with the actual osc pin rows. Ranking by NEAREST bbox
        // edge dodges the `pin_side` corner-pin bug.
        let pin_edge = |at: [f64; 2]| -> (i32, i32) {
            let (l, r, b, t) = (at[0] - lo[0], hi[0] - at[0], at[1] - lo[1], hi[1] - at[1]);
            let mn = l.min(r).min(b).min(t);
            if mn == l {
                (-1, 0)
            } else if mn == r {
                (1, 0)
            } else if mn == t {
                (0, -1)
            } else {
                (0, 1)
            }
        };
        let mut edge_pins: Vec<(&str, f64)> = items[ai]
            .geom
            .pins
            .iter()
            .filter(|p| pin_edge(p.at) == (dcol, drow))
            .map(|p| (p.number.as_str(), if dcol != 0 { -p.at[1] } else { p.at[0] }))
            .collect();
        edge_pins.sort_by(|a, b| a.1.total_cmp(&b.1));
        let n_edge = edge_pins.len() as i32;
        let off_of = |num: &str| -> i32 {
            edge_pins
                .iter()
                .position(|(pn, _)| *pn == num)
                .map(|i| i as i32 - (n_edge - 1) / 2)
                .unwrap_or(0)
        };
        if dbg {
            eprintln!(
                "IDIOM crystal {} FIRES (ai={}, caps {}@{} {}@{}, dcol={dcol} drow={drow})",
                items[yi].refdes, items[ai].refdes, items[ca].refdes, off_of(&pa),
                items[cb].refdes, off_of(&pb)
            );
        }
        // Placement: a tight cluster just off the osc edge. The crystal sits ONE step
        // out, between the two osc pins; each load cap sits one step FURTHER out at
        // its osc pin's offset — the textbook oscillator block hugging the IC.
        let (acol, arow) = (anchor_col[&ai], anchor_row[&ai]);
        let (oa, ob) = (off_of(&pa), off_of(&pb));
        let cell_at = |step: i32, off: i32, orient: Orient| -> Cell {
            if dcol != 0 {
                Cell { col: acol + step * dcol, row: arow + off, orient }
            } else {
                Cell { col: acol + off, row: arow + step * drow, orient }
            }
        };
        // Crystal vertical on an E/W edge (pin1 toward its higher osc pin), horizontal
        // on an N/S edge.
        let p1 = items[yi].pins.first().and_then(|p| p.2.as_deref());
        let (o1, o2) = if p1 == Some(xa.as_str()) { (oa, ob) } else { (ob, oa) };
        let y_orient = if dcol != 0 {
            if o1 <= o2 { Orient::Down } else { Orient::Up }
        } else {
            Orient::Right
        };
        // Clamp the cluster toward the IC: a coarse-grid satellite row is spaced wider
        // than the IC's own 2.54 mm pin pitch, so a high-rank osc pin (near the top of
        // a tall MCU) would fling the cluster well above the body. Keep it hugging the
        // IC and let the router run the (short) leads — the two caps still flank the
        // crystal, higher osc pin on top.
        let c = ((oa + ob) / 2).clamp(-1, 1);
        let (ca_off, cb_off) = if oa <= ob { (c - 1, c + 1) } else { (c + 1, c - 1) };
        let cells = vec![
            (items[yi].refdes.clone(), cell_at(1, c, y_orient)),
            (
                items[ca].refdes.clone(),
                cell_at(2, ca_off, orient_for(&items[ca].pins, &xa, true)),
            ),
            (
                items[cb].refdes.clone(),
                cell_at(2, cb_off, orient_for(&items[cb].pins, &xb, true)),
            ),
        ];
        Some(cells)
    }
}

/// The single anchor pin a satellite taps, as (anchor index, pin number, net), or
/// None if it touches zero or several anchor pins.
fn anchor_tap(
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    si: usize,
    rails: &BTreeMap<String, Band>,
) -> Option<(usize, String, String)> {
    let mut hits = Vec::new();
    for (_, _, net) in &items[si].pins {
        let Some(net) = net else { continue };
        for (j, num) in inc.get(net).into_iter().flatten() {
            if anchors.contains(j) {
                hits.push((*j, num.clone(), net.clone()));
            }
        }
    }
    // Resolve on a single distinct ANCHOR, not a single pin. A satellite whose
    // rail leg ALSO lands on the same IC (its VCC/GND pins) used to be rejected
    // as multi-hit, scattering it to a spare column. If every hit is on ONE
    // anchor, prefer the tap on a NON-rail (signal) net — the meaningful pin — so
    // the part flanks that pin. Only a tap spanning two DIFFERENT anchors is
    // genuinely ambiguous.
    let distinct: BTreeSet<usize> = hits.iter().map(|h| h.0).collect();
    if distinct.len() != 1 {
        return None;
    }
    hits.sort_by_key(|h| rails.contains_key(&h.2));
    let chosen = hits.into_iter().next()?;
    // A RAIL-ONLY tap doesn't make the part adjacent to the IC: a coax/connector
    // that touches the chip only through GND (its signal goes elsewhere, via a
    // DC-block cap) must NOT be tucked under the IC's GND pin. So if the chosen
    // tap is a rail AND this satellite carries a non-rail signal net, it isn't a
    // real tap — let it place elsewhere. A pure decoupler (both nets rails, no
    // signal) keeps its rail tap and flanks the supply pin.
    if rails.contains_key(&chosen.2) {
        let has_signal = items[si]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.as_deref())
            .any(|n| !rails.contains_key(n));
        if has_signal {
            return None;
        }
    }
    Some(chosen)
}

/// Whether an IC should be flipped left↔right: its EAST-side signal pins reach a
/// connector (the upstream input) more than its WEST-side pins do, so flipping
/// turns those pins to face the connector on the left. Reach is checked up to two
/// hops (IC pin → satellite → connector), which catches a series resistor between
/// the IC and the connector.
fn wants_mirror(
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    pin_meta: &BTreeMap<(usize, String), (PinSide, i32)>,
    ai: usize,
) -> bool {
    let is_conn = |i: usize| items[i].part.contains("Connector");
    let net_reaches_conn = |net: &str| -> bool {
        for (j, _) in inc.get(net).into_iter().flatten() {
            if *j != ai && anchors.contains(j) && is_conn(*j) {
                return true;
            }
            // one more hop through a 2-pin part
            if items[*j].geom.pins.len() == 2 {
                for (_, _, n2) in &items[*j].pins {
                    if let Some(n2) = n2 {
                        if n2 != net
                            && inc.get(n2).into_iter().flatten().any(|(k, _)| is_conn(*k))
                        {
                            return true;
                        }
                    }
                }
            }
        }
        false
    };
    let (mut east, mut west) = (0i32, 0i32);
    for (num, _name, net) in &items[ai].pins {
        let Some(net) = net else { continue };
        if !net_reaches_conn(net) {
            continue;
        }
        match pin_meta.get(&(ai, num.clone())).map(|m| m.0) {
            Some(PinSide::East) => east += 1,
            Some(PinSide::West) => west += 1,
            _ => {}
        }
    }
    east > west
}

/// Order anchors left→right: connectors (inputs) first, then ICs, refdes for
/// stability. This is the DEFAULT order for anchors the author did not place via
/// the `layout:` grid; a gridded anchor overrides its column (see `infer_ir`).
fn order_anchors(items: &[Item], inc: &Incidence, anchors: &[usize]) -> Vec<usize> {
    // Base order (the historical convention): connectors vs non-connectors, then
    // refdes. Used as the chain seed + the deterministic tie-break.
    let mut base: Vec<usize> = anchors.to_vec();
    base.sort_by(|&a, &b| {
        let ca = !items[a].part.contains("Connector");
        let cb = !items[b].part.contains("Connector");
        ca.cmp(&cb).then(items[a].refdes.cmp(&items[b].refdes))
    });
    if base.len() <= 2 {
        return base;
    }
    // CONNECTIVITY-AWARE ordering: place strongly-connected anchors CONSECUTIVELY so the
    // shelf-pack lands them in adjacent cells — connected modules sit together instead of
    // scattering across the sheet with long bridging wires (the dominant sprawl defect).
    // Adjacency = number of shared SIGNAL (non-rail) nets, counting a one-hop link through
    // a shared 2-pin satellite (a series R between two ICs). Rails are excluded (they
    // touch everything). Greedy chain from the base seed: repeatedly append the unplaced
    // anchor most-connected to the already-placed set, tie-broken by base order.
    let is_anchor = |i: usize| items[i].geom.pins.len() >= 3;
    let idx_in_anchors: BTreeMap<usize, usize> = base.iter().enumerate().map(|(k, &a)| (a, k)).collect();
    let mut adj: BTreeMap<(usize, usize), i32> = BTreeMap::new();
    let mut bump = |x: usize, y: usize, w: i32| {
        if x != y {
            *adj.entry((x.min(y), x.max(y))).or_insert(0) += w;
        }
    };
    // Direct: anchors sharing a net. A SIGNAL (non-rail) net is a strong link (weight 3);
    // a non-ground SUPPLY rail (3V3/5V/VBUS) is a WEAK link (weight 1) — it captures the
    // power-delivery relationship (an LDO feeding an MCU on 3V3) so those modules still
    // cluster, without it dominating signal flow. GROUND is excluded entirely (it touches
    // every part, so it carries no locality information).
    for (net, pins) in inc {
        if is_ground(net) {
            continue;
        }
        let w = if is_power_net(net) { 1 } else { 3 };
        let ancs: Vec<usize> = pins.iter().map(|(j, _)| *j).filter(|&j| is_anchor(j)).collect();
        for a in 0..ancs.len() {
            for b in (a + 1)..ancs.len() {
                bump(ancs[a], ancs[b], w);
            }
        }
    }
    // One hop: a 2-pin satellite bridging two anchors through its SIGNAL nets (a series R
    // between two ICs). Rail-only bridges (a decoupling cap) are skipped — they'd link
    // every anchor through the shared supply.
    for si in 0..items.len() {
        if items[si].geom.pins.len() != 2 {
            continue;
        }
        let mut ancs: Vec<usize> = Vec::new();
        for (_, _, net) in &items[si].pins {
            if let Some(net) = net {
                if is_power_net(net) {
                    continue;
                }
                for (j, _) in inc.get(net).into_iter().flatten() {
                    if is_anchor(*j) {
                        ancs.push(*j);
                    }
                }
            }
        }
        ancs.sort_unstable();
        ancs.dedup();
        for a in 0..ancs.len() {
            for b in (a + 1)..ancs.len() {
                bump(ancs[a], ancs[b], 3);
            }
        }
    }
    let weight = |x: usize, y: usize| -> i32 {
        *adj.get(&(x.min(y), x.max(y))).unwrap_or(&0)
    };
    let mut placed: Vec<usize> = vec![base[0]];
    let mut remaining: Vec<usize> = base[1..].to_vec();
    while !remaining.is_empty() {
        let pick = remaining
            .iter()
            .enumerate()
            .max_by(|&(_, &x), &(_, &y)| {
                let wx: i32 = placed.iter().map(|&p| weight(p, x)).sum();
                let wy: i32 = placed.iter().map(|&p| weight(p, y)).sum();
                // Higher connectivity first; tie-break by EARLIER base order.
                wx.cmp(&wy)
                    .then_with(|| idx_in_anchors[&y].cmp(&idx_in_anchors[&x]))
            })
            .map(|(k, _)| k)
            .unwrap();
        placed.push(remaining.remove(pick));
    }
    placed
}

/// GLOBAL layered placement of the inferred anchors via `crossmin` (Sugiyama): build a
/// DATAFLOW graph (a net flows from its driving OUTPUT pin to the INPUT pins it feeds,
/// using the preserved KiCAD pin direction), layer it left→right by signal flow, and
/// barycenter-order each layer to minimise crossings. Returns `(layer, order)` per
/// inferred-anchor index — a drop-in replacement for the shelf-pack's `packed` map (the
/// downstream loop maps layer→column, order→row band). Rails are excluded (they touch
/// everything, carrying no flow). `None` if there's nothing to lay out.
fn crossmin_anchor_cells(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    rails: &BTreeMap<String, Band>,
) -> Option<BTreeMap<usize, (i32, i32)>> {
    if anchors.len() < 2 {
        return None;
    }
    let provider = RealSymbolProvider::new(env.clone());
    let node_of: BTreeMap<usize, usize> = anchors.iter().enumerate().map(|(k, &ai)| (ai, k)).collect();
    // Direction of anchor `ai`'s pin on `net` (matched by pin number via the provider).
    let pin_dir = |ai: usize, net: &str| -> PinDir {
        let Some(meta) = provider.symbol(&items[ai].part) else { return PinDir::Unknown };
        let num = items[ai].pins.iter().find(|(_, _, n)| n.as_deref() == Some(net)).map(|(p, _, _)| p);
        match num.and_then(|num| find_pin(&meta.pins, num)) {
            Some(pm) => pm.dir,
            None => PinDir::Unknown,
        }
    };
    let mut g = crossmin::Graph::new(anchors.len());
    for (net, pins) in inc {
        if rails.contains_key(net) {
            continue; // power/ground touch everything — no flow information
        }
        // The subset-anchors on this net, with their pin direction.
        let on: Vec<(usize, PinDir)> = pins
            .iter()
            .filter_map(|(j, _)| node_of.get(j).map(|&nd| (nd, pin_dir(*j, net))))
            .collect();
        if on.len() < 2 {
            continue;
        }
        let drivers: Vec<usize> = on
            .iter()
            .filter(|(_, d)| *d == PinDir::Out)
            .map(|(n, _)| *n)
            .collect();
        let bidir: Vec<usize> = on
            .iter()
            .filter(|(_, d)| *d == PinDir::Bidir)
            .map(|(n, _)| *n)
            .collect();
        let src = if !drivers.is_empty() { Some(drivers) } else if !bidir.is_empty() { Some(bidir) } else { None };
        // Star (not clique) to bound edge count on a wide bus.
        match src {
            Some(srcs) => {
                for &s in &srcs {
                    for &(d, _) in &on {
                        if d != s {
                            g.flow(s, d, 1.0);
                        }
                    }
                }
            }
            None => {
                // No driver: link every anchor to the lowest-indexed one (a star), so
                // they cluster without imposing a (wrong) flow direction.
                let hub = on.iter().map(|(n, _)| *n).min().unwrap();
                for &(d, _) in &on {
                    if d != hub {
                        g.link(hub, d, 1.0);
                    }
                }
            }
        }
    }
    let placed = crossmin::layout(&g, &crossmin::Opts::default());
    Some(anchors.iter().enumerate().map(|(k, &ai)| (ai, (placed.layer[k], placed.order[k]))).collect())
}

/// Compose every block's per-block `layout:` grid into one global relative seed:
/// refdes → (grid col, grid row). Each gridded block occupies its own column band
/// (declaration order, laid left→right); within a band a cell maps its refdes to
/// `(band_base + local_col, local_row)`. `None` (`~`) holes are skipped; a refdes
/// repeated in a column takes its FIRST occurrence (the seed — the search then
/// floats/spans it, since the grid is RELATIVE positioning, never an absolute
/// pin). Blocks with no grid contribute nothing here — the engine infers their
/// internal arrangement. Empty when no block carries a `layout:`.
fn grid_from_layout(design: &Design) -> BTreeMap<String, [i32; 4]> {
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
const COL_GAP: f64 = 6.35; // 5 grid — column channel (clears a wide IC's pin text)
const ROW_GAP: f64 = 5.08; // 4 grid — vertical stack; tighter lets the rotation
// move flip a clean vertical divider leg horizontal (lower wire cost, but
// unconventional), so keep the conventional spacing here.
const MARGIN: f64 = 12.7;

/// One placed component plus the data the compiler needs about it.
#[derive(Clone)]
struct Item {
    refdes: String,
    part: String,
    value: String,
    geom: SymbolGeometry,
    /// (pin number, pin name, net or None for NC). For a multi-unit part this
    /// holds only the pins of THIS item's `unit` (each unit is its own Item).
    pins: Vec<(String, String, Option<String>)>,
    at: [f64; 2],
    angle: f64,
    /// 1-based symbol unit this Item places. Single-unit parts are 1; a
    /// multi-unit part (op-amp/FPGA) splits into one Item per used unit, all
    /// sharing `refdes` but emitted as distinct `(unit N)` instances.
    unit: u8,
    /// Whether this symbol is flipped left↔right. Seeded from `ir.mirror`; lifted
    /// onto the Item (was read off `ir.mirror` at emit) so the placement search
    /// can flip it as a move and the cost sees exactly what ships.
    mirror: bool,
    /// Pinned by an idiom cluster (a crystal + its load caps): the placement search
    /// must NOT move it, so the engine-recognized cohesive arrangement ships intact
    /// instead of the cost dragging a load cap off toward the GND rail.
    frozen: bool,
}

/// Resolve a component's pins to (number, name, net) using geometry + the
/// authored pin map (number first, then name — matching the emitter).
fn resolve_pins(comp: &Component, geom: &SymbolGeometry) -> Vec<(String, String, Option<String>)> {
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

/// Emit a complete `.kicad_sch` for `design` laid out per `ir`.
/// Emit the schematic with the env-defaulted placement strategy (greedy unless
/// `ANNEAL`/`LAYOUT_SEARCH=anneal`). The engine/test default.
pub fn emit(env: &KicadEnv, design: &Design, ir: &LayoutIr) -> io::Result<EmitOutput> {
    emit_strategy(env, design, ir, pick_strategy())
}

/// Emit forcing the simulated-annealing (premium) search regardless of env. The
/// production agent uses this so its boards get the locality-aware anneal — which the
/// candidate pick makes strictly ≥ greedy — instead of the env-defaulted greedy.
pub fn emit_anneal(env: &KicadEnv, design: &Design, ir: &LayoutIr) -> io::Result<EmitOutput> {
    emit_strategy(env, design, ir, Box::new(Anneal))
}

fn emit_strategy(
    env: &KicadEnv,
    design: &Design,
    ir: &LayoutIr,
    strategy: Box<dyn PlacementStrategy>,
) -> io::Result<EmitOutput> {
    let mut items = gather(env, design)?;
    // Seed each item's mirror flag from the IR (lifted onto Item so the search
    // can flip it and the cost/emit read one source of truth).
    for it in &mut items {
        it.mirror = ir.mirror.contains(&it.refdes);
        it.frozen = ir.frozen.contains(&it.refdes);
    }
    let inc = incidence(&items);

    // Which power nets need an ERC PWR_FLAG: a power-INPUT pin (or a declared
    // rail) with no power-OUTPUT pin driving it is "undriven". Computed up front
    // so it can feed both the refinement scorer and the final emission.
    let needs_flag = compute_needs_flag(env, &items, ir);

    // Seed the placement from the IR grid: project the coarse (col,row,orient)
    // cells to mm ONCE, then the search operates DIRECTLY on the items' mm
    // coordinates so its objective IS the geometry that ships (not a pre-polish
    // cell-table proxy that polish/decongest then mutated behind its back).
    let cells = assign_cells(&items, ir);
    apply_cells(&mut items, &cells);
    normalize(&mut items);
    // Placement search behind the strategy interface (`PlacementStrategy`): greedy
    // (free tier) or simulated annealing (premium); both mutate `items` in mm.
    // The placement search now OWNS the continuous polish (small boards: the routed
    // `polish`; large boards: the router-free `polish_proxy`, picked per-board among
    // seat/pack variants), so it returns the FINAL placement — emit no longer
    // re-polishes (which would re-add a seating pass the large-board pick had
    // deliberately rejected). Greedy keeps the exact refine→polish order, so the
    // reference snapshots stay byte-identical.
    strategy.search(env, &mut items, &inc, ir, &needs_flag, SEARCH_SEED);
    // Guarantee no body overlap: the cost-gated refine can leave two parts
    // touching when separating them would transiently raise routed cost (a local
    // minimum), so a final, unconditional relaxation pushes any remaining
    // overlaps apart. Cheap a frame may be, the shipped sheet never collides.
    decongest(&mut items);
    // Snap each frozen idiom cluster to its IC's ACTUAL pin positions in mm. The
    // coarse grid (IC = one cell, but renders tall) packs a cluster's cells OUTSIDE
    // the body, leaving long dog-legs to the pins; this aligns the crystal beside its
    // real oscillator pins. Then one more decongest pushes any non-frozen part the
    // re-positioned cluster now overlaps out of the way (frozen members don't move).
    if align_idiom_clusters(&mut items, ir) {
        decongest(&mut items);
    }
    // Tidy each recognized (report-only) LED indicator: snap its series resistor into a
    // clean vertical leg directly below the LED, so a GPIO indicator reads as one leg
    // instead of the resistor drifting to a spare column. Runs on FINAL positions (the
    // LED already seated by the search), so it never collides a frozen cluster; the
    // follow-up decongest nudges anything the moved resistor now overlaps.
    if align_led_chains(&mut items, &inc, ir) {
        decongest(&mut items);
    }

    let mut w = build_writer(env, design.name.as_deref(), &items, &inc, ir, &needs_flag, true)?;
    // Finalize geometry (text solve, wire split, reframe) BEFORE linting so the
    // reported warnings reflect the actual emitted sheet, not the pre-solve state.
    w.set_frame(true);
    w.prepare();
    let warnings = w.layout_warnings();
    let (body_crossings, ic_crossings, wire_crossings) =
        crossing_counts(env, &items, &inc, ir, &needs_flag);
    let sch = w.finish();
    Ok(EmitOutput {
        sch,
        layout_warnings: warnings,
        body_crossings,
        ic_crossings,
        wire_crossings,
        detected_idioms: ir.idioms.clone(),
    })
}

/// Build the complete schematic writer for a placed `items`: symbols (+mirror),
/// no-connects on unconnected pins, all wiring (rails + routed signals), and ERC
/// flags. Shared by the final emission and the refinement scorer so both judge
/// the same geometry — except `fan_risers`, a finalize-only correctness repair
/// (like `prepare`'s wire-split): two rails whose risers are collinear short, so
/// the shipped sheet fans them apart, but the per-move scorer skips it (the fan
/// is a transient mid-search artifact that would churn the placement otherwise).
fn build_writer(
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
        w.add_symbol(env, &it.part, &it.refdes, &it.value, it.at, it.angle)?;
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
    wire(env, &mut w, items, inc, ir, needs_flag, &mut flag_points, fan_risers)?;
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
fn compute_needs_flag(env: &KicadEnv, items: &[Item], ir: &LayoutIr) -> BTreeSet<String> {
    let provider = RealSymbolProvider::new(env.clone());
    let (mut driven, mut power_input) = (BTreeSet::new(), BTreeSet::new());
    for it in items {
        let Some(meta) = provider.symbol(&it.part) else { continue };
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

fn gather(env: &KicadEnv, design: &Design) -> io::Result<Vec<Item>> {
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
                _ if geom.pins.len() >= 3 => {
                    comp.part.rsplit(':').next().unwrap_or(&comp.part).to_string()
                }
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
                .filter(|pair| pair.1 .2.is_some())
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
                    geom: geom.clone(),
                    pins: unit_pins,
                    at: [0.0, 0.0],
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

/// net -> list of (item index, pin number).
type Incidence = BTreeMap<String, Vec<(usize, String)>>;

fn incidence(items: &[Item]) -> Incidence {
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
fn assign_cells(items: &[Item], ir: &LayoutIr) -> Vec<Cell> {
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
                Some(c) => Cell { col: c.col, row: c.row + k, orient: c.orient },
                None => {
                    let c = spare;
                    spare += 1;
                    Cell { col: c, row: k, orient: Orient::Down }
                }
            }
        })
        .collect()
}

/// Render `cells` to mm: each column sized to its widest member and each row to
/// its tallest, every part at its cell centre — aligned, overlap-free, and as
/// tight as the parts allow.
/// Snap each frozen CRYSTAL cluster to its IC's actual oscillator-pin positions in
/// mm — the coarse grid can only place a cluster's cells, which on a tall IC pack
/// outside the body. The crystal lands one gap out from the osc pins' midpoint and
/// each load cap two gaps out, level with its osc pin. Returns true if it moved
/// anything (so the caller re-runs `decongest`). The cluster members are frozen, so
/// the placement search has already finished around them and won't undo this.
fn align_idiom_clusters(items: &mut [Item], ir: &LayoutIr) -> bool {
    const GAP: f64 = 7.62;
    let snap = crate::grid::snap;
    let mut moves: Vec<(usize, [f64; 2])> = Vec::new();
    for idiom in &ir.idioms {
        if idiom.kind != "crystal" {
            continue;
        }
        let in_idiom = |it: &Item| idiom.parts.contains(&it.refdes);
        let Some(ai) = items.iter().position(|it| it.refdes == idiom.anchor) else {
            continue;
        };
        let Some(yi) = items
            .iter()
            .position(|it| in_idiom(it) && (it.part.contains("Crystal") || it.part.contains("Resonator")))
        else {
            continue;
        };
        let y_refdes = items[yi].refdes.clone();
        let onets: Vec<String> = items[yi].pins.iter().filter_map(|(_, _, n)| n.clone()).collect();
        if onets.len() != 2 {
            continue;
        }
        // The IC's pin-tip world position for an osc net.
        let osc_world = |net: &str| -> Option<[f64; 2]> {
            let num = items[ai].pins.iter().find(|(_, _, n)| n.as_deref() == Some(net))?.0.clone();
            let pg = items[ai].geom.pins.iter().find(|p| p.number == num)?;
            Some(crate::emit::pin_endpoint(pg, items[ai].at, items[ai].angle, items[ai].mirror))
        };
        let (Some(wa), Some(wb)) = (osc_world(&onets[0]), osc_world(&onets[1])) else {
            continue;
        };
        let mid = [(wa[0] + wb[0]) / 2.0, (wa[1] + wb[1]) / 2.0];
        // Outward direction = the IC EDGE the osc pins hug (NOT the dominant axis of
        // mid−centre: a corner osc pin is more vertically than horizontally offset yet
        // still sticks out the SIDE). Classify by nearest edge of the IC's pin bbox.
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for pg in &items[ai].geom.pins {
            let w = crate::emit::pin_endpoint(pg, items[ai].at, items[ai].angle, items[ai].mirror);
            lo[0] = lo[0].min(w[0]);
            lo[1] = lo[1].min(w[1]);
            hi[0] = hi[0].max(w[0]);
            hi[1] = hi[1].max(w[1]);
        }
        let (dl, dr, dt, db) = (mid[0] - lo[0], hi[0] - mid[0], mid[1] - lo[1], hi[1] - mid[1]);
        let m = dl.min(dr).min(dt).min(db);
        let dir: [f64; 2] = if m == dl {
            [-1.0, 0.0]
        } else if m == dr {
            [1.0, 0.0]
        } else if m == dt {
            [0.0, -1.0]
        } else {
            [0.0, 1.0]
        };
        // Unit vector perpendicular to `dir` (the edge the cluster runs ALONG).
        let perp = [dir[1].abs(), dir[0].abs()];
        // The crystal sits one gap out, centred between the two oscillator pins.
        moves.push((yi, [snap(mid[0] + dir[0] * GAP), snap(mid[1] + dir[1] * GAP)]));
        // Each load cap sits two gaps out and a FULL gap to its osc pin's side of the
        // midpoint, NOT at the osc-pin row itself: the pins are one 2.54 mm pitch apart
        // but a cap is ~7.6 mm tall, so placing the caps at the pin rows overlaps them
        // and the de-congest pass then jogs the whole cluster into a knot. Pushing each
        // cap a gap off-centre gives 2·GAP of clearance — the textbook tidy block.
        for (net, w) in [(&onets[0], wa), (&onets[1], wb)] {
            if let Some(ci) = items.iter().position(|it| {
                in_idiom(it)
                    && it.refdes != y_refdes
                    && it.pins.iter().any(|(_, _, n)| n.as_deref() == Some(net.as_str()))
            }) {
                // Which side of the midpoint this osc pin lies on, along the edge.
                let side = if dir[0] != 0.0 { (w[1] - mid[1]).signum() } else { (w[0] - mid[0]).signum() };
                let side = if side == 0.0 { 1.0 } else { side };
                moves.push((
                    ci,
                    [
                        snap(mid[0] + dir[0] * GAP * 2.0 + perp[0] * side * GAP),
                        snap(mid[1] + dir[1] * GAP * 2.0 + perp[1] * side * GAP),
                    ],
                ));
            }
        }
    }
    let moved = !moves.is_empty();
    if std::env::var("IDIOM_DEBUG").is_ok() {
        for (i, at) in &moves {
            eprintln!("ALIGN {} -> [{:.1},{:.1}]", items[*i].refdes, at[0], at[1]);
        }
    }
    for (i, at) in moves {
        items[i].at = at;
    }
    moved
}

/// Snap each recognized LED indicator's series resistor into a clean vertical leg
/// directly below the LED (a GPIO → LED → R → GND drop), instead of letting it sit in
/// a spare column with a long node wire back to the LED. Runs on FINAL mm positions in
/// `emit` (after the search has seated the LED), so it can't collide a frozen cluster;
/// the caller re-runs `decongest` to nudge anything the moved resistor now overlaps.
/// Returns true if it moved anything.
fn align_led_chains(items: &mut [Item], _inc: &Incidence, ir: &LayoutIr) -> bool {
    const DROP: f64 = 10.16; // LED half + gap + resistor half, on grid.
    let snap = crate::grid::snap;
    let mut moves: Vec<(usize, [f64; 2], f64)> = Vec::new();
    for idiom in &ir.idioms {
        if idiom.kind != "led_indicator" {
            continue;
        }
        let Some(li) = items.iter().position(|it| it.refdes == idiom.anchor) else { continue };
        let Some(res_rd) = idiom.parts.first() else { continue };
        let Some(ri) = items.iter().position(|it| &it.refdes == res_rd) else { continue };
        // The node the LED and resistor share (the LED cathode → resistor top).
        let led_nets: Vec<String> = items[li].pins.iter().filter_map(|(_, _, n)| n.clone()).collect();
        let Some(shared) =
            items[ri].pins.iter().filter_map(|(_, _, n)| n.clone()).find(|n| led_nets.contains(n))
        else {
            continue;
        };
        // Re-orient the LED VERTICAL too — cathode (the shared node) DOWN toward the
        // resistor, anode UP toward the driving pin — so the LED and resistor read as one
        // collinear series string rather than an L-bend (a horizontal LED over a vertical
        // resistor). Keep the LED's position; only its angle changes.
        let l_pin1 = items[li].pins.first().and_then(|p| p.2.clone());
        let l_orient = if l_pin1.as_deref() == Some(shared.as_str()) { Orient::Up } else { Orient::Down };
        let l_angle = orient_angle(&items[li].geom, l_orient);
        moves.push((li, items[li].at, l_angle));
        let at = [snap(items[li].at[0]), snap(items[li].at[1] + DROP)];
        // Vertical, shared (cathode) pin UP toward the LED above, GND pin DOWN.
        let r_pin1 = items[ri].pins.first().and_then(|p| p.2.clone());
        let orient = if r_pin1.as_deref() == Some(shared.as_str()) { Orient::Down } else { Orient::Up };
        let angle = orient_angle(&items[ri].geom, orient);
        moves.push((ri, at, angle));
    }
    let moved = !moves.is_empty();
    if std::env::var("IDIOM_DEBUG").is_ok() {
        for (i, at, _) in &moves {
            eprintln!("ALIGN-LED {} -> [{:.1},{:.1}]", items[*i].refdes, at[0], at[1]);
        }
    }
    for (i, at, angle) in moves {
        items[i].at = at;
        items[i].angle = angle;
    }
    moved
}

fn apply_cells(items: &mut [Item], cells: &[Cell]) {
    let angles: Vec<f64> =
        items.iter().zip(cells).map(|(it, c)| orient_angle(&it.geom, c.orient)).collect();

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
        it.at = [crate::grid::snap(col_x[&c.col]), crate::grid::snap(row_y[&c.row])];
        it.angle = angle;
    }
}

/// Pack sized tracks (column widths or row heights) in ascending index order
/// with `gap` between successive tracks, returning each index's centre. The
/// grid is ordinal: a skipped index reserves no space (the LLM uses col/row for
/// order and alignment, not metric spacing).
fn track_centres(sizes: &BTreeMap<i32, f64>, gap: f64) -> BTreeMap<i32, f64> {
    let mut out = BTreeMap::new();
    let mut edge = 0.0;
    for (&idx, &size) in sizes {
        out.insert(idx, edge + size / 2.0);
        edge += size + gap;
    }
    out
}

/// The KiCAD rotation (0/90/180/270) that makes a 2-pin part's pin1→pin2 axis
/// point the way [`Orient`] asks, derived from the symbol's own pin geometry so
/// it is correct whatever the part's native orientation. Multi-pin parts (ICs,
/// connectors) are pre-oriented and stay at 0° (use `mirror` to flip them).
///
/// A local pin `(lx, ly)` maps to sheet offset `(rx, -ry)` after a CCW rotation
/// by the instance angle (see `emit::transform_offset`), so increasing the angle
/// turns the sheet-space axis clockwise. We test the four quarter-turns and pick
/// the one whose resulting cardinal axis matches the request.
fn orient_angle(geom: &SymbolGeometry, orient: Orient) -> f64 {
    if geom.pins.len() != 2 {
        return 0.0;
    }
    let pin = |n: &str| geom.pins.iter().find(|p| p.number == n);
    let (p1, p2) = match (pin("1"), pin("2")) {
        (Some(a), Some(b)) => (a, b),
        _ => (&geom.pins[0], &geom.pins[1]),
    };
    let (dx, dy) = (p2.at[0] - p1.at[0], p2.at[1] - p1.at[1]);
    // Desired pin1→pin2 direction in sheet space (y grows downward).
    let want = match orient {
        Orient::Right => (1.0, 0.0),
        Orient::Left => (-1.0, 0.0),
        Orient::Down => (0.0, 1.0),
        Orient::Up => (0.0, -1.0),
    };
    for deg in [0.0_f64, 90.0, 180.0, 270.0] {
        let (s, c) = deg.to_radians().sin_cos();
        let (sx, sy) = (dx * c - dy * s, -(dx * s + dy * c));
        let card = if sx.abs() >= sy.abs() { (sx.signum(), 0.0) } else { (0.0, sy.signum()) };
        if (card.0 - want.0).abs() < 0.5 && (card.1 - want.1).abs() < 0.5 {
            return deg;
        }
    }
    0.0
}

// ---------------------------------------------------------------------------
// Refinement — nudge satellites for a tidier routed result (anchors fixed).
// ---------------------------------------------------------------------------

/// Hill-climb the satellite (2-pin) cells with the anchors held fixed, accepting
/// only strict improvements to the routed-layout cost. Because every candidate
/// is scored on the ACTUAL routing — not a placement proxy — the loop can never
/// trade a clean wire for a hidden short or label fallback, and it can only
/// improve on (or match) the starting placement. This is the "move and align
/// until it looks good" step a human does after roughing in anchors + satellites.
/// Default deterministic seed for the placement search (the SA's PRNG). Made a
/// parameter so a search is reproducible by seed, not a hard-coded constant.
const SEARCH_SEED: u64 = 0xD1B54A32D192ED03;

/// Pin-count threshold above which the premium anneal takes the router-free FAST
/// LANE. The tuned routed paths (greedy refine + anneals A/B/C + routed polish)
/// route the WHOLE sheet per move, which is fine on the ≤34-pin reference/snapshot
/// fixtures (<1.2 s) but explodes past ~60 pins (a 119-pin agent board took 113 s).
/// Above this, the search uses only the router-free `proxy_cost` (path D) + a
/// router-free `polish_proxy`, paying the true routed cost only a bounded number of
/// times (candidate selection + the one final emit). Set above every reference/
/// snapshot fixture (max 34 pins) so those stay on the exact tuned path —
/// byte-identical snapshots and tuned-fixture quality are untouched. (Set to 34 =
/// the largest reference/snapshot fixture, uart, so EVERY board above it — including
/// the 35-49-pin agent boards whose tuned routed path ran 4-6 s — takes the fast
/// lane; the `> FAST_PINS` test keeps uart itself routed, hence byte-identical.)
const FAST_PINS: usize = 34;

/// One placement-search strategy over the coarse cells. `Greedy` and `Anneal` are
/// swappable COUNTERPARTS (owner: SA is the paid tier, possibly with a richer
/// cost). `cells` is IN = the seed frame (`assign_cells`), OUT = the chosen
/// placement; `seed` drives any randomness so the result is reproducible.
trait PlacementStrategy {
    fn search(
        &self,
        env: &KicadEnv,
        items: &mut [Item],
        inc: &Incidence,
        ir: &LayoutIr,
        needs_flag: &BTreeSet<String>,
        seed: u64,
    );
}

/// Greedy hill-climb (free tier): local, strictly-cost-improving moves only over
/// the seeded mm placement.
struct Greedy;
impl PlacementStrategy for Greedy {
    fn search(
        &self,
        env: &KicadEnv,
        items: &mut [Item],
        inc: &Incidence,
        ir: &LayoutIr,
        needs_flag: &BTreeSet<String>,
        _seed: u64,
    ) {
        refine_items(env, items, inc, ir, needs_flag);
        // Own the continuous polish (moved out of emit), returning the FINAL placement.
        // The free tier uses the ROUTED polish at EVERY size: it is the truthfulness-
        // safe path (each move re-routes, so the cost sees a net merge / short — the
        // router-free proxy polish does NOT, and greedy has no candidate pick to reject
        // a mis-wire). Slow on a dense board, but only the premium anneal (fast lane) is
        // latency-bound. References keep the exact refine→polish order → byte-identical.
        polish(env, items, inc, ir, needs_flag);
    }
}

/// Simulated annealing (paid tier): a seeded refine→anneal AND a broad anneal from
/// the raw seed, keeping whichever the cost prefers (today's multi-start best-of).
struct Anneal;
impl PlacementStrategy for Anneal {
    fn search(
        &self,
        env: &KicadEnv,
        items: &mut [Item],
        inc: &Incidence,
        ir: &LayoutIr,
        needs_flag: &BTreeSet<String>,
        seed: u64,
    ) {
        use rayon::prelude::*;
        let timed_top = std::env::var("DEBUG_SA_TIME").is_ok();

        // FAST LANE (large boards): the tuned routed paths below route the whole sheet
        // per move and cost minutes past ~60 pins. Here the search is router-free —
        // multi-start `anneal_locality` (proxy cost + range-limited cluster jump) from
        // the raw cell seed — and the only routes paid are the bounded candidate
        // selection + the one final emit. Strictly additive safety is preserved: the
        // RAW seed is always a candidate (a floor), and the pick takes fewest real
        // warnings then true cost, so the fast lane never ships worse than the seed.
        let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
        // PORT-HEAVY sheet = a multi-sheet sub-sheet: its inter-block nets each touch only one
        // pin here, so they become single-pin signal PORTS (labels). Such a sheet is small but
        // its bus/port fanout tangles, and the small path leaves the crossings uncorrected (a
        // 6-part I2C sheet sat at 5 crossings though the topology allows ~1). Route it through the
        // fast lane so it gets the route-aware crossing REFINEMENT (validated: io 7→8,
        // power_entry 8→9). Self-contained reference boards have <6 single-pin signal nets, so
        // they stay on the small path ⇒ snapshots byte-identical.
        let port_heavy = {
            let mut npins: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
            for it in items.iter() {
                for (_, _, net) in &it.pins {
                    if let Some(net) = net {
                        *npins.entry(net.clone()).or_insert(0) += 1;
                    }
                }
            }
            let mut signal_ports = 0usize;
            for (net, c) in &npins {
                if *c == 1 && !ir.rails.contains_key(net.as_str()) && !is_power_net(net) {
                    signal_ports += 1;
                }
            }
            signal_ports >= 6
        };
        let force_fast = port_heavy || std::env::var("MULTISHEET_REFINE").is_ok();
        if pins > FAST_PINS || force_fast {
            let raw: Vec<Item> = items.to_vec();
            // Diverse proxy-anneal starts; fewer for very large boards (each candidate
            // costs two real routes at selection, ~1 s each on a 671-pin BGA).
            let n_starts = if pins > 250 { 1 } else { 3 };
            let seeds: Vec<u64> = (0..n_starts)
                .map(|k| seed ^ (0x9E3779B97F4A7C15u64.wrapping_mul(k as u64 + 1)))
                .collect();
            let t_search = std::time::Instant::now();
            let mut starts: Vec<Vec<Item>> = seeds
                .par_iter()
                .map(|&s| {
                    let mut st = raw.clone();
                    anneal_locality(env, &mut st, inc, ir, needs_flag, s);
                    st
                })
                .collect();
            if timed_top {
                eprintln!("  [SA-fast] {n_starts} proxy starts: {:.2}s", t_search.elapsed().as_secs_f64());
            }
            let mut bases = vec![raw];
            bases.append(&mut starts);
            // From each base placement, produce three FULLY-POLISHED candidates with
            // different post-passes: (a) nudge only — the conservative floor; (b) +magnet
            // — seat each satellite tight to the pin it taps (kills the stranded-cap
            // long-route labels); (c) +magnet +gravity — also pack whole modules toward
            // the centre (kills inter-module sprawl). Seating and packing can collide
            // module power-symbols / net-labels (text the proxy can't see), so all three
            // are offered to the pick, which judges on REAL post-solve warnings then true
            // cost — so neither pass can ever ship a worse/colliding sheet than the floor.
            let variants: [(bool, bool); 3] = [(false, false), (true, false), (true, true)];
            let candidates: Vec<Vec<Item>> = bases
                .par_iter()
                .flat_map_iter(|b| {
                    variants.iter().map(move |&(m, g)| {
                        let mut p = b.clone();
                        polish_proxy(&mut p, inc, ir, m, g);
                        decongest(&mut p);
                        p
                    })
                })
                .collect();
            let t_score = std::time::Instant::now();
            let scored: Vec<(usize, usize, f64)> = candidates
                .par_iter()
                .map(|cand| {
                    // TRUTHFULNESS first: a magnet/gravity move can strand two nets onto
                    // one wire (a merge), which warnings DON'T see — reject those here.
                    let b = truthfulness_breaks(env, cand, inc, ir, needs_flag);
                    let w = warning_count(env, cand, inc, ir, needs_flag);
                    let c = premium_score_with_w(env, cand, inc, ir, needs_flag, w);
                    (b, w, c)
                })
                .collect();
            if timed_top {
                eprintln!("  [SA-fast] score {} candidates: {:.2}s", candidates.len(), t_score.elapsed().as_secs_f64());
            }
            let (mut best, mut best_b, mut best_w, mut best_c) =
                (0usize, usize::MAX, usize::MAX, f64::INFINITY);
            for (k, (b, w, c)) in scored.iter().enumerate() {
                let better = (*b, *w).cmp(&(best_b, best_w)) == std::cmp::Ordering::Less
                    || (*b == best_b && *w == best_w && c + 0.5 < best_c);
                if better {
                    best = k;
                    best_b = *b;
                    best_w = *w;
                    best_c = *c;
                }
            }
            if timed_top {
                eprintln!("  [SA-fast] pick cand#{best} scored={scored:?}");
            }
            // ROUTE-AWARE REFINEMENT (large boards). The proxy is crossing-BLIND, so the
            // fast-lane winner is sprawl-optimal but not crossing-optimal. Refine it with a
            // bounded `anneal_items` whose objective is the TRUE routed cost (premium) — the
            // only faithful crossing signal — which no cheap proxy could capture. Seeded
            // from the already-good winner, so its capped budget (≤750 routed iters, the
            // 420k/pins ceiling) is spent polishing, not exploring. Kept ONLY if it wins the
            // SAME (breaks, warnings, true-cost) pick, so it can never ship worse. This
            // trades the ≤5s budget for fewer dense-board crossings, per the user's call.
            // Score a candidate on its FINALISED geometry. CRUCIAL: the emit runs decongest
            // + align_idiom_clusters + align_led_chains (which e.g. snaps each LED's resistor
            // into a clean leg, tidying a tangled candidate dramatically — c08 53→19) BEFORE
            // counting crossings. Measuring pre-finalise ranks candidates the emit then
            // re-orders, so we finalise a clone here first. The picked candidate ships RAW
            // (the emit re-finalises it identically). Order: truthfulness, warnings, total
            // crossings (body+ic+wire), then straightness.
            let score = |c: &[Item]| -> (usize, usize, usize, f64) {
                let mut m = c.to_vec();
                decongest(&mut m);
                if align_idiom_clusters(&mut m, ir) {
                    decongest(&mut m);
                }
                if align_led_chains(&mut m, inc, ir) {
                    decongest(&mut m);
                }
                let b = truthfulness_breaks(env, &m, inc, ir, needs_flag);
                let w = warning_count(env, &m, inc, ir, needs_flag);
                let (bx, ix, wx) = crossing_counts(env, &m, inc, ir, needs_flag);
                (b, w, bx + ix + wx, premium_score_with_w(env, &m, inc, ir, needs_flag, w))
            };
            let (bb, bw, bx, bc) = score(&candidates[best]);
            // SKIP the refinement when the winner is already clean (no breaks/warnings and
            // few crossings): such boards can't meaningfully improve, so the routed budget
            // would be pure wasted wall-time. Every refinement win this far had a best with
            // ≥7 crossings or a warning, so a ≤6/0-warning gate keeps all wins.
            // NEVER skip the refinement on a forced-fast (multi-sheet) sub-sheet: even at 0-1
            // crossings it often has CAP-SCATTER / long satellite runs (a 3V3 bulk cap marooned
            // far from the regulator output) — an HPWL/straightness defect the crossing-based skip
            // misses but the refinement's true routed-cost objective fixes (it's kept only if the
            // premium score improves). Cheap on a small sheet. A big board still skips when clean.
            let small_forced = force_fast && pins <= FAST_PINS;
            if !small_forced && bb == 0 && bw == 0 && bx <= 6 {
                items.clone_from_slice(&candidates[best]);
                return;
            }
            // ROUTE-AWARE REFINEMENT. The proxy is crossing-BLIND, so the fast-lane winner is
            // sprawl-optimal but not crossing-optimal — and no cheap router-free crossing
            // proxy proved faithful (bbox/trunk-segment all failed). So refine the winner with
            // the TRUE router: a bounded `anneal_items` (premium routed cost; iter-capped
            // 80..300 = 30k/pins so even a 173-pin board stays seconds) seeded from it. Kept
            // ONLY if it wins on real (finalised) crossings, so it is strictly additive — a
            // straighter-but-more-crossing result is rejected. Trades the ≤5s budget for fewer
            // dense-board crossings (user-authorised).
            let mut refined = candidates[best].clone();
            let t_ref = std::time::Instant::now();
            let ref_cap = (30_000 / pins).clamp(80, 300);
            anneal_items(env, &mut refined, inc, ir, needs_flag, false, true, seed ^ 0x5EF1, Some(ref_cap));
            decongest(&mut refined);
            let (rb, rw, rx, rc) = score(&refined);
            let refined_wins = (rb, rw, rx).cmp(&(bb, bw, bx)) == std::cmp::Ordering::Less
                || (rb == bb && rw == bw && rx == bx && rc + 0.5 < bc);
            if timed_top {
                eprintln!(
                    "  [SA-fast] route-refine {:.2}s cap={ref_cap}: ({bb},{bw},{bx},{bc:.0})->({rb},{rw},{rx},{rc:.0}) win={refined_wins}",
                    t_ref.elapsed().as_secs_f64()
                );
            }
            let fast_final: Vec<Item> = if refined_wins { refined } else { candidates[best].clone() };
            // force_fast SMALL sub-sheets: the fast lane's locality proxy can be crossing-worse
            // than the small-board path on SIMPLE sheets (split-supply power: 4 here vs 2). Run the
            // small path too and keep whichever has fewer (breaks, warnings, crossings) via the same
            // `score` — so a congested sheet still gets the fast lane's refinement (io 16→13) while a
            // simple sheet gets the small path's cleaner routing. Cheap: only for force_fast smalls.
            if small_forced {
                let sp = small_path_search(env, &bases[0], inc, ir, needs_flag, seed);
                let (fb, fw, fx, fc) = score(&fast_final);
                let (sb, sw, sx, sc) = score(&sp);
                let sp_wins = (sb, sw, sx).cmp(&(fb, fw, fx)) == std::cmp::Ordering::Less
                    || (sb == fb && sw == fw && sx == fx && sc + 0.5 < fc);
                items.clone_from_slice(if sp_wins { &sp } else { &fast_final });
            } else {
                items.clone_from_slice(&fast_final);
            }
            return;
        }

        // Small board: greedy + four parallel anneals, pick the polished winner.
        // Extracted to small_path_search so the force_fast fast lane can run it as a
        // rival candidate; this call reproduces the old inline behaviour exactly.
        let r = small_path_search(env, items, inc, ir, needs_flag, seed);
        items.clone_from_slice(&r);
    }
}

/// The small-board placement search, extracted so the fast lane can run it as a RIVAL
/// candidate for force_fast SMALL sub-sheets (the fast lane's locality proxy is
/// crossing-worse than this on simple sheets — a split-supply power sheet sat at 4
/// crossings via the fast lane vs 2 here). Greedy refine + four parallel anneals (A
/// seeded, B broad, C premium, D locality), then pick the polished winner by
/// (truthfulness, warnings, premium cost). Operates on a COPY of `seed`, returns the
/// POLISHED winner. Behaviour is byte-identical to the old inline else-branch (the
/// placement_snapshot verifies it for the references that take the small path).
fn small_path_search(
    env: &KicadEnv,
    seed: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    rng_seed: u64,
) -> Vec<Item> {
    use rayon::prelude::*;
    let mut work: Vec<Item> = seed.to_vec();
    let seed_state: Vec<Item> = work.clone();
    let timed = std::env::var("DEBUG_SA_TIME").is_ok();
    let tic = |label: &str, f: &mut dyn FnMut()| {
        let t0 = std::time::Instant::now();
        f();
        if timed {
            eprintln!("  [SA] {label}: {:.2}s", t0.elapsed().as_secs_f64());
        }
    };
    let mut state_a: Vec<Item> = Vec::new();
    let mut state_b: Vec<Item> = seed_state;
    let mut state_c: Vec<Item> = Vec::new();
    let mut state_d: Vec<Item> = Vec::new();
    let mut greedy_state: Vec<Item> = Vec::new();
    rayon::scope(|s| {
        s.spawn(|_| { let mut f = || anneal_items(env, &mut state_b, inc, ir, needs_flag, true, false, rng_seed, None); tic("B broad", &mut f); });
        { let mut f = || refine_items(env, &mut work, inc, ir, needs_flag); tic("greedy", &mut f); }
        greedy_state = work.to_vec();
        state_a = greedy_state.clone();
        state_c = greedy_state.clone();
        state_d = greedy_state.clone();
        rayon::join(
            || { let mut f = || anneal_items(env, &mut state_a, inc, ir, needs_flag, false, false, rng_seed, None); tic("A seeded", &mut f); },
            || {
                rayon::join(
                    || { let mut f = || anneal_items(env, &mut state_c, inc, ir, needs_flag, false, true, rng_seed ^ 0x9E3779B97F4A7C15, None); tic("C premium", &mut f); },
                    || { let mut f = || anneal_locality(env, &mut state_d, inc, ir, needs_flag, rng_seed ^ 0x517CC1B727220A95); tic("D locality", &mut f); },
                )
            },
        );
    });
    let annealed = vec![state_a, state_b, state_c, state_d];
    let mut candidates = vec![greedy_state];
    candidates.extend(annealed);
    let scored: Vec<(usize, usize, f64, Vec<Item>)> = candidates
        .par_iter()
        .map(|cand| {
            let mut shipped = cand.clone();
            polish(env, &mut shipped, inc, ir, needs_flag);
            decongest(&mut shipped);
            let b = truthfulness_breaks(env, &shipped, inc, ir, needs_flag);
            let w = warning_count(env, &shipped, inc, ir, needs_flag);
            let c = premium_score_with_w(env, &shipped, inc, ir, needs_flag, w);
            (b, w, c, shipped)
        })
        .collect();
    let (mut best, mut best_b, mut best_w, mut best_c) =
        (0usize, usize::MAX, usize::MAX, f64::INFINITY);
    for (k, (b, w, c, _)) in scored.iter().enumerate() {
        let better = (*b, *w).cmp(&(best_b, best_w)) == std::cmp::Ordering::Less
            || (*b == best_b && *w == best_w && c + 0.5 < best_c);
        if better {
            best = k;
            best_b = *b;
            best_w = *w;
            best_c = *c;
        }
    }
    scored[best].3.clone()
}

/// Layout-warning count of `items` as they would SHIP — build the writer and run
/// the same finalize (`prepare`: split wires, solve text, reframe) the real emit
/// does, then count. Used only to pick among the SA's final candidates (a handful
/// of calls), never per-move, so the text-solve cost is affordable here.
fn warning_count(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> usize {
    match build_writer(env, None, items, inc, ir, needs_flag, true) {
        Ok(mut w) => {
            w.set_frame(true);
            w.prepare();
            w.layout_warnings().len()
        }
        Err(_) => usize::MAX,
    }
}

/// Geometric TRUTHFULNESS breaks (net merges / shorts / foreign taps) of a placement
/// as it would SHIP — the same checks `layout_cost` prices, returned as a hard count
/// so the candidate pick can REJECT any layout that mis-wires. Critical: the
/// readability `warning_count` does NOT detect a merge (a rail-to-rail short actually
/// LOWERS length+junctions), so a placement move (the proxy magnet/gravity) that
/// strands two nets onto one wire would otherwise be shipped as a fewest-warning
/// candidate — the documented dense-board truthfulness failure. Gating the pick on
/// this makes the router-free fast lane truthfulness-safe without a full netlist
/// extraction.
fn truthfulness_breaks(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> usize {
    match build_writer(env, None, items, inc, ir, needs_flag, true) {
        Ok(w) => {
            let wires = w.wires_with_nets();
            count_merges(&wires, &w.junction_positions())
                + count_shorts(env, &w, items, inc, &wires)
                + count_foreign_taps(&wires)
        }
        Err(_) => usize::MAX,
    }
}

/// Select the placement strategy. Free tier = `Greedy`; SA is opt-in (a later
/// `Tier` enum from agent config wires the paid feature here). Honors
/// `LAYOUT_SEARCH=greedy|anneal` and the `ANNEAL=1` / `GREEDY=1` aliases.
fn pick_strategy() -> Box<dyn PlacementStrategy> {
    let anneal = match std::env::var("LAYOUT_SEARCH").ok().as_deref() {
        Some("anneal") => true,
        Some("greedy") => false,
        _ => std::env::var("ANNEAL").is_ok() && std::env::var("GREEDY").is_err(),
    };
    if anneal { Box::new(Anneal) } else { Box::new(Greedy) }
}

/// Greedy hill-climb over the satellites' mm positions/orientation (the seed is
/// the IR grid projected to mm). Local moves — nudge a cell-step, swap a pair,
/// re-orient, side-flip across the served IC — each kept only on strict
/// improvement of the REAL routed cost (`score_items`). Anchors hold. Operating
/// directly on mm means the objective IS the geometry that ships.
fn refine_items(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) {
    let _ = ir;
    let satellites: Vec<usize> =
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    if satellites.is_empty() {
        return;
    }
    let mut best = score_items(env, items, inc, ir, needs_flag);
    const MAX_ROUNDS: usize = 6;
    for _ in 0..MAX_ROUNDS {
        let mut improved = false;
        // Single-part nudges: shift one satellite by one cell-step (grid-snapped).
        for &i in &satellites {
            for d in [[COL_GAP, 0.0], [-COL_GAP, 0.0], [0.0, ROW_GAP], [0.0, -ROW_GAP]] {
                let prev = items[i].at;
                items[i].at = [crate::grid::snap(prev[0] + d[0]), crate::grid::snap(prev[1] + d[1])];
                let c = score_items(env, items, inc, ir, needs_flag);
                if c + 0.5 < best {
                    best = c;
                    improved = true;
                } else {
                    items[i].at = prev;
                }
            }
        }
        // Pairwise swaps: exchange two satellites' positions (keep each orientation).
        for a in 0..satellites.len() {
            for b in (a + 1)..satellites.len() {
                let (i, j) = (satellites[a], satellites[b]);
                let (pi, pj) = (items[i].at, items[j].at);
                items[i].at = pj;
                items[j].at = pi;
                let c = score_items(env, items, inc, ir, needs_flag);
                if c + 0.5 < best {
                    best = c;
                    improved = true;
                } else {
                    items[i].at = pi;
                    items[j].at = pj;
                }
            }
        }
        // Rotation: re-orient one satellite (free — not displacement-penalised).
        for &i in &satellites {
            let mut best_a = items[i].angle;
            for o in [Orient::Up, Orient::Down, Orient::Left, Orient::Right] {
                let a = orient_angle(&items[i].geom, o);
                if (a - best_a).abs() < EPS {
                    continue;
                }
                items[i].angle = a;
                let c = score_items(env, items, inc, ir, needs_flag);
                if c + 0.5 < best {
                    best = c;
                    best_a = a;
                    improved = true;
                }
            }
            items[i].angle = best_a;
        }
        // Side-flip: mirror a satellite across the single IC anchor it serves (the
        // big relocation a one-cell nudge cannot reach), so a part on the wrong
        // side of its IC migrates over (the wire then drops straight).
        for &i in &satellites {
            if let Some(ax) = anchor_x(items, inc, i) {
                let nx = crate::grid::snap(2.0 * ax - items[i].at[0]);
                if (nx - items[i].at[0]).abs() > EPS {
                    let prev = items[i].at;
                    items[i].at = [nx, prev[1]];
                    let c = score_items(env, items, inc, ir, needs_flag);
                    if c + 0.5 < best {
                        best = c;
                        improved = true;
                    } else {
                        items[i].at = prev;
                    }
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// The mm x of the single IC anchor a satellite serves (None if it taps zero or
/// several distinct anchor x's), for the side-flip move. Reads the anchors' live
/// mm positions, so it tracks a moved anchor.
fn anchor_x(items: &[Item], inc: &Incidence, i: usize) -> Option<f64> {
    let mut xs: BTreeSet<i64> = BTreeSet::new();
    let mut x = 0.0;
    for (_, _, net) in &items[i].pins {
        let Some(net) = net else { continue };
        for (j, _) in inc.get(net).into_iter().flatten() {
            if items[*j].geom.pins.len() >= 3 {
                xs.insert((items[*j].at[0] / GRID_KEY).round() as i64);
                x = items[*j].at[0];
            }
        }
    }
    (xs.len() == 1).then_some(x)
}

/// Quantization for comparing mm x's by grid cell (the 1.27 mm grid).
const GRID_KEY: f64 = 1.27;

/// Deterministic-given-IR PRNG (SplitMix64-ish) so annealing reproduces.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    /// Uniform in [0,1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// A small symmetric integer step in [-r, r].
    fn step(&mut self, r: i32) -> i32 {
        self.below((2 * r + 1) as usize) as i32 - r
    }
}

/// Simulated-annealing placement search over the coarse cells: like `refine_cells`
/// but it accepts *worsening* moves with probability `exp(-Δ/T)` (T cooling to ~0),
/// so it escapes the local minima the greedy climb is trapped in — a satellite
/// stranded across the sheet can migrate, in stages, to hug the IC pin it serves.
/// Moves: relocate a satellite to a random nearby cell, re-orient it, swap two,
/// or nudge an anchor. Every candidate is scored on the REAL routed cost (incl.
/// the spread/stray/overlap terms), and the best layout seen is kept — so SA can
/// only match-or-beat the seed it started from.
/// Simulated annealing over the items' mm positions. Same Metropolis loop as the
/// greedy refine's neighbourhood but it accepts *worsening* moves with probability
/// `exp(-Δ/T)` (T cooling to ~0), so it escapes the local minima greedy is trapped
/// in — a satellite stranded across the sheet can migrate, in stages, to hug the
/// IC pin it serves. Moves operate directly on `at`/`angle` (the shipped geometry),
/// scored by `score_items`; the best layout seen is kept. `broad` runs hotter and
/// longer (a wider global search from the raw seed). ANCHORS are mobile here: an
/// anchor nudge frees a whole block to slide.
fn anneal_items(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    broad: bool,
    premium: bool,
    seed: u64,
    iter_cap: Option<usize>,
) {
    // The objective: free tier minimises the base routed cost; the premium run
    // optimises the richer (straighter) objective. Run as an EXTRA candidate so it
    // never displaces the base run's warning-free find — see `Anneal::search`.
    let cost = |env: &KicadEnv, items: &[Item], inc: &Incidence, ir: &LayoutIr, nf: &BTreeSet<String>| {
        if premium {
            premium_score_items(env, items, inc, ir, nf)
        } else {
            score_items(env, items, inc, ir, nf)
        }
    };
    let sats: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    let anchors: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() >= 3).collect();
    if sats.is_empty() {
        return;
    }
    // Cluster locality: each anchor's "block" is the satellites that tap it plus any
    // idiom members it anchors. The block move (below) slides a whole functional unit
    // (an IC and its decoupling/crystal/tap parts) as one rigid group — the GLOBAL
    // structural move a per-part LOCAL search can't reach.
    let blocks = build_anchor_blocks(items, inc, &anchors, &sats, ir);
    let orients = [Orient::Up, Orient::Down, Orient::Left, Orient::Right];
    let mut rng = Rng(seed);

    let mut cur = cost(env, items, inc, ir, needs_flag);
    let mut best_items: Vec<Item> = items.to_vec();
    let mut best = cur;

    // Iterations scale with part count; temperature cools linearly. T0 is set so an
    // early move that adds a crossing/junction (cost ~5) is readily accepted, while
    // a correctness failure (cost ~1000+) never is.
    // Iterations scale with movable count. Swept down empirically: 0.5x of the
    // previous budget holds (fast 7/7, oneshot 0) with margin, 0.4x is the fragile
    // edge, 0.3x breaks — and because the cooling schedule `t = t0·(1−it/iters)`
    // makes the trajectory chaotic-sensitive to the EXACT count, the safe choice is
    // the margin (0.5x), not the edge. These ceilings (750 / 2000) are ~6x fewer
    // evals than the original 4000 / 12000; the mults are unchanged so the
    // binding-ceiling fixtures get exactly the validated 0.5x count.
    let (mult, t0) = if broad { (700, 30.0) } else { (300, 12.0) };
    let mut iters = (mult * sats.len()).clamp(250, if broad { 2000 } else { 750 });
    // Large boards (100-pin / BGA): each `score_items` routes the WHOLE sheet, and
    // routing cost scales with PIN count (a 100-pin MCU is one item but 186 pins),
    // so the full iteration count runs into minutes. Cap total routing work so
    // `iters * pin-count` stays under a fixed budget. Deterministic (seed-driven,
    // never wall-clock-timed); the tuned fixtures (≤58 pins) are below the threshold
    // and completely unchanged. The SA still ships ≥ greedy regardless of iteration
    // count (greedy is always one of the picked candidates), so a smaller budget
    // can never produce a worse layout — only a less-optimised SA path the candidate
    // pick then discards.
    let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
    if pins > 70 {
        iters = iters.min((420_000 / pins).max(800));
    }
    // A route-aware refinement from an already-good seed caps its routed budget tighter
    // (keeps the >5s large-board path bounded — see the fast-lane call site).
    if let Some(cap) = iter_cap {
        iters = iters.min(cap);
    }
    // One grid cell-step in x/y for the relocation moves.
    let relocate = |rng: &mut Rng, at: [f64; 2], n: i32| -> [f64; 2] {
        [
            crate::grid::snap(at[0] + rng.step(n) as f64 * COL_GAP),
            crate::grid::snap(at[1] + rng.step(n) as f64 * ROW_GAP),
        ]
    };
    // NB: no "exit early once `best` plateaus for N iters" rule. Measured the largest
    // plateau that is still FOLLOWED by a real improvement: up to 1154 iters on the
    // 2000-iter broad run, 685 on a 750-iter seeded run. Every
    // run's last improvement lands at 94-99% of its budget — the ~6x iteration cut
    // already removed the dead tail, so the search genuinely uses its whole budget.
    // A patience small enough to save time would cut those late improvements (a
    // measured 555/uart/mcp tidiness regression); a safe patience saves ~nothing.
    for it in 0..iters {
        let t = (t0 * (1.0 - it as f64 / iters as f64)).max(0.05);
        // Snapshot the item(s) a move touches (at + angle) so it can be rolled back.
        let m = rng.below(10);
        let undo: Vec<(usize, [f64; 2], f64)>;
        if m < 6 {
            // Relocate a satellite to a nearby cell (the big move greedy lacks).
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at, items[i].angle)];
            items[i].at = relocate(&mut rng, items[i].at, 2);
        } else if m < 8 {
            // Re-orient a satellite.
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at, items[i].angle)];
            items[i].angle = orient_angle(&items[i].geom, orients[rng.below(4)]);
        } else if m < 9 && sats.len() >= 2 {
            // Swap two satellites' positions (keep each orientation).
            let a = sats[rng.below(sats.len())];
            let b = sats[rng.below(sats.len())];
            undo = vec![(a, items[a].at, items[a].angle), (b, items[b].at, items[b].angle)];
            let (pa, pb) = (items[a].at, items[b].at);
            items[a].at = pb;
            items[b].at = pa;
        } else if !anchors.is_empty() {
            // Nudge an anchor (an IC) by one cell, carrying its whole BLOCK (the
            // satellites that tap it + the idiom clusters it anchors) by the same
            // delta — a coherent global slide of a functional unit. The rng draws
            // match the old anchor-only nudge (anchor pick + relocate); only the
            // block now follows, so the move is no longer self-defeating.
            let i = anchors[rng.below(anchors.len())];
            let new = relocate(&mut rng, items[i].at, 1);
            let d = [new[0] - items[i].at[0], new[1] - items[i].at[1]];
            let mut group = vec![i];
            if let Some(b) = blocks.get(&i) {
                group.extend(b.iter().copied());
            }
            undo = group.iter().map(|&k| (k, items[k].at, items[k].angle)).collect();
            for &k in &group {
                items[k].at =
                    [crate::grid::snap(items[k].at[0] + d[0]), crate::grid::snap(items[k].at[1] + d[1])];
            }
        } else {
            continue;
        }

        let c = cost(env, items, inc, ir, needs_flag);
        let d = c - cur;
        if d < 0.0 || rng.unit() < (-d / t).exp() {
            cur = c;
            if c < best {
                best = c;
                best_items.clone_from_slice(items);
            }
        } else {
            for (i, at, angle) in undo {
                items[i].at = at;
                items[i].angle = angle;
            }
        }
    }
    items.clone_from_slice(&best_items);
}

/// Each anchor's cluster: the satellites that tap it + the idiom members it anchors,
/// the rigid group the block move slides. Built once; includes frozen members so a
/// recognized cluster travels intact.
fn build_anchor_blocks(
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    sats: &[usize],
    ir: &LayoutIr,
) -> BTreeMap<usize, Vec<usize>> {
    let mut blocks: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &si in sats {
        if let Some((ai, _, _)) = anchor_tap(items, inc, anchors, si, &ir.rails) {
            blocks.entry(ai).or_default().push(si);
        }
    }
    for idiom in &ir.idioms {
        if let Some(ai) = items.iter().position(|it| it.refdes == idiom.anchor) {
            for part in &idiom.parts {
                if let Some(mi) = items.iter().position(|it| &it.refdes == part) {
                    blocks.entry(ai).or_default().push(mi);
                }
            }
        }
    }
    for v in blocks.values_mut() {
        v.sort_unstable();
        v.dedup();
    }
    blocks
}

/// For each satellite, the anchor PINS it should hug: its SIGNAL-net anchor pins
/// (a pull-up belongs by the pin it pulls, not by the rail), falling back to its
/// rail-net anchor pins only when it touches no signal anchor (a decoupling cap on
/// two rails → the IC supply pin). Returned as `(sat_idx, [(anchor_idx, pin_geom_idx)])`
/// so [`proxy_cost`] can recompute the live pin world position each move WITHOUT the
/// router. This is the cheap mirror of [`count_stray`]: the true cost knows a satellite
/// belongs by its pin, but the proxy did not — so the locality anneal never proposed
/// pulling a far-flung pull-up (R1 on NRST + a wide V+ rail, whose net bbox dilutes the
/// HPWL gradient) back to its pin. Precomputed once: which pins a part taps is fixed;
/// only positions move.
/// Manhattan distance from `from` to anchor `j`'s pin `pgi` in world coords (the
/// pin's live position under the anchor's placement). Used to pick the nearest
/// supply pin for a decoupling cap's cohesion target.
fn pin_world_dist(items: &[Item], j: usize, pgi: usize, from: [f64; 2]) -> f64 {
    let p = crate::emit::pin_endpoint(&items[j].geom.pins[pgi], items[j].at, items[j].angle, items[j].mirror);
    (p[0] - from[0]).abs() + (p[1] - from[1]).abs()
}

fn cohesion_targets(items: &[Item], inc: &Incidence, ir: &LayoutIr) -> Vec<(usize, Vec<(usize, usize)>)> {
    let is_anchor = |i: usize| items[i].geom.pins.len() >= 3;
    let mut out = Vec::new();
    for si in 0..items.len() {
        if items[si].geom.pins.len() >= 3 || items[si].frozen {
            continue;
        }
        let (mut sig, mut supply, mut gnd): (
            Vec<(usize, usize)>,
            Vec<(usize, usize)>,
            Vec<(usize, usize)>,
        ) = (Vec::new(), Vec::new(), Vec::new());
        for (_, _, net) in &items[si].pins {
            let Some(net) = net else { continue };
            let is_rail = ir.rails.contains_key(net);
            for (j, num) in inc.get(net).into_iter().flatten() {
                if !is_anchor(*j) {
                    continue;
                }
                if let Some(pgi) = items[*j].geom.pins.iter().position(|p| &p.number == num) {
                    if !is_rail {
                        sig.push((*j, pgi));
                    } else if is_ground(net) {
                        gnd.push((*j, pgi));
                    } else {
                        supply.push((*j, pgi));
                    }
                }
            }
        }
        let tgt = if !sig.is_empty() {
            // Has a signal-pin home: hug those pins (a pull-up over the pin it pulls).
            sig
        } else {
            // A pure decoupling/bypass cap (both pins on rails) belongs at ONE IC
            // SUPPLY pin — the nearest pin on its non-ground (V+) rail, falling back
            // to the nearest ground pin. The OLD behaviour averaged EVERY rail pin
            // (all GND + all V+ pins on the board), whose centroid sits in the middle
            // of the sheet, so the proxy stranded the whole decoupling bank there —
            // the #1 critic complaint ("scattered decoupling caps"). One nearest
            // supply pin banks each cap tight to the IC it bypasses (mirrors
            // `supply_pin_target`, but router-free for the proxy).
            let pool = if !supply.is_empty() { &supply } else { &gnd };
            let at = items[si].at;
            let nearest = pool.iter().copied().min_by(|&(ja, pa), &(jb, pb)| {
                let da = pin_world_dist(items, ja, pa, at);
                let db = pin_world_dist(items, jb, pb, at);
                da.total_cmp(&db)
            });
            match nearest {
                Some(p) => vec![p],
                None => Vec::new(),
            }
        };
        if !tgt.is_empty() {
            out.push((si, tgt));
        }
    }
    // MULTI-UNIT COHESION: the units of one chip (op-amp, FPGA — same refdes, ≥3 pins
    // each, so the satellite loop above skipped them as anchors) share no direct nets
    // beyond the power rails, so each unit follows its OWN signals and the symbol sprawls
    // across the sheet (an FPGA's U1A..U1E scattered with huge gaps — the BGA defect). Pull
    // every unit toward its siblings (one representative pin each) so a multi-unit symbol
    // places as ONE coherent cluster. Reuses the `cohere` term, so no new weight; only
    // fires when a refdes has ≥2 anchor units (single-unit boards/references untouched).
    let mut by_refdes: std::collections::BTreeMap<&str, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..items.len() {
        if items[i].geom.pins.len() >= 3 {
            by_refdes.entry(items[i].refdes.as_str()).or_default().push(i);
        }
    }
    for group in by_refdes.values() {
        if group.len() < 2 {
            continue;
        }
        for &i in group {
            if items[i].frozen {
                continue;
            }
            let tgts: Vec<(usize, usize)> =
                group.iter().copied().filter(|&j| j != i).map(|j| (j, 0usize)).collect();
            out.push((i, tgts));
        }
    }
    out
}

/// A cheap, routing-FREE geometric proxy for [`layout_cost`] — the per-move objective
/// of the locality-aware anneal. The correctness wall (body overlaps, authored-grid
/// order) stays EXACT, never approximated; wirelength is the per-net bounding-box
/// half-perimeter (HPWL) over incident item centres — the standard placement-SA inner
/// loop — `spread` is the whole-board bbox, and `cohere` is the per-satellite Manhattan
/// distance to the anchor PIN it taps (the cheap mirror of [`count_stray`], so the inner
/// loop pulls a far-flung pull-up back to its pin instead of leaving it stranded on a
/// wide rail). It omits the ROUTED neatness terms (crossings/corners/congestion/
/// body-cross, which need the router); the `Anneal::search` candidate pick re-asserts the
/// true routed cost + warnings on the result, so a proxy that ranks geometry can never
/// SHIP a worse or untruthful sheet — it only proposes candidates the true cost then judges.
fn proxy_cost(
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    cohesion: &[(usize, Vec<(usize, usize)>)],
) -> f64 {
    let overlaps = body_overlap_count(items);
    let grid_order = grid_order_viol(items, ir);
    let mut hpwl = 0.0;
    for pins in inc.values() {
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for (i, _) in pins {
            let at = items[*i].at;
            lo[0] = lo[0].min(at[0]);
            lo[1] = lo[1].min(at[1]);
            hi[0] = hi[0].max(at[0]);
            hi[1] = hi[1].max(at[1]);
        }
        if hi[0] >= lo[0] {
            hpwl += (hi[0] - lo[0]) + (hi[1] - lo[1]);
        }
    }
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for it in items {
        lo[0] = lo[0].min(it.at[0]);
        lo[1] = lo[1].min(it.at[1]);
        hi[0] = hi[0].max(it.at[0]);
        hi[1] = hi[1].max(it.at[1]);
    }
    let spread = if hi[0] >= lo[0] { (hi[0] - lo[0]) + (hi[1] - lo[1]) } else { 0.0 };
    let mut cohere = 0.0;
    for (si, tgts) in cohesion {
        let (mut cx, mut cy) = (0.0f64, 0.0f64);
        for (j, pgi) in tgts {
            let p = crate::emit::pin_endpoint(
                &items[*j].geom.pins[*pgi],
                items[*j].at,
                items[*j].angle,
                items[*j].mirror,
            );
            cx += p[0];
            cy += p[1];
        }
        let n = tgts.len() as f64;
        let at = items[*si].at;
        cohere += (at[0] - cx / n).abs() + (at[1] - cy / n).abs();
    }
    // HYBRID VLM zone bias: a SOFT pull of each zoned anchor toward the coarse target
    // fraction the LLM chose (left/centre/right, top/bottom), scaled to mm by the board
    // size. Soft so the engine still does the precise placement and can override the
    // LLM where local geometry demands — the LLM only steers the rough arrangement.
    // Empty `ir.zone` (every existing path) ⇒ 0 ⇒ this is a no-op.
    let mut zbias = 0.0;
    if !ir.zone.is_empty() && hi[0] > lo[0] && hi[1] > lo[1] {
        let (bw, bh) = (hi[0] - lo[0], hi[1] - lo[1]);
        for it in items {
            if let Some([tx, ty]) = ir.zone.get(&it.refdes) {
                let fx = (it.at[0] - lo[0]) / bw;
                let fy = (it.at[1] - lo[1]) / bh;
                zbias += (fx - tx).abs() * bw + (fy - ty).abs() * bh;
            }
        }
    }
    1500.0 * overlaps as f64
        + 1200.0 * grid_order as f64
        + 0.15 * hpwl
        + 0.45 * spread
        + 0.5 * cohere
        + 0.8 * zbias
}

/// Locality-aware anneal (see `docs/specs/locality-aware-placement-search.md`). Two
/// things the tuned full-route paths can't afford: (1) a cheap geometric `proxy_cost`
/// per move (no whole-sheet reroute), so it runs a far larger iteration budget and
/// only pays the true routed cost on a new proxy-best; (2) a RANGE-LIMITED CLUSTER
/// JUMP — slide a whole block by a large displacement when hot, decaying to a nudge
/// when cold — the GLOBAL move that lets a coherent idiom migrate across a congested
/// region in one step (the crystal/reset-cluster gap). Run as an EXTRA candidate in
/// `Anneal::search`: the pick ships it only if it beats the tuned paths on the true
/// cost, so it is purely additive and never regresses a tuned fixture.
fn anneal_locality(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    seed: u64,
) {
    let sats: Vec<usize> =
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    let anchors: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() >= 3).collect();
    if sats.is_empty() {
        return;
    }
    let blocks = build_anchor_blocks(items, inc, &anchors, &sats, ir);
    let cohesion = cohesion_targets(items, inc, ir);
    let orients = [Orient::Up, Orient::Down, Orient::Left, Orient::Right];
    let mut rng = Rng(seed);
    let relocate = |rng: &mut Rng, at: [f64; 2], n: i32| -> [f64; 2] {
        [
            crate::grid::snap(at[0] + rng.step(n) as f64 * COL_GAP),
            crate::grid::snap(at[1] + rng.step(n) as f64 * ROW_GAP),
        ]
    };
    // Board extent in cells — the hot cluster-jump radius.
    let (mut blo, mut bhi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for it in items.iter() {
        blo[0] = blo[0].min(it.at[0]);
        blo[1] = blo[1].min(it.at[1]);
        bhi[0] = bhi[0].max(it.at[0]);
        bhi[1] = bhi[1].max(it.at[1]);
    }
    let span_cells = (((bhi[0] - blo[0]).max(bhi[1] - blo[1])) / COL_GAP).ceil().max(2.0) as i32;

    // Cheap proxy ⇒ afford a big budget; no per-move routing, so no pin-count cap.
    let iters = (40 * sats.len()).clamp(800, 8000);
    let t0 = 24.0;
    // The proxy loop is router-free, but each true-cost VERIFY routes (+ text-solves)
    // the whole sheet. On small boards that's cheap, so keep the historical ~256-cap
    // (the tuned fixtures' path-D result is unchanged). On a large board one route is
    // expensive (a 671-pin BGA ~1 s), so cap verifies pin-aware to stay inside the 5 s
    // budget — the final proxy-best is always verified once below regardless, and the
    // candidate pick re-routes the result, so fewer mid-search verifies never ships
    // worse, only tracks a slightly-staler true-best.
    let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
    let max_verifies = if pins > FAST_PINS { (4000 / pins.max(1)).clamp(6, 128) } else { 256 };
    let verify_period = (iters / max_verifies).max(1);
    let mut last_verify = 0usize;

    let mut cur = proxy_cost(items, inc, ir, &cohesion);
    let mut proxy_best = cur;
    let mut proxy_best_items: Vec<Item> = items.to_vec();
    let mut best_true = premium_score_items(env, items, inc, ir, needs_flag);
    let mut best_items: Vec<Item> = items.to_vec();

    for it in 0..iters {
        let p = it as f64 / iters as f64;
        let t = (t0 * (1.0 - p)).max(0.05);
        let m = rng.below(10);
        let undo: Vec<(usize, [f64; 2], f64)>;
        if m < 6 {
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at, items[i].angle)];
            items[i].at = relocate(&mut rng, items[i].at, 2);
        } else if m < 8 {
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at, items[i].angle)];
            items[i].angle = orient_angle(&items[i].geom, orients[rng.below(4)]);
        } else if m < 9 && sats.len() >= 2 {
            let a = sats[rng.below(sats.len())];
            let b = sats[rng.below(sats.len())];
            undo = vec![(a, items[a].at, items[a].angle), (b, items[b].at, items[b].angle)];
            let (pa, pb) = (items[a].at, items[b].at);
            items[a].at = pb;
            items[b].at = pa;
        } else if !anchors.is_empty() {
            // RANGE-LIMITED CLUSTER JUMP: large displacement when hot, decaying to a
            // 1-cell nudge when cold — carries the anchor's whole block rigidly.
            let radius = (((1.0 - p) * span_cells as f64).round() as i32).max(1);
            let i = anchors[rng.below(anchors.len())];
            let new = relocate(&mut rng, items[i].at, radius);
            let d = [new[0] - items[i].at[0], new[1] - items[i].at[1]];
            let mut group = vec![i];
            if let Some(b) = blocks.get(&i) {
                group.extend(b.iter().copied());
            }
            undo = group.iter().map(|&k| (k, items[k].at, items[k].angle)).collect();
            for &k in &group {
                items[k].at =
                    [crate::grid::snap(items[k].at[0] + d[0]), crate::grid::snap(items[k].at[1] + d[1])];
            }
        } else {
            continue;
        }

        let c = proxy_cost(items, inc, ir, &cohesion);
        let d = c - cur;
        if d < 0.0 || rng.unit() < (-d / t).exp() {
            cur = c;
            if c < proxy_best {
                proxy_best = c;
                proxy_best_items.clone_from_slice(items);
                // Pay the true routed cost only on a new proxy-best, throttled.
                if it - last_verify >= verify_period {
                    last_verify = it;
                    let tc = premium_score_items(env, items, inc, ir, needs_flag);
                    if tc < best_true {
                        best_true = tc;
                        best_items.clone_from_slice(items);
                    }
                }
            }
        } else {
            for (i, at, angle) in undo {
                items[i].at = at;
                items[i].angle = angle;
            }
        }
    }
    // Always verify the final proxy-best against the true cost.
    let tc = premium_score_items(env, &proxy_best_items, inc, ir, needs_flag);
    if tc < best_true {
        best_items.clone_from_slice(&proxy_best_items);
    }
    items.clone_from_slice(&best_items);
}

/// Build and score the schematic for `items` exactly as placed (no cell layout).
/// Used by the pin-alignment pass, which nudges raw positions.
fn score_items(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> f64 {
    match build_writer(env, None, items, inc, ir, needs_flag, false) {
        Ok(w) => layout_cost(env, &w, items, inc, ir, false),
        Err(_) => f64::INFINITY,
    }
}

/// PREMIUM-tier cost. The paid SA pays for the ACCURATE objective the free tier
/// can't afford: the REAL post-solve lint warning count (`warning_count` =
/// build + text-solve + count), heavily weighted, so the SA directly minimises the
/// shipped warnings — not a cheap pre-solve proxy, which diverges from the truth
/// (the solver fixes much of the pre-solve crowding; minimising the proxy lands
/// WORSE — measured). Then the straightness-weighted routed cost
/// (`layout_cost(premium=true)`) breaks ties toward a tidier sheet. This is what
/// the SA explores; the shipped finalize uses the base cost, same metric both tiers.
fn premium_score_items(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> f64 {
    let aes = match build_writer(env, None, items, inc, ir, needs_flag, false) {
        Ok(w) => layout_cost(env, &w, items, inc, ir, true),
        Err(_) => return f64::INFINITY,
    };
    // The accurate objective costs a per-move text solve + reroute; on dense boards
    // (selfrepair's 88 nets, bga's 671 pins) that runs into MANY minutes per emit, so
    // there the premium falls back to the straightness cost alone — still additive,
    // just without the warning-minimisation that drove oneshot (186 pins / 23 nets,
    // affordable) to 0. The cheap base eval the free tier uses scales fine; only this
    // accurate variant needs the guard.
    let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
    if pins <= 250 && inc.len() <= 40 {
        10_000.0 * warning_count(env, items, inc, ir, needs_flag) as f64 + aes
    } else {
        aes
    }
}

/// `premium_score_items` when the caller ALREADY knows the shipped warning count
/// `w` (the candidate pick computes it for the primary sort). Identical result,
/// but skips the redundant second text-solving `warning_count` — the candidate
/// evaluation was paying for two full text solves per candidate.
fn premium_score_with_w(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    w: usize,
) -> f64 {
    let aes = match build_writer(env, None, items, inc, ir, needs_flag, false) {
        Ok(wr) => layout_cost(env, &wr, items, inc, ir, true),
        Err(_) => return f64::INFINITY,
    };
    let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
    if pins <= 250 && inc.len() <= 40 {
        10_000.0 * w as f64 + aes
    } else {
        aes
    }
}

/// Pin-alignment polish: slide each satellite onto the AXIS of the signal pin it
/// wires to, so the connecting wire drops (or runs) straight instead of jogging
/// out from a column centre — a vertical part aligns its x to the pin, a
/// horizontal part its y. The coarse cell grid can only place a part at a column
/// centre, so this sub-column offset is done here on raw positions, kept only
/// when it lowers cost (a straighter, shorter wire) and overlaps nothing.
fn align_to_pins(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) {
    let Ok(w0) = build_writer(env, None, items, inc, ir, needs_flag, false) else { return };
    // Per satellite: is it vertical, and where is its signal-pin target?
    let mut plans: Vec<(usize, bool, [f64; 2])> = Vec::new();
    for (si, s) in items.iter().enumerate() {
        if s.geom.pins.len() != 2 {
            continue;
        }
        let pos = |n: &str| w0.pin_dirs(env, &s.refdes, n).ok().and_then(|v| v.first().map(|x| x.0));
        let (Some(p0), Some(p1)) = (pos(&s.geom.pins[0].number), pos(&s.geom.pins[1].number)) else {
            continue;
        };
        let vertical = (p0[1] - p1[1]).abs() >= (p0[0] - p1[0]).abs();
        if let Some(t) = signal_anchor_centroid(env, &w0, items, inc, ir, s, false) {
            // A part touching a real IC SIGNAL pin aligns to it (a pull-up over its
            // pin, a series element onto its pin row).
            plans.push((si, vertical, t));
        } else if let Some(t) = supply_pin_target(env, &w0, items, inc, ir, s) {
            // A decoupling/bypass cap with no signal pin hugs the IC SUPPLY pin it
            // bypasses, so it hangs right at that pin instead of drifting to a far
            // frame column (decongest spreads a bank that all wants one pin x).
            plans.push((si, vertical, t));
        }
    }
    drop(w0);

    let mut best = score_items(env, items, inc, ir, needs_flag);
    for (si, vertical, target) in plans {
        // Walk one grid step at a time TOWARD the pin axis, keeping the cheapest
        // clear position found. Walking (not jumping) means that when the exact
        // axis is taken — two pull-ups for adjacent IC pins want the same x — the
        // part still slides as close as it can instead of staying put.
        let axis = if vertical { 0 } else { 1 };
        let orig = items[si].at;
        let goal = crate::grid::snap(target[axis]);
        let dir = (goal - orig[axis]).signum();
        if dir == 0.0 {
            continue;
        }
        let (mut best_pos, mut best_cost) = (orig, best);
        let mut p = orig;
        for _ in 0..24 {
            p[axis] += dir * 1.27;
            if (p[axis] - goal) * dir > EPS || overlaps_any(items, si, p) {
                break;
            }
            items[si].at = p;
            let c = score_items(env, items, inc, ir, needs_flag);
            if c + 0.5 < best_cost {
                best_cost = c;
                best_pos = p;
            }
        }
        items[si].at = best_pos;
        best = best_cost;
    }
}

/// An item's body rect at position `at`. Uses the FULL `approx_size` (which
/// already pads 2.54 mm/side) so the placement overlap check reserves room for
/// the symbol body *and* its side-mounted value/refdes text — matching what the
/// readability lint flags as an overlap, so a layout the climb accepts is one
/// the lint passes. (The router uses its own, tighter solid extent in
/// `emit::route_scene`; this looser one is only for symbol-vs-symbol spacing.)
fn item_rect(it: &Item, at: [f64; 2]) -> [f64; 4] {
    let s = it.geom.approx_size();
    let quarter = ((it.angle / 90.0).round() as i64).rem_euclid(2) == 1;
    let (w, h) = if quarter { (s[1], s[0]) } else { (s[0], s[1]) };
    let (hw, hh) = ((w / 2.0).max(1.27), (h / 2.0).max(1.27));
    let mut r = [at[0] - hw, at[1] - hh, at[0] + hw, at[1] + hh];
    // Reserve the side-mounted refdes/value text footprint so a tight pack leaves
    // it collision-free — the readability lint flags text-over-body, so the climb
    // must keep a neighbour out of the conventional text spot. KiCAD draws a
    // vertical 2-pin part's fields stacked to the RIGHT, a horizontal part's
    // refdes above / value below. (~1.1 mm/char, ~1.6 mm/line.)
    if it.geom.pins.len() == 2 {
        if quarter {
            r[1] -= 2.0; // refdes line above
            r[3] += 2.0; // value line below
        } else {
            let chars = it.value.chars().count().max(it.refdes.chars().count()) as f64;
            r[2] += chars * 1.1 + 1.27; // field stack to the right
        }
    }
    r
}

fn rects_overlap(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[2] - EPS && b[0] < a[2] - EPS && a[1] < b[3] - EPS && b[1] < a[3] - EPS
}

/// Sub-grid compaction: slide each satellite one grid step toward the drawing's
/// centroid wherever that does NOT raise the routed cost (which already prices
/// whitespace via `spread`, plus length/corners/body-crossings) and creates no
/// overlap. The cell grid can only place parts at column centres with fixed gaps;
/// this closes the slack between them. Strictly cost-gated, so it only ever
/// tightens — it can never regress a layout the optimiser already settled.
fn compact(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) {
    let sats: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    if sats.is_empty() {
        return;
    }
    let mut best = score_items(env, items, inc, ir, needs_flag);
    for _ in 0..8 {
        let (mut cx, mut cy) = (0.0, 0.0);
        for it in items.iter() {
            cx += it.at[0];
            cy += it.at[1];
        }
        let c = [cx / items.len() as f64, cy / items.len() as f64];
        let mut improved = false;
        for &i in &sats {
            for axis in 0..2 {
                let dir = (c[axis] - items[i].at[axis]).signum();
                if dir == 0.0 {
                    continue;
                }
                let orig = items[i].at;
                let mut p = orig;
                p[axis] += dir * 1.27;
                // Keep a full grid of clearance (compaction must not pack two parts
                // into a touch the readability lint flags even when the bare body
                // rects technically clear).
                let r = item_rect(&items[i], p);
                let a = [r[0] - 1.27, r[1] - 1.27, r[2] + 1.27, r[3] + 1.27];
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != i && rects_overlap(a, item_rect(it, it.at)))
                {
                    continue;
                }
                items[i].at = p;
                let sc = score_items(env, items, inc, ir, needs_flag);
                if sc + 0.25 < best {
                    best = sc;
                    improved = true;
                } else {
                    items[i].at = orig;
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// Continuous-placement polish — the post-cell-search optimiser. The cell grid
/// only places parts at column centres; the directed slides (`align_to_pins`,
/// `compact`) and a FREE per-axis nudge recover the sub-grid freedom. Running them
/// run-once-in-sequence is myopic: a part aligned to its pin is never re-considered
/// after a neighbour compacts away. So iterate {align → compact → free-nudge} to a
/// fixpoint, all gated by the real routed cost (`score_items`), so it only ever
/// lowers cost — the references can't regress past their settled minimum, and the
/// busier sheets get the extra freedom to tighten. `decongest` still guarantees
/// no overlap afterwards.
fn polish(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) {
    // Each pass routes the whole sheet per candidate move, so this is the engine's
    // hot loop. The directed slides converge in 1-2 iterations on the small
    // reference sheets (the break fires early); the cap bounds the cost on a dense
    // board (a 121-ball BGA) where there's always a sub-grid step left to find.
    let mut prev = score_items(env, items, inc, ir, needs_flag);
    for _ in 0..3 {
        align_to_pins(env, items, inc, ir, needs_flag);
        compact(env, items, inc, ir, needs_flag);
        free_nudge(env, items, inc, ir, needs_flag);
        let now = score_items(env, items, inc, ir, needs_flag);
        if prev - now < 1.0 {
            break; // converged (or not worth another full sweep)
        }
        prev = now;
    }
}

/// Free per-axis nudge: try sliding each satellite ±1 grid in x and y, keeping any
/// move that lowers the routed cost without creating a (clearance-padded) overlap.
/// The directed slides only move a part TOWARD its pin axis or the centroid; this
/// reaches the off-axis positions they can never propose (e.g. a part that should
/// step sideways to uncross a wire), which is the extra freedom `polish` adds over
/// the old align-then-compact.
fn free_nudge(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) {
    let sats: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    if sats.is_empty() {
        return;
    }
    let mut best = score_items(env, items, inc, ir, needs_flag);
    for _ in 0..2 {
        let mut improved = false;
        for &i in &sats {
            let orig = items[i].at;
            let (mut best_pos, mut best_cost) = (orig, best);
            for (axis, dir) in [(0usize, 1.0), (0, -1.0), (1, 1.0), (1, -1.0)] {
                let mut p = orig;
                p[axis] += dir * 1.27;
                // Keep a full grid of clearance, as `compact` does, so a free nudge
                // never packs two parts into a touch the readability lint flags.
                let r = item_rect(&items[i], p);
                let pad = [r[0] - 1.27, r[1] - 1.27, r[2] + 1.27, r[3] + 1.27];
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != i && rects_overlap(pad, item_rect(it, it.at)))
                {
                    continue;
                }
                items[i].at = p;
                let c = score_items(env, items, inc, ir, needs_flag);
                if c + 0.25 < best_cost {
                    best_cost = c;
                    best_pos = p;
                }
            }
            items[i].at = best_pos;
            if best_pos != orig {
                best = best_cost;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
}

/// Router-free sub-grid polish for LARGE boards (`pins > FAST_PINS`). The routed
/// `polish` (align/compact/free_nudge, each routing the whole sheet per candidate
/// move) is the engine's hot loop and costs tens of seconds past ~60 pins. This
/// does the same essential job — pull each satellite onto the anchor pin it taps and
/// close sub-grid whitespace — but scores moves with `proxy_cost` (overlap wall +
/// HPWL + spread + cohesion-to-pin, no router), so its cost is independent of pin
/// count. The clearance-padded overlap guard matches `free_nudge` so it never packs
/// two parts into a readability-lint touch. The SHIPPED warnings are still measured
/// by the one real route emit runs afterwards; this only positions.
fn polish_proxy(items: &mut [Item], inc: &Incidence, ir: &LayoutIr, magnet: bool, gravity: bool) {
    let sats: Vec<usize> =
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    if sats.is_empty() {
        return;
    }
    let cohesion = cohesion_targets(items, inc, ir);
    // Seat each free satellite next to the pin it taps FIRST (a teleport the ±1-cell
    // nudge below can't reach), so a satellite the SA stranded across the sheet (a
    // reset cap far from NRST → a long blocked route the router gives up on and
    // labels) snaps tight to its pin. Then the nudge settles sub-grid offsets.
    if magnet {
        magnet_proxy(items, &cohesion, ir, inc);
    }
    let mut best = proxy_cost(items, inc, ir, &cohesion);
    for _ in 0..6 {
        let mut improved = false;
        for &i in &sats {
            let orig = items[i].at;
            let (mut best_pos, mut best_cost) = (orig, best);
            for (axis, dir) in [(0usize, 1.0), (0, -1.0), (1, 1.0), (1, -1.0)] {
                let mut p = orig;
                p[axis] += dir * 1.27;
                let r = item_rect(&items[i], p);
                let pad = [r[0] - 1.27, r[1] - 1.27, r[2] + 1.27, r[3] + 1.27];
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != i && rects_overlap(pad, item_rect(it, it.at)))
                {
                    continue;
                }
                items[i].at = p;
                let c = proxy_cost(items, inc, ir, &cohesion);
                if c + 0.25 < best_cost {
                    best_cost = c;
                    best_pos = p;
                }
            }
            items[i].at = best_pos;
            if best_pos != orig {
                best = best_cost;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
    // Optionally close inter-module whitespace (the dominant sprawl) by packing whole
    // blocks toward the centroid — offered as a pick-protected variant by the caller,
    // since over-packing can collide module labels the proxy can't see.
    if gravity {
        block_gravity_proxy(items, inc, ir, &cohesion);
    }
}

/// Router-free satellite SEATING: teleport each free satellite to the best
/// overlap-free cell within ±2 grid of the pin it taps (its cohesion-target
/// centroid), kept only when it lowers `proxy_cost`. The ±1-cell nudge can only walk
/// locally, so a satellite the anneal stranded far from its pin never migrates back;
/// this jumps it home in one move. Greedy + proxy-gated, so it only ever tightens.
fn magnet_proxy(
    items: &mut [Item],
    cohesion: &[(usize, Vec<(usize, usize)>)],
    ir: &LayoutIr,
    inc: &Incidence,
) {
    let mut best = proxy_cost(items, inc, ir, cohesion);
    for (si, tgts) in cohesion {
        let si = *si;
        // Live centroid of the target pins.
        let (mut tx, mut ty) = (0.0f64, 0.0f64);
        for &(j, pgi) in tgts {
            let p = crate::emit::pin_endpoint(
                &items[j].geom.pins[pgi],
                items[j].at,
                items[j].angle,
                items[j].mirror,
            );
            tx += p[0];
            ty += p[1];
        }
        let n = tgts.len() as f64;
        let t = [tx / n, ty / n];
        let orig = items[si].at;
        let (mut best_pos, mut best_c) = (orig, best);
        for dy in -2..=2 {
            for dx in -2..=2 {
                let p = [
                    crate::grid::snap(t[0] + dx as f64 * COL_GAP),
                    crate::grid::snap(t[1] + dy as f64 * ROW_GAP),
                ];
                let r = item_rect(&items[si], p);
                let pad = [r[0] - 1.27, r[1] - 1.27, r[2] + 1.27, r[3] + 1.27];
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != si && rects_overlap(pad, item_rect(it, it.at)))
                {
                    continue;
                }
                items[si].at = p;
                let c = proxy_cost(items, inc, ir, cohesion);
                if c + 0.25 < best_c {
                    best_c = c;
                    best_pos = p;
                }
            }
        }
        items[si].at = best_pos;
        best = best_c;
    }
}

/// Router-free MODULE compaction: slide each anchor's whole BLOCK (the IC + its tap
/// satellites + frozen idiom members) one grid step at a time toward the layout
/// centroid, kept only when it lowers `proxy_cost` and the moved block overlaps no
/// other part. This is the deterministic counterpart to the anneal's random cluster
/// jump — it directly removes the inter-module whitespace (the "modules flung apart /
/// long detour rails" sprawl the per-satellite nudge can't reach) without ever
/// routing. Blocks are rigid, so each block's internal layout (a banked decoupling
/// row, a crystal cluster) travels intact.
fn block_gravity_proxy(
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    cohesion: &[(usize, Vec<(usize, usize)>)],
) {
    let anchors: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() >= 3).collect();
    let sats: Vec<usize> =
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    if anchors.is_empty() {
        return;
    }
    let blocks = build_anchor_blocks(items, inc, &anchors, &sats, ir);
    let mut best = proxy_cost(items, inc, ir, cohesion);
    for _ in 0..12 {
        // Layout centroid (recomputed each sweep as modules pack inward).
        let (mut cx, mut cy) = (0.0f64, 0.0f64);
        for it in items.iter() {
            cx += it.at[0];
            cy += it.at[1];
        }
        let c = [cx / items.len() as f64, cy / items.len() as f64];
        let mut improved = false;
        for &ai in &anchors {
            let mut group = vec![ai];
            if let Some(b) = blocks.get(&ai) {
                group.extend(b.iter().copied());
            }
            let in_group: BTreeSet<usize> = group.iter().copied().collect();
            for axis in 0..2 {
                let dir = (c[axis] - items[ai].at[axis]).signum();
                if dir == 0.0 {
                    continue;
                }
                let mut delta = [0.0; 2];
                delta[axis] = dir * 1.27;
                // Tentatively slide the whole group; reject if any moved member's
                // padded rect now overlaps a NON-group part.
                // Keep a generous inter-module GUTTER (not just the body-clearance
                // `compact`/`free_nudge` use): packed modules carry power symbols and
                // net-label pennants in the gutter between them, and those text boxes
                // collide well before the bodies do — the "compaction trades against
                // text collisions the cost can't see" trap. A wider margin stops the
                // gravity short of label crowding.
                const G: f64 = 5.08;
                let collide = group.iter().any(|&k| {
                    let np = [items[k].at[0] + delta[0], items[k].at[1] + delta[1]];
                    let r = item_rect(&items[k], np);
                    let pad = [r[0] - G, r[1] - G, r[2] + G, r[3] + G];
                    items
                        .iter()
                        .enumerate()
                        .any(|(j, it)| !in_group.contains(&j) && rects_overlap(pad, item_rect(it, it.at)))
                });
                if collide {
                    continue;
                }
                for &k in &group {
                    items[k].at[0] += delta[0];
                    items[k].at[1] += delta[1];
                }
                let nc = proxy_cost(items, inc, ir, cohesion);
                if nc + 0.25 < best {
                    best = nc;
                    improved = true;
                } else {
                    for &k in &group {
                        items[k].at[0] -= delta[0];
                        items[k].at[1] -= delta[1];
                    }
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// Whether placing item `si` at `at` would overlap any other item's body.
fn overlaps_any(items: &[Item], si: usize, at: [f64; 2]) -> bool {
    let a = item_rect(&items[si], at);
    items
        .iter()
        .enumerate()
        .any(|(j, it)| j != si && rects_overlap(a, item_rect(it, it.at)))
}

/// Final overlap relaxation (deterministic): push any two overlapping bodies
/// apart along their axis of least penetration, snapped to the grid, until the
/// sheet is collision-free or a hard iteration cap is hit. ICs (anchors) hold
/// when paired with a 2-pin part — the satellite yields; two of a kind split the
/// push. Only positions move, so connectivity is untouched and the router redraws
/// around the new placement on the following pass.
fn decongest(items: &mut [Item]) {
    const MAX_ITERS: usize = 3000;
    for _ in 0..MAX_ITERS {
        // First overlapping pair in a fixed order (determinism).
        let mut hit = None;
        'scan: for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let (a, b) = (item_rect(&items[i], items[i].at), item_rect(&items[j], items[j].at));
                if rects_overlap(a, b) {
                    hit = Some((i, j, a, b));
                    break 'scan;
                }
            }
        }
        let Some((i, j, a, b)) = hit else { break };
        let pen_x = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
        let pen_y = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
        let axis = if pen_x <= pen_y { 0 } else { 1 };
        let pen = if axis == 0 { pen_x } else { pen_y };
        let push = ((pen / 1.27).ceil() * 1.27).max(1.27);
        // Move j away from i along `axis` (deterministic by the +side of i).
        let dir = if items[j].at[axis] >= items[i].at[axis] { 1.0 } else { -1.0 };
        let (i_anchor, j_anchor) =
            (items[i].geom.pins.len() >= 3, items[j].geom.pins.len() >= 3);
        match (i_anchor, j_anchor) {
            (false, true) => items[i].at[axis] -= dir * push,
            (true, false) => items[j].at[axis] += dir * push,
            _ => {
                let half = (push / 2.0 / 1.27).ceil() * 1.27;
                items[i].at[axis] -= dir * half;
                items[j].at[axis] += dir * half;
            }
        }
    }
}

/// Count pairs of items whose bodies overlap — the hard "never let two symbols
/// collide" wall. Catches adjacent-cell collisions the same-cell check misses.
fn body_overlap_count(items: &[Item]) -> usize {
    let mut n = 0;
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            if rects_overlap(item_rect(&items[i], items[i].at), item_rect(&items[j], items[j].at)) {
                n += 1;
            }
        }
    }
    n
}

/// Wires that run straight THROUGH a 2-pin part's body — a foreign (or trunk)
/// segment crossing the pin-to-pin axis at a point strictly interior to it,
/// perpendicular to the part. This reads as "a wire drawn through a resistor" and
/// the existing parallel-proximity check never catches it (it is a crossing, not
/// a hug). A lead leaving a pin is collinear with / starts at the body endpoint,
/// so it is excluded.
fn count_body_crossings(
    bodies: &[([f64; 2], [f64; 2])],
    wires: &[([f64; 2], [f64; 2], Option<String>)],
) -> usize {
    let mut n = 0;
    for (a, b) in bodies {
        let bh = (a[1] - b[1]).abs() < EPS; // body axis horizontal?
        if (a[0] - b[0]).abs() < EPS && (a[1] - b[1]).abs() < EPS {
            continue;
        }
        for (w1, w2, _) in wires {
            let wh = (w1[1] - w2[1]).abs() < EPS;
            if bh == wh {
                continue; // need a perpendicular wire
            }
            let (interior, on_wire) = if bh {
                let p = [w1[0], a[1]];
                (
                    p[0] > a[0].min(b[0]) + EPS && p[0] < a[0].max(b[0]) - EPS,
                    crate::emit::point_on_segment(p, *w1, *w2),
                )
            } else {
                let p = [a[0], w1[1]];
                (
                    p[1] > a[1].min(b[1]) + EPS && p[1] < a[1].max(b[1]) - EPS,
                    crate::emit::point_on_segment(p, *w1, *w2),
                )
            };
            if interior && on_wire {
                n += 1;
            }
        }
    }
    n
}

/// Wires that run straight THROUGH a 2-pin part COLLINEARLY — a segment on the
/// part's own pin-to-pin axis that extends strictly BEYOND both pins, i.e. it
/// enters one side, slices across the body (and the near pin, a foreign net), and
/// exits the far side. The classic case [`count_body_crossings`] misses: a rail
/// wire reaching a part's FAR pin by going straight through the part instead of
/// approaching from that pin's side (the NE555's GND pin dropping through the LED to
/// the ground rail). A series part's own leads STOP at a pin — never span beyond
/// both — so this never fires on a correctly-drawn in-line resistor/cap.
fn count_collinear_body_crossings(
    bodies: &[([f64; 2], [f64; 2])],
    wires: &[([f64; 2], [f64; 2], Option<String>)],
) -> usize {
    let mut n = 0;
    for (a, b) in bodies {
        if (a[0] - b[0]).abs() < EPS && (a[1] - b[1]).abs() < EPS {
            continue;
        }
        let bh = (a[1] - b[1]).abs() < EPS; // horizontal part (pins differ in x)?
        let axis = if bh { 0 } else { 1 };
        let (plo, phi) = (a[axis].min(b[axis]), a[axis].max(b[axis]));
        for (w1, w2, _) in wires {
            let wh = (w1[1] - w2[1]).abs() < EPS;
            if bh != wh {
                continue; // need a PARALLEL wire (the collinear candidate)
            }
            // ...on the SAME line as the body axis (matching perpendicular coord).
            let perp = if bh { 1 } else { 0 };
            if (w1[perp] - a[perp]).abs() > EPS {
                continue;
            }
            let (wlo, whi) = (w1[axis].min(w2[axis]), w1[axis].max(w2[axis]));
            if wlo < plo - EPS && whi > phi + EPS {
                n += 1;
            }
        }
    }
    n
}

/// Wires routed straight THROUGH an IC (3+ pin) body rectangle — the package
/// equivalent of [`count_body_crossings`] (which only handles a 2-pin part's
/// pin-to-pin axis). A foreign net's segment drawn across the chip box, over its
/// internal glyphs, reads as broken even though the netlist is sound (the body is
/// only priced, never a hard router obstacle). `ic_rects` are body interiors
/// (pin-tip bbox shrunk inward past the pin stubs) so a wire legitimately
/// attaching at a pin tip and routing OUTWARD never counts; only a segment with a
/// portion strictly inside the rect does.
fn count_ic_body_crossings(
    ic_rects: &[[f64; 4]],
    wires: &[([f64; 2], [f64; 2], Option<String>)],
) -> usize {
    let mut n = 0;
    for r in ic_rects {
        if r[2] - r[0] < EPS || r[3] - r[1] < EPS {
            continue;
        }
        for (w1, w2, _) in wires {
            // Wires are axis-aligned; a zero-width interval can't be tested as a
            // 2-D box overlap, so split by orientation: the constant coordinate
            // must be strictly inside the rect, the spanning interval must overlap.
            let cross = if (w1[0] - w2[0]).abs() < EPS {
                let x = w1[0];
                let (ylo, yhi) = (w1[1].min(w2[1]), w1[1].max(w2[1]));
                r[0] + EPS < x && x < r[2] - EPS && ylo.max(r[1]) < yhi.min(r[3]) - EPS
            } else {
                let y = w1[1];
                let (xlo, xhi) = (w1[0].min(w2[0]), w1[0].max(w2[0]));
                r[1] + EPS < y && y < r[3] - EPS && xlo.max(r[0]) < xhi.min(r[2]) - EPS
            };
            if cross {
                n += 1;
            }
        }
    }
    n
}

/// Wires running PARALLEL to a 2-pin part, OFFSET inside its body but off the
/// pin-to-pin centerline — the case [`count_body_crossings`] (perpendicular only)
/// and [`count_collinear_body_crossings`] (on the centerline, beyond both pins) both
/// miss. A 2-pin symbol body (a cap's plates, a resistor's rectangle) is ~3 mm wide,
/// so a riser one 1.27 mm grid step off the part's axis still slices through the
/// drawn body — exactly what a dense vertical-cap column produces. The part's OWN
/// leads attach at the pin ENDS (outside the central body span), so a correctly
/// drawn in-line part never fires.
fn count_parallel_body_crossings(
    bodies: &[([f64; 2], [f64; 2])],
    wires: &[([f64; 2], [f64; 2], Option<String>)],
) -> usize {
    const PLATE_HALF: f64 = 1.4; // half the drawn 2-pin body width (catches a 1.27 mm offset)
    const PIN_STUB: f64 = 2.54; // exclude the pin stubs at each end
    let mut n = 0;
    for (a, b) in bodies {
        if (a[0] - b[0]).abs() < EPS && (a[1] - b[1]).abs() < EPS {
            continue;
        }
        let bh = (a[1] - b[1]).abs() < EPS; // horizontal part?
        let (axis, perp) = if bh { (0, 1) } else { (1, 0) };
        let (plo, phi) = (a[axis].min(b[axis]), a[axis].max(b[axis]));
        let (blo, bhi) = (plo + PIN_STUB, phi - PIN_STUB); // central body, past the stubs
        if bhi <= blo + EPS {
            continue;
        }
        for (w1, w2, _) in wires {
            let wh = (w1[1] - w2[1]).abs() < EPS;
            if bh != wh {
                continue; // need a PARALLEL wire (perpendicular is count_body_crossings)
            }
            if (w1[perp] - a[perp]).abs() > PLATE_HALF - EPS {
                continue; // outside the drawn body width
            }
            let (wlo, whi) = (w1[axis].min(w2[axis]), w1[axis].max(w2[axis]));
            if wlo < bhi - EPS && whi > blo + EPS {
                n += 1;
            }
        }
    }
    n
}

/// Wire corners (L-bends): points where exactly two perpendicular same-net
/// segments meet. Length alone treats a jiggly L-jog path and a straight run as
/// equal; this penalises the BENDS, so the optimiser prefers straight drops and
/// straight runs along rails — the single term that most separates a clean
/// reference layout from a compact-but-jiggly diagonal staircase. A ≥3-way meet
/// (a junction/tap) is not a corner and is excluded by the exact-two test.
fn count_corners(wires: &[([f64; 2], [f64; 2], Option<String>)]) -> usize {
    // (net, point) -> orientations of the segments ending there (true = horizontal).
    let mut at: BTreeMap<(String, u64, u64), Vec<bool>> = BTreeMap::new();
    for (a, b, n) in wires {
        let Some(net) = n else { continue };
        let horiz = (a[1] - b[1]).abs() < EPS;
        for p in [a, b] {
            at.entry((net.clone(), p[0].to_bits(), p[1].to_bits())).or_default().push(horiz);
        }
    }
    at.values().filter(|o| o.len() == 2 && o[0] != o[1]).count()
}

/// Foreign taps = the post-split short class: a wire endpoint of one net lying
/// strictly interior to a wire of a DIFFERENT net. The finalize wire-split makes
/// such a contact a real connection in the netlist (KiCAD splits the through-wire
/// at the tap), so it must read as a short here or the hill-climb would create
/// one to save length. A same-net riser tapping its own rail is the intended case
/// and is excluded by the net check.
fn count_foreign_taps(wires: &[([f64; 2], [f64; 2], Option<String>)]) -> usize {
    let strict_interior = |p: [f64; 2], a: [f64; 2], b: [f64; 2]| {
        let is_end = |q: [f64; 2]| near(p, q);
        !is_end(a) && !is_end(b) && crate::emit::point_on_segment(p, a, b)
    };
    let mut n = 0;
    for (a1, a2, an) in wires {
        for (b1, b2, bn) in wires {
            if an == bn || an.is_none() || bn.is_none() {
                continue;
            }
            if strict_interior(*a1, *b1, *b2) || strict_interior(*a2, *b1, *b2) {
                n += 1;
            }
        }
    }
    n
}

/// Weighted aesthetic cost of a built schematic. Label fallbacks and shorts
/// dominate (they are correctness/quality failures); then visual wire crossings,
/// then junction dots, with total wire length as a light tiebreaker.
fn layout_cost(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    premium: bool,
) -> f64 {
    let fallbacks = w.signal_label_count();
    let junctions = w.junction_count();
    let wires = w.wires_with_nets();
    let length: f64 =
        wires.iter().map(|(a, b, _)| (a[0] - b[0]).abs() + (a[1] - b[1]).abs()).sum();
    let crossings = count_crossings(&wires);
    let corners = count_corners(&wires);
    let merges = count_merges(&wires, &w.junction_positions())
        + count_shorts(env, w, items, inc, &wires)
        + count_foreign_taps(&wires);
    // Two symbols whose bodies collide is never acceptable; a heavy (but
    // below-merge) wall lets the climb escape an overlapping seed yet never move
    // INTO an overlap, so the final layout is overlap-free even from a poor frame.
    // Symbol-vs-port-label collisions count here too (the annealer likes to slide
    // a decoupling cap onto the TXD1/RXD1 edge pentagons).
    let label_boxes = w.cluster_label_boxes();
    let overlaps = body_overlap_count(items)
        + items
            .iter()
            .filter(|it| {
                let r = item_rect(it, it.at);
                label_boxes.iter().any(|b| rects_overlap(r, *b))
            })
            .count();
    // Each 2-pin part's BODY AXIS (its pin-to-pin line) joins the closeness check
    // as an obstacle, so "a foreign wire hugging a resistor's body" is the same
    // parallel-proximity test as "a wire hugging a wire" — one rule, no rect math.
    // A series part's own wire lies ON its axis (distance 0) and is ignored.
    let bodies: Vec<([f64; 2], [f64; 2])> = items
        .iter()
        .filter(|i| i.geom.pins.len() == 2)
        .filter_map(|it| {
            let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
            match (w.pin_dirs(env, &it.refdes, n0), w.pin_dirs(env, &it.refdes, n1)) {
                (Ok(d0), Ok(d1)) => match (d0.first(), d1.first()) {
                    (Some((a, _)), Some((b, _))) => Some((*a, *b)),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect();
    let congestion = count_congestion(&w.junction_positions()) + count_close_wires(&wires, &bodies);
    // IC (3+ pin) body interiors: pin-tip bbox shrunk inward past the pin stubs so
    // a wire attaching at a pin tip and routing outward is not a crossing. A
    // foreign wire drawn across the package box IS (the SN74 VCCA→GND-rail riser).
    let ic_rects: Vec<[f64; 4]> = items
        .iter()
        .filter(|it| it.geom.pins.len() >= 3)
        .filter_map(|it| {
            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
            let mut any = false;
            for pg in &it.geom.pins {
                if let Ok(d) = w.pin_dirs(env, &it.refdes, &pg.number) {
                    if let Some((p, _)) = d.first() {
                        lo[0] = lo[0].min(p[0]);
                        lo[1] = lo[1].min(p[1]);
                        hi[0] = hi[0].max(p[0]);
                        hi[1] = hi[1].max(p[1]);
                        any = true;
                    }
                }
            }
            // Shrink 2.0 mm/side: past the pin-stub roots, onto the body rectangle.
            any.then(|| [lo[0] + 2.0, lo[1] + 2.0, hi[0] - 2.0, hi[1] - 2.0])
        })
        .collect();
    let body_cross = count_body_crossings(&bodies, &wires)
        + count_collinear_body_crossings(&bodies, &wires)
        + count_parallel_body_crossings(&bodies, &wires)
        + count_ic_body_crossings(&ic_rects, &wires);
    let stray = count_stray(env, w, items, inc, ir);
    // Orientation convention: a draughtsman runs a 2-pin part VERTICAL when it
    // bridges a rail and an internal node (a pull-up/down, a divider leg, a
    // decoupling cap between two rails), and HORIZONTAL when it sits in the signal
    // flow (between two signals, or feeding a rail from/ to a board port — a series
    // resistor, an input fuse). Penalising the wrong axis stops the router's
    // length-minimisation from flopping a series resistor vertical into an L-jog.
    let mut orient_viol = 0usize;
    let mut leg_viol = 0usize;
    for it in items.iter().filter(|i| i.geom.pins.len() == 2) {
        // Classify by how many of its nets are rails (NOT by port presence — a
        // divider leg like [OUT, GND] touches a port AND a rail yet is still a
        // vertical rail-to-node leg, not a series element):
        //   0 rails → series in the signal flow → horizontal;
        //   2 rails → spans two rails (decoupling) → vertical;
        //   1 rail  → AMBIGUOUS (a pull/leg is vertical, an input fuse feeding the
        //             rail is horizontal) → impose no preference, let length/frame decide.
        let rail_count = it
            .pins
            .iter()
            .filter(|(_, _, n)| n.as_deref().is_some_and(|n| ir.rails.contains_key(n)))
            .count();
        let prefer_vertical: Option<bool> = match rail_count {
            // No rail → a series element in the signal flow → HORIZONTAL. (Tried
            // relaxing this to "let corners decide" so the 555 timing chain
            // DIS→R2→THR could stack vertically — it badly REGRESSED uart, whose
            // series-termination R13/R15/R20 immediately flopped vertical into a
            // tall L-jogged tower. The horizontal prior is load-bearing; keep it.)
            0 => Some(false),
            // Two rails → spans the rails (decoupling) → vertical.
            2 => Some(true),
            // One rail → distinguish a BOARD-EDGE FEED (an input fuse / series part
            // whose non-rail net is a degree-1 port stub, e.g. F1 on 5V_BUS) which
            // runs HORIZONTAL into the rail, from a LEG whose non-rail net is a
            // shared internal node (a divider leg, pull-up — degree ≥2) which hangs
            // VERTICAL. Length/frame alone left F1 vertical; this fixes it without
            // flipping the divider's R8 (its OUT node is degree-3).
            _ => {
                let nonrail = it
                    .pins
                    .iter()
                    .filter_map(|(_, _, n)| n.as_deref())
                    .find(|n| !ir.rails.contains_key(*n));
                let degree = nonrail.and_then(|n| inc.get(n)).map_or(0, |p| p.len());
                Some(degree >= 2)
            }
        };
        let Some(prefer_vertical) = prefer_vertical else { continue };
        let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
        if let (Ok(d0), Ok(d1)) = (w.pin_dirs(env, &it.refdes, n0), w.pin_dirs(env, &it.refdes, n1)) {
            if let (Some((a, _)), Some((b, _))) = (d0.first(), d1.first()) {
                let horizontal = (a[0] - b[0]).abs() > (a[1] - b[1]).abs();
                if prefer_vertical == horizontal {
                    orient_viol += 1;
                    // A 1-rail LEG (pull-up/down: prefer vertical, degree≥2 node) is
                    // the RELIABLE branch — track it apart so the premium boost can
                    // bite it without touching the heuristic 0-rail "series→horizontal"
                    // rule, which the uart's legitimately-vertical 62R terminators trip.
                    if rail_count == 1 {
                        leg_viol += 1;
                    }
                } else if rail_count == 1 && prefer_vertical {
                    // Correctly-VERTICAL 1-rail leg: also enforce the up/down DIRECTION.
                    // The rail pin must sit on its band side — V+ UP (smaller y), GND
                    // DOWN — so the power symbol hangs the right way; a flipped leg (a
                    // +3V3 pull-up with the rail symbol at the BOTTOM) reads upside down.
                    let net_of =
                        |pn: &str| it.pins.iter().find(|(p, _, _)| p == pn).and_then(|(_, _, n)| n.as_deref());
                    let n0_rail = net_of(n0).is_some_and(|n| ir.rails.contains_key(n));
                    let rail = if n0_rail { net_of(n0) } else { net_of(n1) };
                    if let Some(rn) = rail {
                        let (rail_pos, other_pos) = if n0_rail { (a, b) } else { (b, a) };
                        let rail_up = rail_pos[1] < other_pos[1] - EPS;
                        if !is_ground(rn) != rail_up {
                            leg_viol += 1;
                        }
                    }
                }
            }
        }
    }
    // Spine collinearity: two VERTICAL 2-pin legs that share a non-rail node and
    // whose FAR ends are each a rail form a divider / totem-pole spine
    // (VCC→R7→node→R8→GND). A draughtsman draws them in ONE column. Length-min
    // alone slides the shared node sideways toward a port to shave a stub, which
    // breaks the spine (the divider's R8 gets banished to its own column). Penalise
    // a spine pair whose bodies are not in the same column (cross-axis offset > 1
    // grid). Narrow by construction: parallel decoupling caps share RAILS not a
    // node, and a series part with a non-rail far end (555 R2) is not a spine leg,
    // so neither is touched.
    let legs: Vec<(usize, Vec<&str>, bool)> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| it.geom.pins.len() == 2)
        .map(|(i, it)| {
            let nets: Vec<&str> = it.pins.iter().filter_map(|(_, _, n)| n.as_deref()).collect();
            let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
            let vertical = match (w.pin_dirs(env, &it.refdes, n0), w.pin_dirs(env, &it.refdes, n1)) {
                (Ok(d0), Ok(d1)) => match (d0.first(), d1.first()) {
                    (Some((a, _)), Some((b, _))) => (a[1] - b[1]).abs() > (a[0] - b[0]).abs(),
                    _ => false,
                },
                _ => false,
            };
            (i, nets, vertical)
        })
        .collect();
    // Are these two legs a series SPINE? They must share a non-rail node AND each
    // run to a rail, and those two far rails must DIFFER — one pulls the node up
    // (VCC), the other down (GND). Two legs to the SAME rail (R8 and C3 both
    // OUT→GND) are PARALLEL drops, not a spine, and must NOT be forced collinear
    // (they'd overlap). Distinct far rails select exactly the divider/totem case.
    let is_spine = |a: &[&str], b: &[&str]| -> bool {
        let is_rail = |n: &str| ir.rails.contains_key(n);
        let Some(node) = a.iter().copied().find(|n| b.contains(n) && !is_rail(n)) else {
            return false;
        };
        let ra = a.iter().copied().find(|n| *n != node && is_rail(n));
        let rb = b.iter().copied().find(|n| *n != node && is_rail(n));
        matches!((ra, rb), (Some(x), Some(y)) if x != y)
    };
    let mut spine_viol = 0usize;
    for a in 0..legs.len() {
        for b in (a + 1)..legs.len() {
            let (ia, na, va) = (legs[a].0, &legs[a].1, legs[a].2);
            let (ib, nb, vb) = (legs[b].0, &legs[b].1, legs[b].2);
            // A capacitor is a SHUNT tap, never a through-path spine leg: it hangs
            // to the side so the resistive divider / indicator chain reads straight
            // (R7 over R8, not R7 over the filter cap C3). Exclude cap legs.
            let cap = |i: usize| items[i].refdes.starts_with('C');
            // Penalise ANY cross-axis offset, not just >1 grid: a spine should be
            // EXACTLY collinear. The looser >1.27 tolerance let the free per-axis
            // nudge slide a leg one grid off the spine (a visible jog) at no cost.
            if va && vb && !cap(ia) && !cap(ib) && is_spine(na, nb)
                && (items[ia].at[0] - items[ib].at[0]).abs() > EPS
            {
                spine_viol += 1;
            }
        }
    }

    // Compactness: the bounding-box half-perimeter of all part bodies. Length
    // alone rewards short wires but tolerates a part flung into open space if its
    // own wire stays short; this penalises the wasted-whitespace spread directly
    // (the #1 visual complaint), pulling the whole drawing tight.
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for it in items {
        let r = item_rect(it, it.at);
        lo[0] = lo[0].min(r[0]);
        lo[1] = lo[1].min(r[1]);
        hi[0] = hi[0].max(r[2]);
        hi[1] = hi[1].max(r[3]);
    }
    let spread = if lo[0].is_finite() { (hi[0] - lo[0]) + (hi[1] - lo[1]) } else { 0.0 };
    // The author's per-block `layout:` relative ordering. Weighted JUST BELOW the
    // body-overlap wall (so it never forces a collision) but ABOVE every routing /
    // aesthetic term, so the grid is "relatively rigid": the search holds gridded
    // parts in their authored left/right + top/bottom order even when flipping one
    // across its anchor would shave a long wire — exact positions stay free, only
    // the order is held. Empty grid (sidecar / no `layout:`) ⇒ zero, so tuned
    // references are untouched.
    let grid_order = grid_order_viol(items, ir);
    // Merges/shorts are hard correctness failures (a rail-to-rail short lowers
    // length+junctions, so without this the hill-climb would happily create
    // one); fallbacks degrade a wire to a label; then crossings; then CONGESTION
    // (junction dots packed against each other — the "dot knot" / wires-collapse-
    // into-a-resistor look, which length-minimisation otherwise rewards); then
    // junctions and length. The big coefficients keep correctness off the table.
    // Correctness + convention terms (identical for both tiers): a layout that
    // shorts, overlaps, drops a label to a fallback, breaks the authored grid, or
    // runs a wire through a body is wrong regardless of price.
    let correctness = 2000.0 * merges as f64
        + 1500.0 * overlaps as f64
        + 1000.0 * fallbacks as f64
        + 1200.0 * grid_order as f64
        + 30.0 * body_cross as f64
        + 12.0 * orient_viol as f64
        + 10.0 * spine_viol as f64;
    // STRAIGHTNESS / neatness terms — the PREMIUM (paid SA) tier weighs these ~3x
    // to push past the local minimum the free greedy tier accepts: straighter wires
    // (fewer corners/crossings) and less dot-knot congestion. Crucially this does
    // NOT scale the COMPACTNESS terms (spread/stray/length): packing tighter trades
    // against text-collision warnings the routed cost is blind to (it has no text
    // solve), so a premium that squeezed harder would ship a tidier-but-colliding
    // sheet — observed as mixed-signal regressing 0→1. Neatness is safe; tightness
    // is not, until the cost can see the lint's text collisions.
    let neat = if premium { 3.0 } else { 1.0 };
    let base = correctness
        + neat * (5.0 * crossings as f64 + 7.0 * congestion as f64 + 7.0 * corners as f64)
        + 1.0 * junctions as f64
        + 0.5 * stray
        + 0.15 * length
        + 0.45 * spread;
    // MULTI-UNIT COHESION. A multi-unit part's units (op-amp A/B + its V+/V- power unit)
    // share a refdes but NO net, so length-min lets them drift apart — scattering the part
    // and its decoupling across the sheet. Penalise the bounding-box spread of same-refdes
    // items so the units cluster as one IC. ZERO for single-unit parts (every refdes is one
    // item), so `base + 0.0 == base` keeps the free path bit-identical and references
    // unchanged; added OUTSIDE `base` (never re-parenthesising it) per the note above.
    let mut by_refdes: BTreeMap<&str, [f64; 4]> = BTreeMap::new();
    for it in items {
        let e = by_refdes.entry(&it.refdes).or_insert([f64::MAX, f64::MAX, f64::MIN, f64::MIN]);
        e[0] = e[0].min(it.at[0]); e[1] = e[1].min(it.at[1]);
        e[2] = e[2].max(it.at[0]); e[3] = e[3].max(it.at[1]);
    }
    let sib_spread: f64 = by_refdes.values().map(|e| (e[2] - e[0]) + (e[3] - e[1])).sum();
    let multiunit = SIB_COHESION * sib_spread;
    // PREMIUM compaction boost. The neat terms above amplify STRAIGHTNESS ~3x; left
    // unbalanced, the paid SA straightens a wire by flinging its part into open space
    // — the "straight but sprawled" look EVERY visual review flagged as the #1 defect.
    // ADD a matching compaction pull so premium packs as hard as it straightens. This
    // is ADDED, never folded into the base sum: re-parenthesising the base shifts its
    // last bits and flips the chaotic SA acceptances (a measured mixed-signal 0->1
    // regression), so the free path (premium=false) must stay bit-identical. Safe to
    // push hard: on affordable boards premium_score_items scores the REAL per-move
    // warning_count, so an over-tight text/wire collision is rejected mid-search; on
    // big boards the final candidate pick (fewest real warnings, greedy always a
    // candidate) caps it — premium can never ship more warnings than greedy.
    if premium {
        base + multiunit + COMPACT_BOOST * (0.15 * length + 0.45 * spread)
            + ORIENT_BOOST * leg_viol as f64
    } else {
        base + multiunit
    }
    // NB: a premium body-cross BOOST was tried and dropped — on the uart (the only
    // reference that ships crossings) body_xing stayed at 2 from boost 0 to 1000:
    // the SA's move set can't reach a crossing-free layout and the crossings come
    // from the ROUTER drawing through a body, not from placement, so a heavier
    // placement penalty only inflates cost. A real fix belongs in route-around logic.
}

/// Extra PREMIUM-tier weight on orientation violations, on TOP of the shared base
/// (12). A 1-rail leg (pull-up/down) or 2-rail decoupling tap wants to be VERTICAL;
/// in an IC-LESS circuit the base 12 loses to length/spread and the SA ships a
/// HORIZONTAL pull-up, which drags the rail's power-symbol label alongside the part's
/// value text ("10k 3V3" — the collision the user flagged). The paid tier prices the
/// violation hard enough to flip it. Premium-only so the free path stays bit-identical
/// (snapshot unchanged). Bites ONLY rail_count=1 leg violations (`leg_viol`): the
/// references ship 0 of those in their final layouts, and the uart's vertical 62R
/// series terminators are rail_count=0 so they're untouched — verified the uart's
/// premium winner is unchanged at boost 0..200, while a synthetic IC-less pull-up
/// flips horizontal→vertical at 50.
const ORIENT_BOOST: f64 = 50.0;

/// Extra weight the PREMIUM tier puts on compactness (length+spread), on TOP of the
/// base 1x, so the paid SA's straightness pull (`neat`=3x) can't win by spreading
/// parts into open space (the "straight but sprawled" defect every visual review
/// flagged). 2.0 → premium compaction ~3x, matching the straightness amplification.
/// Swept on the four reference fixtures: it tightens the two loosest (555 130→121,
/// uart 165→157 shipped-bbox half-perimeter) with NO warning regression, and stays
/// clear of the over-tight edge (boost 4 destabilises uart). Only the premium branch
/// of `layout_cost` reads it, so the free path stays bit-identical.
const COMPACT_BOOST: f64 = 2.0;

/// Cohesion pull on a multi-unit part's units (same refdes, no shared net): penalises
/// their bounding-box spread so an op-amp's A/B/power units cluster as one IC instead of
/// drifting apart and scattering the part's decoupling. ZERO on single-unit boards (one
/// item per refdes → zero spread), so the free path + all single-unit references stay
/// bit-identical. Both cost tiers read it (clustering is a correctness-of-organisation
/// pull, not a premium nicety).
const SIB_COHESION: f64 = 3.0;

/// Ground truth behind the visual "a wire runs through a part" complaint:
/// `(2-pin transverse + collinear body crossings, IC body crossings)` for a placed
/// item set. Mirrors the obstacle extraction in [`layout_cost`] (kept separate so the
/// hot cost path stays untouched). Surfaced on [`EmitOutput`] (`body_crossings` /
/// `ic_crossings`) — authoritative for grounding a vision critic, which over-reports
/// wire-through-body on correctly-drawn series parts and op-amp triangles.
fn crossing_counts(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> (usize, usize, usize) {
    // Measure the SHIPPED geometry (`fan_risers = true`): the finalize riser jog
    // clears trunk-through-body crossings, so the reported count must reflect the
    // jogged sheet, not the raw per-move one.
    let Ok(w) = build_writer(env, None, items, inc, ir, needs_flag, true) else {
        return (0, 0, 0);
    };
    let wires = w.wires_with_nets();
    let bodies: Vec<([f64; 2], [f64; 2])> = items
        .iter()
        .filter(|i| i.geom.pins.len() == 2)
        .filter_map(|it| {
            let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
            match (w.pin_dirs(env, &it.refdes, n0), w.pin_dirs(env, &it.refdes, n1)) {
                (Ok(d0), Ok(d1)) => match (d0.first(), d1.first()) {
                    (Some((a, _)), Some((b, _))) => Some((*a, *b)),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect();
    let ic_rects: Vec<[f64; 4]> = items
        .iter()
        .filter(|it| it.geom.pins.len() >= 3)
        .filter_map(|it| {
            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
            let mut any = false;
            for pg in &it.geom.pins {
                if let Ok(d) = w.pin_dirs(env, &it.refdes, &pg.number) {
                    if let Some((p, _)) = d.first() {
                        lo[0] = lo[0].min(p[0]);
                        lo[1] = lo[1].min(p[1]);
                        hi[0] = hi[0].max(p[0]);
                        hi[1] = hi[1].max(p[1]);
                        any = true;
                    }
                }
            }
            any.then(|| [lo[0] + 2.0, lo[1] + 2.0, hi[0] - 2.0, hi[1] - 2.0])
        })
        .collect();
    (
        count_body_crossings(&bodies, &wires)
            + count_collinear_body_crossings(&bodies, &wires)
            + count_parallel_body_crossings(&bodies, &wires),
        count_ic_body_crossings(&ic_rects, &wires),
        count_crossings(&wires),
    )
}

/// Violations of the author's per-block `layout:` relative ordering (`ir.grid`).
/// For each pair of gridded parts whose grid boxes are DISJOINT on an axis, the
/// search must hold that order: A strictly left of B (`A.col_max < B.col_min`)
/// requires A's body centre left of B's; A strictly above B (`A.row_max <
/// B.row_min`) requires A above B (smaller y). Boxes that OVERLAP on an axis — a
/// column-span float like a tall IC — impose no constraint on that axis, so the
/// part floats within its span. Empty grid ⇒ 0 (no `layout:` / sidecar path).
fn grid_order_viol(items: &[Item], ir: &LayoutIr) -> usize {
    if ir.grid.is_empty() {
        return 0;
    }
    let pos: BTreeMap<&str, [f64; 2]> = items.iter().map(|it| (it.refdes.as_str(), it.at)).collect();
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
            if (ba[2] < bb[0] && pa[0] >= pb[0] - EPS)
                || (bb[2] < ba[0] && pb[0] >= pa[0] - EPS)
            {
                viol += 1;
            }
            // Rows → above/below (smaller y is higher), only when row-disjoint.
            if (ba[3] < bb[1] && pa[1] >= pb[1] - EPS)
                || (bb[3] < ba[1] && pb[1] >= pa[1] - EPS)
            {
                viol += 1;
            }
        }
    }
    viol
}

/// "Stay near your pin": total Manhattan distance from each satellite (2-pin
/// part) to the centroid of the ANCHOR pins it wires to. A pull-up belongs by the
/// SIGNAL pin it pulls, not the rail, so signal (non-rail) anchor pins are used
/// when present; only a part that touches no signal anchor (a decoupling cap, two
/// rails) falls back to its rail anchor pins (→ the IC power pin). This stops a
/// satellite drifting across the chip to dodge a spacing penalty. A part with no
/// anchor pin at all (e.g. an IC-less divider) contributes nothing.
fn count_stray(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
) -> f64 {
    items
        .iter()
        .filter(|s| s.geom.pins.len() < 3)
        .filter_map(|s| {
            signal_anchor_centroid(env, w, items, inc, ir, s, true)
                .map(|c| (s.at[0] - c[0]).abs() + (s.at[1] - c[1]).abs())
        })
        .sum()
}

/// Centroid of the anchor pins a satellite `s` should sit by: its SIGNAL
/// (non-rail) anchor pins if it has any (a pull-up belongs by the pin it pulls).
/// With `rail_fallback`, a part touching no signal anchor (a decoupling cap, two
/// rails) falls back to its rail anchor pins (→ the IC power pin) — wanted for
/// the gentle stray pull, but NOT for hard pin-alignment (which would snap every
/// decoupling cap onto one power pin and cram them). `None` if no anchor applies.
fn signal_anchor_centroid(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    s: &Item,
    rail_fallback: bool,
) -> Option<[f64; 2]> {
    let is_anchor = |i: usize| items[i].geom.pins.len() >= 3;
    let collect = |rails: bool| -> ([f64; 2], f64) {
        let (mut sum, mut cnt) = ([0.0f64, 0.0f64], 0.0f64);
        for (_, _, net) in &s.pins {
            let Some(net) = net else { continue };
            if ir.rails.contains_key(net) != rails {
                continue;
            }
            for (j, num) in inc.get(net).into_iter().flatten() {
                if is_anchor(*j) {
                    if let Ok(eps) = w.pin_dirs(env, &items[*j].refdes, num) {
                        for (p, _) in &eps {
                            sum[0] += p[0];
                            sum[1] += p[1];
                            cnt += 1.0;
                        }
                    }
                }
            }
        }
        (sum, cnt)
    };
    let (sum, cnt) = match collect(false) {
        (_, 0.0) if rail_fallback => collect(true),
        signal => signal,
    };
    (cnt > 0.0).then(|| [sum[0] / cnt, sum[1] / cnt])
}

/// The position of the IC supply pin a decoupling cap bypasses, to hug it. The
/// cap's non-ground rail net (its V+ side) names the supply; among the IC pins on
/// that net, pick the one nearest the cap so it slides to the closest supply pin
/// (the relevant IC when several share the rail). `None` if the cap touches no
/// non-ground rail with an IC pin (e.g. a pure rail-to-rail divider leg, left to
/// the rail spread).
fn supply_pin_target(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    s: &Item,
) -> Option<[f64; 2]> {
    let mut best: Option<([f64; 2], f64)> = None;
    for (_, _, net) in &s.pins {
        let Some(net) = net else { continue };
        // V+ side only: the rail that is NOT ground (a GND-hung cap aligns by its
        // supply pin, not its ground return).
        if !ir.rails.contains_key(net) || is_ground(net) {
            continue;
        }
        for (j, num) in inc.get(net).into_iter().flatten() {
            if items[*j].geom.pins.len() < 3 {
                continue; // only IC/connector pins anchor a cap
            }
            if let Ok(eps) = w.pin_dirs(env, &items[*j].refdes, num) {
                for (p, _) in eps {
                    let d = (p[0] - s.at[0]).abs() + (p[1] - s.at[1]).abs();
                    if best.map_or(true, |(_, bd)| d < bd) {
                        best = Some((p, d));
                    }
                }
            }
        }
    }
    best.map(|(p, _)| p)
}

/// Two parallel axis-aligned segments running too close for a sustained length —
/// nearly on top of each other, which reads as cramped. Returns true past the
/// per-call `near` cutoff: wire-vs-wire uses 1 grid (a 2-grid gap, e.g. risers
/// off adjacent IC pins, is fine), but wire-vs-body uses a wider cutoff because a
/// part's body has width, so a wire hugging the *edge* sits ~2 grid off the
/// pin-to-pin *centre line*.
fn parallel_too_close(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2], near: f64) -> bool {
    const MIN_OVERLAP: f64 = 6.35; // only a sustained parallel run reads as cramped
    let horiz = |a: &[f64; 2], b: &[f64; 2]| (a[1] - b[1]).abs() < EPS;
    let vert = |a: &[f64; 2], b: &[f64; 2]| (a[0] - b[0]).abs() < EPS;
    let (perp, lo, hi) = if horiz(&a1, &a2) && horiz(&b1, &b2) {
        (
            (a1[1] - b1[1]).abs(),
            a1[0].min(a2[0]).max(b1[0].min(b2[0])),
            a1[0].max(a2[0]).min(b1[0].max(b2[0])),
        )
    } else if vert(&a1, &a2) && vert(&b1, &b2) {
        (
            (a1[0] - b1[0]).abs(),
            a1[1].min(a2[1]).max(b1[1].min(b2[1])),
            a1[1].max(a2[1]).min(b1[1].max(b2[1])),
        )
    } else {
        return false;
    };
    perp > EPS && perp < near - EPS && hi - lo > MIN_OVERLAP
}

/// Cramped-spacing count: parallel wires hugging each other (1-grid cutoff) AND
/// wires hugging a 2-pin part's body axis (wider 1.5-grid cutoff — see
/// [`parallel_too_close`]). One rule covers wire-vs-wire and wire-vs-body.
fn count_close_wires(
    wires: &[([f64; 2], [f64; 2], Option<String>)],
    bodies: &[([f64; 2], [f64; 2])],
) -> usize {
    const NEAR_WIRE: f64 = 2.54; // wires closer than 2 grid (i.e. 1 grid) are too close
    const NEAR_BODY: f64 = 3.81; // a body's width pushes the hug ~1 grid further off centre
    let mut n = 0;
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let (a1, a2, _) = wires[i];
            let (b1, b2, _) = wires[j];
            if parallel_too_close(a1, a2, b1, b2, NEAR_WIRE) {
                n += 1;
            }
        }
    }
    for (a1, a2, _) in wires {
        for (b1, b2) in bodies {
            if parallel_too_close(*a1, *a2, *b1, *b2, NEAR_BODY) {
                n += 1;
            }
        }
    }
    n
}

/// Congestion: pairs of junction dots crammed within `TIGHT` mm of each other —
/// the cramped node a human would spread out (e.g. a pull-up's tap landing right
/// on a series resistor's pin). Unavoidable IC-pin-spacing pairs add a constant
/// baseline that does not bias the search; only the avoidable cramming varies.
fn count_congestion(junctions: &[[f64; 2]]) -> usize {
    const TIGHT: f64 = 3.81;
    let mut n = 0;
    for i in 0..junctions.len() {
        for j in (i + 1)..junctions.len() {
            let (dx, dy) = (junctions[i][0] - junctions[j][0], junctions[i][1] - junctions[j][1]);
            if dx.hypot(dy) < TIGHT - EPS {
                n += 1;
            }
        }
    }
    n
}

/// Net merges KiCAD would actually make: two DIFFERENT-net wires that (a)
/// collinear-overlap, or (b) both pass through a junction dot. KiCAD does NOT
/// fuse a wire end (or pin) landing on another wire's interior without a
/// junction, so — unlike the router's stricter `segments_conflict` — those near
/// misses are excluded here, else the scorer chases phantom shorts on a layout
/// ERC calls clean.
fn count_merges(
    wires: &[([f64; 2], [f64; 2], Option<String>)],
    junctions: &[[f64; 2]],
) -> usize {
    let mut n = 0;
    // (a) Collinear overlaps.
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let (a1, a2, an) = &wires[i];
            let (b1, b2, bn) = &wires[j];
            if an == bn {
                continue;
            }
            if collinear_overlap(*a1, *a2, *b1, *b2) {
                n += 1;
            }
        }
    }
    // (b) Junctions touching more than one net (a junction fuses every wire
    // through it — if those carry different nets, that is a real short).
    for &jp in junctions {
        let mut nets: BTreeSet<&str> = BTreeSet::new();
        for (a, b, wn) in wires {
            if let Some(net) = wn {
                if crate::emit::point_on_segment(jp, *a, *b) {
                    nets.insert(net.as_str());
                }
            }
        }
        if nets.len() > 1 {
            n += 1;
        }
    }
    n
}

/// Two axis-aligned segments that lie on the same line and overlap (KiCAD fuses
/// these). Endpoint-only touches of perpendicular segments are NOT included.
fn collinear_overlap(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2]) -> bool {
    let a_h = (a1[1] - a2[1]).abs() < EPS;
    let b_h = (b1[1] - b2[1]).abs() < EPS;
    let a_v = (a1[0] - a2[0]).abs() < EPS;
    let b_v = (b1[0] - b2[0]).abs() < EPS;
    if a_h && b_h && (a1[1] - b1[1]).abs() < EPS {
        let (alo, ahi) = (a1[0].min(a2[0]), a1[0].max(a2[0]));
        let (blo, bhi) = (b1[0].min(b2[0]), b1[0].max(b2[0]));
        alo < bhi - EPS && blo < ahi - EPS
    } else if a_v && b_v && (a1[0] - b1[0]).abs() < EPS {
        let (alo, ahi) = (a1[1].min(a2[1]), a1[1].max(a2[1]));
        let (blo, bhi) = (b1[1].min(b2[1]), b1[1].max(b2[1]));
        alo < bhi - EPS && blo < ahi - EPS
    } else {
        false
    }
}

/// Visual wire crossings: pairs of different-net segments, one horizontal and
/// one vertical, intersecting at a point interior to both (KiCAD draws no
/// junction there — the wires just cross over).
fn count_crossings(wires: &[([f64; 2], [f64; 2], Option<String>)]) -> usize {
    let horiz = |a: &[f64; 2], b: &[f64; 2]| (a[1] - b[1]).abs() < EPS;
    let vert = |a: &[f64; 2], b: &[f64; 2]| (a[0] - b[0]).abs() < EPS;
    let interior = |v: f64, lo: f64, hi: f64| v > lo + EPS && v < hi - EPS;
    let mut n = 0;
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let (a1, a2, an) = &wires[i];
            let (b1, b2, bn) = &wires[j];
            if an == bn {
                continue; // same net: a deliberate join, not a crossing
            }
            let (h, v) = if horiz(a1, a2) && vert(b1, b2) {
                ((a1, a2), (b1, b2))
            } else if vert(a1, a2) && horiz(b1, b2) {
                ((b1, b2), (a1, a2))
            } else {
                continue; // parallel (collinear overlap is a same/foreign issue, not a crossing)
            };
            let (hy, vx) = (h.0[1], v.0[0]);
            let (hx_lo, hx_hi) = (h.0[0].min(h.1[0]), h.0[0].max(h.1[0]));
            let (vy_lo, vy_hi) = (v.0[1].min(v.1[1]), v.0[1].max(v.1[1]));
            if interior(vx, hx_lo, hx_hi) && interior(hy, vy_lo, vy_hi) {
                n += 1;
            }
        }
    }
    n
}

/// Placement shorts: a pin whose connection point coincides exactly with the
/// ENDPOINT of a different net's wire (two wire/pin terminals at one point fuse
/// in KiCAD). A pin merely sitting on a wire's interior is NOT a connection
/// without a junction, so — matching `count_merges` — those are excluded.
fn count_shorts(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    wires: &[([f64; 2], [f64; 2], Option<String>)],
) -> usize {
    let mut n = 0;
    for (net, pins) in inc {
        for (i, num) in pins {
            let Ok(eps) = w.pin_dirs(env, &items[*i].refdes, num) else { continue };
            for (ep, _) in eps {
                for (a, b, wn) in wires {
                    if wn.as_deref() == Some(net.as_str()) {
                        continue; // own net
                    }
                    // A pin coinciding with a foreign wire's endpoint, OR landing
                    // on its interior (KiCAD connects a pin to a wire it touches),
                    // is a short on a different net.
                    if near(ep, *a) || near(ep, *b) || crate::emit::point_on_segment(ep, *a, *b) {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}

fn normalize(items: &mut [Item]) {
    let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
    for it in items.iter() {
        min_x = min_x.min(it.at[0]);
        min_y = min_y.min(it.at[1]);
    }
    if !min_x.is_finite() {
        return;
    }
    let dx = MARGIN - min_x;
    let dy = MARGIN - min_y;
    for it in items.iter_mut() {
        it.at = [it.at[0] + dx, it.at[1] + dy];
    }
}

// ---------------------------------------------------------------------------
// Wiring: rails, signal routing, ports.
// ---------------------------------------------------------------------------

fn wire(
    env: &KicadEnv,
    w: &mut SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    flag_points: &mut BTreeMap<String, ([f64; 2], f64)>,
    fan_risers: bool,
) -> io::Result<()> {
    let refdes_of = |i: usize| items[i].refdes.clone();
    // Auto-distributing a spread rail into local power symbols only applies to LARGER
    // boards (`pins > FAST_PINS`). Every reference/snapshot fixture (≤34 pins) keeps
    // its tuned short trunk even when its GND rail happens to span the sheet width, so
    // those greedy renders stay byte-identical. The author opt-in (`rail_locals`)
    // still works on any board.
    let pin_total: usize = items.iter().map(|it| it.geom.pins.len()).sum();

    // Endpoints of every net first, so all rails can share common bands.
    let mut net_eps: BTreeMap<String, Vec<([f64; 2], Dir)>> = BTreeMap::new();
    for (net, pins) in inc {
        let mut eps: Vec<([f64; 2], Dir)> = Vec::new();
        for (i, num) in pins {
            for (ep, dir) in w.pin_dirs(env, &refdes_of(*i), num)? {
                eps.push((ep, dir));
            }
        }
        if !eps.is_empty() {
            net_eps.insert(net.clone(), eps);
        }
    }

    // Rail y per net. Rails in a band share a base y so they align, but two
    // rails whose x-ranges OVERLAP (e.g. VCC3V3 and VCCD flanking one IC) must
    // sit on different rows or their wires would merge into one net. Assign
    // y-levels by greedy interval colouring.
    let rail_y_map = assign_rail_levels(&net_eps, ir);

    // Fan colliding rail risers off shared columns so two rails never merge into
    // one net (the stacked-BGA-balls GND/1V2 short). Finalize-only: the per-move
    // scorer passes `fan_risers = false` so transient mid-search collisions never
    // perturb the placement.
    let riser_offsets =
        if fan_risers { plan_riser_offsets(&net_eps, ir, &rail_y_map) } else { BTreeMap::new() };

    // 2-pin body segments (finalize-only, so the per-move scorer is untouched) so a
    // rail riser can JOG around a part body it would otherwise be drawn straight
    // through — the stacked same-rail cap column the SA can't always pull apart.
    let bodies: Vec<([f64; 2], [f64; 2])> = if fan_risers {
        items
            .iter()
            .filter(|it| it.geom.pins.len() == 2)
            .filter_map(|it| {
                let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
                match (w.pin_dirs(env, &it.refdes, n0), w.pin_dirs(env, &it.refdes, n1)) {
                    (Ok(d0), Ok(d1)) => match (d0.first(), d1.first()) {
                        (Some((a, _)), Some((b, _))) => Some((*a, *b)),
                        _ => None,
                    },
                    _ => None,
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    // Phase A — rails (shared wires + stubs + power symbols), so their wires are
    // in the writer before we build the routing scene.
    for (net, eps) in &net_eps {
        if let Some(band) = ir.rails.get(net) {
            let flag = needs_flag.contains(net).then_some(&mut *flag_points);
            // Draw DISTRIBUTED local power symbols (`rail_y = None` ⇒ one power symbol
            // per pin) when either the author marked the net (≥2 placed power symbols)
            // OR the net's pins are spread far enough that a single spanning trunk
            // would be a long cross-sheet detour with a knot of converging risers —
            // the professional idiom on a multi-module board, and the fix for the
            // recurring "scattered caps / congested rail knot / long detour rails"
            // critic complaints. Tight/small rails (every reference fixture) stay
            // under the span gate and keep their clean short trunk → byte-identical.
            let distribute = ir.rail_locals.contains(net)
                || (pin_total > FAST_PINS && rail_should_distribute(eps));
            let rail_y = rail_y_map.get(net).copied().filter(|_| !distribute);
            emit_rail(env, w, net, eps, *band, rail_y, flag, &riser_offsets, &bodies)?;
        }
    }

    // Phase B — the routing scene: component bodies become obstacles, rail wires
    // become foreign segments, and EVERY signal net's pins become foreign points
    // so one net's wire can never run onto another's pin (which would merge them
    // — the old TXD1/RXD1 short).
    let mut scene = w.route_scene();
    for (net, eps) in &net_eps {
        if ir.rails.contains_key(net) {
            continue;
        }
        for (p, _) in eps {
            scene.points.push((*p, net.clone()));
        }
        // Reserve each port's pennant box up front so a LATER net's wire routes
        // around it instead of straight through someone else's edge tag. The label
        // is added during Phase C — too late to obstruct nets routed before it.
        if let Some(side) = effective_port_side(ir.ports.get(net).copied(), eps) {
            let at = port_exit_point(eps, side);
            scene.label_solids.push((port_label_obstacle(at, side, net), net.clone()));
        }
    }

    // Phase C — route every signal/port net with the direction-aware,
    // obstacle-avoiding elbow router so wires leave pins along their facing
    // direction and detour around bodies (never through them).
    for (net, eps) in &net_eps {
        if ir.rails.contains_key(net) {
            continue;
        }
        route_signal(env, w, items, inc, net, eps, ir.ports.get(net).copied(), &mut scene)?;
    }
    Ok(())
}

/// Route one signal/port net's terminals as a tree (MST) with the direction-
/// aware elbow router. A port adds a virtual terminal just past the net's extent
/// on the named side, then a label there; failure falls back to per-pin labels.
fn route_signal(
    env: &KicadEnv,
    w: &mut SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    net: &str,
    eps: &[([f64; 2], Dir)],
    port: Option<Side>,
    scene: &mut crate::route::RouteScene,
) -> io::Result<()> {
    // A single-pin port follows its pin's real direction (see effective_port_side):
    // a MOSFET gate faces left but the name heuristic would exit it right, onto the
    // body. Multi-pin marked ports keep the name-inferred side.
    let port = effective_port_side(port, eps);

    // Terminals: real pins (with outward dir) + an optional virtual port exit.
    let mut terms: Vec<([f64; 2], Option<Dir>)> = eps.iter().map(|(p, d)| (*p, Some(*d))).collect();
    let port_idx = port.map(|side| {
        terms.push((port_exit_point(eps, side), None));
        terms.len() - 1
    });

    if terms.len() < 2 {
        // A lone pin with no port is an intentionally-unconnected signal (e.g. an
        // unused connector RTS/CTS): mark it no-connect — the professional way to
        // show "deliberately dangling" — rather than leaving a floating named
        // label that ERC flags as an isolated pin.
        if let Some((i, num)) = inc.get(net).and_then(|p| p.first()) {
            w.add_no_connect(env, &items[*i].refdes, num)?;
        }
        return Ok(());
    }

    // A LOCAL node — terminals clustered with no component body between them —
    // is drawn as one clean trunk + stubs (a tee), not an MST of independent
    // elbows whose overlapping collinear runs over-junction the node.
    if route_local_tee(w, net, &terms, scene) {
        if let (Some(side), Some(pi)) = (port, port_idx) {
            w.add_cluster_label(net, terms[pi].0, side_dir(side), true);
        }
        return Ok(());
    }

    // Map each real-pin terminal (the first `eps.len()`) back to its (item, pin),
    // rebuilt in the same order `wire()` flattened `inc[net]` into `eps`, so a
    // disconnected component can be bridged with a net label on one of its pins.
    let mut term_pin: Vec<Option<(usize, String)>> = vec![None; terms.len()];
    {
        let mut k = 0;
        for (i, num) in inc.get(net).into_iter().flatten() {
            if let Ok(ds) = w.pin_dirs(env, &items[*i].refdes, num) {
                for _ in ds {
                    if k < eps.len() {
                        term_pin[k] = Some((*i, num.clone()));
                        k += 1;
                    }
                }
            }
        }
    }

    let pts: Vec<[f64; 2]> = terms.iter().map(|t| t.0).collect();
    // Union-find over terminals: a successful edge merges its endpoints; a failed
    // one leaves them split. Route each edge as it succeeds and commit it to the
    // scene immediately so later edges detour around it (partial progress, never
    // the old all-or-nothing that label-bombed the whole net on one bad edge).
    let mut parent: Vec<usize> = (0..terms.len()).collect();
    fn find(parent: &mut [usize], x: usize) -> usize {
        let mut r = x;
        while parent[r] != r {
            r = parent[r];
        }
        let mut c = x;
        while parent[c] != r {
            let n = parent[c];
            parent[c] = r;
            c = n;
        }
        r
    }
    let mut paths: Vec<crate::route::Path> = Vec::new();
    for (i, j) in crate::route::mst_edges(&pts) {
        let (a, da, b) = match (terms[i].1, terms[j].1) {
            (Some(d), _) => (pts[i], d, pts[j]),
            (None, Some(d)) => (pts[j], d, pts[i]),
            (None, None) => (pts[i], dir_toward(pts[i], pts[j]), pts[j]),
        };
        if let Some(p) = crate::route::route_edge(a, da, b, net, scene) {
            for seg in p.windows(2) {
                w.add_wire_on_net(seg[0], seg[1], net);
                scene.segments.push((seg[0], seg[1], net.to_string()));
            }
            paths.push(p);
            let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
            parent[ri] = rj;
        }
    }

    // Bridge connected components by net name: every component must carry the net
    // somewhere. A component holding the port exit is named by the port label; any
    // other component gets one net label on a real pin. With one component the net
    // is fully wired and no label is emitted.
    let mut roots: BTreeMap<usize, Option<(usize, String)>> = BTreeMap::new();
    for k in 0..terms.len() {
        let r = find(&mut parent, k);
        let slot = roots.entry(r).or_insert(None);
        if slot.is_none() {
            if let Some(pin) = &term_pin[k] {
                *slot = Some(pin.clone());
            }
        }
    }
    let port_root = port_idx.map(|pi| find(&mut parent, pi));
    if roots.len() > 1 {
        for (root, pin) in &roots {
            if Some(*root) == port_root {
                continue; // named by the port label below
            }
            if let Some((i, num)) = pin {
                w.add_signal_label(env, &items[*i].refdes, num, net)?;
                if let Ok(ds) = w.pin_dirs(env, &items[*i].refdes, num) {
                    for (p, _) in ds {
                        scene.points.push((p, net.to_string()));
                    }
                }
            }
        }
    }

    // Junction dots: 3-way meets among the routed paths.
    let mut all = paths.clone();
    for (a, b) in w.wire_segments_on_net(net) {
        all.push(vec![a, b]);
    }
    for j in crate::route::junction_points(&all) {
        w.add_junction(j);
    }
    // A terminal landing inside another same-net segment is a T-join.
    for (p, _) in &terms {
        let interior = w.wire_segments_on_net(net).iter().any(|(a, b)| {
            let ends = near(*p, *a) || near(*p, *b);
            !ends && crate::emit::point_on_segment(*p, *a, *b)
        });
        if interior {
            w.add_junction(*p);
        }
    }
    // The port label sits at the virtual exit terminal, facing the edge.
    if let (Some(side), Some(pi)) = (port, port_idx) {
        w.add_cluster_label(net, terms[pi].0, side_dir(side), true);
    }
    Ok(())
}

/// Draw a clustered net as a single-trunk tee (one straight trunk + a short
/// stub from each terminal), returning true if it applied. Used when the
/// terminals are close together AND no component body sits between them, so a
/// trunk is safe — far cleaner than an MST of overlapping elbows. Spread or
/// obstacle-crossing nets return false and fall through to the router.
fn route_local_tee(
    w: &mut SchematicWriter,
    net: &str,
    terms: &[([f64; 2], Option<Dir>)],
    scene: &mut crate::route::RouteScene,
) -> bool {
    const LOCAL: f64 = 30.48;
    let xs: Vec<f64> = terms.iter().map(|t| t.0[0]).collect();
    let ys: Vec<f64> = terms.iter().map(|t| t.0[1]).collect();
    let (min_x, max_x) = (xs.iter().cloned().fold(f64::MAX, f64::min), xs.iter().cloned().fold(f64::MIN, f64::max));
    let (min_y, max_y) = (ys.iter().cloned().fold(f64::MAX, f64::min), ys.iter().cloned().fold(f64::MIN, f64::max));
    if max_x - min_x > LOCAL || max_y - min_y > LOCAL {
        return false;
    }
    // A body strictly inside the terminal bbox would be cut by the trunk.
    let bbox = [min_x, min_y, max_x, max_y];
    let hits_body = scene.solids.iter().any(|r| {
        r[0] < bbox[2] - EPS && bbox[0] < r[2] - EPS && r[1] < bbox[3] - EPS && bbox[1] < r[3] - EPS
    });
    if hits_body {
        return false;
    }
    // Trunk along the longer axis, on the (lower-)median terminal line so the
    // most terminals sit on it without a stub.
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[(v.len() - 1) / 2]
    };
    let horizontal = (max_x - min_x) >= (max_y - min_y);
    if horizontal {
        let ty = crate::grid::snap(median(ys));
        w.add_wire_on_net([min_x, ty], [max_x, ty], net);
        scene.segments.push(([min_x, ty], [max_x, ty], net.to_string()));
        for (p, _) in terms {
            if (p[1] - ty).abs() > EPS {
                w.add_wire_on_net(*p, [p[0], ty], net);
            }
            if p[0] > min_x + EPS && p[0] < max_x - EPS {
                w.add_junction([p[0], ty]);
            }
        }
    } else {
        let tx = crate::grid::snap(median(xs));
        w.add_wire_on_net([tx, min_y], [tx, max_y], net);
        scene.segments.push(([tx, min_y], [tx, max_y], net.to_string()));
        for (p, _) in terms {
            if (p[0] - tx).abs() > EPS {
                w.add_wire_on_net(*p, [tx, p[1]], net);
            }
            if p[1] > min_y + EPS && p[1] < max_y - EPS {
                w.add_junction([tx, p[1]]);
            }
        }
    }
    true
}

/// A virtual port-exit point just past the net's pin extent on `side`.
fn port_exit_point(eps: &[([f64; 2], Dir)], side: Side) -> [f64; 2] {
    // A single-pin port (a gate / divider tap) needs only a short stub to seat its
    // pennant clear of its own body; a long one would push the pennant into the
    // NEXT symbol in a packed row (an h-bridge's four FETs at minimum pitch). A
    // multi-pin port exits past the whole net's extent, so it keeps the longer
    // reach to clear the last pin.
    let reach: f64 = if eps.len() == 1 { 2.54 } else { 7.62 };
    let xs: Vec<f64> = eps.iter().map(|(p, _)| p[0]).collect();
    let ys: Vec<f64> = eps.iter().map(|(p, _)| p[1]).collect();
    let (min_x, max_x) = (xs.iter().cloned().fold(f64::MAX, f64::min), xs.iter().cloned().fold(f64::MIN, f64::max));
    let (min_y, max_y) = (ys.iter().cloned().fold(f64::MAX, f64::min), ys.iter().cloned().fold(f64::MIN, f64::max));
    // Align the exit with the pin nearest that edge so the wire runs straight.
    match side {
        Side::Right => {
            let y = eps.iter().max_by(|a, b| a.0[0].total_cmp(&b.0[0])).map(|t| t.0[1]).unwrap_or(min_y);
            [crate::grid::snap(max_x + reach), y]
        }
        Side::Left => {
            let y = eps.iter().min_by(|a, b| a.0[0].total_cmp(&b.0[0])).map(|t| t.0[1]).unwrap_or(min_y);
            [crate::grid::snap(min_x - reach), y]
        }
        Side::Top => {
            let x = eps.iter().min_by(|a, b| a.0[1].total_cmp(&b.0[1])).map(|t| t.0[0]).unwrap_or(min_x);
            [x, crate::grid::snap(min_y - reach)]
        }
        Side::Bottom => {
            let x = eps.iter().max_by(|a, b| a.0[1].total_cmp(&b.0[1])).map(|t| t.0[0]).unwrap_or(max_x);
            [x, crate::grid::snap(max_y + reach)]
        }
    }
}

fn side_dir(side: Side) -> Dir {
    match side {
        Side::Right => Dir::East,
        Side::Left => Dir::West,
        Side::Top => Dir::North,
        Side::Bottom => Dir::South,
    }
}

/// The sheet edge a pin facing `dir` exits toward — the inverse of [`side_dir`].
/// Used so a single-pin port's exit follows the pin's real orientation.
fn dir_to_side(dir: Dir) -> Side {
    match dir {
        Dir::East => Side::Right,
        Dir::West => Side::Left,
        Dir::North => Side::Top,
        Dir::South => Side::Bottom,
    }
}

/// The sheet edge a port net actually exits toward. A SINGLE-pin port follows its
/// pin's real direction (geometry beats the name heuristic that picks Left/Right
/// from the net name); a multi-pin port keeps the name-inferred side from `ir.ports`.
fn effective_port_side(port: Option<Side>, eps: &[([f64; 2], Dir)]) -> Option<Side> {
    match port {
        Some(_) if eps.len() == 1 => Some(dir_to_side(eps[0].1)),
        other => other,
    }
}

/// The box a port pennant occupies, for the router to keep FOREIGN wires out of it
/// (a wire drawn across someone else's edge tag). Directional: the pennant + text
/// extend OUTWARD from the exit anchor along `side`; `BACK` covers the connecting
/// vertex that reaches slightly back toward the wire. `HALF` is the text half-height.
fn port_label_obstacle(at: [f64; 2], side: Side, net: &str) -> [f64; 4] {
    let w = crate::textplace::text_width(net) + 2.54;
    const BACK: f64 = 1.27;
    const HALF: f64 = 2.0;
    match side {
        Side::Left => [at[0] - w, at[1] - HALF, at[0] + BACK, at[1] + HALF],
        Side::Right => [at[0] - BACK, at[1] - HALF, at[0] + w, at[1] + HALF],
        // Top/Bottom pennants render rotated: text runs along y, so the long extent
        // is vertical and the cross-extent is the text height.
        Side::Top => [at[0] - HALF, at[1] - w, at[0] + HALF, at[1] + BACK],
        Side::Bottom => [at[0] - HALF, at[1] - BACK, at[0] + HALF, at[1] + w],
    }
}

/// A coarse Manhattan direction from `a` toward `b`.
fn dir_toward(a: [f64; 2], b: [f64; 2]) -> Dir {
    if (b[0] - a[0]).abs() >= (b[1] - a[1]).abs() {
        if b[0] >= a[0] { Dir::East } else { Dir::West }
    } else if b[1] >= a[1] {
        Dir::South
    } else {
        Dir::North
    }
}

fn near(p: [f64; 2], q: [f64; 2]) -> bool {
    (p[0] - q[0]).abs() < 1e-6 && (p[1] - q[1]).abs() < 1e-6
}

/// Assign each drawn rail (≥3 pins) a y. Rails in a band share a base y, but
/// overlapping x-ranges are pushed to successive rows (away from the content)
/// via greedy interval colouring, so distinct rails never merge into one wire.
/// Half-perimeter span (mm) of a rail net above which a single spanning trunk is a
/// long cross-sheet detour and the net is better drawn as distributed local power
/// symbols. ~30 grid cells; every reference/snapshot fixture's rails span far less
/// (≤34-pin compact boards), so they keep their trunk and stay byte-identical.
const RAIL_DISTRIBUTE_SPAN: f64 = 76.0;

/// Whether a rail net's pins are spread far enough to prefer DISTRIBUTED local power
/// symbols over one spanning trunk (see [`RAIL_DISTRIBUTE_SPAN`]). A net with <3 pins
/// already draws per-pin symbols, so it's irrelevant there.
fn rail_should_distribute(eps: &[([f64; 2], Dir)]) -> bool {
    if eps.len() < 3 {
        return false;
    }
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for (p, _) in eps {
        lo[0] = lo[0].min(p[0]);
        lo[1] = lo[1].min(p[1]);
        hi[0] = hi[0].max(p[0]);
        hi[1] = hi[1].max(p[1]);
    }
    (hi[0] - lo[0]) + (hi[1] - lo[1]) > RAIL_DISTRIBUTE_SPAN
}

fn assign_rail_levels(
    net_eps: &BTreeMap<String, Vec<([f64; 2], Dir)>>,
    ir: &LayoutIr,
) -> BTreeMap<String, f64> {
    const RAIL_GAP: f64 = 6.35;
    let mut out = BTreeMap::new();
    for band in [Band::Top, Band::Bottom] {
        // (net, min_x, max_x), only rails actually drawn as a wire.
        let mut rails: Vec<(String, f64, f64)> = ir
            .rails
            .iter()
            .filter(|(_, b)| **b == band)
            .filter_map(|(n, _)| {
                let e = net_eps.get(n)?;
                if e.len() < 3 {
                    return None;
                }
                let min_x = e.iter().map(|(p, _)| p[0]).fold(f64::MAX, f64::min);
                let max_x = e.iter().map(|(p, _)| p[0]).fold(f64::MIN, f64::max);
                Some((n.clone(), min_x, max_x))
            })
            .collect();
        if rails.is_empty() {
            continue;
        }
        rails.sort_by(|a, b| a.1.total_cmp(&b.1));
        // Base y: the band edge across all these rails' pins.
        let ys = rails.iter().filter_map(|(n, _, _)| net_eps.get(n)).flatten().map(|(p, _)| p[1]);
        let base = match band {
            Band::Top => ys.fold(f64::MAX, f64::min) - 5.08,
            Band::Bottom => ys.fold(f64::MIN, f64::max) + 5.08,
        };
        // Greedy interval colouring: level = first row with no x-overlap.
        let mut levels: Vec<Vec<(f64, f64)>> = Vec::new();
        for (net, lo, hi) in rails {
            let mut placed = false;
            for (lvl, occ) in levels.iter_mut().enumerate() {
                if occ.iter().all(|&(a, b)| hi < a - EPS || lo > b + EPS) {
                    occ.push((lo, hi));
                    let y = base + lvl as f64 * RAIL_GAP * if band == Band::Top { -1.0 } else { 1.0 };
                    out.insert(net.clone(), y);
                    placed = true;
                    break;
                }
            }
            if !placed {
                let lvl = levels.len();
                levels.push(vec![(lo, hi)]);
                let y = base + lvl as f64 * RAIL_GAP * if band == Band::Top { -1.0 } else { 1.0 };
                out.insert(net, y);
            }
        }
    }
    out
}

const EPS: f64 = 1e-6;

/// A side (E/W) pin leads OUTWARD this far before its riser climbs to the rail,
/// so the riser never runs up the IC edge past the other pins on that side.
const RAIL_LEAD: f64 = 2.54;

/// Lane width used to fan colliding rail risers off a shared column. Half the
/// 2.54 BGA pitch, so an offset riser sits in the gutter between two ball columns
/// rather than landing on a neighbouring pin.
const RAIL_LANE: f64 = 1.27;

/// The x a pin's vertical riser sits at, before any anti-collision offset: side
/// pins lead out, top/bottom pins climb straight up. Must match `emit_rail`.
fn riser_base_x(ep: &[f64; 2], dir: Dir) -> f64 {
    match dir {
        Dir::East => ep[0] + RAIL_LEAD,
        Dir::West => ep[0] - RAIL_LEAD,
        _ => ep[0],
    }
}

fn col_key(x: f64) -> i64 {
    (x * 100.0).round() as i64
}

/// Plan per-net horizontal offsets so two rails whose vertical risers would share
/// a column and overlap in y — a SHORT, e.g. stacked BGA balls GND below / 1V2
/// above whose risers cross in the gap — get fanned into separate columns.
/// Returns `(net, riser_base_column) -> dx`. Risers in an uncontested column get
/// no entry (dx = 0), so simple boards (the references) are untouched.
fn plan_riser_offsets(
    net_eps: &BTreeMap<String, Vec<([f64; 2], Dir)>>,
    ir: &LayoutIr,
    rail_y_map: &BTreeMap<String, f64>,
) -> BTreeMap<(String, i64), f64> {
    // Every drawn riser as (net, riser_x, y_lo, y_hi).
    let mut risers: Vec<(String, f64, f64, f64)> = Vec::new();
    for (net, eps) in net_eps {
        if !ir.rails.contains_key(net) || eps.len() < 3 {
            continue;
        }
        let Some(&ry) = rail_y_map.get(net) else { continue };
        for (p, dir) in eps {
            let x = riser_base_x(p, *dir);
            risers.push((net.clone(), x, p[1].min(ry), p[1].max(ry)));
        }
    }
    // A column is contested only when two DIFFERENT rails' risers are EXACTLY
    // collinear (same x to float tolerance — that's the one geometry KiCAD merges)
    // and overlap in y by more than a point. The exactness matters: mid-search the
    // geometry is continuous mm (pins not yet grid-snapped), so two risers can pass
    // within microns without ever shorting — a coarse bucket would fan those and
    // churn the placement (the references / grid-demo). column_key -> nets to fan.
    let mut contested: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for i in 0..risers.len() {
        for j in (i + 1)..risers.len() {
            let (ref na, xa, loa, hia) = risers[i];
            let (ref nb, xb, lob, hib) = risers[j];
            if na == nb || (xa - xb).abs() > EPS {
                continue;
            }
            if hia < lob + EPS || hib < loa + EPS {
                continue; // no real y-overlap (separated or just touching)
            }
            let nets = contested.entry(col_key(xa)).or_default();
            nets.insert(na.clone());
            nets.insert(nb.clone());
        }
    }
    let mut offsets = BTreeMap::new();
    for (col, nets) in &contested {
        // Fan the contested nets into distinct lanes, deterministic by name:
        // 0 -> +lane, 1 -> -lane, 2 -> +2·lane, 3 -> -2·lane, …
        for (rank, net) in nets.iter().enumerate() {
            let step = (rank / 2 + 1) as f64 * RAIL_LANE;
            let dx = if rank % 2 == 0 { step } else { -step };
            offsets.insert((net.clone(), *col), dx);
        }
    }
    offsets
}

/// Map a net name to its `power:` symbol lib_id (best-effort, KiCAD aliases).
fn power_lib_id(net: &str) -> String {
    let alias = match net.to_ascii_uppercase().as_str() {
        "3V3" | "+3V3" => "+3V3",
        "5V" | "+5V" => "+5V",
        "9V" | "+9V" => "+9V",
        "3.3V" => "+3.3V",
        "12V" | "+12V" => "+12V",
        "VCC" => "VCC",
        "VDD" => "VDD",
        g if g.starts_with("GND") || g.starts_with("VSS") || g == "AGND" || g == "DGND" => "GND",
        // Custom rail (e.g. VCC3V3, VCCD): a generic donor symbol whose Value
        // names the net — KiCAD derives the global net from the Value field.
        _ => "VCC",
    };
    format!("power:{alias}")
}

/// True if a vertical riser drawn at `x` spanning y∈[ylo,yhi] would pass through
/// the CENTRAL body (past the pin stubs) of some 2-pin part — the case where a
/// rail trunk's riser is drawn straight through a cap/resistor it does not connect
/// to (a stacked same-rail cap column threads the upper cap's riser through the
/// lower body; a mis-oriented cap threads its own). Mirrors the parallel/collinear/
/// perpendicular body-crossing detectors so a jog that clears this also clears the
/// counted crossing. Endpoint-only contact (the riser's own pin) is excluded by the
/// PIN_STUB inset on the body span.
fn riser_hits_body(x: f64, ylo: f64, yhi: f64, bodies: &[([f64; 2], [f64; 2])]) -> bool {
    const PLATE_HALF: f64 = 1.4;
    const PIN_STUB: f64 = 2.54;
    for (a, b) in bodies {
        if (a[0] - b[0]).abs() < EPS {
            // Vertical part: a riser collinear/parallel within the plate width that
            // spans the central body.
            if (x - a[0]).abs() >= PLATE_HALF {
                continue;
            }
            let (blo, bhi) = (a[1].min(b[1]) + PIN_STUB, a[1].max(b[1]) - PIN_STUB);
            if bhi > blo + EPS && ylo < bhi - EPS && yhi > blo + EPS {
                return true;
            }
        } else if (a[1] - b[1]).abs() < EPS {
            // Horizontal part: a riser crossing it perpendicular, strictly inside
            // the central span (its own connecting riser lands at a pin END → outside).
            let (xlo, xhi) = (a[0].min(b[0]) + PIN_STUB, a[0].max(b[0]) - PIN_STUB);
            if xhi > xlo + EPS && x > xlo + EPS && x < xhi - EPS && ylo < a[1] - EPS && yhi > a[1] + EPS {
                return true;
            }
        }
    }
    false
}

/// A rail: with ≥3 pins, draw a horizontal wire at `rail_y` spanning them, stub
/// each pin to it, and put one power symbol at the left end. With fewer pins (or
/// no common band), emit a per-pin power symbol instead (the clustered case,
/// e.g. a divider's two GNDs).
fn emit_rail(
    env: &KicadEnv,
    w: &mut SchematicWriter,
    net: &str,
    eps: &[([f64; 2], Dir)],
    band: Band,
    rail_y: Option<f64>,
    flag: Option<&mut BTreeMap<String, ([f64; 2], f64)>>,
    riser_offsets: &BTreeMap<(String, i64), f64>,
    bodies: &[([f64; 2], [f64; 2])],
) -> io::Result<()> {
    let lib = power_lib_id(net);
    let Some(rail_y) = rail_y.filter(|_| eps.len() >= 3) else {
        // One power symbol per pin — but MERGE a pin into a nearby, COLLINEAR
        // already-placed symbol (≤2 grid, same x or y) via a short connecting wire
        // instead of stamping a second symbol. Two adjacent same-net pins (e.g. the
        // 3V3 tops of two I2C pull-ups) otherwise render duplicate side-by-side "3V3"
        // labels (the recurring text-overlap defect). The ≤2-grid limit only fuses an
        // immediate neighbour, never the whole spread (which would recreate the long
        // trunk distribution exists to avoid).
        const MERGE: f64 = 5.08;
        let mut syms: Vec<[f64; 2]> = Vec::new();
        let mut idx = 0usize;
        for (ep, dir) in eps.iter() {
            if let Some(&near) = syms.iter().find(|&&p| {
                let d = (p[0] - ep[0]).abs() + (p[1] - ep[1]).abs();
                d > EPS && d <= MERGE && ((p[0] - ep[0]).abs() < EPS || (p[1] - ep[1]).abs() < EPS)
            }) {
                w.add_wire_on_net(*ep, near, net);
                continue;
            }
            let angle = power_angle(*dir);
            w.add_power_symbol(env, &lib, &format!("#PWR_{net}_{idx}"), net, *ep, angle)?;
            syms.push(*ep);
            idx += 1;
        }
        // One ERC flag per net (KiCAD treats an undriven power-input pin as an
        // error here). Place it COINCIDENT with the first power symbol, rotated
        // so its diamond extends the SAME outward direction as that symbol's
        // arrow/triangle — into the open space the power symbol already claims,
        // so the flag reads as part of the supply marker, never a floating leash.
        if let (Some(flag_points), Some((ep, dir))) = (flag, eps.first()) {
            flag_points.entry(net.to_string()).or_insert((*ep, flag_angle(*dir)));
        }
        return Ok(());
    };
    // Each pin's attach point on the rail. A side (E/W) pin leads OUTWARD first
    // and attaches there, so its riser never runs up the IC edge past the other
    // pins on that side (which would block their signals). On top of that, a riser
    // sharing a column with a different rail's overlapping riser gets fanned into
    // a separate lane (`riser_offsets`) so the two rails never merge into a short.
    // Final riser x per pin: the base column + any anti-short fan offset, THEN a
    // finalize JOG one lane at a time off any part body the straight riser would be
    // drawn through (a stacked same-rail cap column, or a mis-oriented cap whose own
    // body sits between its pin and the rail). The riser then leads sideways out of
    // the pin and descends in a clear lane — clearing both its own body and a
    // neighbour's. `bodies` is empty on the per-move scorer (finalize-only), so the
    // placement is never churned by this.
    let attaches: Vec<f64> = eps
        .iter()
        .map(|(ep, dir)| {
            let base = riser_base_x(ep, *dir);
            let mut ax =
                base + riser_offsets.get(&(net.to_string(), col_key(base))).copied().unwrap_or(0.0);
            if !bodies.is_empty() {
                let (rlo, rhi) = (ep[1].min(rail_y), ep[1].max(rail_y));
                if riser_hits_body(ax, rlo, rhi, bodies) {
                    if let Some(clear) = (1..=8)
                        .flat_map(|k| [k as f64, -(k as f64)])
                        .map(|m| ax + m * RAIL_LANE)
                        .find(|&c| !riser_hits_body(c, rlo, rhi, bodies))
                    {
                        ax = clear;
                    }
                }
            }
            ax
        })
        .collect();
    let span_lo = attaches.iter().copied().fold(f64::MAX, f64::min);
    let span_hi = attaches.iter().copied().fold(f64::MIN, f64::max);
    w.add_wire_on_net([span_lo, rail_y], [span_hi, rail_y], net);
    for ((ep, _dir), &ax) in eps.iter().zip(&attaches) {
        if (ax - ep[0]).abs() > 1e-6 {
            w.add_wire_on_net(*ep, [ax, ep[1]], net); // lead out
        }
        w.add_wire_on_net([ax, ep[1]], [ax, rail_y], net); // riser
        w.add_junction([ax, rail_y]);
    }
    // One power symbol at the left end (pin coincident with the rail). A top
    // rail's symbol sits above, a bottom rail's below — both at angle 0.
    let flag_at = [span_lo, rail_y];
    w.add_power_symbol(env, &lib, &format!("#PWR_{net}"), net, flag_at, 0.0)?;
    // The ERC flag (only when this net needs one) sits COINCIDENT with the rail's
    // power symbol, rotated to extend the same way the symbol does (up for a top
    // V+ rail, down for a bottom GND rail) — into open space, no dangling stub.
    if let Some(flag_points) = flag {
        let angle = if band == Band::Top { 0.0 } else { 180.0 };
        flag_points.entry(net.to_string()).or_insert(([span_lo, rail_y], angle));
    }
    Ok(())
}

/// Angle for a per-pin power symbol given the pin's outward direction.
///
/// KiCAD power symbols (`GND`, `+5V`, …) have their connection pin facing the
/// way the rail naturally attaches: at angle 0 a GND triangle hangs below a
/// downward (South) part pin and a VCC arrow rises above an upward (North) part
/// pin — both correct at angle 0. Horizontal pins rotate ±90°.
fn power_angle(dir: Dir) -> f64 {
    match dir {
        Dir::North | Dir::South => 0.0,
        Dir::East => 90.0,
        Dir::West => 270.0,
    }
}

/// Angle (CCW) for a `PWR_FLAG` so its diamond — which points up (North) at 0° —
/// extends in the pin's outward `dir`, matching the power symbol it sits on.
fn flag_angle(dir: Dir) -> f64 {
    match dir {
        Dir::North => 0.0,
        Dir::West => 90.0,
        Dir::South => 180.0,
        Dir::East => 270.0,
    }
}

#[cfg(test)]
mod grid_tests {
    use super::*;
    use circuit_lang::model::{Block, Component, Design, LayoutGrid};
    use indexmap::IndexMap;

    fn cells(names: &[&str]) -> Vec<Option<String>> {
        names
            .iter()
            .map(|n| if *n == "~" { None } else { Some((*n).to_string()) })
            .collect()
    }

    fn block(refs: &[&str], layout: LayoutGrid) -> Block {
        let mut components = IndexMap::new();
        for r in refs {
            components.insert((*r).to_string(), Component::default());
        }
        Block { note: None, components, layout }
    }

    #[test]
    fn per_block_grids_compose_into_column_bands_with_spans_and_holes() {
        let mut design = Design::default();
        // block `usb`: J1 | hole | R1  -> width 3
        design.blocks.insert(
            "usb".into(),
            block(&["J1", "R1"], vec![cells(&["J1", "~", "R1"])]),
        );
        // block `mcu`: U1 spans its column across two rows (repeated) -> first occ.
        design.blocks.insert(
            "mcu".into(),
            block(&["U1"], vec![cells(&["U1"]), cells(&["U1"])]),
        );

        let g = grid_from_layout(&design);
        // usb band starts at col 0; the `~` hole reserves col 1. Boxes are
        // [col_min, row_min, col_max, row_max].
        assert_eq!(g["J1"], [0, 0, 0, 0]);
        assert_eq!(g["R1"], [2, 0, 2, 0]);
        // mcu band starts AFTER usb's 3 columns (no overlap); U1 SPANS rows 0..1 in
        // its column (repeated down it), so its box grows in the row axis.
        assert_eq!(g["U1"], [3, 0, 3, 1]);
        assert_eq!(g.len(), 3);
    }

    #[test]
    fn collinear_body_crossing_fires_on_passthrough_not_on_series() {
        // Vertical 2-pin part, pins at (10,0) (top) and (10,10) (bottom).
        let body = vec![([10.0, 0.0], [10.0, 10.0])];
        let w = |a: [f64; 2], b: [f64; 2]| (a, b, None);
        // A wire on the SAME line (x=10) running from above the top pin to below the
        // bottom pin slices straight THROUGH the part — 1 crossing.
        assert_eq!(count_collinear_body_crossings(&body, &[w([10.0, -5.0], [10.0, 15.0])]), 1);
        // A correctly-drawn series part: leads STOP at each pin (two segments, neither
        // spanning beyond both pins) — 0.
        assert_eq!(
            count_collinear_body_crossings(
                &body,
                &[w([10.0, -5.0], [10.0, 0.0]), w([10.0, 10.0], [10.0, 15.0])],
            ),
            0
        );
        // A parallel wire on a DIFFERENT line (x=20) is not collinear — 0.
        assert_eq!(count_collinear_body_crossings(&body, &[w([20.0, -5.0], [20.0, 15.0])]), 0);
        // A PERPENDICULAR wire (handled by count_body_crossings, not this) — 0 here.
        assert_eq!(count_collinear_body_crossings(&body, &[w([0.0, 5.0], [20.0, 5.0])]), 0);
        // A wire reaching one pin from outside but stopping inside the body — 0.
        assert_eq!(count_collinear_body_crossings(&body, &[w([10.0, -5.0], [10.0, 5.0])]), 0);
    }

    #[test]
    fn block_without_layout_contributes_nothing() {
        let mut design = Design::default();
        design.blocks.insert("main".into(), block(&["U1", "R1"], Vec::new()));
        assert!(grid_from_layout(&design).is_empty());
    }

    #[test]
    fn dir_to_side_inverts_side_dir() {
        for s in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            assert_eq!(dir_to_side(side_dir(s)), s);
        }
    }

    #[test]
    fn single_pin_port_exit_follows_pin_with_short_reach() {
        // The h-bridge regression: a lone WEST-facing gate pin must seat its port
        // pennant on a SHORT stub to its own side (clear of its body and of the
        // next symbol in a packed row), NOT the long multi-pin reach.
        let west = [([20.0, 0.0], Dir::West)];
        assert_eq!(port_exit_point(&west, Side::Left), [crate::grid::snap(20.0 - 2.54), 0.0]);
        let east = [([20.0, 0.0], Dir::East)];
        assert_eq!(port_exit_point(&east, Side::Right), [crate::grid::snap(20.0 + 2.54), 0.0]);
        // A multi-pin port keeps the longer reach so it clears the last pin.
        let two = [([20.0, 0.0], Dir::East), ([24.0, 0.0], Dir::East)];
        assert_eq!(port_exit_point(&two, Side::Right), [crate::grid::snap(24.0 + 7.62), 0.0]);
    }
}

