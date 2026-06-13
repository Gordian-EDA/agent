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

use std::collections::BTreeMap;
use std::io;

use circuit_lang::model::{Component, Design, PinTarget};
use kicad_bridge::env::KicadEnv;
use kicad_bridge::geometry::SymbolGeometry;
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

/// A coarse, unitless placement cell. The engine maps columns/rows to mm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub col: i32,
    pub row: i32,
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
    LayoutIr { flow: Flow::Lr, rails, place: BTreeMap::new(), ports: BTreeMap::new() }
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
const DX: f64 = 22.86; // column pitch (18 grid)
const DLAYER: f64 = 16.51; // vertical layer pitch (13 grid)
const MARGIN: f64 = 12.7;

/// One placed component plus the data the compiler needs about it.
struct Item {
    refdes: String,
    part: String,
    value: String,
    geom: SymbolGeometry,
    /// (pin number, pin name, net or None for NC).
    pins: Vec<(String, String, Option<String>)>,
    is_anchor: bool,
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
    let layers = layer_nets(&items, ir, &inc);
    let max_layer = layers.values().copied().max().unwrap_or(1).max(1);

    // Phase 1 — place anchors (ICs) from the IR's coarse cells.
    place_anchors(&mut items, ir, max_layer);

    // Phase 2 — read each anchor pin's true sheet position/direction. A
    // throwaway writer applies the same placement transform the final emit
    // uses; passives are then placed relative to these (translation-invariant,
    // so the later `normalize` shift stays consistent).
    let apins = {
        let mut probe = SchematicWriter::new();
        for it in items.iter().filter(|i| i.is_anchor) {
            probe.add_symbol(env, &it.part, &it.refdes, &it.value, it.at, it.angle)?;
        }
        anchor_pin_map(&probe, env, &items)?
    };

    // Phase 3 — place passives relative to anchor pins / rail regions.
    place_passives(&mut items, ir, &layers, max_layer, &apins);
    normalize(&mut items);

    // Phase 4 — build the real schematic.
    let mut w = SchematicWriter::new();
    if let Some(name) = &design.name {
        w.set_title(name);
    }
    for it in &items {
        w.add_symbol(env, &it.part, &it.refdes, &it.value, it.at, it.angle)?;
    }
    for it in &items {
        for (num, _name, net) in &it.pins {
            if net.is_none() {
                w.add_no_connect(env, &it.refdes, num)?;
            }
        }
    }
    wire(env, &mut w, &items, &inc, ir)?;

    let warnings = w.layout_warnings();
    let sch = w.finish();
    Ok(EmitOutput { sch, layout_warnings: warnings, relayout_blocks: Default::default() })
}

/// net → anchor-pin sheet endpoints and outward directions.
type AnchorPins = BTreeMap<String, Vec<([f64; 2], Dir)>>;

