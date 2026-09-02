//! `decouple` sugar: "give this IC N caps of value V across its rails".
//!
//! Model-level, not syntax — the bulk-create tool
//! expand it the same way — so it lives with the model. The caps are ordinary
//! [`Component`]s tagged [`Origin::Synthesized`], which is what lets a round trip
//! re-collapse them onto their parent.

use circuit_graph::netclass::{is_ground, is_power_net};
use indexmap::IndexMap;

use crate::model::*;
use crate::{Diagnostic, PinType, SymbolTable};

/// Supply/return pairs a component's decoupling caps go across.
pub struct Rails {
    per_net: Vec<RailPair>,
    per_pin: Vec<RailPair>,
}

#[derive(Clone)]
struct RailPair {
    vdd: NetName,
    gnd: NetName,
}

struct ConnectedPowerPin<'a> {
    index: usize,
    unit: u8,
    net: &'a NetName,
}

/// Find the rails to decouple from the symbol's connected power-input pins.
/// Each distinct supply net gets a pair, and repeated supply pins are retained
/// so an explicit count can request one capacitor per physical pin. A ground in
/// the same unit and nearest in symbol-table order is preferred. Symbols with no
/// power-input metadata fall back to the conventional pin-name heuristic.
pub fn rails(refdes: &str, comp: &Component, provider: &SymbolTable) -> Result<Rails, Diagnostic> {
    let Some(meta) = provider.symbol(&comp.part) else {
        return fallback_rails(refdes, comp, None);
    };
    let power_inputs: Vec<ConnectedPowerPin<'_>> = meta
        .pins
        .iter()
        .enumerate()
        .filter(|(_, pin)| pin.etype == PinType::PowerInput)
        .filter_map(|(index, pin)| {
            let PinTarget::Net(net) = pin_target(comp, &pin.number)? else {
                return None;
            };
            Some(ConnectedPowerPin {
                index,
                unit: pin.unit,
                net,
            })
        })
        .collect();
    if !meta.pins.iter().any(|pin| pin.etype == PinType::PowerInput) {
        return fallback_rails(refdes, comp, Some(&meta));
    }

    let grounds: Vec<&ConnectedPowerPin<'_>> = power_inputs
        .iter()
        .filter(|pin| is_ground(pin.net))
        .collect();
    let supplies: Vec<&ConnectedPowerPin<'_>> = power_inputs
        .iter()
        .filter(|pin| is_power_net(pin.net) && !is_ground(pin.net))
        .collect();
    if supplies.is_empty() || grounds.is_empty() {
        return Err(ambiguous(
            refdes,
            "power_in pins classified with power-net names",
            distinct_nets(&supplies),
            distinct_nets(&grounds),
        ));
    }

    let per_pin: Vec<RailPair> = supplies
        .iter()
        .map(|supply| {
            let ground = grounds
                .iter()
                .min_by_key(|ground| {
                    (
                        usize::from(supply.unit != ground.unit),
                        supply.index.abs_diff(ground.index),
                    )
                })
                .expect("non-empty ground candidates");
            RailPair {
                vdd: supply.net.clone(),
                gnd: ground.net.clone(),
            }
        })
        .collect();
    let mut per_net = IndexMap::<NetName, RailPair>::new();
    for pair in &per_pin {
        per_net
            .entry(pair.vdd.clone())
            .or_insert_with(|| pair.clone());
    }
    Ok(Rails {
        per_net: per_net.into_values().collect(),
        per_pin,
    })
}

fn pin_target<'a>(comp: &'a Component, number: &str) -> Option<&'a PinTarget> {
    comp.pins
        .get(number)
        .or_else(|| comp.units.values().find_map(|unit| unit.get(number)))
}

