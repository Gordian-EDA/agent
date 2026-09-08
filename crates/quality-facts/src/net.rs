//! Connectivity: schematic geometry to a net partition.
//!
//! Every connection point — pin tips, wire ends, label anchors, junctions,
//! no-connects, sheet pins — becomes a node keyed to the micrometre, and things
//! at the same point are connected. A wire joins its own two ends and only its
//! ends; junctions, sheet pins and labels attach anywhere along a wire's length.
//! A no-connect marker makes its point inert. Names then merge partitions.
//!
//! This answers to `kicad-cli sch export netlist`: a partition holding one pin
//! and no name is not a net, which is the line KiCad draws when it calls such a
//! pin `unconnected-(…)`.

use std::collections::{HashMap, HashSet};

use crate::geom::{Point, UnionFind, on_segment};
use crate::sch::{LabelKind, PlacedPin, Schematic, unescape};

/// Where a net's name came from, weakest to strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NetSource {
    Auto,
    SheetPin,
    Hier,
    Local,
    Power,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PinRef {
    pub refdes: String,
    pub unit: u32,
    pub pin: String,
}

impl PinRef {
    fn of(pin: &PlacedPin) -> PinRef {
        PinRef {
            refdes: pin.refdes.clone(),
            unit: pin.unit,
            pin: pin.number.clone(),
        }
    }

