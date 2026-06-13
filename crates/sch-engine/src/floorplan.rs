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

impl Item {
    /// The net on a given pin number, if connected.
    fn net_of(&self, number: &str) -> Option<&str> {
        self.pins
            .iter()
            .find(|(n, _, _)| n == number)
            .and_then(|(_, _, net)| net.as_deref())
    }
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

    place(&mut items, ir, &inc);

    let mut w = SchematicWriter::new();
    if let Some(name) = &design.name {
        w.set_title(name);
    }

    // Place symbols.
    for it in &items {
        w.add_symbol(env, &it.part, &it.refdes, &it.value, it.at, it.angle)?;
    }
    // No-connect markers for unmentioned pins (kernel auto-NCs them).
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

fn place(items: &mut [Item], ir: &LayoutIr, inc: &Incidence) {
    // Net layers: top rails = 0, bottom rails = max. Internal nets get a layer
    // by longest-path from the top boundary. This drives vertical placement of
    // power passives (decoupling, dividers) so power flows top→bottom.
    let layers = layer_nets(items, ir, inc);
    let max_layer = layers.values().copied().max().unwrap_or(1).max(1);

    // Anchors first: column from IR cell (or sequential), y at mid band.
    let mid_y = (max_layer as f64) * DLAYER / 2.0;
    let mut next_anchor_col = 0i32;
    let mut anchor_cols: Vec<i32> = Vec::new();
    for it in items.iter_mut().filter(|i| i.is_anchor) {
        let col = ir.place.get(&it.refdes).map(|c| c.col).unwrap_or_else(|| {
            let c = next_anchor_col;
            next_anchor_col += 1;
            c
        });
        anchor_cols.push(col);
        it.at = [col_x(col), mid_y];
        it.angle = 0.0;
    }

    // Passives: assign columns by walking vertical strands. A strand is a
    // maximal series path of 2-pin parts descending from a top rail toward a
    // bottom rail; series-connected parts share a column (divider leg), and a
    // branch off a node spawns the next column to its right (the shunt cap).
    let next_col = next_anchor_col
        .max(anchor_cols.iter().copied().max().map(|c| c + 1).unwrap_or(0));
    let cols = assign_columns(items, &layers, next_col);
    for idx in 0..items.len() {
        if items[idx].is_anchor {
            continue;
        }
        let nets: Vec<(String, String)> = items[idx]
            .pins
            .iter()
            .filter_map(|(num, _n, net)| net.clone().map(|nn| (num.clone(), nn)))
            .collect();
        let col = cols.get(&idx).copied().unwrap_or(0);
        if nets.len() == 2 {
            let la = *layers.get(&nets[0].1).unwrap_or(&0);
            let lb = *layers.get(&nets[1].1).unwrap_or(&max_layer);
            // Vertical orientation: lower-layer (more top) net pin on top.
            // Device:R/C at angle 0 has pin 1 on top.
            let pin1_layer = pin_layer(&nets, "1", &layers, la);
            let pin2_layer = pin_layer(&nets, "2", &layers, lb);
            let angle = if pin1_layer <= pin2_layer { 0.0 } else { 180.0 };
            let y = (la.min(lb) as f64 + la.max(lb) as f64) / 2.0 * DLAYER;
            items[idx].at = [col_x(col), y];
            items[idx].angle = angle;
        } else {
            items[idx].at = [col_x(col), mid_y];
            items[idx].angle = 0.0;
        }
    }

    // Translate everything into the positive quadrant with a margin.
    normalize(items);
}

fn col_x(col: i32) -> f64 {
    col as f64 * DX
}

/// Layer of the net on pin `num`, or `default`.
fn pin_layer(
    nets: &[(String, String)],
    num: &str,
    layers: &BTreeMap<String, i32>,
    default: i32,
) -> i32 {
    nets.iter()
        .find(|(n, _)| n == num)
        .and_then(|(_, net)| layers.get(net).copied())
        .unwrap_or(default)
}

/// Two connected nets of a passive `idx`, as (net, layer), or None if not 2-pin.
fn passive_nets(item: &Item) -> Option<Vec<String>> {
    let nets: Vec<String> = item.pins.iter().filter_map(|(_, _, n)| n.clone()).collect();
    (nets.len() == 2).then_some(nets)
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

    for (net, pins) in inc {
        // Endpoints of every pin on this net, in sheet space.
        let mut eps: Vec<([f64; 2], Dir)> = Vec::new();
        for (i, num) in pins {
            for (ep, dir) in w.pin_dirs(env, &refdes_of(*i), num)? {
                eps.push((ep, dir));
            }
        }
        if eps.is_empty() {
            continue;
        }

        if ir.rails.contains_key(net) {
            emit_rail(env, w, net, &eps)?;
        } else if let Some(side) = ir.ports.get(net) {
            emit_port(w, net, &eps, *side);
        } else {
            // Local net: connect with wires (star from first endpoint).
            connect_node(w, net, &eps);
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

/// A rail: if ≥3 pins, draw a horizontal wire spanning them at the band edge and
/// stub each pin to it (one PWR_FLAG). Otherwise emit a per-pin power symbol.
fn emit_rail(
    env: &KicadEnv,
    w: &mut SchematicWriter,
    net: &str,
    eps: &[([f64; 2], Dir)],
) -> io::Result<()> {
    let lib = power_lib_id(net);
    if eps.len() < 3 {
        for (idx, (ep, dir)) in eps.iter().enumerate() {
            let angle = power_angle(*dir);
            let refdes = format!("#PWR_{net}_{idx}");
            w.add_power_symbol(env, &lib, &refdes, net, *ep, angle)?;
        }
        return Ok(());
    }
    // Rail wire at the extreme y among the endpoints (top rail = min y).
    let is_bottom = is_ground(net);
    let rail_y = if is_bottom {
        eps.iter().map(|(p, _)| p[1]).fold(f64::MIN, f64::max) + 5.08
    } else {
        eps.iter().map(|(p, _)| p[1]).fold(f64::MAX, f64::min) - 5.08
    };
    let min_x = eps.iter().map(|(p, _)| p[0]).fold(f64::MAX, f64::min);
    let max_x = eps.iter().map(|(p, _)| p[0]).fold(f64::MIN, f64::max);
    w.add_wire_on_net([min_x, rail_y], [max_x, rail_y], net);
    for (ep, _) in eps {
        w.add_wire_on_net(*ep, [ep[0], rail_y], net);
        w.add_junction([ep[0], rail_y]);
    }
    // One flag + label at the left end of the rail.
    let flag_at = [min_x, rail_y];
    w.add_power_symbol(env, &lib, &format!("#PWR_{net}"), net, flag_at, if is_bottom { 0.0 } else { 180.0 })?;
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
