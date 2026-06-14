//! Floorplan engine — human-style schematic layout from a minimal Layout IR.
//!
//! Layer 2 of the two-layer design (see
//! `docs/superpowers/specs/2026-06-13-floorplan-engine-design.md`). Given a
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
use circuit_lang::{find_pin, PinType, SymbolProvider};
use kicad_bridge::env::KicadEnv;
use kicad_bridge::geometry::SymbolGeometry;
use kicad_bridge::provider::RealSymbolProvider;
use serde::{Deserialize, Serialize};

use crate::emit::{Dir, SchematicWriter};
use crate::reconcile::EmitOutput;

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
    }
}

/// Ground-like net name heuristic.
fn is_ground(net: &str) -> bool {
    let u = net.to_ascii_uppercase();
    u == "GND" || u == "GNDD" || u == "AGND" || u == "DGND" || u == "VSS" || u.starts_with("GND")
}

// ---------------------------------------------------------------------------
// Compiler internal model.
// ---------------------------------------------------------------------------

/// Spacing constants (mm). All on the 1.27 grid.
const COL_GAP: f64 = 6.35; // 5 grid — column channel (room for side-mounted value text)
const ROW_GAP: f64 = 5.08; // 4 grid — slightly tighter vertical stack
const MARGIN: f64 = 12.7;

