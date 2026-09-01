//! Connectivity: schematic geometry to a net partition, in pure Rust.
//!
//! Every connection point in the sheet — pin tips, wire ends, label anchors,
//! junctions, no-connects, sheet pins — becomes a node keyed by its position
//! quantised to 1 µm, and things at the same point are connected.
//!
//! A wire joins its own two ends, and *only* its ends. Touching a wire partway
//! along is not a connection: KiCAD leaves a pin sitting in the middle of a
//! wire unconnected, and leaves a wire that ends on another wire's middle
//! unconnected too. A junction and a sheet pin do attach anywhere along a wire,
//! and a junction is how two crossing wires are joined; a label attaches that
//! way only where there is no pin — on a pin tip it binds to the pin and leaves
//! the wire running past alone.
//!
//! A no-connect marker makes its point inert: nothing joins through it, which
//! is what severs the pin it settles rather than merely excusing it.
//!
//! Names then merge partitions: same-named labels within the sheet, and power
//! symbols and hidden power pins by the name they carry.
//!
//! A partition holding one pin and no name is not a net — a dangling wire off a
//! pin does not make one — which is the same line `kicad-cli` draws when it
//! calls such a pin `unconnected-(…)`. A hierarchical sheet pin counts as a
//! name: the child sheet drives that net even though this file cannot see how.

use std::collections::{HashMap, HashSet};

use geom::{Point2, UnionFind};

use crate::doc::SchDoc;
use crate::model::{Item, LabelKind, SymbolInst, instance_paths};
use crate::pins::{PlacedPin, pins_of};

mod diff;

pub use diff::NetDelta;

/// Where a net's name came from, ordered weakest to strongest so the strongest
/// driver on a partition names it.
///
/// The order is KiCAD's own: a global label outranks a power symbol, which
/// outranks a local label, which outranks a hierarchical one, which outranks a
/// sheet pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NetSource {
    /// Generated, e.g. `Net-(R1-Pad1)`.
    Auto,
    /// A pin on a hierarchical sheet, which lends the net its bare pin name.
    /// It never merges, so two sheets with an `IN` pin leave two nets `IN`.
    SheetPin,
    /// A hierarchical label.
    Hier,
    /// A local label.
    Local,
    /// A power symbol or an implicit hidden power pin.
    Power,
    /// A global label.
    Global,
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

/// Ordered by what identifies the pin. `dnp` is a build attribute of the
/// symbol, not part of which pin this is, so it stays out of the key that sorts
/// nets and picks a lead.
impl Ord for PinRef {
    fn cmp(&self, other: &PinRef) -> std::cmp::Ordering {
        (&self.refdes, self.unit, &self.pin).cmp(&(&other.refdes, other.unit, &other.pin))
    }
}

