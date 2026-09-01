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
//!
//! A partition holding one pin and no name is not a net — a dangling wire off a
//! pin does not make one — which is the same line `kicad-cli` draws when it
//! calls such a pin `unconnected-(…)`.

use std::collections::{HashMap, HashSet};

use geom::{Point2, UnionFind};

use crate::doc::SchDoc;
use crate::model::{Item, LabelKind, SymbolInst};
use crate::pins::{PlacedPin, pins_of};

mod diff;

pub use diff::NetDelta;

/// Where a net's name came from, ordered weakest to strongest so the strongest
/// driver on a partition names it.
///
/// The order is KiCAD's own: a global label outranks a power symbol, which
/// outranks a local label, which outranks a hierarchical one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NetSource {
    /// Generated, e.g. `Net-(R1-Pad1)`.
    Auto,
    /// A hierarchical label.
    Hier,
    /// A local label.
    Local,
    /// A power symbol or an implicit hidden power pin.
    Power,
    /// A global label.
    Global,
}

/// One pin on a net. Ordered by reference, unit and pin number.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PinRef {
    pub refdes: String,
    pub unit: u32,
    pub pin: String,
    /// The owning symbol is marked do-not-populate. DNP symbols still connect.
    pub dnp: bool,
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
    /// Pins alone on their node with no name — loose ends, in the same sense as
    /// `kicad-cli`'s `unconnected-(…)` nets.
    pub unconnected: Vec<PinRef>,
    /// Pins that would be loose ends but carry a no-connect marker.
    pub no_connect: Vec<PinRef>,
    /// Anything the extractor could not model, such as buses.
    pub warnings: Vec<String>,
}

impl Netlist {
    /// The partition alone: each net as a sorted list of `refdes.pin`.
    /// Comparing these ignores generated net names, which is what an oracle
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
    let warnings = survey(doc);
    let placed: Vec<PlacedPin> = doc.symbols().flat_map(|s| pins_of(doc, s)).collect();
    let (nodes, segments) = intern(doc, &placed);

    let mut sets = UnionFind::new(nodes.points.len());
    for seg in &segments {
        sets.union(seg.a, seg.b);
    }
    join_interiors(&nodes, &segments, &mut sets);

    let named = names(doc, &nodes, &placed);
    for (_, members) in named.values() {
        for pair in members.windows(2) {
            sets.union(pair[0], pair[1]);
        }
    }

    let roots: Vec<usize> = (0..nodes.points.len()).map(|n| sets.find(n)).collect();
    let anchors = Anchors::new(doc, &nodes, &named, &roots);
    let (nets, unconnected, no_connect) = emit(&placed, &nodes, &roots, &anchors);
    Netlist {
        nets,
        unconnected,
        no_connect,
        warnings,
    }
}

/// Intern every connection point on the sheet and the wire segments over them.
fn intern(doc: &SchDoc, placed: &[PlacedPin]) -> (Nodes, Vec<Segment>) {
    let mut nodes = Nodes::default();
    let mut segments = Vec::new();
    for wire in doc.wires() {
        for pair in wire.points.windows(2) {
            let (a, b) = (nodes.intern(pair[0]), nodes.intern(pair[1]));
            segments.push(Segment {
                a,
                b,
                from: pair[0],
                to: pair[1],
            });
        }
    }
    for pin in placed {
        nodes.intern(pin.at);
    }
    for item in doc.items() {
        match item {
            Item::Junction(junction) => {
                nodes.intern(junction.at);
            }
            Item::NoConnect(no_connect) => {
                nodes.intern(no_connect.at);
            }
            Item::Label(label) => {
                nodes.intern(label.at.point());
            }
            Item::Sheet(sheet) => {
                // A sheet pin's `at` is already in sheet coordinates.
                for pin in &sheet.pins {
                    nodes.intern(pin.at.point());
                }
            }
            _ => {}
        }
    }
    (nodes, segments)
}

/// Every name claimed on the sheet, with the strongest source that claimed it
/// and the nodes carrying it. Labels name by their text; rail symbols name by
/// their `Value`; a legacy part's hidden power input names by its pin name.
fn names(
    doc: &SchDoc,
    nodes: &Nodes,
    placed: &[PlacedPin],
) -> HashMap<String, (NetSource, Vec<usize>)> {
    let power = power_symbol_names(doc);
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
            note(crate::text::unescape(&label.text), source, node);
        }
    }
    for pin in placed {
        let Some(node) = nodes.get(pin.at) else {
            continue;
        };
        if pin.etype != "power_in" {
            continue;
        }
        // Only a power *input* names a net: that is what separates a rail
        // symbol from a PWR_FLAG, whose power_out pin names nothing.
        if pin.power_symbol {
            if let Some(name) = power.get(&pin.owner) {
                note(crate::text::unescape(name), NetSource::Power, node);
            }
        } else if pin.hidden {
            note(crate::text::unescape(&pin.name), NetSource::Power, node);
        }
    }
    named
}

/// Per-partition facts the emitter needs, resolved once the sets are final.
struct Anchors {
    /// The winning name and its source, for partitions that have one.
    name: HashMap<usize, (NetSource, String)>,
    /// Partitions a no-connect marker settles.
    settled: HashSet<usize>,
}

