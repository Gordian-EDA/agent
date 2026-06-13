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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
            if ir.mirror.contains(&it.refdes) {
                probe.set_mirror_last();
            }
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
        if ir.mirror.contains(&it.refdes) {
            w.set_mirror_last();
        }
    }
    for it in &items {
        for (num, _name, net) in &it.pins {
            if net.is_none() {
                w.add_no_connect(env, &it.refdes, num)?;
            }
        }
    }
    let mut flag_points: BTreeMap<String, [f64; 2]> = BTreeMap::new();
    wire(env, &mut w, &items, &inc, ir, &mut flag_points)?;

    // ERC power flags: a net carrying a power-INPUT pin (or a declared rail)
    // with no power-OUTPUT pin driving it is "undriven" — supply one PWR_FLAG.
    let provider = RealSymbolProvider::new(env.clone());
    let (mut driven, mut power_input) = (BTreeSet::new(), BTreeSet::new());
    for it in &items {
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
    for net in &needs_flag {
        if let Some(at) = flag_points.get(net) {
            w.add_power_flag_at(env, &format!("#FLG_{net}"), *at)?;
        }
    }

    let warnings = w.layout_warnings();
    let sch = w.finish();
    Ok(EmitOutput { sch, layout_warnings: warnings, relayout_blocks: Default::default() })
}

/// net → anchor-pin occurrences: (anchor refdes, sheet endpoint, outward dir).
type AnchorPins = BTreeMap<String, Vec<(String, [f64; 2], Dir)>>;

