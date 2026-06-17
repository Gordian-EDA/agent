//! The attributed circuit graph: component nodes joined by nets.
//!
//! A circuit is naturally a *hypergraph* — a net touches arbitrarily many pins —
//! so we keep nets as first-class incidence lists rather than forcing pairwise
//! edges. Nodes carry the attributes an idiom predicate needs (lib id, value,
//! pins); nets carry an electrical *kind* (power / ground / signal) so a pattern
//! can say "to a ground rail" without hard-coding net names like `GND`.

use std::collections::BTreeMap;

/// How a net behaves electrically. Lets edge predicates distinguish a rail tap
/// from a signal hop without naming nets, so the same idiom matches `GND`, `VSS`,
/// `AGND`, … alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetKind {
    /// A positive supply rail (`+3V3`, `VCC`, `5V`, …).
    Power,
    /// A ground / return rail (`GND`, `VSS`, `AGND`, …).
    Ground,
    /// Everything else — an actual signal.
    Signal,
}

/// One pin of a component and the net it lands on (if any).
#[derive(Debug, Clone)]
pub struct Pin {
    pub number: String,
    pub name: String,
    pub net: Option<String>,
}

/// One component instance: a node in the graph.
#[derive(Debug, Clone)]
pub struct Node {
    pub refdes: String,
    /// KiCAD symbol id, e.g. `"Device:Crystal"`, `"Device:C"`, `"MCU_ST_STM32F1:STM32F103C8Tx"`.
    pub lib_id: String,
    /// Free-text value, e.g. `"22pF"`; empty for most ICs.
    pub value: String,
    pub pins: Vec<Pin>,
}

impl Node {
    pub fn pin_count(&self) -> usize {
        self.pins.len()
    }
    /// The distinct nets this node touches (in pin order, with duplicates).
    pub fn nets(&self) -> impl Iterator<Item = &str> {
        self.pins.iter().filter_map(|p| p.net.as_deref())
    }
}

/// The circuit as an attributed hypergraph. Build once with [`CircuitGraph::new`]
/// and query incidence cheaply during matching.
#[derive(Debug, Clone, Default)]
pub struct CircuitGraph {
    pub nodes: Vec<Node>,
    /// net name -> the `(node index, pin number)` pairs it connects.
    nets: BTreeMap<String, Vec<(usize, String)>>,
    /// net name -> electrical kind.
    kinds: BTreeMap<String, NetKind>,
}

impl CircuitGraph {
    /// Assemble the incidence index from `nodes`. `kind_of` classifies each net
    /// name into power / ground / signal — the host supplies it because rail
    /// naming is a project convention, not a graph property.
    pub fn new(nodes: Vec<Node>, kind_of: impl Fn(&str) -> NetKind) -> Self {
        let mut nets: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
        for (i, n) in nodes.iter().enumerate() {
            for p in &n.pins {
                if let Some(net) = &p.net {
                    nets.entry(net.clone()).or_default().push((i, p.number.clone()));
                }
            }
        }
        let kinds = nets.keys().map(|k| (k.clone(), kind_of(k))).collect();
        CircuitGraph { nodes, nets, kinds }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn net_kind(&self, net: &str) -> NetKind {
        self.kinds.get(net).copied().unwrap_or(NetKind::Signal)
    }

    /// The `(node, pin)` incidences of a net (empty slice if unknown).
    pub fn net_nodes(&self, net: &str) -> &[(usize, String)] {
        self.nets.get(net).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Does node `idx` touch any net of the given kind?
    pub fn touches_kind(&self, idx: usize, kind: NetKind) -> bool {
        self.nodes[idx].nets().any(|n| self.net_kind(n) == kind)
    }

    /// The first net (and its kind) that nodes `a` and `b` share, if any. Used by
    /// role↔role edge predicates ("the crystal shares an osc net with the MCU").
    pub fn shared_net(&self, a: usize, b: usize) -> Option<(&str, NetKind)> {
        for net in self.nodes[a].nets() {
            if self.net_nodes(net).iter().any(|(j, _)| *j == b) {
                return Some((net, self.net_kind(net)));
            }
        }
        None
    }

    /// Every net `a` and `b` both touch (deduplicated, sorted) — used when an
    /// idiom needs the *count* or *kinds* of shared nets, not just existence.
    pub fn shared_nets(&self, a: usize, b: usize) -> Vec<String> {
        let mut out: Vec<String> = self.nodes[a]
            .nets()
            .filter(|net| self.net_nodes(net).iter().any(|(j, _)| *j == b))
            .map(str::to_string)
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn refdes(&self, idx: usize) -> &str {
        &self.nodes[idx].refdes
    }
}
