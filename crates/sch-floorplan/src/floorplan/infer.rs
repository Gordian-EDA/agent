//! Infer stage — connectivity → Layout IR: which nets are rails and which way each
//! band runs, which nets exit as ports, and each block's authored tree. Everything a
//! block's own arrangement decides belongs to the tree, not here.

use std::collections::{BTreeMap, BTreeSet};

use kicad::KicadInstallation;
use sch_check::model::Design;

use super::place::{gather, incidence};
use super::*;
use circuit_graph::netclass::{is_ground, is_power_net};

/// Each block's authored arrangement, the tree the typesetter draws it from.
fn block_trees(design: &Design) -> sch_model::tree::Trees {
    design
        .blocks
        .iter()
        .filter_map(|(name, block)| Some((name.clone(), block.layout.clone()?)))
        .collect()
}

/// A deterministic baseline IR for designs without an LLM-produced one: rails
/// from the design's power nets (ground-like → bottom, else top), no explicit
/// anchor cells, no ports. Good enough to render; not tuned for aesthetics.
pub fn baseline_ir(design: &Design) -> LayoutIr {
    let mut rails = BTreeMap::new();
    for (net, attrs) in &design.nets {
        if attrs.power {
            let band = if is_ground(net) {
                Band::Bottom
            } else {
                Band::Top
            };
            rails.insert(net.clone(), band);
        }
    }
    LayoutIr {
        rails,
        ports: BTreeMap::new(),
        trees: block_trees(design),
        rail_locals: local_rail_nets(design),
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
                    if let sch_check::model::PinTarget::Net(net) = target {
                        *count.entry(net.clone()).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    // ≥2 authored symbols on a net = the author drew per-use arrows (the human
    // convention on dense sheets); ONE symbol = one spanning rail, keeping
    // decoupling caps in a tidy row (the 9-scoring idiom-stm32 declares each
    // positive rail once). Every checked-in fixture declares positives once, so
    // this is authored-intent, not a behaviour change for them.
    count
        .into_iter()
        .filter(|(_, n)| *n >= 2)
        .map(|(net, _)| net)
        .collect()
}

/// Connectivity-driven frame inference: derive a full Layout IR — rails, anchor
/// columns, satellite cells/orientation by the spec's inference rules, and edge
/// ports — straight from the netlist + symbol pin geometry, so the engine owns the
/// whole layout and needs no LLM `place`. The coarse cells it emits are polished
/// by the same refine/align/decongest passes the LLM-frame path uses.
pub fn infer_ir(env: &KicadInstallation, design: &Design) -> LayoutIr {
    let Ok(items) = gather(env, design) else {
        return baseline_ir(design);
    };
    let inc = incidence(&items);

    // Rails: declared power nets, V+ on top, ground on bottom.
    let mut rails = BTreeMap::new();
    for (net, attrs) in &design.nets {
        if attrs.power {
            rails.insert(
                net.clone(),
                if is_ground(net) {
                    Band::Bottom
                } else {
                    Band::Top
                },
            );
        }
    }
    // Agent boards routinely NAME nets `GND`/`3V3`/`VBUS` but place NO `power:`
    // symbols, so `attrs.power` is unset and every supply net would route as a long
    // cross-sheet SIGNAL wire — the dominant source of the central rail knot. When the
    // design declares NO power symbols at all, infer the rails from net NAMES instead.
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

    // Ports: a net the author EXPLICITLY marked (a `label:global` component → the
    // `port` flag) OR — as a convenience — a single-pin signal net that obviously
    // exits the sheet. The explicit mark is what lets a degree-2+ output be a port;
    // the degree-1 rule alone cannot see it. Heuristic side: input-ish name left,
    // else right.
    let mut ports = BTreeMap::new();
    for (net, pins) in &inc {
        let attrs = design.nets.get(net);
        let power = attrs.map(|a| a.power).unwrap_or(false);
        let marked = attrs.map(|a| a.port).unwrap_or(false);
        // Not a no-connect (NC_*) and not a power rail (rails draw their own symbols).
        let nc = net.to_ascii_uppercase().starts_with("NC");
        if !power && !nc && (marked || pins.len() == 1) {
            let side = if net_is_input(net) {
                Side::Left
            } else {
                Side::Right
            };
            ports.insert(net.clone(), side);
        }
    }

    LayoutIr {
        rails,
        ports,
        trees: block_trees(design),
        rail_locals: local_rail_nets(design),
    }
}

/// Whether a net name reads as something entering the sheet rather than leaving it.
fn net_is_input(net: &str) -> bool {

    let u = net.to_ascii_uppercase();
    u.contains("IN")
        || u.contains("VIN")
        || u.contains("BUS")
        || u.contains("RX")
        || u.contains("TX_RAW")
}