impl PartialOrd for PinRef {
    fn partial_cmp(&self, other: &PinRef) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PinRef {
    fn of(pin: &PlacedPin) -> PinRef {
        PinRef {
            refdes: pin.refdes.clone(),
            unit: pin.unit,
            pin: pin.number.clone(),
            dnp: pin.dnp,
        }
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
    /// Pins alone on their node with no name — loose ends, in the same sense as
    /// `kicad-cli`'s `unconnected-(…)` nets.
    pub unconnected: Vec<PinRef>,
    /// Pins a no-connect marker severed. The marker makes its point inert, so
    /// these are loose ends on purpose rather than by oversight.
    pub no_connect: Vec<PinRef>,
    /// Anything the extractor could not model, such as buses.
    pub warnings: Vec<String>,
}

impl Netlist {
    /// The partition alone: each net as a sorted list of `refdes.pin`, sorted.
    ///
    /// Two extractions can be compared with `==` on this without net names or
    /// pin order getting in the way. It keeps every pin, `#`-prefixed power
    /// symbols included, so it is not directly comparable to a `kicad-cli`
    /// netlist, which leaves those out.
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
    ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64)
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
    let p = partition(doc);
    let (nets, unconnected, no_connect) = emit(&p.placed, &p.nodes, &p.roots, &p.anchors);
    Netlist {
        nets,
        unconnected,
        no_connect,
        warnings,
    }
}

/// The sheet's geometry, partitioned and named — everything [`extract`] derives
/// its answer from, before it is reduced to pins.
struct Partition {
    placed: Vec<PlacedPin>,
    nodes: Nodes,
    segments: Vec<Segment>,
    roots: Vec<usize>,
    anchors: Anchors,
}

fn partition(doc: &SchDoc) -> Partition {
    let placed: Vec<PlacedPin> = doc.symbols().flat_map(|s| pins_of(doc, s)).collect();
    let interned = intern(doc, &placed);
    let Interned {
        nodes,
        segments,
        attachments,
        severed,
    } = &interned;

    let mut sets = UnionFind::new(nodes.points.len());
    for seg in segments {
        if !severed.contains(&seg.a) && !severed.contains(&seg.b) {
            sets.union(seg.a, seg.b);
        }
    }
    attach(nodes, segments, attachments, severed, &mut sets);

    let named = names(doc, nodes, &placed);
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
    let anchors = Anchors::new(doc, nodes, &named, &roots);
    let Interned {
        nodes, segments, ..
    } = interned;
    Partition {
        placed,
        nodes,
        segments,
        roots,
        anchors,
    }
}

/// What net every connection point and wire segment on the sheet carries.
///
/// [`extract`] answers "which pins share a net"; a router needs the inverse —
/// "what is already drawn here, and may I touch it". A partition that carries
/// no pin and no name gets a synthetic `#node<n>`, which is foreign to
/// everything and so is never merged into by accident.
#[derive(Debug, Clone, Default)]
pub struct Scene {
    /// Connection points: pin tips, wire ends, label anchors, junctions.
    pub points: Vec<(Point2, String)>,
    /// Wire segments as drawn, with the net they carry.
    pub segments: Vec<(Point2, Point2, String)>,
}

/// Derive the [`Scene`] for a sheet.
pub fn scene(doc: &SchDoc) -> Scene {
    let p = partition(doc);
    let mut pins_by_root: HashMap<usize, Vec<&PlacedPin>> = HashMap::new();
    for pin in &p.placed {
        if let Some(node) = p.nodes.get(pin.at) {
            pins_by_root.entry(p.roots[node]).or_default().push(pin);
        }
    }
    let name_of = |root: usize| match p.anchors.name.get(&root) {
        Some((_, name)) => name.clone(),
        None => match pins_by_root.get(&root) {
            Some(pins) if pins.len() > 1 => auto_name(pins),
            _ => format!("#node{root}"),
        },
    };
    Scene {
        points: p
            .nodes
            .points
            .iter()
            .enumerate()
            .map(|(index, at)| (*at, name_of(p.roots[index])))
            .collect(),
        segments: p
            .segments
            .iter()
            .map(|s| (s.from, s.to, name_of(p.roots[s.a])))
            .collect(),
    }
}

/// The interned sheet: its connection points, its wires, and the two node
/// classes the partitioner treats specially.
struct Interned {
    nodes: Nodes,
    segments: Vec<Segment>,
    /// Nodes that join a wire anywhere along its length, not just at its ends.
    attachments: Vec<usize>,
    /// Nodes a no-connect marker made inert.
    severed: HashSet<usize>,
}

/// Intern every connection point on the sheet and the wires over them.
fn intern(doc: &SchDoc, placed: &[PlacedPin]) -> Interned {
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
    let pin_nodes: HashSet<usize> = placed.iter().map(|pin| nodes.intern(pin.at)).collect();

    let mut attachments = Vec::new();
    let mut severed = HashSet::new();
    for item in doc.items() {
        match item {
            Item::Junction(junction) => attachments.push(nodes.intern(junction.at)),
            Item::Label(label) => {
                // A label on a pin tip binds to the pin; the wire running past
                // the pin is not part of that net.
                let node = nodes.intern(label.at.point());
                if !pin_nodes.contains(&node) {
                    attachments.push(node);
                }
            }
            Item::Sheet(sheet) => {
                // A sheet pin's `at` is already in sheet coordinates.
                for pin in &sheet.pins {
                    attachments.push(nodes.intern(pin.at.point()));
                }
            }
            Item::NoConnect(no_connect) => {
                severed.insert(nodes.intern(no_connect.at));
            }
            _ => {}
        }
    }
    Interned {
        nodes,
        segments,
        attachments,
        severed,
    }
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
        let node = nodes
            .get(label.at.point())
            .expect("label anchors are interned");
        let source = match label.kind {
            LabelKind::Local => NetSource::Local,
            LabelKind::Global => NetSource::Global,
            LabelKind::Hier => NetSource::Hier,
        };
        note(crate::text::unescape(&label.text), source, node);
    }
    for pin in placed {
        let node = nodes.get(pin.at).expect("placed pins are interned");
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
            // Strongest source wins. At equal strength KiCAD keeps the label
            // that sorts *first* — the opposite of the sheet-pin rule below,
            // which keeps the last; both are what kicad-cli does.
            let candidate = (*source, text.clone());
            let better =
                name.get(&root)
                    .is_none_or(|(held_source, held)| match held_source.cmp(source) {
                        std::cmp::Ordering::Less => true,
                        std::cmp::Ordering::Equal => *text < *held,
                        std::cmp::Ordering::Greater => false,
                    });
            if better {
                name.insert(root, candidate);
            }
        }
        // A sheet pin names its net only when nothing stronger does, and it
        // never merges: two sheets can each have a pin called `IN`.
        for sheet in doc.items().iter().filter_map(|item| match item {
            Item::Sheet(sheet) => Some(sheet),
            _ => None,
        }) {
            for pin in &sheet.pins {
                let Some(node) = nodes.get(pin.at.point()) else {
                    continue;
                };
                // Sheet pins tie-break the other way: the name that sorts last.
                let candidate = (NetSource::SheetPin, crate::text::unescape(&pin.name));
                let root = roots[node];
                if name.get(&root).is_none_or(|held| *held < candidate) {
                    name.insert(root, candidate);
                }
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
fn emit<'a>(
    placed: &'a [PlacedPin],
    nodes: &Nodes,
    roots: &[usize],
    anchors: &Anchors,
) -> (Vec<Net>, Vec<PinRef>, Vec<PinRef>) {
    let mut groups: HashMap<usize, Vec<&'a PlacedPin>> = HashMap::new();
    for pin in placed {
        let node = nodes.get(pin.at).expect("every placed pin is interned");
        groups.entry(roots[node]).or_default().push(pin);
    }

    let (mut nets, mut unconnected, mut no_connect) = (Vec::new(), Vec::new(), Vec::new());
    for (root, members) in groups {
        let named = anchors.name.get(&root);
        let mut pins: Vec<PinRef> = members.iter().map(|p| PinRef::of(p)).collect();
        if pins.len() < 2 && named.is_none() {
            match anchors.settled.contains(&root) {
                true => no_connect.append(&mut pins),
                false => unconnected.append(&mut pins),
            }
            continue;
        }
        let (source, name) = named
            .cloned()
            .unwrap_or_else(|| (NetSource::Auto, auto_name(&members)));
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
    if doc
        .symbols()
        .any(|s| instance_paths(s.retained().node()) > 1)
    {
        warnings.push(
            "sheet is instantiated more than once; reference designators are ambiguous \
             outside the hierarchy"
                .to_string(),
        );
    }
    warnings
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

/// KiCAD's fallback name for an unnamed net: `Net-(<lead pin>)`.
///
/// The lead is the net's strongest pin driver: a pin with a name of its own
/// outranks an unnamed (`~`) one, a real component outranks a `#`-prefixed
/// symbol such as a power flag, and ties go to whichever label sorts first.
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

/// Keep only the pins that pass, unless that would leave none.
fn narrow<'a>(pool: Vec<&'a PlacedPin>, keep: fn(&PlacedPin) -> bool) -> Vec<&'a PlacedPin> {
    let kept: Vec<&'a PlacedPin> = pool.iter().copied().filter(|p| keep(p)).collect();
    match kept.is_empty() {
        true => pool,
        false => kept,
    }
}

/// How KiCAD writes a pin when it names a net after it: the reference, a unit
/// letter for a multi-unit part, then the pin's name — or `Pad<number>` when it
/// has none.
fn driver_label(pin: &PlacedPin) -> String {
    let pad = match has_pin_name(pin) {
        true => crate::text::unescape(&pin.name),
        false => format!("Pad{}", pin.number),
    };
    format!("{}{}-{pad}", pin.refdes, unit_letter(pin))
}

/// Whether a pin says anything its number does not. A name that is empty, `~`,
/// or a restatement of the number is no name at all.
fn has_pin_name(pin: &PlacedPin) -> bool {
    !pin.name.is_empty() && pin.name != "~" && pin.name != pin.number
}

/// `1` -> `A`, `27` -> `AA`, as KiCAD suffixes a multi-unit reference.
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

/// Join each attaching node to every wire whose length passes through it.
///
/// Only junctions, sheet pins, and labels away from a pin attach this way; a
/// pin or a wire end that merely touches a wire partway along is not connected
/// to it, which is what makes a junction meaningful. Axis-aligned wires — everything KiCAD
/// normally draws — are answered from a row/column index over the attachment
/// points, so this stays near-linear on boards with tens of thousands of wires;
/// the rare diagonal wire falls back to a scan.
fn attach(
    nodes: &Nodes,
    segments: &[Segment],
    attachments: &[usize],
    severed: &HashSet<usize>,
    sets: &mut UnionFind,
) {
    let attachments: Vec<usize> = attachments
        .iter()
        .copied()
        .filter(|node| !severed.contains(node))
        .collect();
    if attachments.is_empty() {
        return;
    }
    let mut rows: HashMap<i64, Vec<(i64, usize)>> = HashMap::new();
    let mut cols: HashMap<i64, Vec<(i64, usize)>> = HashMap::new();
    for &idx in &attachments {
        let (x, y) = key(nodes.points[idx]);
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
            for &idx in &attachments {
                if inside(nodes.points[idx], seg.from, seg.to) {
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
