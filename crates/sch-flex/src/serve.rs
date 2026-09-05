//! Which part a support part SERVES, and on which side of it.
//!
//! A decoupler, a pull-up, a reset cap, a crystal load cap: two pins, one on a rail, the
//! other on a net that reaches exactly one device. A human draws it beside the pin it
//! serves — a five-millimetre wire, and the node never needs a name. This is the relation
//! that says which pin that is, so the typesetter can seat the part there whenever the
//! author left it no place of its own.
//!
//! A net that reaches TWO devices is a signal, not a service: nobody can seat one cap
//! beside both ends, and the humans do not try.

use std::collections::{BTreeMap, BTreeSet};

use circuit_graph::netclass::is_power_net;
use geom::Dir;
use sch_model::geometry::quantize_dir;
use sch_model::item::Item;

/// A support part's service: the part it serves and the side of that part its pin leaves
/// from, with the symbol upright.
pub(crate) struct Serves {
    pub served: usize,
    pub side: Dir,
}

/// A device is a part a chain ENDS at rather than passes through — what a support part
/// can be seated beside.
fn is_device(item: &Item) -> bool {
    item.pins.len() >= 3
}

/// Each member of the block that plainly serves one pin of another member, and which pin.
pub(crate) fn serving(items: &[Item], members: &[usize]) -> BTreeMap<usize, Serves> {
    let mut on_net: BTreeMap<&str, Vec<(usize, &str)>> = BTreeMap::new();
    for (i, item) in items.iter().enumerate() {
        for (number, _, net) in &item.pins {
            if let Some(net) = net.as_deref() {
                on_net.entry(net).or_default().push((i, number.as_str()));
            }
        }
    }
    let here: BTreeSet<usize> = members.iter().copied().collect();
    let mut out: BTreeMap<usize, Serves> = BTreeMap::new();
    for &i in members {
        let Some(net) = served_net(&items[i]) else {
            continue;
        };
        let others: Vec<(usize, &str)> = on_net
            .get(net)
            .into_iter()
            .flatten()
            .copied()
            .filter(|(j, _)| *j != i)
            .collect();
        let devices: Vec<(usize, &str)> = others
            .iter()
            .copied()
            .filter(|(j, _)| is_device(&items[*j]))
            .collect();
        let (served, pin) = match (devices.len(), others.len()) {
            (1, _) => devices[0],
            (0, 1) => others[0],
            _ => continue,
        };
        if !here.contains(&served) {
            continue;
        }
        let Some(geom) = items[served].geom.pins.iter().find(|p| p.number == pin) else {
            continue;
        };
        out.insert(
            i,
            Serves {
                served,
                side: quantize_dir(geom.angle, 0.0, false),
            },
        );
    }
    // A support part serving another support part is a chain — a divider, an LED and its
    // resistor. Seating one inside the other nests the drawing and leaves neither beside
    // its own pin, so only the outermost service is kept.
    let servers: BTreeSet<usize> = out.keys().copied().collect();
    out.retain(|_, s| !servers.contains(&s.served));
    out
}

/// The net a two-pin part's non-rail pin sits on, when its other pin sits on a rail.
fn served_net(item: &Item) -> Option<&str> {
    if item.pins.len() != 2 {
        return None;
    }
    let rail = |net: &Option<String>| net.as_deref().is_some_and(is_power_net);
    let rails = item.pins.iter().filter(|(_, _, net)| rail(net)).count();
    if rails != 1 {
        return None;
    }
    item.pins
        .iter()
        .find(|(_, _, net)| !rail(net))
        .and_then(|(_, _, net)| net.as_deref())
}