fn anchor_pin_map(w: &SchematicWriter, env: &KicadEnv, items: &[Item]) -> io::Result<AnchorPins> {
    let mut map: AnchorPins = BTreeMap::new();
    for it in items.iter().filter(|i| i.is_anchor) {
        for (num, _name, net) in &it.pins {
            if let Some(net) = net {
                for (pos, dir) in w.pin_dirs(env, &it.refdes, num)? {
                    map.entry(net.clone()).or_default().push((pos, dir));
                }
            }
        }
    }
    Ok(map)
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
            let connected = pins.iter().filter(|(_, _, n)| n.is_some()).count();
            let is_anchor = geom.pins.len() >= 3 || connected >= 3;
            items.push(Item {
                refdes: refdes.clone(),
                part: comp.part.clone(),
                value: comp.value.clone().unwrap_or_default(),
                geom,
                pins,
                is_anchor,
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
// Placement.
// ---------------------------------------------------------------------------

/// x spacing per anchor cell column (leaves room for flanking passives).
const ANCHOR_PITCH: f64 = 50.8;

/// Place anchors (ICs) from the IR's coarse cells: col → x, row → y offset from
/// the mid band. Anchors without a cell are ordered left-to-right.
fn place_anchors(items: &mut [Item], ir: &LayoutIr, max_layer: i32) {
    let mid_y = (max_layer as f64) * DLAYER / 2.0;
    let mut next = 0i32;
    for it in items.iter_mut().filter(|i| i.is_anchor) {
        let cell = ir.place.get(&it.refdes).copied();
        let col = cell.map(|c| c.col).unwrap_or_else(|| {
            let c = next;
            next += 1;
            c
        });
        let row = cell.map(|c| c.row).unwrap_or(0);
        it.at = [col as f64 * ANCHOR_PITCH, mid_y + row as f64 * DLAYER];
        it.angle = 0.0;
    }
}

/// Per-rail region: the anchor-pin centroid x and the outward growth direction
/// (flanking passives pack away from the IC: a West VI pin grows left, an East
/// VO pin grows right).
fn rail_regions(apins: &AnchorPins) -> (BTreeMap<String, f64>, BTreeMap<String, f64>) {
    let mut x = BTreeMap::new();
    let mut dir = BTreeMap::new();
    for (net, pins) in apins {
        let cx = pins.iter().map(|(p, _)| p[0]).sum::<f64>() / pins.len() as f64;
        x.insert(net.clone(), cx);
        let mut d = 0.0;
        for (_, dd) in pins {
            match dd {
                Dir::East => d += 1.0,
                Dir::West => d -= 1.0,
                _ => {}
            }
        }
        dir.insert(net.clone(), if d < 0.0 { -1.0 } else { 1.0 });
    }
    (x, dir)
}

/// Place every passive: pack rail-flanking parts into their rail's region,
/// continue node strands in the same column, and lay the rest in free columns.
fn place_passives(
    items: &mut [Item],
    ir: &LayoutIr,
    layers: &BTreeMap<String, i32>,
    max_layer: i32,
    apins: &AnchorPins,
) {
    let (region_x, region_dir) = rail_regions(apins);
    let layer_of = |n: &str| layers.get(n).copied().unwrap_or(0);
    let passives: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| !it.is_anchor)
        .map(|(i, _)| i)
        .collect();

    // Resolve nets-with-pin-numbers for a passive.
    let nets_of = |it: &Item| -> Vec<(String, String)> {
        it.pins
            .iter()
            .filter_map(|(num, _n, net)| net.clone().map(|nn| (num.clone(), nn)))
            .collect()
    };

    let mut placed_x: BTreeMap<usize, f64> = BTreeMap::new();
    let mut cursor: BTreeMap<String, i32> = BTreeMap::new();

    // A passive is vertical when its two nets sit in different power layers AND
    // neither is a port (a port→rail feed flows in horizontally from the edge).
    let is_vert = |nets: &[(String, String)]| -> bool {
        layer_of(&nets[0].1) != layer_of(&nets[1].1)
            && !nets.iter().any(|(_, n)| ir.ports.contains_key(n))
    };

    // Orient + position a 2-pin part at column `x`, accounting for the symbol's
    // native pin axis (R/C are vertical at angle 0; LED/D are horizontal). To
    // draw a part *vertical* we leave a vertical symbol at 0/180 but rotate a
    // horizontal one 90°, and vice-versa for *horizontal*.
    let orient_at = |it: &mut Item, nets: &[(String, String)], x: f64, vertical: bool| {
        let l1 = layer_of(&nets[0].1);
        let l2 = layer_of(&nets[1].1);
        let nat_vert = native_vertical(&it.geom);
        if vertical {
            let y = (l1.min(l2) as f64 + l1.max(l2) as f64) / 2.0 * DLAYER;
            it.at = [x, y];
            it.angle = if !nat_vert {
                90.0
            } else if l1 <= l2 {
                0.0
            } else {
                180.0
            };
        } else {
            it.at = [x, l1.min(l2) as f64 * DLAYER];
            it.angle = if nat_vert { 90.0 } else { 0.0 };
        }
    };

    // Which anchored *top* rail (if any) this passive flanks. Only top rails
    // attract flanking columns; the bottom (GND) rail spans the whole sheet, so
    // a GND pin must never pull a part out of its signal strand.
    let flank_rail = |nets: &[(String, String)]| -> Option<String> {
        nets.iter()
            .map(|(_, n)| n.clone())
            .filter(|n| region_x.contains_key(n) && ir.rails.get(n) == Some(&Band::Top))
            .min_by_key(|n| layer_of(n))
    };

    // Pass 1a: vertical passives flanking an anchored rail (decoupling, output
    // strands), packed away from the IC. 1b: horizontal feeds on the same rail.
    for vertical_pass in [true, false] {
        for &i in &passives {
            if placed_x.contains_key(&i) {
                continue;
            }
            let nets = nets_of(&items[i]);
            if nets.len() != 2 {
                continue;
            }
            if is_vert(&nets) != vertical_pass {
                continue;
            }
            if let Some(rail) = flank_rail(&nets) {
                let k = cursor.entry(rail.clone()).or_insert(0);
                *k += 1;
                let x = region_x[&rail] + region_dir[&rail] * (*k as f64) * DX;
                orient_at(&mut items[i], &nets, x, vertical_pass);
                placed_x.insert(i, x);
            }
        }
    }

    // Pass 2: node-continuation strands — a passive sharing a private node with
    // an already-placed passive sits in the same column (e.g. R2 below the LED).
    loop {
        let mut progress = false;
        for &i in &passives {
            if placed_x.contains_key(&i) {
                continue;
            }
            let nets = nets_of(&items[i]);
            if nets.len() != 2 {
                continue;
            }
            let mut found = None;
            for (_, net) in &nets {
                if ir.rails.contains_key(net) {
                    continue; // rails span horizontally; not a column anchor
                }
                for &j in &passives {
                    if j != i && placed_x.contains_key(&j) && nets_of(&items[j]).iter().any(|(_, n)| n == net) {
                        found = Some(placed_x[&j]);
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            if let Some(x) = found {
                let v = is_vert(&nets);
                orient_at(&mut items[i], &nets, x, v);
                placed_x.insert(i, x);
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }

    // Pass 3: everything left (e.g. an IC-less divider) via vertical strands in
    // free columns to the right of all placed content.
    let leftover: Vec<usize> = passives.iter().copied().filter(|i| !placed_x.contains_key(i)).collect();
    if !leftover.is_empty() {
        let free_base = placed_x
            .values()
            .copied()
            .chain(items.iter().filter(|i| i.is_anchor).map(|i| i.at[0]))
            .fold(f64::MIN, f64::max);
        let free_base = if free_base.is_finite() { free_base + DX } else { 0.0 };
        let cols = assign_columns(items, layers, 0);
        for &i in &leftover {
            let nets = nets_of(&items[i]);
            if nets.len() == 2 {
                let col = cols.get(&i).copied().unwrap_or(0);
                let v = is_vert(&nets);
                orient_at(&mut items[i], &nets, free_base + col as f64 * DX, v);
            } else {
                items[i].at = [free_base, (max_layer as f64) * DLAYER / 2.0];
                items[i].angle = 0.0;
            }
        }
    }
}

/// Two connected nets of a passive `idx`, as (net, layer), or None if not 2-pin.
fn passive_nets(item: &Item) -> Option<Vec<String>> {
    let nets: Vec<String> = item.pins.iter().filter_map(|(_, _, n)| n.clone()).collect();
    (nets.len() == 2).then_some(nets)
}

/// Whether a symbol's two pins are stacked vertically in its native (angle-0)
/// orientation (true for Device:R/C; false for the horizontal Device:LED/D).
fn native_vertical(g: &SymbolGeometry) -> bool {
    let pins: Vec<_> = g.pins.iter().collect();
    if pins.len() < 2 {
        return true;
    }
    let dy = (pins[0].at[1] - pins[1].at[1]).abs();
    let dx = (pins[0].at[0] - pins[1].at[0]).abs();
    dy >= dx
}

/// Assign each passive a column index by walking vertical strands: series parts
/// descending from a top rail share a column; leftover parts (shunts, isolated
/// passives) get their own subsequent columns.
fn assign_columns(
    items: &[Item],
    layers: &BTreeMap<String, i32>,
    start_col: i32,
) -> BTreeMap<usize, i32> {
    // net -> passive indices on it.
    let mut adj: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let passives: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| !it.is_anchor)
        .map(|(i, _)| i)
        .collect();
    for &i in &passives {
        if let Some(nets) = passive_nets(&items[i]) {
            for n in nets {
                adj.entry(n).or_default().push(i);
            }
        }
    }

    let layer_of = |net: &str| layers.get(net).copied().unwrap_or(0);
    let mut col: BTreeMap<usize, i32> = BTreeMap::new();
    let mut next = start_col;

    // Walk down a strand from `start`, assigning `column`.
    let walk = |start: usize, column: i32, col: &mut BTreeMap<usize, i32>| {
        let mut cur = start;
        loop {
            col.insert(cur, column);
            let Some(nets) = passive_nets(&items[cur]) else { break };
            // The lower (higher-layer) net is the one we descend through.
            let lower = nets
                .iter()
                .max_by_key(|n| layer_of(n))
                .cloned()
                .unwrap_or_default();
            let lower_layer = layer_of(&lower);
            // Next unplaced passive on `lower` that continues strictly downward.
            let nxt = adj.get(&lower).into_iter().flatten().copied().find(|&q| {
                q != cur
                    && !col.contains_key(&q)
                    && passive_nets(&items[q])
                        .map(|qn| qn.iter().any(|n| layer_of(n) > lower_layer))
                        .unwrap_or(false)
            });
            match nxt {
                Some(q) => cur = q,
                None => break,
            }
        }
    };

    // Pass 1: strands rooted at a top rail (layer 0).
    for &i in &passives {
        if col.contains_key(&i) {
            continue;
        }
        let on_top = passive_nets(&items[i])
            .map(|nets| nets.iter().any(|n| layer_of(n) == 0))
            .unwrap_or(false);
        if on_top {
            let c = next;
            next += 1;
            walk(i, c, &mut col);
        }
    }
    // Pass 2: leftover passives (shunts, isolated) — each its own column.
    for &i in &passives {
        if !col.contains_key(&i) {
            let c = next;
            next += 1;
            walk(i, c, &mut col);
        }
    }
    col
}

/// Longest-path layering of nets between top-rail (0) and bottom-rail (max)
/// boundaries, over the 2-pin-passive graph. Non-rail nets get an interior
/// layer; rails are pinned to their band.
fn layer_nets(items: &[Item], ir: &LayoutIr, inc: &Incidence) -> BTreeMap<String, i32> {
    let mut layer: BTreeMap<String, i32> = BTreeMap::new();
    let mut top_nets: Vec<String> = Vec::new();
    let mut bottom_nets: Vec<String> = Vec::new();
    for (net, band) in &ir.rails {
        match band {
            Band::Top => {
                layer.insert(net.clone(), 0);
                top_nets.push(net.clone());
            }
            Band::Bottom => bottom_nets.push(net.clone()),
        }
    }
    // Relax: a passive edge (a,b) wants layer(b) > layer(a) when a is "more
    // top". Iterate a few times for the small graphs we handle.
    // Build undirected edges between net pairs joined by a 2-pin passive.
    let edges: Vec<(String, String)> = items
        .iter()
        .filter(|i| !i.is_anchor)
        .filter_map(|it| {
            let nets: Vec<String> = it
                .pins
                .iter()
                .filter_map(|(_, _, n)| n.clone())
                .collect();
            if nets.len() == 2 {
                Some((nets[0].clone(), nets[1].clone()))
            } else {
                None
            }
        })
        .collect();

    let bottom_layer = 1 + top_nets.len().max(1) as i32; // provisional; refined below
    for b in &bottom_nets {
        layer.insert(b.clone(), bottom_layer);
    }
    // Propagate: any net adjacent to a top rail and not a bottom rail sits at 1.
    // Generalised longest-path from top boundary.
    for _ in 0..(items.len() + 2) {
        for (a, b) in &edges {
            let la = layer.get(a).copied();
            let lb = layer.get(b).copied();
            match (la, lb) {
                (Some(x), None) if !bottom_nets.contains(b) => {
                    layer.insert(b.clone(), (x + 1).min(bottom_layer - 1).max(1));
                }
                (None, Some(y)) if !bottom_nets.contains(a) => {
                    layer.insert(a.clone(), (y + 1).min(bottom_layer - 1).max(1));
                }
                _ => {}
            }
        }
    }
    // Any net still unlayered (isolated signal nets) → mid.
    let _ = inc;
    for net in inc.keys() {
        layer.entry(net.clone()).or_insert(bottom_layer / 2);
    }
    layer
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

    // Common rail y per band, so every top rail aligns and every bottom rail
    // aligns (a rail is only drawn as a wire when it has ≥3 pins).
    let rail_band_y = |want: Band| -> Option<f64> {
        let ys: Vec<f64> = ir
            .rails
            .iter()
            .filter(|(_, b)| **b == want)
            .filter_map(|(n, _)| net_eps.get(n))
            .filter(|e| e.len() >= 3)
            .flat_map(|e| e.iter().map(|(p, _)| p[1]))
            .collect();
        if ys.is_empty() {
            return None;
        }
        Some(match want {
            Band::Top => ys.iter().cloned().fold(f64::MAX, f64::min) - 5.08,
            Band::Bottom => ys.iter().cloned().fold(f64::MIN, f64::max) + 5.08,
        })
    };
    let top_y = rail_band_y(Band::Top);
    let bot_y = rail_band_y(Band::Bottom);

    for (net, eps) in &net_eps {
        if let Some(band) = ir.rails.get(net) {
            let rail_y = match band {
                Band::Top => top_y,
                Band::Bottom => bot_y,
            };
            emit_rail(env, w, net, eps, *band, rail_y)?;
        } else if let Some(side) = ir.ports.get(net) {
            emit_port(w, net, eps, *side);
        } else {
            connect_node(w, net, eps);
        }
    }
    Ok(())
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
        g if g == "GND" || g == "GNDD" || g.starts_with("GND") => "GND",
        _ => return format!("power:{net}"),
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
) -> io::Result<()> {
    let lib = power_lib_id(net);
    let Some(rail_y) = rail_y.filter(|_| eps.len() >= 3) else {
        for (idx, (ep, dir)) in eps.iter().enumerate() {
            let angle = power_angle(*dir);
            let refdes = format!("#PWR_{net}_{idx}");
            w.add_power_symbol(env, &lib, &refdes, net, *ep, angle)?;
        }
        return Ok(());
    };
    let min_x = eps.iter().map(|(p, _)| p[0]).fold(f64::MAX, f64::min);
    let max_x = eps.iter().map(|(p, _)| p[0]).fold(f64::MIN, f64::max);
    w.add_wire_on_net([min_x, rail_y], [max_x, rail_y], net);
    for (ep, _) in eps {
        w.add_wire_on_net(*ep, [ep[0], rail_y], net);
        w.add_junction([ep[0], rail_y]);
    }
    // One power symbol at the left end (pin coincident with the rail). A top
    // rail's symbol sits above, a bottom rail's below — both at angle 0.
    let flag_at = [min_x, rail_y];
    w.add_power_symbol(env, &lib, &format!("#PWR_{net}"), net, flag_at, 0.0)?;
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

/// A port net: route to the named edge and drop a hierarchical-style label.
fn emit_port(w: &mut SchematicWriter, net: &str, eps: &[([f64; 2], Dir)], side: Side) {
    // First connect the pins together, then add a label at the chosen side end.
    connect_node(w, net, eps);
    // Place the label at the extreme endpoint toward `side`.
    let pick = match side {
        Side::Right => eps.iter().max_by(|a, b| a.0[0].total_cmp(&b.0[0])),
        Side::Left => eps.iter().min_by(|a, b| a.0[0].total_cmp(&b.0[0])),
        Side::Top => eps.iter().min_by(|a, b| a.0[1].total_cmp(&b.0[1])),
        Side::Bottom => eps.iter().max_by(|a, b| a.0[1].total_cmp(&b.0[1])),
    };
    if let Some((ep, _)) = pick {
        let dir = match side {
            Side::Right => Dir::East,
            Side::Left => Dir::West,
            Side::Top => Dir::North,
            Side::Bottom => Dir::South,
        };
        w.add_cluster_label(net, *ep, dir);
    }
}

/// Connect the endpoints of a node net as a horizontal **bus**: a single
/// horizontal segment at a common y spanning the endpoints, with a short
/// vertical stub from each pin down/up to the bus and a junction where an
/// interior stub meets it. This is the clean "T-node" humans draw (e.g. the
/// divider's OUT node), not a star of diagonals.
fn connect_node(w: &mut SchematicWriter, net: &str, eps: &[([f64; 2], Dir)]) {
    if eps.len() < 2 {
        return;
    }
    if eps.len() == 2 {
        // Two pins: a plain L (or straight) elbow between them.
        let (a, b) = (eps[0].0, eps[1].0);
        w.add_wire_on_net(a, [b[0], a[1]], net);
        w.add_wire_on_net([b[0], a[1]], b, net);
        return;
    }
    // Bus at the average y of the endpoints.
    let sum_y: f64 = eps.iter().map(|(p, _)| p[1]).sum();
    let bus_y = crate::grid::snap(sum_y / eps.len() as f64);
    let min_x = eps.iter().map(|(p, _)| p[0]).fold(f64::MAX, f64::min);
    let max_x = eps.iter().map(|(p, _)| p[0]).fold(f64::MIN, f64::max);
    w.add_wire_on_net([min_x, bus_y], [max_x, bus_y], net);
    for (ep, _) in eps {
        w.add_wire_on_net(*ep, [ep[0], bus_y], net);
        // Junction where an interior pin taps the bus.
        if ep[0] > min_x + 0.01 && ep[0] < max_x - 0.01 {
            w.add_junction([ep[0], bus_y]);
        }
    }
}
