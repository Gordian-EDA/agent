//! Chain contraction: collapse the pin-level netlist into a reduced multigraph of
//! NODES (anchors, connectors, rails, junctions) joined by CHAINS — maximal runs of
//! series 2-pin parts. This is the parse tree of the schematic grammar: every
//! 2-pin part belongs to exactly one chain, and the classic part "roles" fall out
//! of a chain's terminals instead of per-part heuristics:
//!
//! - supply ↔ ground, one part      = decoupling / bulk cap
//! - anything ↔ rail                = shunt leg (drawn vertical, corpus law)
//! - non-rail ↔ non-rail            = series element (drawn along the signal flow)

use std::collections::BTreeMap;

use sch_place::item::{Incidence, Item};
use sch_place::netclass::is_connector_like;

use crate::net::NetClass;

/// A vertex of the reduced graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum NodeKind {
    /// A placed multi-pin part (item index): IC, connector, or a part with a
    /// single connected pin (test point, jumper stub).
    Part(usize),
    /// A power net (ground or supply): one logical node no matter how many pins.
    Rail(String),
    /// A signal net acting as a fan-out point (3+ attachments, a dangling end,
    /// or a chain cycle seam) that chains radiate from.
    Junction(String),
}

/// Where a chain end lands: the node, plus the pin (for `Part` nodes) and the
/// net the last segment runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub node: usize,
    /// Pin number on the `Part` node this end attaches to (empty for rails/junctions).
    pub pin: String,
    /// The net of the chain's last segment on this side.
    pub net: String,
}

/// A maximal run of series 2-pin parts between two terminals. `parts` may be
/// empty: a bare net between two multi-pin parts is a 0-part chain.
#[derive(Debug, Clone)]
pub struct Chain {
    pub a: Terminal,
    pub b: Terminal,
    /// Item indices in order from `a` to `b`.
    pub parts: Vec<usize>,
    /// Nets along the run, `parts.len() + 1` entries: `nets[0]` touches `a`,
    /// `nets[last]` touches `b`.
    pub nets: Vec<String>,
}

/// What a chain is, judged by its two terminals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainRole {
    /// Both ends on rails: decoupling / bulk / divider-without-tap.
    RailToRail,
    /// Exactly one end on a rail: pull-up, pull-down, LED leg, filter shunt.
    ShuntLeg,
    /// Both ends on signals: an element of the signal path.
    Series,
}

impl Chain {
    /// Terminal net classes decide the chain's role.
    pub fn role(&self, classes: &BTreeMap<String, NetClass>) -> ChainRole {
        let class = |net: &str| *classes.get(net).unwrap_or(&NetClass::Signal);
        match (class(&self.a.net).is_rail(), class(&self.b.net).is_rail()) {
            (true, true) => ChainRole::RailToRail,
            (false, false) => ChainRole::Series,
            _ => ChainRole::ShuntLeg,
        }
    }
}

/// The reduced multigraph.
#[derive(Debug, Default)]
pub struct Reduced {
    pub nodes: Vec<NodeKind>,
    pub chains: Vec<Chain>,
}

/// The two distinct nets of a chainable part, in pin order.
fn two_nets(item: &Item) -> Option<(String, String)> {
    let mut nets = item.pins.iter().filter_map(|(_, _, n)| n.as_deref());
    let (a, b) = (nets.next()?, nets.next()?);
    (nets.next().is_none() && a != b).then(|| (a.to_string(), b.to_string()))
}

/// A chainable series part: exactly two connected pins on two DIFFERENT nets,
/// not connector-like (a 2-pin jumper/header is a terminal, not an element), not
/// a switch/button (an interaction point anchors its strap cluster the way an IC
/// anchors its passives), and a genuine 2-pin SYMBOL — a multi-unit IC's 2-pin
/// unit (an op-amp power unit) is an IC fragment, not a series element.
fn chainable(item: &Item) -> bool {
    two_nets(item).is_some()
        && item.geom.pins.len() <= 2
        && !is_connector_like(&item.part)
        && !item.part.contains("SW_")
        && !item.part.contains("Switch")
        && !item.part.contains("Button")
}

