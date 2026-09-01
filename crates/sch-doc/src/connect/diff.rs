//! Comparing two extractions: what an edit did to the net partition.
//!
//! Partitions are identified by the pins they hold, never by their name: one
//! sheet can legitimately carry several distinct partitions under the same
//! name — two local labels with the same text that were never wired together,
//! hierarchical sheet pins, `#` synthetics — and keying by name would fuse
//! them and hide a rewiring between them.

use std::collections::HashMap;

use super::{Netlist, PinRef};

/// How the net partition changed across an edit. Every editing tool reports one.
///
/// The names in it are labels on the partitions that changed, not keys: the
/// same name may appear twice when two same-named partitions both moved.
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
    /// Pins that were on no net and now are on one. A wire that pulls a
    /// dangling pin onto a live net changes nothing else, so without this the
    /// delta would call that edit harmless.
    pub pins_now_connected: Vec<PinRef>,
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
            && self.pins_now_connected.is_empty()
    }
}

type PinKey = (String, u32, String);

fn key(pin: &PinRef) -> PinKey {
    (pin.refdes.clone(), pin.unit, pin.pin.clone())
}

/// Which partition each pin sits in, as an index into `netlist.nets`.
fn owners(netlist: &Netlist) -> HashMap<PinKey, usize> {
    netlist
        .nets
        .iter()
        .enumerate()
        .flat_map(|(idx, net)| net.pins.iter().map(move |pin| (key(pin), idx)))
        .collect()
}

/// For each partition, the distinct counterpart partitions its pins landed in.
fn images(netlist: &Netlist, other: &HashMap<PinKey, usize>) -> Vec<Vec<usize>> {
    netlist
        .nets
        .iter()
        .map(|net| {
            let mut hit: Vec<usize> = net
                .pins
                .iter()
                .filter_map(|p| other.get(&key(p)))
                .copied()
                .collect();
            hit.sort_unstable();
            hit.dedup();
            hit
        })
        .collect()
}

fn names(netlist: &Netlist, idx: &[usize]) -> Vec<String> {
    let mut out: Vec<String> = idx.iter().map(|&i| netlist.nets[i].name.clone()).collect();
    out.sort();
    out
}

impl Netlist {
    /// Compare two extractions of the same sheet.
    pub fn diff(before: &Netlist, after: &Netlist) -> NetDelta {
        let before_of = owners(before);
        let after_of = owners(after);
        let forward = images(before, &after_of);
        let backward = images(after, &before_of);

        let mut delta = NetDelta::default();
        for (idx, targets) in forward.iter().enumerate() {
            let name = &before.nets[idx].name;
            match targets.as_slice() {
                [] => delta.removed.push(name.clone()),
                [target] => {
                    let renamed = backward[*target].len() == 1 && &after.nets[*target].name != name;
                    if renamed {
                        delta
                            .renamed
                            .push((name.clone(), after.nets[*target].name.clone()));
                    }
                }
                _ => delta.split.push((name.clone(), names(after, targets))),
            }
        }
        for (idx, sources) in backward.iter().enumerate() {
            let name = &after.nets[idx].name;
            match sources.as_slice() {
                [] => delta.created.push(name.clone()),
                [_] => {}
                _ => delta.merged.push((names(before, sources), name.clone())),
            }
        }
        for net in &before.nets {
            for pin in &net.pins {
                if !after_of.contains_key(&key(pin)) {
                    delta.pins_now_unconnected.push(pin.clone());
                }
            }
        }
        for net in &after.nets {
            for pin in &net.pins {
                if !before_of.contains_key(&key(pin)) {
                    delta.pins_now_connected.push(pin.clone());
                }
            }
        }
        delta.created.sort();
        delta.removed.sort();
        delta.merged.sort();
        delta.split.sort();
        delta.renamed.sort();
        delta.pins_now_unconnected.sort();
        delta.pins_now_connected.sort();
        delta
    }
}
