//! Infer stage — connectivity → Layout IR. Net classification (rails/power/
//! ground), baseline + LLM-assisted IR inference (`infer_ir`), and turning
//! detected idioms into IR cells/reports (decoupling banks, crystals, pull-ups).
//! Idiom DETECTION lives in the sibling `idiom` module; this stage consumes it.
//! Reads the placed Items (`super::*`); produces the IR the place engine runs.

use std::collections::{BTreeMap, BTreeSet};

use circuit_lang::model::Design;
use kicad_cli::env::KicadEnv;


use super::idiom;
use super::*;
use sch_place::netclass::{is_connector_like, is_ground, is_neg_supply, is_power_net, pin_side, PinSide};
use super::place::{gather, grid_from_layout, incidence};
use sch_place::item::{Incidence, Item};

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
    let detected = idiom::detect_idioms(
        &items, &inc, &anchors, &sats, &rails, &pin_meta, &anchor_col, &anchor_row,
    );
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut idiom_reports: Vec<sch_place::result::IdiomReport> = Vec::new();
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
        idiom_reports.push(sch_place::result::IdiomReport {
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
    // then holds that relative order via the `grid_order` cost.
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


/// The IC a decoupling bank actually bypasses: among anchors with a pin on the bank's
/// V+ rail, the real IC (not a connector/jumper) with the most pins on that rail, then
/// the most pins overall. `None` if the caps share no non-ground rail with any anchor.
pub(super) fn best_decoupling_anchor(
    items: &[Item],
    anchors: &[usize],
    rails: &BTreeMap<String, Band>,
    caps: &[usize],
) -> Option<usize> {
    // The V+ rail the bank bypasses: the most common non-ground rail among the caps.
    let mut vp_count: BTreeMap<String, usize> = BTreeMap::new();
    for &ci in caps {
        for (_, _, n) in &items[ci].pins {
            if let Some(n) = n.as_deref()
                && rails.contains_key(n) && !is_ground(n) {
                    *vp_count.entry(n.to_string()).or_insert(0) += 1;
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
pub(super) fn place_decoupling(
    items: &[Item],
    inc: &Incidence,
    anchors: &[usize],
    rails: &BTreeMap<String, Band>,
    anchor_col: &BTreeMap<usize, i32>,
    anchor_row: &BTreeMap<usize, i32>,
    ai: usize,
    caps: &[usize],
    out: &[idiom::Idiom],
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
    // Single-sheet references keep the strict per-rail≥3 bank (snapshot-locked, e.g. mcp1703). On
    // MULTI-SHEET sub-sheets, bank ALL of the IC's bypass caps even when split thin ACROSS rails: a
    // multi-rail regulator/DDR3/FPGA has only 1-2 caps on ANY single rail (V_in + V_out, or VDD+VDDQ+
    // VREF), so the per-rail gate dropped the whole bank → the caps scattered (DDR3 sdram=6, "5 caps
    // scattered not in a tidy bank"; multi-rail LDO power sheets). The circuit-graph matcher already
    // accepts caps across rails (per-cap power-net binding), so the only blocker was THIS filter.
    // Banking them into one aligned row above the IC reads far cleaner than the scatter.
    let multisheet = std::env::var("MULTISHEET_REFINE").is_ok();
    let mut bank: Vec<usize> = if multisheet {
        by_rail.into_values().flatten().collect()
    } else {
        by_rail.into_values().filter(|v| v.len() >= 3).flatten().collect()
    };
    bank.sort_unstable();
    if bank.len() < if multisheet { 2 } else { 3 } {
        return None;
    }
    let (acol, arow) = (anchor_col[&ai], anchor_row[&ai]);
    let n = bank.len() as i32;
    let crystal_l = out.iter().any(|id| {
        id.kind == "crystal" && id.anchor == ai && id.cells.iter().any(|(_, c)| c.col < acol)
    });
    // Past the right edge (crystal on the left) or past the left edge. EXCEPT a SMALL anchor
    // (a 3-4 pin LDO/regulator, pinned with its bank on multi-sheet): a tall-IC bank seats far
    // to the left, but a small LDO is the same width as its caps, so `-(n+1)` flings the bank to
    // the far (often negative) left and it sprawls away. Seat it DIRECTLY ABOVE the LDO (base 0)
    // so the cap row hugs the pinned regulator — the textbook compact power-entry block.
    let small_anchor = multisheet && (3..=4).contains(&items[ai].geom.pins.len());
    let base = if small_anchor {
        0
    } else if crystal_l {
        2
    } else {
        -(n + 1)
    };
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
/// Reserve two adjacent cells for a USB-C CC-pulldown pair beside the connector, so the two
/// 5.1k twins FREEZE together as a tidy pair instead of the second drifting to a spare column
/// (the power-entry R2-exiled defect). Coarse — no per-pin edge alignment like `place_crystal`;
/// the win is that freezing RESERVES the pair's cells (the report-only snap of iter 16 failed
/// because the target spot was already taken). Routing follows.
pub(super) fn place_cc_pulldown(
    anchor_col: &BTreeMap<usize, i32>,
    anchor_row: &BTreeMap<usize, i32>,
    ai: usize,
    ra: &str,
    rb: &str,
) -> Option<Vec<(String, Cell)>> {
    let acol = *anchor_col.get(&ai)?;
    let arow = *anchor_row.get(&ai)?;
    Some(vec![
        (ra.to_string(), Cell { col: acol + 1, row: arow, orient: Orient::Down }),
        (rb.to_string(), Cell { col: acol + 2, row: arow, orient: Orient::Down }),
    ])
}

/// Seat an I2C pull-up pair as a reserved column beside the IC, tapping UP to power (the mirror
/// of `place_cc_pulldown`, whose pair taps DOWN to ground). Same reserved cells beside the
/// anchor so the SA leaves room and the bus pull-ups co-place by their SDA/SCL pins instead of
/// drifting far below the IC.
pub(super) fn place_i2c_pullup(
    anchor_col: &BTreeMap<usize, i32>,
    anchor_row: &BTreeMap<usize, i32>,
    ai: usize,
    ra: &str,
    rb: &str,
) -> Option<Vec<(String, Cell)>> {
    let acol = *anchor_col.get(&ai)?;
    let arow = *anchor_row.get(&ai)?;
    Some(vec![
        (ra.to_string(), Cell { col: acol + 1, row: arow, orient: Orient::Up }),
        (rb.to_string(), Cell { col: acol + 2, row: arow, orient: Orient::Up }),
    ])
}

pub(super) fn place_crystal(
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
            // Exclude the crystal itself: a 4-pin Crystal_GND24 has ≥3 pins so it lands in `anchors`,
            // and an OSC net taps BOTH the IC and the crystal — without this the net reads as tapping
            // two anchors and is rejected (the crystal idiom then never fires for grounded-case parts).
            .filter(|(j, _)| anchors.contains(j) && *j != yi)
            .map(|(j, num)| (*j, num.clone()))
            .collect();
        (hits.len() == 1).then(|| hits[0].clone())
    };
    {
        // The crystal's OSC nets = its nets that tap exactly THIS anchor. A 4-pin Crystal_GND24 also
        // carries 2 case-GROUND (or NC) nets that do NOT tap the IC — ignore those. Requiring the
        // crystal to have EXACTLY 2 nets total wrongly rejected every grounded-case crystal (the COMMON
        // real variant the agent emits), so the idiom silently never fired and the crystal sprawled far
        // from the OSC pins with long dog-leg routes (the recurring MCU-sheet defect).
        let osc: Vec<(String, String)> = items[yi]
            .pins
            .iter()
            .filter_map(|(_, _, n)| n.clone())
            .filter_map(|n| anchor_pin(&n).filter(|(j, _)| *j == ai).map(|(_, num)| (n, num)))
            .collect();
        if osc.len() != 2 || osc[0].0 == osc[1].0 {
            return None;
        }
        let (xa, pa) = (osc[0].0.clone(), osc[0].1.clone());
        let (xb, pb) = (osc[1].0.clone(), osc[1].1.clone());
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
pub(super) fn anchor_tap(
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
                    if let Some(n2) = n2
                        && n2 != net
                            && inc.get(n2).into_iter().flatten().any(|(k, _)| is_conn(*k))
                        {
                            return true;
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
