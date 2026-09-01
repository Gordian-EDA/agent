//! Connectivity: schematic geometry to a net partition, in pure Rust.
//!
//! Every connection point in the sheet — pin tips, wire ends, label anchors,
//! junctions, no-connects, sheet pins — becomes a node keyed by its position
//! quantised to 1 µm. Wires join their own ends; a node lying *inside* a wire
//! joins that wire, which is what makes a pin-on-wire or a wire-end-on-wire
//! connect with no dot while two wires merely crossing stay apart. A junction is
//! just another node, so it connects the crossing wires it sits on without a
//! special case. Names then merge partitions: same-named labels within the
//! sheet, and power symbols and hidden power pins by the name they carry.

use std::collections::HashMap;

use geom::{Point2, UnionFind};

use crate::doc::SchDoc;
use crate::model::{Item, LabelKind};
use crate::pins::{PlacedPin, pins_of};

/// Where a net's name came from. Ordered weakest to strongest so the strongest
/// source of a partition wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NetSource {
    /// Generated, e.g. `Net-(R1-Pad1)`.
    Auto,
    /// A local label.
    Local,
    /// A hierarchical label.
    Hier,
    /// A global label.
    Global,
    /// A power symbol or an implicit hidden power pin.
    Power,
}

/// One pin on a net.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PinRef {
    pub refdes: String,
    pub unit: u32,
    pub pin: String,
    /// The owning symbol is marked do-not-populate. DNP symbols still connect.
    pub dnp: bool,
}

impl PinRef {
    fn key(&self) -> (&str, u32, &str) {
        (&self.refdes, self.unit, &self.pin)
    }
}

/// A net and the pins on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Net {
    pub name: String,
    pub source: NetSource,
    pub pins: Vec<PinRef>,
}

/// The extracted connectivity of one sheet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Netlist {
    /// Nets sorted by name.
    pub nets: Vec<Net>,
    /// Pins with nothing attached and no no-connect marker.
    pub unconnected: Vec<PinRef>,
    /// Anything the extractor could not model, such as buses.
    pub warnings: Vec<String>,
}

/// Quantise to 1 µm so float dust never splits a node.
fn key(p: Point2) -> (i64, i64) {
    (
        (p.x * 1000.0).round() as i64,
        (p.y * 1000.0).round() as i64,
    )
}

#[derive(Default)]
struct Nodes {
    index: HashMap<(i64, i64), usize>,
    points: Vec<Point2>,
}

impl Nodes {
    fn intern(&mut self, p: Point2) -> usize {
        let k = key(p);
        match self.index.get(&k) {
            Some(&idx) => idx,
            None => {
                let idx = self.points.len();
                self.index.insert(k, idx);
                self.points.push(p);
                idx
            }
        }
    }

    fn get(&self, p: Point2) -> Option<usize> {
        self.index.get(&key(p)).copied()
    }
}

/// A wire as the partitioner sees it: two node ids and the segment they span.
struct Segment {
    a: usize,
    b: usize,
    from: Point2,
    to: Point2,
}

