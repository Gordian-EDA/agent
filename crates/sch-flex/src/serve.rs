//! Which part a support part SERVES, and on which side of it.
//!
//! A decoupler, a pull-up, a reset cap, a crystal load cap: two pins, one on a rail, the
//! other on a net that reaches one unambiguous part. A human draws it beside the pin it
//! serves — a five-millimetre wire, and the node never needs a name. This is the relation
//! that says which pin that is, so the typesetter can seat the part there whenever the
//! author left it no place of its own.
//!
//! A net that reaches two devices is a signal, not a service: nobody can seat one cap
//! beside both ends, and the humans do not try.

use std::collections::{BTreeMap, BTreeSet};

use circuit_graph::netclass::is_power_net;
use geom::Dir;
use sch_model::geometry::quantize_dir;
use sch_model::item::Item;

/// A support part's service: the part it serves, and where beside it the support goes.
pub(crate) struct Serves {
    pub served: usize,
    pub seat: Seat,
    /// How far DOWN the served part the pin sits, with the symbol upright. A column seated
    /// in this order reaches its pins in order, so its wires do not cross each other.
    pub line: f64,
}

/// Where a support part sits relative to the part it serves.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Seat {
    /// In the column against the side of a DEVICE that its pin leaves from — the caps a
    /// human stacks along the edge of an MCU.
    Beside(Dir),
    /// In one row after the device: a bank of decoupling caps, which touch no signal pin
    /// and so have no pin line to sit on. Nothing about where it stands makes a wire, so
    /// it takes the shape that costs the drawing least — a row beside the device it
    /// supports, which is where a human writes it.
    Bank,
    /// In the next seat of the row, hanging off the node — how a leg off a chain of
    /// two-pin parts is drawn. A column there would buy a whole column's width for one
    /// part that only ever needed the seat next door.
    Next,
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
        let links: Vec<(usize, &str)> = others
            .iter()
            .copied()
            .filter(|(j, _)| links_a_chain(&items[*j]))
            .collect();
        let devices: Vec<(usize, &str)> = others
            .iter()
            .copied()
            .filter(|(j, _)| items[*j].pins.len() >= 3)
            .collect();
        // A part the signal passes THROUGH wins over the device at the end of the net: a
        // crystal's load cap reaches the crystal AND the MCU, and the pair a human draws
        // is the cap with its crystal. Anything else has to be unambiguous.
        let (served, pin) = match (links.len(), devices.len(), others.len()) {
            (1, _, _) => links[0],
            (0, 1, _) => devices[0],
            (0, 0, 1) => others[0],
            _ => continue,
        };
        if !here.contains(&served) {
            continue;
        }
        let Some(geom) = items[served].geom.pins.iter().find(|p| p.number == pin) else {
            continue;
        };
        let seat = match links_a_chain(&items[served]) {
            true => Seat::Next,
            false => Seat::Beside(quantize_dir(geom.angle, 0.0, false)),
        };
        out.insert(
            i,
            Serves {
                served,
                seat,
                line: -geom.at.y,
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

/// A part the signal passes THROUGH: two pins, or a discrete whose only other pins are
/// rails — a crystal, shield pins and all. A support part belongs beside one of these
/// before it belongs beside the device at the far end of the net.
fn links_a_chain(item: &Item) -> bool {
    let rails = |net: &Option<String>| net.as_deref().is_some_and(is_power_net);
    let signal = item.pins.iter().filter(|(_, _, net)| !rails(net)).count();
    signal == 2 && (item.pins.len() == 2 || item.part.starts_with("Device:"))
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