    pub fn label(&self) -> String {
        format!("{}.{}", self.refdes, self.pin)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Net {
    pub name: String,
    pub source: NetSource,
    pub pins: Vec<PinRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Netlist {
    pub nets: Vec<Net>,
    /// Pins alone on their node with no name.
    pub unconnected: Vec<PinRef>,
    /// Pins a no-connect marker severed on purpose.
    pub no_connect: Vec<PinRef>,
    pub warnings: Vec<String>,
}

impl Netlist {
    /// Each net as a sorted list of `refdes.pin`, sorted.
    pub fn partition(&self) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = self
            .nets
            .iter()
            .map(|net| {
                let mut pins: Vec<String> = net.pins.iter().map(PinRef::label).collect();
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

#[derive(Default)]
struct Nodes {
    index: HashMap<(i64, i64), usize>,
    points: Vec<Point>,
}

impl Nodes {
    fn intern(&mut self, p: Point) -> usize {
        *self.index.entry(p.key()).or_insert_with(|| {
            self.points.push(p);
            self.points.len() - 1
        })
    }

    fn get(&self, p: Point) -> Option<usize> {
        self.index.get(&p.key()).copied()
    }
}

struct Segment {
    a: usize,
    from: Point,
    to: Point,
}

/// The net every wire segment carries, for the visual measurements.
pub struct Scene {
    pub segments: Vec<(Point, Point, String)>,
}

struct Partitioned {
    placed: Vec<PlacedPin>,
    nodes: Nodes,
    segments: Vec<Segment>,
    roots: Vec<usize>,
    names: HashMap<usize, (NetSource, String)>,
    settled: HashSet<usize>,
}

fn partition(doc: &Schematic) -> Partitioned {
    let placed = doc.placed_pins();
    let mut nodes = Nodes::default();
    let mut segments = Vec::new();
    let mut ends = Vec::new();
    for wire in &doc.wires {
        let (a, b) = (nodes.intern(wire[0]), nodes.intern(wire[1]));
        segments.push(Segment {
            a,
            from: wire[0],
            to: wire[1],
        });
        ends.push((a, b));
    }
    let pin_nodes: HashSet<usize> = placed.iter().map(|pin| nodes.intern(pin.at)).collect();

    // (node, how many wire segments must touch it before it attaches)
    let mut attachments: Vec<(usize, usize)> = Vec::new();
    for at in &doc.junctions {
        attachments.push((nodes.intern(*at), 1));
    }
    for label in &doc.labels {
        let node = nodes.intern(label.at);
        // One passing segment stays separate from a coincident pin; at a
        // crossing KiCad makes the labelled pin a junction.
        let required = if pin_nodes.contains(&node) { 2 } else { 1 };
        attachments.push((node, required));
    }
    for sheet in &doc.sheets {
        for pin in &sheet.pins {
            attachments.push((nodes.intern(pin.at), 1));
        }
    }
    let severed: HashSet<usize> = doc.no_connects.iter().map(|at| nodes.intern(*at)).collect();

    let mut sets = UnionFind::new(nodes.points.len());
    for (a, b) in ends {
        if !severed.contains(&a) && !severed.contains(&b) {
            sets.union(a, b);
        }
    }
    attach(&nodes, &segments, &attachments, &severed, &mut sets);

    let named = claimed_names(doc, &nodes, &placed);
    for (_, members) in named.values() {
        let live: Vec<usize> = members
            .iter()
            .copied()
            .filter(|node| !severed.contains(node))
            .collect();
        for pair in live.windows(2) {
            sets.union(pair[0], pair[1]);
        }
    }

    let roots: Vec<usize> = (0..nodes.points.len()).map(|n| sets.find(n)).collect();
    let names = winning_names(doc, &nodes, &named, &roots);
    let settled = doc
        .no_connects
        .iter()
        .filter_map(|at| nodes.get(*at))
        .map(|node| roots[node])
        .collect();
    Partitioned {
        placed,
        nodes,
        segments,
        roots,
        names,
        settled,
    }
}

/// Extract the net partition of a single sheet.
pub fn extract(doc: &Schematic) -> Netlist {
    let warnings = survey(doc);
    let p = partition(doc);

    let mut groups: HashMap<usize, Vec<&PlacedPin>> = HashMap::new();
    for pin in &p.placed {
        let node = p.nodes.get(pin.at).expect("every placed pin is interned");
        groups.entry(p.roots[node]).or_default().push(pin);
    }

    let (mut nets, mut unconnected, mut no_connect) = (Vec::new(), Vec::new(), Vec::new());
    for (root, members) in groups {
        let named = p.names.get(&root);
        let mut pins: Vec<PinRef> = members.iter().map(|pin| PinRef::of(pin)).collect();
        let loose = named.is_none()
            && (pins.len() < 2 || members.iter().all(|pin| pin.etype == "no_connect"));
        if loose {
            match p.settled.contains(&root) {
                true => no_connect.append(&mut pins),
                false => unconnected.append(&mut pins),
            }
            continue;
        }
        let (source, name) = named
            .cloned()
            .unwrap_or_else(|| (NetSource::Auto, auto_name(&members)));
        pins.sort();
        pins.dedup();
        nets.push(Net { name, source, pins });
    }
    nets.sort_by(|a, b| (&a.name, &a.pins).cmp(&(&b.name, &b.pins)));
    unconnected.sort();
    no_connect.sort();
    Netlist {
        nets,
        unconnected,
        no_connect,
        warnings,
    }
}

/// Every wire segment paired with the net it carries.
pub fn scene(doc: &Schematic) -> Scene {
    let p = partition(doc);
    let mut pins_by_root: HashMap<usize, Vec<&PlacedPin>> = HashMap::new();
    for pin in &p.placed {
        if let Some(node) = p.nodes.get(pin.at) {
            pins_by_root.entry(p.roots[node]).or_default().push(pin);
        }
    }
    let name_of = |root: usize| match p.names.get(&root) {
        Some((_, name)) => name.clone(),
        None => match pins_by_root.get(&root) {
            Some(pins) if pins.len() > 1 => auto_name(pins),
            _ => format!("#node{root}"),
        },
    };
    Scene {
        segments: p
            .segments
            .iter()
            .map(|s| (s.from, s.to, name_of(p.roots[s.a])))
            .collect(),
    }
}

/// Every name claimed on the sheet, with the strongest claimant and its nodes.
fn claimed_names(
    doc: &Schematic,
    nodes: &Nodes,
    placed: &[PlacedPin],
) -> HashMap<String, (NetSource, Vec<usize>)> {
    let power: HashMap<&str, &str> = doc
        .symbols
        .iter()
        .filter(|s| doc.definition(&s.lib_key).is_some_and(|d| d.is_power))
        .map(|s| (s.uuid.as_str(), s.value()))
        .collect();
    let mut named: HashMap<String, (NetSource, Vec<usize>)> = HashMap::new();
    let mut note = |name: String, source: NetSource, node: usize| {
        let entry = named.entry(name).or_insert((source, Vec::new()));
        entry.0 = entry.0.max(source);
        entry.1.push(node);
    };
    for label in &doc.labels {
        let Some(node) = nodes.get(label.at) else {
            continue;
        };
        let source = match label.kind {
            LabelKind::Local => NetSource::Local,
            LabelKind::Global => NetSource::Global,
            LabelKind::Hier => NetSource::Hier,
        };
        note(unescape(&label.text), source, node);
    }
    for pin in placed {
        let Some(node) = nodes.get(pin.at) else {
            continue;
        };
        // Only a power *input* names a net: that is what separates a rail symbol
        // from a PWR_FLAG, whose power_out pin names nothing.
        if pin.etype != "power_in" {
            continue;
        }
        if pin.power_symbol {
            if let Some(name) = power.get(pin.owner.as_str()) {
                note(unescape(name), NetSource::Power, node);
            }
        } else if pin.hidden {
            note(unescape(&pin.name), NetSource::Power, node);
        }
    }
    named
}

/// The name each partition ends up with: strongest source, then lowest text.
fn winning_names(
    doc: &Schematic,
    nodes: &Nodes,
    named: &HashMap<String, (NetSource, Vec<usize>)>,
    roots: &[usize],
) -> HashMap<usize, (NetSource, String)> {
    let mut name: HashMap<usize, (NetSource, String)> = HashMap::new();
    for (text, (source, members)) in named {
        let Some(&first) = members.first() else {
            continue;
        };
        let root = roots[first];
        let better =
            name.get(&root)
                .is_none_or(|(held_source, held)| match held_source.cmp(source) {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Equal => *text < *held,
                    std::cmp::Ordering::Greater => false,
                });
        if better {
            name.insert(root, (*source, text.clone()));
        }
    }
    // A sheet pin names its net only when nothing stronger does, and it never
    // merges: two sheets can each have a pin called `IN`.
    for sheet in &doc.sheets {
        for pin in &sheet.pins {
            let Some(node) = nodes.get(pin.at) else {
                continue;
            };
            let candidate = (NetSource::SheetPin, unescape(&pin.name));
            let root = roots[node];
            if name.get(&root).is_none_or(|held| *held < candidate) {
                name.insert(root, candidate);
            }
        }
    }
    name
}

/// What the extraction cannot vouch for on this sheet.
fn survey(doc: &Schematic) -> Vec<String> {
    let mut warnings = Vec::new();
    if doc.has_bus {
        warnings.push("unmodeled connectivity: buses are not extracted".to_string());
    }
    let mut missing: Vec<&str> = doc
        .symbols
        .iter()
        .filter(|s| doc.definition(&s.lib_key).is_none())
        .map(|s| s.lib_id.as_str())
        .collect();
    missing.sort_unstable();
    missing.dedup();
    for lib_id in missing {
        warnings.push(format!(
            "no embedded lib_symbols definition for {lib_id}: its pins are not placed"
        ));
    }
    let mut seen: HashSet<(&str, u32)> = HashSet::new();
    let mut duplicated: Vec<&str> = doc
        .symbols
        .iter()
        .filter(|s| !seen.insert((s.refdes(), s.unit)))
        .map(|s| s.refdes())
        .collect();
    duplicated.sort_unstable();
    duplicated.dedup();
    if !duplicated.is_empty() {
        warnings.push(format!(
            "reference designators are not unique ({}): pins cannot be told apart by name",
            duplicated.join(", ")
        ));
    }
    warnings
}

/// KiCad's fallback name for an unnamed net: `Net-(<lead pin>)`.
fn auto_name(pins: &[&PlacedPin]) -> String {
    let pool = narrow(pins.to_vec(), has_pin_name);
    let pool = narrow(pool, |p| !p.refdes.starts_with('#'));
    let lead = pool
        .iter()
        .map(|p| driver_label(p))
        .min()
        .expect("nets always hold a pin");
    format!("Net-({lead})")
}

fn narrow<'a>(pool: Vec<&'a PlacedPin>, keep: fn(&PlacedPin) -> bool) -> Vec<&'a PlacedPin> {
    let kept: Vec<&'a PlacedPin> = pool.iter().copied().filter(|p| keep(p)).collect();
    match kept.is_empty() {
        true => pool,
        false => kept,
    }
}

fn driver_label(pin: &PlacedPin) -> String {
    let pad = match has_pin_name(pin) {
        true => unescape(&pin.name),
        false => format!("Pad{}", pin.number),
    };
    format!("{}{}-{pad}", pin.refdes, unit_letter(pin))
}

/// Whether a pin says anything its number does not.
fn has_pin_name(pin: &PlacedPin) -> bool {
    !pin.name.is_empty() && pin.name != "~" && pin.name != pin.number
}

/// `1` -> `A`, `27` -> `AA`, as KiCad suffixes a multi-unit reference.
fn unit_letter(pin: &PlacedPin) -> String {
    if !pin.multi_unit {
        return String::new();
    }
    let mut index = pin.unit.max(1) - 1;
    let mut out = String::new();
    loop {
        out.insert(0, char::from(b'A' + (index % 26) as u8));
        if index < 26 {
            return out;
        }
        index = index / 26 - 1;
    }
}

/// Join each attaching node to every wire passing through it.
fn attach(
    nodes: &Nodes,
    segments: &[Segment],
    attachments: &[(usize, usize)],
    severed: &HashSet<usize>,
    sets: &mut UnionFind,
) {
    let mut required: HashMap<usize, usize> = HashMap::new();
    for &(node, needs) in attachments.iter().filter(|(n, _)| !severed.contains(n)) {
        required
            .entry(node)
            .and_modify(|held| *held = (*held).min(needs))
            .or_insert(needs);
    }
    if required.is_empty() {
        return;
    }
    let mut touched: HashMap<usize, Vec<usize>> = HashMap::new();
    for segment in segments {
        for (&node, _) in required.iter() {
            if on_segment(nodes.points[node], segment.from, segment.to) {
                touched.entry(node).or_default().push(segment.a);
            }
        }
    }
    for (node, segments) in touched {
        if segments.len() >= required[&node] {
            for segment in segments {
                sets.union(node, segment);
            }
        }
    }
}

// --- diff -------------------------------------------------------------------

/// How the net partition changed across an edit.
#[derive(Debug, Clone, Default)]
pub struct NetDelta {
    pub created: Vec<String>,
    pub removed: Vec<String>,
    pub merged: Vec<(Vec<String>, String)>,
    pub split: Vec<(String, Vec<String>)>,
    pub renamed: Vec<(String, String)>,
    pub pins_now_unconnected: Vec<PinRef>,
    pub pins_now_connected: Vec<PinRef>,
}

impl NetDelta {
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

fn owners(netlist: &Netlist) -> HashMap<PinRef, usize> {
    netlist
        .nets
        .iter()
        .enumerate()
        .flat_map(|(idx, net)| net.pins.iter().map(move |pin| (pin.clone(), idx)))
        .collect()
}

/// For each partition, the distinct counterpart partitions its pins landed in.
fn images(netlist: &Netlist, other: &HashMap<PinRef, usize>) -> Vec<Vec<usize>> {
    netlist
        .nets
        .iter()
        .map(|net| {
            let mut hit: Vec<usize> = net
                .pins
                .iter()
                .filter_map(|p| other.get(p))
                .copied()
                .collect();
            hit.sort_unstable();
            hit.dedup();
            hit
        })
        .collect()
}

fn names_of(netlist: &Netlist, idx: &[usize]) -> Vec<String> {
    let mut out: Vec<String> = idx.iter().map(|&i| netlist.nets[i].name.clone()).collect();
    out.sort();
    out
}

/// Compare two extractions. Partitions are identified by the pins they hold,
/// never by their name: one sheet can carry several distinct partitions under
/// the same name, and keying by name would fuse them.
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
                if backward[*target].len() == 1 && &after.nets[*target].name != name {
                    delta
                        .renamed
                        .push((name.clone(), after.nets[*target].name.clone()));
                }
            }
            _ => delta.split.push((name.clone(), names_of(after, targets))),
        }
    }
    for (idx, sources) in backward.iter().enumerate() {
        let name = &after.nets[idx].name;
        match sources.as_slice() {
            [] => delta.created.push(name.clone()),
            [_] => {}
            _ => delta.merged.push((names_of(before, sources), name.clone())),
        }
    }
    for net in &before.nets {
        for pin in &net.pins {
            if !after_of.contains_key(pin) {
                delta.pins_now_unconnected.push(pin.clone());
            }
        }
    }
    for net in &after.nets {
        for pin in &net.pins {
            if !before_of.contains_key(pin) {
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