/// Extract the net partition of a single sheet.
///
/// Scope is the file: hierarchical sheet pins are connection points but do not
/// pull in the child sheet's connectivity. Callers merging sheets merge only
/// global and power names.
pub fn extract(doc: &SchDoc) -> Netlist {
    let mut warnings = Vec::new();
    if doc
        .items()
        .iter()
        .any(|i| matches!(i.head(), "bus" | "bus_entry" | "bus_alias"))
    {
        warnings.push("unmodeled connectivity: buses are not extracted".to_string());
    }

    let placed: Vec<PlacedPin> = doc.symbols().flat_map(|s| pins_of(doc, s)).collect();
    let power_names = power_symbol_names(doc);

    let mut nodes = Nodes::default();
    let mut segments = Vec::new();
    for wire in doc.wires() {
        for pair in wire.points.windows(2) {
            let a = nodes.intern(pair[0]);
            let b = nodes.intern(pair[1]);
            segments.push(Segment {
                a,
                b,
                from: pair[0],
                to: pair[1],
            });
        }
    }
    for pin in &placed {
        nodes.intern(pin.at);
    }
    for item in doc.items() {
        match item {
            Item::Junction(j) => {
                nodes.intern(j.at);
            }
            Item::NoConnect(n) => {
                nodes.intern(n.at);
            }
            Item::Label(l) => {
                nodes.intern(l.at.point());
            }
            Item::Sheet(s) => {
                for pin in &s.pins {
                    nodes.intern(Point2::new(s.at.x + pin.at.x, s.at.y + pin.at.y));
                }
            }
            _ => {}
        }
    }

    let mut sets = UnionFind::new(nodes.points.len());
    for seg in &segments {
        sets.union(seg.a, seg.b);
    }
    join_interiors(&nodes, &segments, &mut sets);

    let mut named: HashMap<String, (NetSource, Vec<usize>)> = HashMap::new();
    let mut note = |name: String, source: NetSource, node: usize| {
        let entry = named.entry(name).or_insert((source, Vec::new()));
        entry.0 = entry.0.max(source);
        entry.1.push(node);
    };
    for label in doc.labels() {
        if let Some(node) = nodes.get(label.at.point()) {
            let source = match label.kind {
                LabelKind::Local => NetSource::Local,
                LabelKind::Global => NetSource::Global,
                LabelKind::Hier => NetSource::Hier,
            };
            note(label.text.clone(), source, node);
        }
    }
    for pin in &placed {
        let Some(node) = nodes.get(pin.at) else {
            continue;
        };
        if pin.power_symbol {
            if let Some(name) = power_names.get(&pin.refdes) {
                note(name.clone(), NetSource::Power, node);
            }
        } else if pin.hidden && pin.etype == "power_in" {
            note(pin.name.clone(), NetSource::Power, node);
        }
    }
    for (_, (_, members)) in named.iter() {
        for pair in members.windows(2) {
            sets.union(pair[0], pair[1]);
        }
    }

    let mut name_of_root: HashMap<usize, (NetSource, String)> = HashMap::new();
    for (name, (source, members)) in &named {
        let Some(&first) = members.first() else {
            continue;
        };
        let root = sets.find(first);
        let candidate = (*source, name.clone());
        match name_of_root.get(&root) {
            Some(existing) if *existing >= candidate => {}
            _ => {
                name_of_root.insert(root, candidate);
            }
        }
    }

    let no_connects: Vec<usize> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::NoConnect(n) => nodes.get(n.at),
            _ => None,
        })
        .map(|n| sets.find(n))
        .collect();

    let mut wired: Vec<bool> = vec![false; nodes.points.len()];
    for seg in &segments {
        wired[sets.find(seg.a)] = true;
    }
    for (_, (_, members)) in named.iter() {
        for &m in members {
            let root = sets.find(m);
            wired[root] = true;
        }
    }

    let mut groups: HashMap<usize, Vec<PinRef>> = HashMap::new();
    let mut unconnected = Vec::new();
    for pin in &placed {
        let reference = PinRef {
            refdes: pin.refdes.clone(),
            unit: pin.unit,
            pin: pin.number.clone(),
            dnp: pin.dnp,
        };
        match nodes.get(pin.at) {
            Some(node) => groups.entry(sets.find(node)).or_default().push(reference),
            None => unconnected.push(reference),
        }
    }

    let mut nets = Vec::new();
    for (root, pins) in groups {
        if pins.len() < 2 && !wired[root] {
            if !no_connects.contains(&root) {
                unconnected.extend(pins);
            }
            continue;
        }
        let (source, name) = name_of_root
            .get(&root)
            .cloned()
            .unwrap_or_else(|| (NetSource::Auto, auto_name(&pins)));
        let mut pins = pins;
        pins.sort_by(|a, b| a.key().cmp(&b.key()));
        nets.push(Net { name, source, pins });
    }
    nets.sort_by(|a, b| (&a.name, a.pins.len()).cmp(&(&b.name, b.pins.len())));
    unconnected.sort_by(|a, b| a.key().cmp(&b.key()));

    Netlist {
        nets,
        unconnected,
        warnings,
    }
}