fn anchor_pin_map(w: &SchematicWriter, env: &KicadEnv, items: &[Item]) -> io::Result<AnchorPins> {
    let mut map: AnchorPins = BTreeMap::new();
    for it in items.iter().filter(|i| i.is_anchor) {
        for (num, _name, net) in &it.pins {
            if let Some(net) = net {
                for (pos, dir) in w.pin_dirs(env, &it.refdes, num)? {
                    map.entry(net.clone()).or_default().push((it.refdes.clone(), pos, dir));
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

/// Per-rail region: the anchor-pin centroid x and the outward growth direction.
/// Flanking passives pack AWAY from the IC body: a West VI pin grows left, an
/// East VO pin grows right, and a top/bottom rail pin grows toward whichever
/// side of the chip centre it sits on (so two supply rails split left/right
/// instead of piling their decoupling caps on one side).
fn rail_regions(
    apins: &AnchorPins,
    anchor_cx: &BTreeMap<String, f64>,
) -> (BTreeMap<String, f64>, BTreeMap<String, f64>) {
    let mut x = BTreeMap::new();
    let mut dir = BTreeMap::new();
    for (net, pins) in apins {
        let cx = pins.iter().map(|(_, p, _)| p[0]).sum::<f64>() / pins.len() as f64;
        x.insert(net.clone(), cx);
        let mut d = 0.0;
        for (rd, p, dd) in pins {
            match dd {
                Dir::East => d += 1.0,
                Dir::West => d -= 1.0,
                _ => {
                    if let Some(c) = anchor_cx.get(rd) {
                        d += if p[0] >= *c { 1.0 } else { -1.0 };
                    }
                }
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
    let anchor_cx: BTreeMap<String, f64> =
        items.iter().filter(|i| i.is_anchor).map(|i| (i.refdes.clone(), i.at[0])).collect();
    let (region_x, region_dir) = rail_regions(apins, &anchor_cx);
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

    // Anchor x-extents, for placing flanking columns just outside the body.
    let anchor_half: BTreeMap<String, (f64, f64)> = items
        .iter()
        .filter(|i| i.is_anchor)
        .map(|i| (i.refdes.clone(), (i.at[0], i.geom.approx_size()[0] / 2.0)))
        .collect();

    // A passive is vertical when its two nets sit in different power layers.
    // (A single-element port→rail feed is made horizontal by giving the feed
    // port the rail's layer in `layer_nets`, so it falls out of this test.)
    let is_vert = |nets: &[(String, String)]| -> bool {
        layer_of(&nets[0].1) != layer_of(&nets[1].1)
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

    // Anchor pin counts: when a part taps two ICs, the bigger one (more pins) is
    // the main chip — place the part beside IT, not the connector.
    let pincount: BTreeMap<String, usize> =
        items.iter().filter(|i| i.is_anchor).map(|i| (i.refdes.clone(), i.geom.pins.len())).collect();

    // Pass 0: anchor taps — a passive on an IC signal pin sits beside that pin.
    // A series element (same power layer at both ends, e.g. a termination R)
    // lies in-line with the pin at the pin's own row, one slot out; anything
    // else falls back to a side column. Pullups/decoupling (touch a top rail)
    // are left to the rail-flank pass.
    let mut side_cursor: BTreeMap<(String, bool), i32> = BTreeMap::new();
    let mut inline_cursor: BTreeMap<(String, i64), i32> = BTreeMap::new();
    for &i in &passives {
        let nets = nets_of(&items[i]);
        if nets.len() != 2 {
            continue;
        }
        // A part with a SIGNAL pin on an IC is tapped beside that pin (a pullup
        // hugs its signal, not the rail region). Only pure decoupling — both
        // pins on rails, no signal anchor pin — falls through to rail-flank.
        // Tap pin: a non-rail net on an anchor pin; prefer the anchor with most pins.
        let tap = nets
            .iter()
            .filter(|(_, n)| !ir.rails.contains_key(n))
            .filter_map(|(_, n)| apins.get(n))
            .flatten()
            .max_by_key(|(rd, _, _)| pincount.get(rd).copied().unwrap_or(0))
            .cloned();
        let Some((anchor, pos, dir)) = tap else { continue };
        // A part between two non-rail signals is a SERIES element (in-line with
        // the pin), even if a pull-up gave one end a different power layer.
        let series = !nets.iter().any(|(_, n)| ir.rails.contains_key(n));
        let vertical = is_vert(&nets);
        if matches!(dir, Dir::East | Dir::West) && series {
            // Series element in-line with the pin, at its row, one slot out
            // (further out for a second part tapping the same pin).
            let half = part_half_span(&items[i].geom);
            let sign = if dir == Dir::East { 1.0 } else { -1.0 };
            let k = inline_cursor.entry((anchor.clone(), (pos[1] * 100.0) as i64)).or_insert(0);
            let reach = half + 3.81 + (*k as f64) * (2.0 * half + 5.08);
            *k += 1;
            items[i].at = [crate::grid::snap(pos[0] + sign * reach), pos[1]];
            items[i].angle = if native_vertical(&items[i].geom) { 90.0 } else { 0.0 };
            placed_x.insert(i, items[i].at[0]);
        } else {
            let (ax, ahw) = anchor_half.get(&anchor).copied().unwrap_or((pos[0], 5.08));
            let right = match dir {
                Dir::East => true,
                Dir::West => false,
                _ => pos[0] >= ax,
            };
            let k = side_cursor.entry((anchor.clone(), right)).or_insert(0);
            *k += 1;
            let x = if right { ax + ahw + (*k as f64) * DX } else { ax - ahw - (*k as f64) * DX };
            orient_at(&mut items[i], &nets, x, vertical);
            placed_x.insert(i, x);
        }
    }

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
                let placed_idxs: Vec<usize> = anchor_half
                    .keys()
                    .filter_map(|r| items.iter().position(|it| &it.refdes == r))
                    .chain(placed_x.keys().copied())
                    .collect();
                nudge_clear(items, i, &placed_idxs);
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
            .fold(f64::NEG_INFINITY, f64::max);
        let free_base = if free_base.is_finite() { free_base + DX } else { 0.0 };
        let cols = assign_columns(items, layers, 0);
        let mut placed_idxs: Vec<usize> = anchor_half
            .keys()
            .filter_map(|r| items.iter().position(|it| &it.refdes == r))
            .chain(placed_x.keys().copied())
            .collect();
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
            nudge_clear(items, i, &placed_idxs);
            placed_idxs.push(i);
        }
    }
}

/// Two connected nets of a passive `idx`, as (net, layer), or None if not 2-pin.
fn passive_nets(item: &Item) -> Option<Vec<String>> {
    let nets: Vec<String> = item.pins.iter().filter_map(|(_, _, n)| n.clone()).collect();
    (nets.len() == 2).then_some(nets)
}

/// An item's axis-aligned footprint `[min_x,min_y,max_x,max_y]` (body extents,
/// rotation-aware), padded so neighbours keep a little air.
fn footprint(it: &Item) -> [f64; 4] {
    let s = it.geom.approx_size();
    let quarter_turn = ((it.angle / 90.0).round() as i64).rem_euclid(2) == 1;
    let (hw, hh) = if quarter_turn { (s[1] / 2.0, s[0] / 2.0) } else { (s[0] / 2.0, s[1] / 2.0) };
    [it.at[0] - hw, it.at[1] - hh, it.at[0] + hw, it.at[1] + hh]
}

fn rects_overlap(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[2] - 1e-6 && b[0] < a[2] - 1e-6 && a[1] < b[3] - 1e-6 && b[1] < a[3] - 1e-6
}

/// Bump item `i` downward (in `DLAYER` steps) until its footprint clears every
/// already-placed item in `placed`, so the catch-all passes never stack parts
/// (overlapping bodies block the router and force ugly label fallback).
fn nudge_clear(items: &mut [Item], i: usize, placed: &[usize]) {
    for _ in 0..24 {
        let fi = footprint(&items[i]);
        let hit = placed.iter().any(|&j| j != i && rects_overlap(fi, footprint(&items[j])));
        if !hit {
            return;
        }
        items[i].at[1] += DLAYER;
    }
}

/// Half the pin-to-pin span of a 2-pin symbol (mm), for placing it one slot off
/// a pin so its near lead lands just outside the IC.
fn part_half_span(g: &SymbolGeometry) -> f64 {
    let p: Vec<_> = g.pins.iter().collect();
    if p.len() < 2 {
        return 2.54;
    }
    let d = ((p[0].at[0] - p[1].at[0]).powi(2) + (p[0].at[1] - p[1].at[1]).powi(2)).sqrt();
    (d / 2.0).max(2.54)
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

/// Two-sided longest-path layering: power flows top (rails at 0) → bottom (rails
/// at the max layer). A net's layer is its longest-path distance from a top
/// rail; nets reachable only from the bottom (signal strands hanging to GND,
/// e.g. an LED+resistor off an IC output) are layered up from the bottom so
/// they spread into a vertical strand instead of collapsing onto one row.
fn layer_nets(items: &[Item], ir: &LayoutIr, inc: &Incidence) -> BTreeMap<String, i32> {
    let edges: Vec<(String, String)> = items
        .iter()
        .filter(|i| !i.is_anchor)
        .filter_map(|it| {
            let nets: Vec<String> = it.pins.iter().filter_map(|(_, _, n)| n.clone()).collect();
            (nets.len() == 2).then(|| (nets[0].clone(), nets[1].clone()))
        })
        .collect();

    let top: Vec<String> = ir.rails.iter().filter(|(_, b)| **b == Band::Top).map(|(n, _)| n.clone()).collect();
    let bottom: Vec<String> = ir.rails.iter().filter(|(_, b)| **b == Band::Bottom).map(|(n, _)| n.clone()).collect();
    let rails_set: BTreeSet<&String> = ir.rails.keys().collect();

    // Shortest-path (BFS) distance from a seed set, NOT traversing *through* a
    // rail (rails are barriers, so cycles via the GND/Vcc rails — decoupling
    // caps, LED loops — don't inflate or diverge). A rail still receives a
    // distance but its neighbours are not expanded from it.
    let bfs = |seeds: &[String]| -> BTreeMap<String, i32> {
        let mut d: BTreeMap<String, i32> = BTreeMap::new();
        let mut q: VecDeque<String> = VecDeque::new();
        for s in seeds {
            d.insert(s.clone(), 0);
            q.push_back(s.clone());
        }
        while let Some(u) = q.pop_front() {
            let du = d[&u];
            for (a, b) in &edges {
                let v = if *a == u {
                    b
                } else if *b == u {
                    a
                } else {
                    continue;
                };
                if d.contains_key(v) {
                    continue;
                }
                d.insert(v.clone(), du + 1);
                if !rails_set.contains(v) {
                    q.push_back(v.clone());
                }
            }
        }
        d
    };

    let d_top = bfs(&top);
    let d_bot = bfs(&bottom);
    // Max depth among non-bottom nets sets the bottom band one row deeper.
    let max_top = d_top.iter().filter(|(n, _)| !bottom.contains(n)).map(|(_, &v)| v).max().unwrap_or(1).max(1);
    let max_layer = max_top + 1;

    let mut layer = BTreeMap::new();
    for net in inc.keys() {
        let l = if bottom.contains(net) {
            max_layer
        } else if let Some(&d) = d_top.get(net) {
            d.min(max_layer - 1)
        } else if let Some(&d) = d_bot.get(net) {
            (max_layer - d).max(1)
        } else {
            max_layer / 2
        };
        layer.insert(net.clone(), l);
    }

    // A feed port — a port net joined to exactly one passive whose other end is
    // a rail (e.g. 5V_BUS → F1 → 5V) — takes that rail's layer so its series
    // element lays out horizontally as an edge feed, not a vertical span. A
    // multi-connection port that is really an internal node (e.g. OUT) is left
    // at its computed layer so its divider/strand stays vertical.
    for port in ir.ports.keys() {
        let touching: Vec<&String> = edges
            .iter()
            .filter_map(|(a, b)| {
                if a == port {
                    Some(b)
                } else if b == port {
                    Some(a)
                } else {
                    None
                }
            })
            .collect();
        if touching.len() == 1 && ir.rails.contains_key(touching[0]) {
            if let Some(&rl) = layer.get(touching[0]) {
                layer.insert(port.clone(), rl);
            }
        }
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
            emit_rail(env, w, net, eps, *band, rail_y_map.get(net).copied(), flag_points)?;
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
        // Lone pin (no port): a stubbed name label, not a floating pin.
        if let Some((i, num)) = inc.get(net).and_then(|p| p.first()) {
            w.add_signal_label(env, &items[*i].refdes, num, net)?;
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
        w.add_cluster_label(net, terms[pi].0, side_dir(side));
    }
    Ok(())
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
    flag_points: &mut BTreeMap<String, [f64; 2]>,
) -> io::Result<()> {
    let lib = power_lib_id(net);
    let Some(rail_y) = rail_y.filter(|_| eps.len() >= 3) else {
        for (idx, (ep, dir)) in eps.iter().enumerate() {
            let angle = power_angle(*dir);
            let refdes = format!("#PWR_{net}_{idx}");
            w.add_power_symbol(env, &lib, &refdes, net, *ep, angle)?;
        }
        // The ERC flag hangs off a short horizontal stub from the first pin, so
        // it sits clear of the power symbol and the component body.
        if let Some((ep, _)) = eps.first() {
            let stub = [ep[0] - 5.08, ep[1]];
            w.add_wire_on_net(*ep, stub, net);
            w.add_junction(*ep);
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
    // The ERC flag attaches to the far (right) end of the rail wire — on the
    // net, clear of the power symbol at the left end.
    flag_points.entry(net.to_string()).or_insert([span_hi, rail_y]);
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