fn distinct_nets(pins: &[&ConnectedPowerPin<'_>]) -> Vec<NetName> {
    let mut nets: Vec<NetName> = pins.iter().map(|pin| pin.net.clone()).collect();
    nets.sort();
    nets.dedup();
    nets
}

fn fallback_rails(
    refdes: &str,
    comp: &Component,
    meta: Option<&crate::SymbolMeta>,
) -> Result<Rails, Diagnostic> {
    let pin_name = |key: &str| -> String {
        meta.and_then(|meta| crate::pins::resolve(meta, key).first().copied())
            .map(|pin| pin.name.clone())
            .unwrap_or_else(|| key.to_string())
    };
    let matching = |prefixes: &[&str]| -> Vec<NetName> {
        let mut nets: Vec<NetName> = comp
            .pins
            .iter()
            .chain(comp.units.values().flatten())
            .filter(|(k, _)| {
                let k = pin_name(k).to_ascii_uppercase();
                prefixes.iter().any(|p| k.starts_with(p))
            })
            .filter_map(|(_, t)| match t {
                PinTarget::Net(n) => Some(n.clone()),
                PinTarget::NoConnect => None,
            })
            .collect();
        nets.sort();
        nets.dedup();
        nets
    };
    let vdd = matching(&["VDD", "VCC"]);
    let gnd = matching(&["VSS", "GND"]);
    if vdd.len() != 1 || gnd.len() != 1 {
        return Err(ambiguous(
            refdes,
            "no power_in pins; fallback looked for VDD*/VCC* and VSS*/GND* pin names",
            vdd,
            gnd,
        ));
    }
    Ok(Rails {
        per_net: vec![RailPair {
            vdd: vdd[0].clone(),
            gnd: gnd[0].clone(),
        }],
        per_pin: Vec::new(),
    })
}

fn ambiguous(
    refdes: &str,
    source: &str,
    supplies: Vec<NetName>,
    grounds: Vec<NetName>,
) -> Diagnostic {
    Diagnostic::error(
        "decouple-ambiguous",
        format!(
            "{refdes}: decouple {source}; needs supply and ground candidates \
             (found {supplies:?} / {grounds:?}) — write the caps explicitly"
        ),
    )
}

/// The caps `values` (value → count) asks for, keyed `__dec_<parent>_<n>`.
///
/// Values are expanded in sorted order so a canonical round trip assigns the
/// same `Origin::Synthesized { index }` however the author ordered them.
pub fn expand(
    refdes: &str,
    values: &IndexMap<String, u32>,
    rails: &Rails,
) -> Vec<(RefDes, Component)> {
    let mut entries: Vec<(&String, &u32)> = values.iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    let mut out = Vec::new();
    let mut index = 0u32;
    for (value, count) in entries {
        let count = usize::try_from(*count)
            .unwrap_or(usize::MAX)
            .max(rails.per_net.len());
        let repeated_pins = rails.per_pin.iter().enumerate().filter_map(|(i, pair)| {
            rails.per_pin[..i]
                .iter()
                .any(|earlier| earlier.vdd == pair.vdd)
                .then_some(pair)
        });
        let pairs = rails.per_net.iter().chain(repeated_pins);
        let pairs: Vec<&RailPair> = pairs.collect();
        for pair in pairs.iter().cycle().take(count) {
            index += 1;
            let mut cap = Component {
                part: "Device:C".into(),
                value: Some(value.clone()),
                // A synthesized decoupler is a physical part the board needs a
                // footprint for and the author never sees it to assign one.
                footprint: Some("Capacitor_SMD:C_0402_1005Metric".into()),
                origin: Origin::Synthesized {
                    parent: refdes.to_string(),
                    role: "decouple".into(),
                    index,
                },
                ..Default::default()
            };
            cap.pins
                .insert("1".into(), PinTarget::Net(pair.vdd.clone()));
            cap.pins
                .insert("2".into(), PinTarget::Net(pair.gnd.clone()));
            out.push((format!("__dec_{refdes}_{index}"), cap));
        }
    }
    out
}

/// Give every synthesized decoupling cap a real `C<n>` refdes, continuing after
/// the highest authored `C`.
///
/// The `__dec_*` key is what re-collapses the caps onto their parent, but it is
/// also what the writer stamps as the KiCAD refdes — so rename late, keeping
/// each cap's `Origin` (identity lives there, not in the name). Deterministic
/// (block then component order, one global counter), so the round trip stays a
/// fixpoint.
pub fn renumber(d: &mut Design) {
    let is_synth =
        |c: &Component| matches!(&c.origin, Origin::Synthesized { role, .. } if role == "decouple");
    let mut next = 1 + d
        .blocks
        .values()
        .flat_map(|b| b.components.keys())
        .filter_map(|k| k.strip_prefix('C').and_then(|n| n.parse::<u32>().ok()))
        .max()
        .unwrap_or(0);
    for block in d.blocks.values_mut() {
        let synth: Vec<RefDes> = block
            .components
            .iter()
            .filter(|(_, c)| is_synth(c))
            .map(|(k, _)| k.clone())
            .collect();
        for old in synth {
            if let Some(comp) = block.components.shift_remove(&old) {
                block.components.insert(format!("C{next}"), comp);
                next += 1;
            }
        }
    }
}