/// One placed component plus the data the compiler needs about it.
struct Item {
    refdes: String,
    part: String,
    value: String,
    geom: SymbolGeometry,
    /// (pin number, pin name, net or None for NC).
    pins: Vec<(String, String, Option<String>)>,
    at: [f64; 2],
    angle: f64,
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
pub fn emit(env: &KicadEnv, design: &Design, ir: &LayoutIr) -> io::Result<EmitOutput> {
    let mut items = gather(env, design)?;
    let inc = incidence(&items);

    // Which power nets need an ERC PWR_FLAG: a power-INPUT pin (or a declared
    // rail) with no power-OUTPUT pin driving it is "undriven". Computed up front
    // so it can feed both the refinement scorer and the final emission.
    let needs_flag = compute_needs_flag(env, &items, ir);

    // Placement: start from the LLM's coarse (col,row,orient) grid, then let the
    // refinement loop nudge the satellites (anchors stay put) to a tidier wiring
    // — fewer label fallbacks, crossings, and junctions — judged on the ACTUAL
    // routed result. Finally render the cells to mm as a sized table.
    let mut cells = assign_cells(&items, ir);
    if std::env::var("NO_REFINE").is_err() {
        refine_cells(env, &mut items, &inc, ir, &needs_flag, &mut cells);
    }
    apply_cells(&mut items, &cells);
    normalize(&mut items);
    // Slide satellites onto the axis of the pin they wire to (straight drops),
    // which the column-centre table layout cannot express.
    if std::env::var("NO_REFINE").is_err() {
        align_to_pins(env, &mut items, &inc, ir, &needs_flag);
    }

    let w = build_writer(env, design.name.as_deref(), &items, &inc, ir, &needs_flag)?;
    let warnings = w.layout_warnings();
    let sch = w.finish();
    Ok(EmitOutput { sch, layout_warnings: warnings, relayout_blocks: Default::default() })
}

/// Build the complete schematic writer for a placed `items`: symbols (+mirror),
/// no-connects on unconnected pins, all wiring (rails + routed signals), and ERC
/// flags. Shared by the final emission and the refinement scorer so both judge
/// exactly the geometry that ships.
fn build_writer(
    env: &KicadEnv,
    title: Option<&str>,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> io::Result<SchematicWriter> {
    let mut w = SchematicWriter::new();
    if let Some(name) = title {
        w.set_title(name);
    }
    for it in items {
        w.add_symbol(env, &it.part, &it.refdes, &it.value, it.at, it.angle)?;
        if ir.mirror.contains(&it.refdes) {
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
    let mut flag_points: BTreeMap<String, [f64; 2]> = BTreeMap::new();
    wire(env, &mut w, items, inc, ir, needs_flag, &mut flag_points)?;
    for net in needs_flag {
        if let Some(at) = flag_points.get(net) {
            w.add_power_flag_at(env, &format!("#FLG_{net}"), *at)?;
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
            let geom = SymbolGeometry::load(env, &comp.part)?;
            let pins = resolve_pins(comp, &geom);
            items.push(Item {
                refdes: refdes.clone(),
                part: comp.part.clone(),
                value: comp.value.clone().unwrap_or_default(),
                geom,
                pins,
                at: [0.0, 0.0],
                angle: 0.0,
            });
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
    items
        .iter()
        .map(|it| match ir.place.get(&it.refdes) {
            Some(c) => *c,
            None => {
                let c = spare;
                spare += 1;
                Cell { col: c, row: 0, orient: Orient::Down }
            }
        })
        .collect()
}

/// Render `cells` to mm: each column sized to its widest member and each row to
/// its tallest, every part at its cell centre — aligned, overlap-free, and as
/// tight as the parts allow.
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
fn refine_cells(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    cells: &mut [Cell],
) {
    let satellites: Vec<usize> =
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3).collect();
    if satellites.is_empty() {
        return;
    }
    // Anchor the search to the initial (LLM/human) placement: a satellite that
    // strays far is penalised, so refinement makes LOCAL fixes (close a label
    // fallback, uncross a pair) rather than globally relocating a part to a
    // cheaper-but-nonsensical spot (a pull-up flung to the far corner).
    let orig: Vec<Cell> = cells.to_vec();
    let disp = |cs: &[Cell]| -> f64 {
        // Gentle anti-thrash backstop only; the real positional anchor is the
        // "stray" term in `layout_cost` (a satellite is pulled to the anchor pin
        // it serves, which is far stronger and correctly placed).
        const DISP_W: f64 = 1.0;
        DISP_W
            * satellites
                .iter()
                .map(|&i| {
                    ((cs[i].col - orig[i].col).unsigned_abs()
                        + (cs[i].row - orig[i].row).unsigned_abs()) as f64
                })
                .sum::<f64>()
    };
    let mut best = score_cells(env, items, inc, ir, needs_flag, cells) + disp(cells);
    const MAX_ROUNDS: usize = 6;
    for _ in 0..MAX_ROUNDS {
        let mut improved = false;
        // Single-part nudges: shift one satellite by one cell in any direction.
        for &i in &satellites {
            for cand in nudges(cells[i]) {
                let prev = cells[i];
                cells[i] = cand;
                let c = score_cells(env, items, inc, ir, needs_flag, cells) + disp(cells);
                if c + 0.5 < best {
                    best = c;
                    improved = true;
                } else {
                    cells[i] = prev;
                }
            }
        }
        // Pairwise swaps: exchange two satellites' (col,row) to reorder them
        // (keeps each part's own orientation).
        for a in 0..satellites.len() {
            for b in (a + 1)..satellites.len() {
                let (i, j) = (satellites[a], satellites[b]);
                let (ci, cj) = (cells[i], cells[j]);
                cells[i] = Cell { col: cj.col, row: cj.row, orient: ci.orient };
                cells[j] = Cell { col: ci.col, row: ci.row, orient: cj.orient };
                let c = score_cells(env, items, inc, ir, needs_flag, cells) + disp(cells);
                if c + 0.5 < best {
                    best = c;
                    improved = true;
                } else {
                    cells[i] = ci;
                    cells[j] = cj;
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// The four orthogonal one-cell shifts of `c`.
fn nudges(c: Cell) -> [Cell; 4] {
    [
        Cell { col: c.col - 1, ..c },
        Cell { col: c.col + 1, ..c },
        Cell { row: c.row - 1, ..c },
        Cell { row: c.row + 1, ..c },
    ]
}

/// Apply `cells`, build the schematic, and return its routed-layout cost. Leaves
/// `items` positioned per `cells` (the caller re-applies the chosen cells).
fn score_cells(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    cells: &[Cell],
) -> f64 {
    // Two parts in one cell sit at the same point — an overlap, never wanted.
    for i in 0..cells.len() {
        for j in (i + 1)..cells.len() {
            if cells[i].col == cells[j].col && cells[i].row == cells[j].row {
                return f64::INFINITY;
            }
        }
    }
    apply_cells(items, cells);
    // Match the real emit exactly (which normalizes before building): routing is
    // not perfectly translation-invariant near the origin, so scoring the
    // un-normalized layout would see phantom rail/lead touches.
    normalize(items);
    score_items(env, items, inc, ir, needs_flag)
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
    match build_writer(env, None, items, inc, ir, needs_flag) {
        Ok(w) => layout_cost(env, &w, items, inc, ir),
        Err(_) => f64::INFINITY,
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
    let Ok(w0) = build_writer(env, None, items, inc, ir, needs_flag) else { return };
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
        // No rail fallback: only parts with a real signal pin are aligned, so a
        // decoupling cap (rail-only) is left where the table layout spread it.
        if let Some(t) = signal_anchor_centroid(env, &w0, items, inc, ir, s, false) {
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

/// Whether placing item `si` at `at` would overlap any other item's body (the
/// pin-extent rect — `approx_size` shrunk to the connection points, the same
/// extent the router treats as solid).
fn overlaps_any(items: &[Item], si: usize, at: [f64; 2]) -> bool {
    let rect = |it: &Item, at: [f64; 2]| -> [f64; 4] {
        let s = it.geom.approx_size();
        let quarter = ((it.angle / 90.0).round() as i64).rem_euclid(2) == 1;
        let (w, h) = if quarter { (s[1], s[0]) } else { (s[0], s[1]) };
        let (hw, hh) = ((w / 2.0 - 2.54).max(1.27), (h / 2.0 - 2.54).max(1.27));
        [at[0] - hw, at[1] - hh, at[0] + hw, at[1] + hh]
    };
    let a = rect(&items[si], at);
    items.iter().enumerate().any(|(j, it)| {
        j != si && {
            let b = rect(it, it.at);
            a[0] < b[2] - EPS && b[0] < a[2] - EPS && a[1] < b[3] - EPS && b[1] < a[3] - EPS
        }
    })
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
) -> f64 {
    let fallbacks = w.signal_label_count();
    let junctions = w.junction_count();
    let wires = w.wires_with_nets();
    let length: f64 =
        wires.iter().map(|(a, b, _)| (a[0] - b[0]).abs() + (a[1] - b[1]).abs()).sum();
    let crossings = count_crossings(&wires);
    let merges = count_merges(&wires, &w.junction_positions()) + count_shorts(env, w, items, inc, &wires);
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
    let stray = count_stray(env, w, items, inc, ir);
    // Merges/shorts are hard correctness failures (a rail-to-rail short lowers
    // length+junctions, so without this the hill-climb would happily create
    // one); fallbacks degrade a wire to a label; then crossings; then CONGESTION
    // (junction dots packed against each other — the "dot knot" / wires-collapse-
    // into-a-resistor look, which length-minimisation otherwise rewards); then
    // junctions and length. The big coefficients keep correctness off the table.
    2000.0 * merges as f64
        + 1000.0 * fallbacks as f64
        + 5.0 * crossings as f64
        + 7.0 * congestion as f64
        + 1.0 * junctions as f64
        + 0.4 * stray
        + 0.15 * length
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
                    if near(ep, *a) || near(ep, *b) {
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
    flag_points: &mut BTreeMap<String, [f64; 2]>,
) -> io::Result<()> {
    let refdes_of = |i: usize| items[i].refdes.clone();

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

    // Phase A — rails (shared wires + stubs + power symbols), so their wires are
    // in the writer before we build the routing scene.
    for (net, eps) in &net_eps {
        if let Some(band) = ir.rails.get(net) {
            let flag = needs_flag.contains(net).then_some(&mut *flag_points);
            emit_rail(env, w, net, eps, *band, rail_y_map.get(net).copied(), flag)?;
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

    let pts: Vec<[f64; 2]> = terms.iter().map(|t| t.0).collect();
    let mut paths: Vec<crate::route::Path> = Vec::new();
    let mut ok = true;
    for (i, j) in crate::route::mst_edges(&pts) {
        let (a, da, b) = match (terms[i].1, terms[j].1) {
            (Some(d), _) => (pts[i], d, pts[j]),
            (None, Some(d)) => (pts[j], d, pts[i]),
            (None, None) => (pts[i], dir_toward(pts[i], pts[j]), pts[j]),
        };
        match crate::route::route_edge(a, da, b, net, scene) {
            Some(p) => paths.push(p),
            None => {
                ok = false;
                break;
            }
        }
    }

    if !ok {
        // Fallback: per-pin stub labels (still connected by net name).
        let mut seen = BTreeSet::new();
        for (i, num) in inc.get(net).into_iter().flatten() {
            if seen.insert((*i, num.clone())) {
                w.add_signal_label(env, &items[*i].refdes, num, net)?;
            }
        }
        return Ok(());
    }

    for path in &paths {
        for seg in path.windows(2) {
            w.add_wire_on_net(seg[0], seg[1], net);
            scene.segments.push((seg[0], seg[1], net.to_string()));
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
    const REACH: f64 = 7.62;
    let xs: Vec<f64> = eps.iter().map(|(p, _)| p[0]).collect();
    let ys: Vec<f64> = eps.iter().map(|(p, _)| p[1]).collect();
    let (min_x, max_x) = (xs.iter().cloned().fold(f64::MAX, f64::min), xs.iter().cloned().fold(f64::MIN, f64::max));
    let (min_y, max_y) = (ys.iter().cloned().fold(f64::MAX, f64::min), ys.iter().cloned().fold(f64::MIN, f64::max));
    // Align the exit with the pin nearest that edge so the wire runs straight.
    match side {
        Side::Right => {
            let y = eps.iter().max_by(|a, b| a.0[0].total_cmp(&b.0[0])).map(|t| t.0[1]).unwrap_or(min_y);
            [crate::grid::snap(max_x + REACH), y]
        }
        Side::Left => {
            let y = eps.iter().min_by(|a, b| a.0[0].total_cmp(&b.0[0])).map(|t| t.0[1]).unwrap_or(min_y);
            [crate::grid::snap(min_x - REACH), y]
        }
        Side::Top => {
            let x = eps.iter().min_by(|a, b| a.0[1].total_cmp(&b.0[1])).map(|t| t.0[0]).unwrap_or(min_x);
            [x, crate::grid::snap(min_y - REACH)]
        }
        Side::Bottom => {
            let x = eps.iter().max_by(|a, b| a.0[1].total_cmp(&b.0[1])).map(|t| t.0[0]).unwrap_or(max_x);
            [x, crate::grid::snap(max_y + REACH)]
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

/// A rail: with ≥3 pins, draw a horizontal wire at `rail_y` spanning them, stub
/// each pin to it, and put one power symbol at the left end. With fewer pins (or
/// no common band), emit a per-pin power symbol instead (the clustered case,
/// e.g. a divider's two GNDs).
fn emit_rail(
    env: &KicadEnv,
    w: &mut SchematicWriter,
    net: &str,
    eps: &[([f64; 2], Dir)],
    _band: Band,
    rail_y: Option<f64>,
    flag: Option<&mut BTreeMap<String, [f64; 2]>>,
) -> io::Result<()> {
    let lib = power_lib_id(net);
    let Some(rail_y) = rail_y.filter(|_| eps.len() >= 3) else {
        for (idx, (ep, dir)) in eps.iter().enumerate() {
            let angle = power_angle(*dir);
            let refdes = format!("#PWR_{net}_{idx}");
            w.add_power_symbol(env, &lib, &refdes, net, *ep, angle)?;
        }
        // One ERC flag per net (KiCAD treats an undriven power-input pin as an
        // error here). Hang it off a short horizontal stub into open space so the
        // PWR_FLAG diamond never overlaps the power symbol or a component body.
        if let (Some(flag_points), Some((ep, _))) = (flag, eps.first()) {
            let stub = [ep[0] - 5.08, ep[1]];
            w.add_wire_on_net(*ep, stub, net);
            flag_points.entry(net.to_string()).or_insert(stub);
        }
        return Ok(());
    };
    // Each pin's attach point on the rail. A side (E/W) pin leads OUTWARD first
    // and attaches there, so its riser never runs up the IC edge past the other
    // pins on that side (which would block their signals).
    const LEAD: f64 = 2.54;
    let attach_x = |ep: &[f64; 2], dir: &Dir| match dir {
        Dir::East => ep[0] + LEAD,
        Dir::West => ep[0] - LEAD,
        _ => ep[0],
    };
    let span_lo = eps.iter().map(|(p, d)| attach_x(p, d)).fold(f64::MAX, f64::min);
    let span_hi = eps.iter().map(|(p, d)| attach_x(p, d)).fold(f64::MIN, f64::max);
    w.add_wire_on_net([span_lo, rail_y], [span_hi, rail_y], net);
    for (ep, dir) in eps {
        let ax = attach_x(ep, dir);
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
    // The ERC flag (only when this net needs one) tucks just left of the power
    // symbol on a short rail extension — at the supply's entry, the way an
    // engineer marks it. Driven rails get no extension, so nothing dangles.
    if let Some(flag_points) = flag {
        let flag_stub = [span_lo - 5.08, rail_y];
        w.add_wire_on_net([span_lo, rail_y], flag_stub, net);
        flag_points.entry(net.to_string()).or_insert(flag_stub);
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

