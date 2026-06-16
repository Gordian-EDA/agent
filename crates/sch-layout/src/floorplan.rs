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
    }
}

/// Ground-like net name heuristic.
fn is_ground(net: &str) -> bool {
    let u = net.to_ascii_uppercase();
    u == "GND" || u == "GNDD" || u == "AGND" || u == "DGND" || u == "VSS" || u.starts_with("GND")
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
    let mut next_col = max_gcol + 1;
    let mut anchor_col: BTreeMap<usize, i32> = BTreeMap::new();
    let mut anchor_row: BTreeMap<usize, i32> = BTreeMap::new();
    for &ai in &order {
        let rd = &items[ai].refdes;
        let (gcol, grow) = match authored.get(rd) {
            Some(b) => (b[0], b[1]), // seed from the box's top-left cell
            None => {
                let c = next_col;
                next_col += 1;
                (c, 0)
            }
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
            } else if is_ground(other) && is_rail(other) {
                // Pull-down / ground return → vertical in the GND band below.
                let c = col_for_side(side);
                Cell { col: c, row: arow + 2, orient: orient_for(&s.pins, &n1, true) }
            } else {
                // Series element in the signal flow → horizontal beside the pin.
                let c = col_for_side(side);
                let horiz = if side == PinSide::West { Orient::Left } else { Orient::Right };
                Cell { col: c, row: arow + rank, orient: series_orient(&s.pins, &tap_net, horiz) }
            }
        } else if is_vplus(&n1) && is_ground(&n2) || is_ground(&n1) && is_vplus(&n2) {
            // Pure decoupling cap (rail to rail, no anchor pin): hang vertical in a
            // spare column, spread along the rail.
            let key = (spare_col, MID);
            let f = band_fill.entry(key).or_insert(0);
            let c = spare_col;
            *f += 1;
            spare_col += 1;
            Cell { col: c, row: MID, orient: orient_for(&s.pins, &n1, true) }
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

    // Ports: a single-pin signal net (not power) exits the sheet. Heuristic side —
    // an input-ish name on the left, else right.
    let mut ports = BTreeMap::new();
    for (net, pins) in &inc {
        let power = design.nets.get(net).map(|a| a.power).unwrap_or(false);
        // A single-pin signal net is a board I/O port — UNLESS it reads like an
        // intentional no-connect (NC_*), which stays a no-connect marker.
        let nc = net.to_ascii_uppercase().starts_with("NC");
        if pins.len() == 1 && !power && !nc {
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

    LayoutIr { flow: Flow::Lr, rails, place, ports, mirror, grid: authored }
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
    let mut order: Vec<usize> = anchors.to_vec();
    order.sort_by(|&a, &b| {
        let ca = !items[a].part.contains("Connector");
        let cb = !items[b].part.contains("Connector");
        ca.cmp(&cb).then(items[a].refdes.cmp(&items[b].refdes))
    });
    let _ = inc;
    order
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
    // Seed each item's mirror flag from the IR (lifted onto Item so the search
    // can flip it and the cost/emit read one source of truth).
    for it in &mut items {
        it.mirror = ir.mirror.contains(&it.refdes);
    }
    let inc = incidence(&items);

    if std::env::var("DEBUG_SIZE").is_ok() {
        let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
        eprintln!("SIZE items={} pins={} nets={}", items.len(), pins, inc.len());
    }

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
    if std::env::var("NO_REFINE").is_err() {
        // Placement search behind the strategy interface (`PlacementStrategy`):
        // greedy by default (free tier), simulated annealing opt-in (paid tier).
        // Both mutate `items` in mm; selection is the one `pick_strategy` factory.
        pick_strategy().search(env, &mut items, &inc, ir, &needs_flag, SEARCH_SEED);
    }
    // Continuous-placement polish: iterate the directed slides (onto pin axes,
    // toward the centroid) AND a free per-axis nudge to convergence — giving the
    // continuous phase the freedom the column-centre cell table cannot express.
    if std::env::var("NO_REFINE").is_err() {
        polish(env, &mut items, &inc, ir, &needs_flag);
    }
    // Guarantee no body overlap: the cost-gated refine can leave two parts
    // touching when separating them would transiently raise routed cost (a local
    // minimum), so a final, unconditional relaxation pushes any remaining
    // overlaps apart. Cheap a frame may be, the shipped sheet never collides.
    decongest(&mut items);

    let mut w = build_writer(env, design.name.as_deref(), &items, &inc, ir, &needs_flag, true)?;
    // Finalize geometry (text solve, wire split, reframe) BEFORE linting so the
    // reported warnings reflect the actual emitted sheet, not the pre-solve state.
    w.set_frame(true);
    w.prepare();
    let warnings = w.layout_warnings();
    let (body_crossings, ic_crossings) = crossing_counts(env, &items, &inc, ir, &needs_flag);
    let sch = w.finish();
    Ok(EmitOutput { sch, layout_warnings: warnings, body_crossings, ic_crossings })
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
            if comp.part.starts_with("power:") {
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
/// Default deterministic seed for the placement search (the SA's PRNG). Made a
/// parameter so a search is reproducible by seed, not a hard-coded constant.
const SEARCH_SEED: u64 = 0xD1B54A32D192ED03;

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
        let seed_state: Vec<Item> = items.to_vec();
        // Path A: greedy refine, then a seeded anneal from there. Keep the greedy
        // result as its OWN candidate — the SA must never SHIP more layout
        // warnings than greedy (the "SA regresses a tuned frame" trap).
        refine_items(env, items, inc, ir, needs_flag);
        let greedy_state: Vec<Item> = items.to_vec();
        anneal_items(env, items, inc, ir, needs_flag, false, false, seed);
        let state_a: Vec<Item> = items.to_vec();
        // Path B: a broad anneal from the raw seed (a wider global search).
        items.clone_from_slice(&seed_state);
        anneal_items(env, items, inc, ir, needs_flag, true, false, seed);
        let state_b: Vec<Item> = items.to_vec();
        // Path C (PREMIUM): a seeded anneal from the greedy result optimising the
        // richer straightness objective. Added as an EXTRA candidate — never
        // replaces A/B — so the premium tier is strictly ≥ the base SA: it can only
        // be picked when it is BOTH as clean AND tidier. (This is why a premium cost
        // that perturbs the SA's trajectory can't lose us a warning-free find — the
        // base runs still carry it.)
        items.clone_from_slice(&greedy_state);
        anneal_items(env, items, inc, ir, needs_flag, false, true, seed ^ 0x9E3779B97F4A7C15);
        let state_c: Vec<Item> = items.to_vec();
        // Pick the candidate with the FEWEST layout warnings (the quality metric
        // the per-move cost can't afford — it needs the text solve), tie-broken by
        // routed cost. Each candidate is measured THROUGH the same `polish` +
        // `decongest` that `emit` runs after this search — those passes can add or
        // clear a warning (e.g. decongest nudging a part into a neighbour's field
        // text), so judging the raw pre-polish state would ship a worse layout than
        // greedy. Cheap: a handful of passes at the END, never per-eval. Guarantees
        // the SA ≥ greedy on a tuned (sidecar) frame while still winning on a loose
        // (INFER) frame.
        let candidates = [greedy_state, state_a, state_b, state_c];
        let (mut best, mut best_w, mut best_c) = (0usize, usize::MAX, f64::INFINITY);
        for (k, cand) in candidates.iter().enumerate() {
            let mut shipped = cand.clone();
            polish(env, &mut shipped, inc, ir, needs_flag);
            decongest(&mut shipped);
            let w = warning_count(env, &shipped, inc, ir, needs_flag);
            // Tie-break by the PREMIUM cost: among equally-clean candidates the SA
            // ships the tidier one (where its extra optimisation actually shows).
            let c = premium_score_items(env, &shipped, inc, ir, needs_flag);
            if std::env::var("DEBUG_SA").is_ok() {
                let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
                for it in &shipped {
                    let r = item_rect(it, it.at);
                    lo[0] = lo[0].min(r[0]); lo[1] = lo[1].min(r[1]);
                    hi[0] = hi[0].max(r[2]); hi[1] = hi[1].max(r[3]);
                }
                let spread = (hi[0] - lo[0]) + (hi[1] - lo[1]);
                let (body_xing, ic_xing) = crossing_counts(env, &shipped, inc, ir, needs_flag);
                eprintln!(
                    "SA cand k={k} warnings={w} premium_cost={c:.1} spread={spread:.0} \
                     body_xing={body_xing} ic_xing={ic_xing}"
                );
            }
            if w < best_w || (w == best_w && c + 0.5 < best_c) {
                best = k;
                best_w = w;
                best_c = c;
            }
        }
        if std::env::var("DEBUG_SA").is_ok() {
            eprintln!("SA WINNER k={best} (0=greedy 1=seededA 2=broadB 3=PREMIUM)");
        }
        items.clone_from_slice(&candidates[best]);
    }
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
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3).collect();
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
    let sats: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3).collect();
    let anchors: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() >= 3).collect();
    if sats.is_empty() {
        return;
    }
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
    // One grid cell-step in x/y for the relocation moves.
    let relocate = |rng: &mut Rng, at: [f64; 2], n: i32| -> [f64; 2] {
        [
            crate::grid::snap(at[0] + rng.step(n) as f64 * COL_GAP),
            crate::grid::snap(at[1] + rng.step(n) as f64 * ROW_GAP),
        ]
    };
    // NB: no "exit early once `best` plateaus for N iters" rule. Measured (DEBUG_SA
    // max_gap) the largest plateau that is still FOLLOWED by a real improvement: up
    // to 1154 iters on the 2000-iter broad run, 685 on a 750-iter seeded run. Every
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
            // Nudge an anchor (an IC) by one cell — frees the whole block to slide,
            // which a satellite-only search cannot do.
            let i = anchors[rng.below(anchors.len())];
            undo = vec![(i, items[i].at, items[i].angle)];
            items[i].at = relocate(&mut rng, items[i].at, 1);
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
    let sats: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3).collect();
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
    let sats: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3).collect();
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
        base + COMPACT_BOOST * (0.15 * length + 0.45 * spread)
            + ORIENT_BOOST * leg_viol as f64
    } else {
        base
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

/// DEBUG-only ground truth behind the visual "a wire runs through a part" complaint:
/// `(2-pin transverse body crossings, IC body crossings)` for a placed item set.
/// Mirrors the obstacle extraction in [`layout_cost`] (kept separate so the hot cost
/// path stays untouched / bit-identical). Used by the `DEBUG_SA` trace to tell a real
/// crossing from a vision-model false positive on a correctly-drawn series part.
fn crossing_counts(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> (usize, usize) {
    let Ok(w) = build_writer(env, None, items, inc, ir, needs_flag, false) else {
        return (0, 0);
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
        count_body_crossings(&bodies, &wires) + count_collinear_body_crossings(&bodies, &wires),
        count_ic_body_crossings(&ic_rects, &wires),
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

    // Phase A — rails (shared wires + stubs + power symbols), so their wires are
    // in the writer before we build the routing scene.
    for (net, eps) in &net_eps {
        if let Some(band) = ir.rails.get(net) {
            let flag = needs_flag.contains(net).then_some(&mut *flag_points);
            emit_rail(
                env,
                w,
                net,
                eps,
                *band,
                rail_y_map.get(net).copied(),
                flag,
                &riser_offsets,
            )?;
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
            if std::env::var("DEBUG_RAIL").is_ok() {
                eprintln!("RISER-FAN col={col} net={net} dx={dx}");
            }
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
) -> io::Result<()> {
    let lib = power_lib_id(net);
    let Some(rail_y) = rail_y.filter(|_| eps.len() >= 3) else {
        for (idx, (ep, dir)) in eps.iter().enumerate() {
            let angle = power_angle(*dir);
            let refdes = format!("#PWR_{net}_{idx}");
            w.add_power_symbol(env, &lib, &refdes, net, *ep, angle)?;
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
    let attach_x = |ep: &[f64; 2], dir: &Dir| {
        let base = riser_base_x(ep, *dir);
        base + riser_offsets
            .get(&(net.to_string(), col_key(base)))
            .copied()
            .unwrap_or(0.0)
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
}

