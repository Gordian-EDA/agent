//! Which part a support part SERVES, and on which side of it.
//!
//! A decoupler, a pull-up, a reset cap: two pins, one on a rail, and the other alone on a
//! net with one device. A human draws it beside the pin it serves — a five-millimetre
//! wire, and the node never needs a name. This is the relation that says which pin that
//! is, so the typesetter can seat the part there whenever the author left it no place of
//! its own.
//!
//! A net that reaches anything ELSE as well is a node, not a service: nobody can seat one
//! part beside both ends of it, and moving it to one end is what turns a node somebody had
//! already drawn as wire into a pair of labels.

use std::collections::{BTreeMap, BTreeSet};

use circuit_graph::netclass::{is_connector_like, is_power_net};
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
        // Exactly one other part on the net, or this is a NODE rather than a service:
        // a pull-up that also feeds a motor, a cap between a crystal and its MCU. Seating
        // the part beside one of them tears it away from the other, and a node already
        // drawn as wire comes apart into a label pair.
        let [(served, pin)] = others[..] else {
            continue;
        };
        if !here.contains(&served) {
            continue;
        }
        let Some(geom) = items[served].geom.pins.iter().find(|p| p.number == pin) else {
            continue;
        };
        if !seatable_against(&items[served]) {
            continue;
        }
        out.insert(
            i,
            Serves {
                served,
                seat: Seat::Beside(quantize_dir(geom.angle, 0.0, false)),
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

/// Whether a support part can be seated against this one at all.
///
/// A DEVICE: enough pins to have sides, and not a part people plug into or press. A part
/// the signal passes THROUGH — two pins, or a discrete whose only other pins are rails, a
/// crystal shield and all — keeps its place in the chain it links, and seating something
/// against it only reorders that chain. Nor is a MECHANICAL part a device: a connector, a
/// jumper, a switch. The typesetter deliberately never puts a column on a connector's pin
/// lines, so a part seated against one lands beside nothing and takes its old neighbours'
/// drawing with it — and nobody stacks decoupling caps along the edge of a switch either.
fn seatable_against(item: &Item) -> bool {
    let rails = |net: &Option<String>| net.as_deref().is_some_and(is_power_net);
    let signal = item.pins.iter().filter(|(_, _, net)| !rails(net)).count();
    let links_a_chain = signal == 2 && (item.pins.len() == 2 || item.part.starts_with("Device:"));
    let mechanical = is_connector_like(&item.part)
        || item.part.starts_with("Switch:")
        || item.refdes.starts_with('J')
        || item.refdes.starts_with("SW");
    item.pins.len() >= 3 && !links_a_chain && !mechanical
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
