//! Semantic lints over the kernel model.
//!
//! Every rule here judges what the circuit *means*, so it runs the same over a
//! design extracted from a live `.kicad_sch` as over one compiled from text. The
//! spelling checks that only an authored document can fail — an unknown lib_id,
//! an unknown pin key, one physical pin claimed by two map keys — belong to the
//! front end that accepted the text (`circuit_lang::authored`), not here:
//! a live document names its symbols and pins by construction.

use crate::diag::{Diagnostic, Diagnostics};
use crate::model::*;
use crate::{PinType, SymbolTable};
use circuit_graph::netclass::is_power_net as is_power_like_net_name;

pub fn lint(d: &Design, provider: &SymbolTable) -> Diagnostics {
    let mut diags = Diagnostics::default();
    let mut net_pins: indexmap::IndexMap<&str, Vec<String>> = indexmap::IndexMap::new();
    let mut net_parts: indexmap::IndexMap<&str, Vec<String>> = indexmap::IndexMap::new();
    let mut net_blocks: indexmap::IndexMap<&str, std::collections::BTreeSet<String>> =
        indexmap::IndexMap::new();
    let mut net_power_inputs: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();
    let mut net_power_sources: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();
    let mut passive_fuse_links: Vec<(String, String, String)> = Vec::new();
    // Net-sanity: nets carrying a crystal/oscillator pin, and nets carrying a
    // reset/boot control pin. A net with BOTH shorts the oscillator to reset —
    // almost always a mis-wire (an OSC_OUT net the model named after a reset pin).
    let mut osc_nets: std::collections::BTreeSet<String> = Default::default();
    let mut ctrl_pins: indexmap::IndexMap<String, Vec<String>> = Default::default();

    for (bname, block) in &d.blocks {
        for (refdes, comp) in block.components.iter() {
            let all_pins = comp.pins.iter().chain(comp.units.values().flatten());
            for (key, target) in all_pins.clone() {
                if let PinTarget::Net(n) = target {
                    net_blocks
                        .entry(n.as_str())
                        .or_default()
                        .insert(bname.clone());
                    net_pins
                        .entry(n.as_str())
                        .or_default()
                        .push(format!("{refdes}.{key}"));
                    net_parts
                        .entry(n.as_str())
                        .or_default()
                        .push(comp.part.clone());
                }
            }

            // A part the symbol table cannot resolve is not a defect here — the
            // document may legitimately carry a symbol from a library this run
            // did not load. Its pin-typed checks are simply unavailable.
            let Some(meta) = provider.symbol(&comp.part) else {
                continue;
            };

            if let Some((a, b)) = passive_series_fuse_nets(comp, &meta) {
                passive_fuse_links.push((a, b, refdes.clone()));
            }

            // Resolve each map key to physical pins: exact number, else name.
            // A key that resolves to nothing is silently ignored (see the module
            // docs); the checks below only need the pins that do resolve.
            let mut covered: std::collections::HashMap<&str, &str> = Default::default(); // number -> key
            for (key, _) in all_pins.clone() {
                for p in crate::pins::resolve(&meta, key) {
                    covered.insert(&p.number, key);
                    if let Some(PinTarget::Net(n)) = pin_target_for(comp, key) {
                        match p.etype {
                            PinType::PowerInput => {
                                net_power_inputs
                                    .entry(n.clone())
                                    .or_default()
                                    .push(format!("{refdes}.{}", p.name));
                            }
                            PinType::PowerOutput => {
                                net_power_sources
                                    .entry(n.clone())
                                    .or_default()
                                    .push(format!("{refdes}.{}", p.name));
                            }
                            _ => {}
                        }
                        if comp.part.starts_with("power:") || is_connector_part(&comp.part) {
                            net_power_sources
                                .entry(n.clone())
                                .or_default()
                                .push(refdes.to_string());
                        }
                    }
                }
            }

            // Every power-input pin must be covered AND on a net.
            for p in meta.pins.iter().filter(|p| p.etype == PinType::PowerInput) {
                let on_net = covered.get(p.number.as_str()).is_some_and(|key| {
                    pin_target_for(comp, key).is_some_and(|t| matches!(t, PinTarget::Net(_)))
                });
                if !on_net {
                    diags.push(Diagnostic::error(
                        "power-pin-unconnected",
                        format!(
                            "{refdes}: power-input pin {} ({}) is not connected to a net",
                            p.number, p.name
                        ),
                    ));
                }
            }

            // Record oscillator nets (any pin of a crystal/resonator) and reset/boot
            // control pins (by resolved pin NAME) for the net-sanity check below.
            let is_osc = comp.part.contains("Crystal") || comp.part.contains("Resonator");
            for (key, target) in all_pins.clone() {
                let PinTarget::Net(n) = target else { continue };
                if is_osc {
                    osc_nets.insert(n.clone());
                }
                let pin_name = crate::pins::resolve(&meta, key)
                    .first()
                    .map(|p| p.name.as_str())
                    .unwrap_or(key.as_str());
                let u = pin_name.to_ascii_uppercase();
                if matches!(u.as_str(), "NRST" | "RESET" | "RST" | "NMI") || u.starts_with("BOOT") {
                    ctrl_pins
                        .entry(n.clone())
                        .or_default()
                        .push(format!("{refdes}.{pin_name}"));
                }
            }
        }
    }

    // A crystal/oscillator net that also carries a reset/boot pin is almost certainly
    // a mis-wire — the oscillator output shorted to reset. Surface it so the design is
    // corrected rather than shipping a non-oscillating board.
    if !d.lint_allow.contains("osc-reset-short") {
        for net in &osc_nets {
            if let Some(rpins) = ctrl_pins.get(net) {
                diags.push(Diagnostic::warning(
                    "osc-reset-short",
                    format!(
                        "net `{net}` joins a crystal/oscillator pin and a reset/boot pin ({}) — \
                         likely a mis-wire shorting the oscillator to a control pin",
                        rpins.join(", ")
                    ),
                ));
            }
        }
    }

    let allow = |code: &str| d.lint_allow.contains(code);

    // Blocks are independently placed schematic regions. Splitting a wide signal
    // bank by component category turns every channel into off-region labels and can
    // overwhelm both placement and human review. Flag only a strong signal: at least
    // twelve distinct non-power nets crossing the same pair of blocks.
    if !allow("fragmented-block-floorplan") {
        let mut pair_nets: std::collections::BTreeMap<(String, String), Vec<&str>> =
            Default::default();
        for (net, blocks) in &net_blocks {
            let upper = net.to_ascii_uppercase();
            if is_power_like_net_name(net) || upper == "GND" || upper.starts_with("GND") {
                continue;
            }
            let blocks = blocks.iter().collect::<Vec<_>>();
            for i in 0..blocks.len() {
                for j in (i + 1)..blocks.len() {
                    pair_nets
                        .entry((blocks[i].clone(), blocks[j].clone()))
                        .or_default()
                        .push(net);
                }
            }
        }
        for ((a, b), nets) in pair_nets {
            if nets.len() >= 12 {
                diags.push(Diagnostic::warning(
                    "fragmented-block-floorplan",
                    format!(
                        "blocks `{a}` and `{b}` share {} signal nets ({}) — keep the repeated channel bank/end-to-end paths in one block to avoid a label-only fragmented floorplan",
                        nets.len(),
                        nets.iter().take(6).copied().collect::<Vec<_>>().join(", ")
                    ),
                ));
            }
        }
    }

    // A connector or regulator can legitimately feed a named rail through a
    // fuse. Passive fuse symbols do not declare a power-output pin, so carry
    // source status across their two terminals explicitly. Keep this confined
    // to exact fuse symbol classes whose library metadata is two passive pins;
    // resistors, beads, and other arbitrary series parts must not suppress the
    // warning. Iterating also handles the uncommon but valid series-fuse chain.
    let mut changed = true;
    while changed {
        changed = false;
        for (a, b, refdes) in &passive_fuse_links {
            let a_sourced = net_power_sources.contains_key(a);
            let b_sourced = net_power_sources.contains_key(b);
            if a_sourced && !b_sourced {
                net_power_sources
                    .entry(b.clone())
                    .or_default()
                    .push(format!("{refdes} (through fuse from {a})"));
                changed = true;
            } else if b_sourced && !a_sourced {
                net_power_sources
                    .entry(a.clone())
                    .or_default()
                    .push(format!("{refdes} (through fuse from {b})"));
                changed = true;
            }
        }
    }

    if !allow("control-passive-island") {
        for (net, parts) in &net_parts {
            let attrs = d.nets.get(*net);
            let exempt = attrs.map(|a| a.power || a.port).unwrap_or(false);
            let pins = net_pins.get(net).map(Vec::as_slice).unwrap_or(&[]);
            if exempt || pins.len() < 2 || !is_control_net_name(net) {
                continue;
            }
            if parts.iter().all(|part| is_passive_control_part(part)) {
                diags.push(Diagnostic::warning(
                    "control-passive-island",
                    format!(
                        "control net `{net}` only touches passive/switch parts ({}) — likely missing an MCU/control pin connection",
                        pins.join(", ")
                    ),
                ));
            }
        }
    }

    if !allow("unsourced-power-net") {
        for (net, consumers) in &net_power_inputs {
            let attrs = d.nets.get(net.as_str());
            if !attrs.map(|a| a.power).unwrap_or(false) && !is_power_like_net_name(net) {
                continue;
            }
            if net_power_sources
                .get(net)
                .is_some_and(|sources| !sources.is_empty())
            {
                continue;
            }
            diags.push(Diagnostic::warning(
                "unsourced-power-net",
                format!(
                    "power net `{net}` feeds power-input pins ({}) but has no power symbol, connector, or power-output pin source",
                    consumers.join(", ")
                ),
            ));
        }
    }

    if !allow("single-pin-net") {
        for (net, pins) in &net_pins {
            // A power rail or an author-marked PORT legitimately has one pin (the
            // symbol/label is the connection) — not a typo, so don't warn on it.
            let attrs = d.nets.get(*net);
            let exempt = attrs.map(|a| a.power || a.port).unwrap_or(false);
            if pins.len() == 1 && !exempt {
                diags.push(Diagnostic::warning(
                    "single-pin-net",
                    format!("net `{net}` has only one pin ({}) — typo?", pins[0]),
                ));
            }
        }
    }
    if !allow("near-name") {
        // Compare against the dedup union of referenced and declared-only nets,
        // so a typo'd `nets:` entry one edit away from a wired net still warns.
        let mut names: Vec<&str> = net_pins.keys().copied().collect();
        for net in d.nets.keys() {
            if !net_pins.contains_key(net.as_str()) {
                names.push(net.as_str());
            }
        }
        // A genuine typo forks a net, leaving one side with 0–1 pins. When BOTH
        // nets have ≥2 pins they are deliberate (GPIO buses: PA0/PA1/…), and
        // warning floods real designs — skip those pairs.
        let pin_count = |n: &str| net_pins.get(n).map_or(0, Vec::len);
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                if pin_count(a) >= 2 && pin_count(b) >= 2 {
                    continue;
                }
                if strsim::levenshtein(a, b) == 1 {
                    diags.push(Diagnostic::warning(
                        "near-name",
                        format!("nets `{a}` and `{b}` differ by one character — intentional?"),
                    ));
                }
            }
        }
    }
    if !allow("unreferenced-net") {
        for (net, _) in &d.nets {
            if !net_pins.contains_key(net.as_str()) {
                diags.push(Diagnostic::warning(
                    "unreferenced-net",
                    format!("net `{net}` is declared in `nets:` but no pin references it"),
                ));
            }
        }
    }
    diags
}