/// Power symbols name their net with their `Value` field, not their pin name.
fn power_symbol_names(doc: &SchDoc) -> HashMap<String, String> {
    doc.symbols()
        .filter(|s| {
            doc.lib_symbols()
                .and_then(|libs| crate::pins::resolve(libs, &s.lib_id))
                .is_some_and(crate::pins::is_power_definition)
        })
        .map(|s| (s.refdes().to_string(), s.value().to_string()))
        .collect()
}

/// KiCAD's fallback name for an unnamed net.
fn auto_name(pins: &[PinRef]) -> String {
    let lead = pins
        .iter()
        .min_by(|a, b| a.key().cmp(&b.key()))
        .expect("nets always hold a pin");
    format!("Net-({}-Pad{})", lead.refdes, lead.pin)
}

/// Join every node that lies strictly inside a wire to that wire.
///
/// Axis-aligned wires — everything KiCAD normally draws — are answered from a
/// row/column index so this stays near-linear on boards with tens of thousands
/// of wires; the rare diagonal wire falls back to a scan.
fn join_interiors(nodes: &Nodes, segments: &[Segment], sets: &mut UnionFind) {
    let mut rows: HashMap<i64, Vec<(i64, usize)>> = HashMap::new();
    let mut cols: HashMap<i64, Vec<(i64, usize)>> = HashMap::new();
    for (idx, p) in nodes.points.iter().enumerate() {
        let (x, y) = key(*p);
        rows.entry(y).or_default().push((x, idx));
        cols.entry(x).or_default().push((y, idx));
    }
    for bucket in rows.values_mut().chain(cols.values_mut()) {
        bucket.sort_unstable();
    }

    for seg in segments {
        let (from, to) = (key(seg.from), key(seg.to));
        let bucket = if from.1 == to.1 {
            rows.get(&from.1).map(|b| (b, from.0, to.0))
        } else if from.0 == to.0 {
            cols.get(&from.0).map(|b| (b, from.1, to.1))
        } else {
            for (idx, p) in nodes.points.iter().enumerate() {
                if inside(*p, seg.from, seg.to) {
                    sets.union(idx, seg.a);
                }
            }
            continue;
        };
        let Some((bucket, lo, hi)) = bucket else {
            continue;
        };
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let start = bucket.partition_point(|(v, _)| *v < lo);
        for &(v, idx) in &bucket[start..] {
            if v > hi {
                break;
            }
            sets.union(idx, seg.a);
        }
    }
}

/// Whether `p` lies on the closed segment `from`–`to`, to within a micron.
fn inside(p: Point2, from: Point2, to: Point2) -> bool {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let (px, py) = (p.x - from.x, p.y - from.y);
    let cross = dx * py - dy * px;
    let len = (dx * dx + dy * dy).sqrt();
    if len == 0.0 || cross.abs() / len > 0.001 {
        return false;
    }
    let t = (px * dx + py * dy) / (len * len);
    (-1e-9..=1.0 + 1e-9).contains(&t)
}

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
        delta
            .pins_now_unconnected
            .sort_by(|a, b| a.key().cmp(&b.key()));
        delta
    }

    /// The partition alone: each net as a sorted list of `refdes.unit.pin`.
    /// Comparing these ignores auto-generated names, which is what an oracle
    /// check against `kicad-cli` needs.
    pub fn partition(&self) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = self
            .nets
            .iter()
            .map(|net| {
                let mut pins: Vec<String> = net
                    .pins
                    .iter()
                    .map(|p| format!("{}.{}", p.refdes, p.pin))
                    .collect();
                pins.sort();
                pins.dedup();
                pins
            })
            .filter(|pins| !pins.is_empty())
            .collect();
        out.sort();
        out
    }
}
