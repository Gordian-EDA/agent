//! Comparing two extractions: what an edit did to the net partition.

use std::collections::HashMap;

use super::{Netlist, PinRef};

/// How the net partition changed across an edit. Every editing tool reports one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetDelta {
    /// Nets that exist only after.
    pub created: Vec<String>,
    /// Nets that existed only before.
    pub removed: Vec<String>,
    /// `(before names, after name)` for partitions that fused.
    pub merged: Vec<(Vec<String>, String)>,
    /// `(before name, after names)` for partitions that broke apart.
    pub split: Vec<(String, Vec<String>)>,
    /// Same pins, different name.
    pub renamed: Vec<(String, String)>,
    /// Pins that were on a net and now are on none.
    pub pins_now_unconnected: Vec<PinRef>,
}

impl NetDelta {
    /// Whether the edit left connectivity untouched.
    pub fn is_empty(&self) -> bool {
        self.created.is_empty()
            && self.removed.is_empty()
            && self.merged.is_empty()
            && self.split.is_empty()
            && self.renamed.is_empty()
            && self.pins_now_unconnected.is_empty()
    }
}

type PinKey = (String, u32, String);

fn pin_to_net(netlist: &Netlist) -> HashMap<PinKey, &str> {
    netlist
        .nets
        .iter()
        .flat_map(|net| {
            net.pins.iter().map(move |p| {
                (
                    (p.refdes.clone(), p.unit, p.pin.clone()),
                    net.name.as_str(),
                )
            })
        })
        .collect()
}

/// Distinct counterpart net names each net's pins landed in, in sorted order.
fn images<'a>(
    netlist: &'a Netlist,
    other: &HashMap<PinKey, &'a str>,
) -> HashMap<&'a str, Vec<String>> {
    let mut out: HashMap<&str, Vec<String>> = HashMap::new();
    for net in &netlist.nets {
        let entry = out.entry(net.name.as_str()).or_default();
        for pin in &net.pins {
            if let Some(name) = other.get(&(pin.refdes.clone(), pin.unit, pin.pin.clone()))
                && !entry.iter().any(|n| n == name)
            {
                entry.push((*name).to_string());
            }
        }
        entry.sort();
    }
    out
}

impl Netlist {
    /// Compare two extractions of the same sheet.
    pub fn diff(before: &Netlist, after: &Netlist) -> NetDelta {
        let before_of = pin_to_net(before);
        let after_of = pin_to_net(after);
        let forward = images(before, &after_of);
        let backward = images(after, &before_of);

        let mut delta = NetDelta::default();
        for (name, targets) in &forward {
            match targets.len() {
                0 => delta.removed.push((*name).to_string()),
                1 => {
                    let target = &targets[0];
                    let sources = backward.get(target.as_str()).cloned().unwrap_or_default();
                    if sources.len() == 1 && target != name {
                        delta.renamed.push(((*name).to_string(), target.clone()));
                    }
                }
                _ => delta.split.push(((*name).to_string(), targets.clone())),
            }
        }
        for (name, sources) in &backward {
            match sources.len() {
                0 => delta.created.push((*name).to_string()),
                1 => {}
                _ => delta.merged.push((sources.clone(), (*name).to_string())),
            }
        }
        for net in &before.nets {
            for pin in &net.pins {
                if !after_of.contains_key(&(pin.refdes.clone(), pin.unit, pin.pin.clone())) {
                    delta.pins_now_unconnected.push(pin.clone());
                }
            }
        }
        delta.created.sort();
        delta.removed.sort();
        delta.merged.sort();
        delta.split.sort();
        delta.renamed.sort();
        delta.pins_now_unconnected.sort();
        delta
    }
}