/// Contract `items`+`inc` into the reduced graph.
pub fn contract(items: &[Item], inc: &Incidence, classes: &BTreeMap<String, NetClass>) -> Reduced {
    let mut g = Reduced::default();
    let mut node_of: BTreeMap<NodeKind, usize> = BTreeMap::new();

    fn intern(g: &mut Reduced, node_of: &mut BTreeMap<NodeKind, usize>, kind: NodeKind) -> usize {
        *node_of.entry(kind.clone()).or_insert_with(|| {
            g.nodes.push(kind);
            g.nodes.len() - 1
        })
    }

    let att = |net: &str| -> &[(usize, String)] { inc.get(net).map_or(&[], |v| v.as_slice()) };
    let class = |net: &str| *classes.get(net).unwrap_or(&NetClass::Signal);

    // A net is chain-INTERIOR when it is a signal with exactly 2 attachments,
    // both on chainable parts.
    let interior = |net: &str| -> bool {
        !class(net).is_rail() && {
            let a = att(net);
            a.len() == 2 && a.iter().all(|(i, _)| chainable(&items[*i]))
        }
    };

    // Terminal node for a chain end arriving on `net` from chain part `from`
    // (None for 0-part chains, where the caller names the part itself).
    let terminal = |g: &mut Reduced,
                    node_of: &mut BTreeMap<NodeKind, usize>,
                    net: &str,
                    from: Option<usize>|
     -> Terminal {
        if class(net).is_rail() {
            let node = intern(g, node_of, NodeKind::Rail(net.to_string()));
            return Terminal {
                node,
                pin: String::new(),
                net: net.to_string(),
            };
        }
        let others: Vec<&(usize, String)> =
            att(net).iter().filter(|(i, _)| Some(*i) != from).collect();
        if let [(i, pin)] = others.as_slice()
            && !chainable(&items[*i])
        {
            let node = intern(g, node_of, NodeKind::Part(*i));
            return Terminal {
                node,
                pin: pin.clone(),
                net: net.to_string(),
            };
        }
        // Fan-out, dangling end, or a cycle seam: the net itself is the node.
        let node = intern(g, node_of, NodeKind::Junction(net.to_string()));
        Terminal {
            node,
            pin: String::new(),
            net: net.to_string(),
        }
    };

    // ── Chains through chainable parts. `walk(in_net, cur)`: `cur` was entered
    // via `in_net`; returns outward (parts, nets) with nets[k] beyond parts[k],
    // so nets.len() == parts.len().
    let mut taken = vec![false; items.len()];
    let walk = |in_net: &str, mut cur: usize, taken: &mut Vec<bool>| {
        let (mut parts, mut nets) = (Vec::new(), Vec::new());
        let mut inbound = in_net.to_string();
        loop {
            parts.push(cur);
            taken[cur] = true;
            let (n1, n2) = two_nets(&items[cur]).expect("chainable invariant");
            let out = if n1 == inbound { n2 } else { n1 };
            nets.push(out.clone());
            if !interior(&out) {
                break;
            }
            match att(&out)
                .iter()
                .map(|(i, _)| *i)
                .find(|i| *i != cur && !taken[*i])
            {
                Some(next) => {
                    inbound = out;
                    cur = next;
                }
                None => break, // cycle seam
            }
        }
        (parts, nets)
    };

    for start in 0..items.len() {
        if !chainable(&items[start]) || taken[start] {
            continue;
        }
        let (n1, _n2) = two_nets(&items[start]).expect("chainable invariant");
        // Rightward: treat n1 as the inbound side, so the walk leaves via n2.
        let (parts_r, nets_r) = walk(&n1, start, &mut taken);
        // Leftward: continue across n1 if it is interior and its far part is free.
        let (parts_l, nets_l) = if interior(&n1) {
            match att(&n1)
                .iter()
                .map(|(i, _)| *i)
                .find(|i| *i != start && !taken[*i])
            {
                Some(next) => walk(&n1, next, &mut taken),
                None => (Vec::new(), Vec::new()),
            }
        } else {
            (Vec::new(), Vec::new())
        };

        // Assemble a→b: reversed left, then right; nets bracket the parts.
        let parts: Vec<usize> = parts_l
            .iter()
            .rev()
            .chain(parts_r.iter())
            .copied()
            .collect();
        let nets: Vec<String> = parts_l
            .iter()
            .rev()
            .map(|_| ())
            .zip(nets_l.iter().rev())
            .map(|(_, n)| n.clone())
            .chain(std::iter::once(n1.clone()))
            .chain(nets_r.iter().cloned())
            .collect();
        debug_assert_eq!(nets.len(), parts.len() + 1, "chain nets must bracket parts");

        let a = terminal(&mut g, &mut node_of, &nets[0], parts.first().copied());
        let b = terminal(
            &mut g,
            &mut node_of,
            &nets[nets.len() - 1],
            parts.last().copied(),
        );
        g.chains.push(Chain { a, b, parts, nets });
    }

    // ── 0-part chains: signal nets among non-chainable parts only.
    for (net, pins) in inc {
        if class(net).is_rail() {
            continue;
        }
        let non_chain: Vec<&(usize, String)> = pins
            .iter()
            .filter(|(i, _)| !chainable(&items[*i]))
            .collect();
        if non_chain.len() == 2 && pins.len() == 2 {
            let (i0, p0) = (non_chain[0].0, non_chain[0].1.clone());
            let (i1, p1) = (non_chain[1].0, non_chain[1].1.clone());
            let na = intern(&mut g, &mut node_of, NodeKind::Part(i0));
            let nb = intern(&mut g, &mut node_of, NodeKind::Part(i1));
            g.chains.push(Chain {
                a: Terminal {
                    node: na,
                    pin: p0,
                    net: net.clone(),
                },
                b: Terminal {
                    node: nb,
                    pin: p1,
                    net: net.clone(),
                },
                parts: Vec::new(),
                nets: vec![net.clone()],
            });
        } else if pins.len() >= 3 && !non_chain.is_empty() {
            let j = intern(&mut g, &mut node_of, NodeKind::Junction(net.clone()));
            for (i, pin) in non_chain {
                let pn = intern(&mut g, &mut node_of, NodeKind::Part(*i));
                g.chains.push(Chain {
                    a: Terminal {
                        node: j,
                        pin: String::new(),
                        net: net.clone(),
                    },
                    b: Terminal {
                        node: pn,
                        pin: pin.clone(),
                        net: net.clone(),
                    },
                    parts: Vec::new(),
                    nets: vec![net.clone()],
                });
            }
        }
    }

    g
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::classify_nets;
    use kicad_symbol::geometry::{PinGeom, SymbolGeometry};
    use sch_place::ir::LayoutIr;
    use sch_place::item::Item;

    fn geom(lib: &str, npins: usize) -> SymbolGeometry {
        SymbolGeometry {
            lib_id: lib.to_string(),
            pins: (1..=npins)
                .map(|k| PinGeom {
                    number: k.to_string(),
                    name: format!("p{k}"),
                    at: [0.0, k as f64].into(),
                    angle: 0.0,
                    length: 2.54,
                    unit: 1,
                })
                .collect(),
            raw_definition: String::new(),
        }
    }

    fn item(refdes: &str, part: &str, nets: &[Option<&str>]) -> Item {
        Item {
            refdes: refdes.to_string(),
            part: part.to_string(),
            value: String::new(),
            footprint: None,
            geom: geom(part, nets.len()),
            pins: nets
                .iter()
                .enumerate()
                .map(|(k, n)| {
                    (
                        (k + 1).to_string(),
                        format!("p{}", k + 1),
                        n.map(str::to_string),
                    )
                })
                .collect(),
            at: [0.0, 0.0].into(),
            angle: 0.0,
            unit: 1,
            mirror: false,
            frozen: false,
        }
    }

    fn incidence(items: &[Item]) -> Incidence {
        let mut inc = Incidence::new();
        for (i, it) in items.iter().enumerate() {
            for (num, _n, net) in &it.pins {
                if let Some(net) = net {
                    inc.entry(net.clone()).or_default().push((i, num.clone()));
                }
            }
        }
        inc
    }

    fn reduced(items: &[Item]) -> (Reduced, BTreeMap<String, NetClass>) {
        let inc = incidence(items);
        let classes = classify_nets(&inc, &LayoutIr::default());
        (contract(items, &inc, &classes), classes)
    }

    /// IC → R → C → IC2: one 2-part series chain between two Part nodes.
    #[test]
    fn straight_series_chain() {
        let items = vec![
            item("U1", "MCU", &[Some("A"), Some("X"), Some("Y")]),
            item("R1", "Device:R", &[Some("A"), Some("B")]),
            item("C1", "Device:C", &[Some("B"), Some("C")]),
            item("U2", "OPAMP", &[Some("C"), Some("W"), Some("Z")]),
        ];
        let (g, classes) = reduced(&items);
        let series: Vec<&Chain> = g.chains.iter().filter(|c| !c.parts.is_empty()).collect();
        assert_eq!(series.len(), 1);
        let c = series[0];
        assert_eq!(c.parts.len(), 2);
        assert_eq!(c.nets, ["A", "B", "C"]);
        assert_eq!(c.role(&classes), ChainRole::Series);
        assert!(matches!(g.nodes[c.a.node], NodeKind::Part(0)));
        assert!(matches!(g.nodes[c.b.node], NodeKind::Part(3)));
    }

    /// VCC → C → GND: a 1-part rail-to-rail chain (decoupling).
    #[test]
    fn decouple_is_rail_to_rail() {
        let items = vec![item("C1", "Device:C", &[Some("VCC"), Some("GND")])];
        let (g, classes) = reduced(&items);
        assert_eq!(g.chains.len(), 1);
        assert_eq!(g.chains[0].role(&classes), ChainRole::RailToRail);
    }

    /// IC pin → R → GND: shunt leg.
    #[test]
    fn pulldown_is_shunt_leg() {
        let items = vec![
            item("U1", "MCU", &[Some("EN"), Some("X"), Some("Y")]),
            item("R1", "Device:R", &[Some("EN"), Some("GND")]),
        ];
        let (g, classes) = reduced(&items);
        let shunt: Vec<&Chain> = g.chains.iter().filter(|c| c.parts == [1]).collect();
        assert_eq!(shunt.len(), 1);
        assert_eq!(shunt[0].role(&classes), ChainRole::ShuntLeg);
    }

    /// Divider with a sensed tap: VCC → R1 → TAP ← R2 ← GND plus TAP → IC.
    /// TAP has 3 attachments → Junction; three chains meet there.
    #[test]
    fn divider_tap_becomes_junction() {
        let items = vec![
            item("R1", "Device:R", &[Some("VCC"), Some("TAP")]),
            item("R2", "Device:R", &[Some("TAP"), Some("GND")]),
            item("U1", "ADC", &[Some("TAP"), Some("X"), Some("Y")]),
        ];
        let (g, _) = reduced(&items);
        let j = g
            .nodes
            .iter()
            .position(|n| matches!(n, NodeKind::Junction(net) if net == "TAP"))
            .expect("TAP junction");
        // R1 chain and R2 chain terminate at the junction; a 0-part spoke joins U1.
        let touching = g
            .chains
            .iter()
            .filter(|c| c.a.node == j || c.b.node == j)
            .count();
        assert_eq!(touching, 3);
    }

    /// Direct IC-to-IC net: 0-part chain.
    #[test]
    fn bare_net_is_zero_part_chain() {
        let items = vec![
            item("U1", "MCU", &[Some("SIG"), Some("X"), Some("Y")]),
            item("U2", "PHY", &[Some("SIG"), Some("W"), Some("Z")]),
        ];
        let (g, _) = reduced(&items);
        assert_eq!(g.chains.len(), 1);
        assert!(g.chains[0].parts.is_empty());
        assert_eq!(g.chains[0].nets, ["SIG"]);
    }

    /// Every chainable part lands in exactly one chain, even on a long ladder.
    #[test]
    fn ladder_parts_partition_into_chains() {
        // U1 → R1 → R2 → R3 → U2 (a 3-part chain).
        let items = vec![
            item("U1", "MCU", &[Some("N0"), Some("X"), Some("Y")]),
            item("R1", "Device:R", &[Some("N0"), Some("N1")]),
            item("R2", "Device:R", &[Some("N1"), Some("N2")]),
            item("R3", "Device:R", &[Some("N2"), Some("N3")]),
            item("U2", "PHY", &[Some("N3"), Some("W"), Some("Z")]),
        ];
        let (g, _) = reduced(&items);
        let with_parts: Vec<&Chain> = g.chains.iter().filter(|c| !c.parts.is_empty()).collect();
        assert_eq!(with_parts.len(), 1);
        assert_eq!(with_parts[0].parts.len(), 3);
        let mut ns = with_parts[0].nets.clone();
        // Chain may be discovered from either direction; accept both orders.
        if ns[0] == "N3" {
            ns.reverse();
        }
        assert_eq!(ns, ["N0", "N1", "N2", "N3"]);
    }
}