impl Anchors {
    fn new(
        doc: &SchDoc,
        nodes: &Nodes,
        named: &HashMap<String, (NetSource, Vec<usize>)>,
        roots: &[usize],
    ) -> Anchors {
        let mut name: HashMap<usize, (NetSource, String)> = HashMap::new();
        for (text, (source, members)) in named {
            let Some(&first) = members.first() else {
                continue;
            };
            let root = roots[first];
            // Strongest source wins; at equal strength KiCAD keeps the name
            // that sorts first.
            let candidate = (*source, text.clone());
            let better = name
                .get(&root)
                .is_none_or(|(held_source, held)| match held_source.cmp(source) {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Equal => *text < *held,
                    std::cmp::Ordering::Greater => false,
                });
            if better {
                name.insert(root, candidate);
            }
        }
        let settled = doc
            .items()
            .iter()
            .filter_map(|item| match item {
                Item::NoConnect(no_connect) => nodes.get(no_connect.at),
                _ => None,
            })
            .map(|node| roots[node])
            .collect();
        Anchors { name, settled }
    }
}

/// Group the pins by partition into nets, and report the loose ends.
///
/// `kicad-cli` names a one-pin net `unconnected-(…)` unless something names it,
/// so that is exactly what counts as a loose end here.
fn emit(
    placed: &[PlacedPin],
    nodes: &Nodes,
    roots: &[usize],
    anchors: &Anchors,
) -> (Vec<Net>, Vec<PinRef>, Vec<PinRef>) {
    let mut groups: HashMap<usize, Vec<PinRef>> = HashMap::new();
    for pin in placed {
        let node = nodes.get(pin.at).expect("every placed pin is interned");
        groups.entry(roots[node]).or_default().push(PinRef {
            refdes: pin.refdes.clone(),
            unit: pin.unit,
            pin: pin.number.clone(),
            dnp: pin.dnp,
        });
    }

    let (mut nets, mut unconnected, mut no_connect) = (Vec::new(), Vec::new(), Vec::new());
    for (root, mut pins) in groups {
        let named = anchors.name.get(&root);
        if pins.len() < 2 && named.is_none() {
            match anchors.settled.contains(&root) {
                true => no_connect.append(&mut pins),
                false => unconnected.append(&mut pins),
            }
            continue;
        }
        let (source, name) = named
            .cloned()
            .unwrap_or_else(|| (NetSource::Auto, auto_name(&pins)));
        pins.sort();
        nets.push(Net { name, source, pins });
    }
    nets.sort_by(|a, b| (&a.name, &a.pins).cmp(&(&b.name, &b.pins)));
    unconnected.sort();
    no_connect.sort();
    (nets, unconnected, no_connect)
}

/// What the extraction cannot vouch for on this sheet.
fn survey(doc: &SchDoc) -> Vec<String> {
    let mut warnings = Vec::new();
    if doc
        .items()
        .iter()
        .any(|i| matches!(i.head(), "bus" | "bus_entry" | "bus_alias"))
    {
        warnings.push("unmodeled connectivity: buses are not extracted".to_string());
    }
    let mut missing: Vec<&str> = doc
        .symbols()
        .map(|s| s.lib_id.as_str())
        .filter(|lib_id| definition(doc, lib_id).is_none())
        .collect();
    missing.sort_unstable();
    missing.dedup();
    for lib_id in missing {
        warnings.push(format!(
            "no embedded lib_symbols definition for {lib_id}: its pins are not placed"
        ));
    }
    // The units of one multi-unit part share a reference on purpose; only a
    // repeated (reference, unit) makes a pin impossible to name.
    let mut seen: HashSet<(&str, u32)> = HashSet::new();
    let mut duplicated: Vec<&str> = doc
        .symbols()
        .filter(|s| !seen.insert((s.refdes(), s.unit)))
        .map(SymbolInst::refdes)
        .collect();
    duplicated.sort_unstable();
    duplicated.dedup();
    if !duplicated.is_empty() {
        warnings.push(format!(
            "reference designators are not unique ({}): pins cannot be told apart by name",
            duplicated.join(", ")
        ));
    }
    if doc.symbols().any(|s| instance_paths(s) > 1) {
        warnings.push(
            "sheet is instantiated more than once; reference designators are ambiguous \
             outside the hierarchy"
                .to_string(),
        );
    }
    warnings
}

/// How many `(instances … (path …))` entries a symbol carries.
fn instance_paths(symbol: &SymbolInst) -> usize {
    let Some(instances) = crate::sexpr::child(symbol.retained().node(), "instances") else {
        return 0;
    };
    crate::sexpr::items(instances)
        .iter()
        .map(|project| {
            crate::sexpr::items(project)
                .iter()
                .filter(|c| crate::sexpr::head(c) == Some("path"))
                .count()
        })
        .sum()
}

/// Power symbols name their net with their `Value` field, not their pin name.
///
/// Keyed by UUID, not reference: an un-annotated schematic is full of `#PWR?`,
/// and keying by that would short every rail on the sheet together.
fn power_symbol_names(doc: &SchDoc) -> HashMap<String, String> {
    doc.symbols()
        .filter(|s| definition(doc, &s.lib_id).is_some_and(crate::pins::is_power_definition))
        .map(|s| (s.uuid.clone(), s.value().to_string()))
        .collect()
}

/// The embedded definition behind a `lib_id`, with `extends` followed.
fn definition<'a>(doc: &'a SchDoc, lib_id: &str) -> Option<&'a kiutils_sexpr::Node> {
    crate::pins::resolve(doc.lib_symbols()?, lib_id)
}

/// KiCAD's fallback name for an unnamed net.
fn auto_name(pins: &[PinRef]) -> String {
    let lead = pins.iter().min().expect("nets always hold a pin");
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