fn is_control_net_name(net: &str) -> bool {
    let u = net.to_ascii_uppercase();
    u.contains("BOOT") || u.contains("RESET") || u.contains("RST") || u.contains("RUN")
}

fn is_passive_control_part(part: &str) -> bool {
    part.starts_with("Switch:")
        || matches!(
            part,
            "Device:R"
                | "Device:C"
                | "Device:L"
                | "Device:FerriteBead"
                | "Device:FerriteBead_Small"
        )
}

fn is_connector_part(part: &str) -> bool {
    part.starts_with("Connector:")
}

/// Return the two nets joined by an ordinary passive fuse symbol.
///
/// Match the exact symbol class, then verify the resolved library metadata and
/// authored connections. This intentionally excludes fuzzy names such as
/// `FuseHolder` and non-passive/polarized fuse symbols (whose power-output pin
/// already participates in normal source detection).
fn passive_series_fuse_nets(
    comp: &Component,
    meta: &crate::SymbolMeta,
) -> Option<(String, String)> {
    if !matches!(
        comp.part.as_str(),
        "Device:Fuse" | "Device:Fuse_Small" | "Device:Polyfuse" | "Device:Polyfuse_Small"
    ) {
        return None;
    }

    let mut physical_pins: indexmap::IndexMap<&str, &crate::PinMeta> = indexmap::IndexMap::new();
    for pin in &meta.pins {
        physical_pins.entry(pin.number.as_str()).or_insert(pin);
    }
    if physical_pins.len() != 2
        || physical_pins
            .values()
            .any(|pin| pin.etype != PinType::Passive)
    {
        return None;
    }

    let mut nets = Vec::with_capacity(2);
    for pin in physical_pins.values() {
        let target = comp
            .pins
            .iter()
            .chain(comp.units.values().flatten())
            .find(|(key, _)| *key == &pin.number || *key == &pin.name)
            .map(|(_, target)| target)?;
        let PinTarget::Net(net) = target else {
            return None;
        };
        nets.push(net.clone());
    }
    (nets[0] != nets[1]).then(|| (nets[0].clone(), nets[1].clone()))
}

/// Resolve a component pin map key to its `PinTarget`, searching the top-level
/// pin map first and then any unit pin maps.
fn pin_target_for<'a>(comp: &'a Component, key: &str) -> Option<&'a PinTarget> {
    comp.pins
        .get(key)
        .or_else(|| comp.units.values().find_map(|u| u.get(key)))
}
