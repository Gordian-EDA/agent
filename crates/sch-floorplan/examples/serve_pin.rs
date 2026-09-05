//! Every SERVING PAIR on a sheet: a two-pin support part and the pin it serves, with the
//! distance between them and whether the node between them is drawn or merely named.
//!
//! ```text
//! cargo run --release -p sch-floorplan --example serve_pin -- <dir-or-sheet>…
//! ```
//!
//! A human seats a decoupler, a pull-up, a reset cap, a crystal load cap or a series
//! resistor BESIDE the pin it serves: the wire is a few millimetres long and the node
//! never needs a name. Our blocks put those parts in an anonymous row instead, so the node
//! grows a label and the sheet loses the drawing that says what the part is for. This
//! measures exactly that: median and p90 pin-to-pin distance, and the share of pairs whose
//! node is DRAWN (a wire path from one pin to the other) rather than named.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use circuit_graph::netclass::is_power_net;
use geom::{EPS, Point2, Segment, UnionFind};
use sch_doc::SchDoc;
use sch_doc::connect::{self, PinRef};
use sch_doc::PlacedPin;

/// What a support part is doing for the pin it serves.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// One pin on a rail, the other on the served pin's net: decoupler, pull-up/down,
    /// reset cap, load cap.
    Shunt,
    /// Between two served pins: a series resistor, a coupling cap.
    Series,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Shunt => "shunt",
            Kind::Series => "series",
        }
    }
}

struct Pair {
    kind: Kind,
    /// Both parts joined the same block, so seating one beside the other was the
    /// typesetter's to decide. A pair split across two blocks is the packer's business,
    /// not the typesetter's.
    together: bool,
    part: String,
    served: String,
    net: String,
    dist: f64,
    wired: bool,
}

fn main() {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    assert!(!args.is_empty(), "usage: serve_pin <dir-or-sheet>…");
    let verbose = std::env::var("SERVE_PIN_ALL").is_ok();
    let mut sheets: Vec<PathBuf> = Vec::new();
    for arg in args {
        match arg.is_dir() {
            true => sheets.extend(walk(&arg)),
            false => sheets.push(arg),
        }
    }
    sheets.retain(|p| p.extension().is_some_and(|e| e == "kicad_sch"));
    sheets.sort();

    let mut all: Vec<Pair> = Vec::new();
    for path in &sheets {
        let Ok(doc) = SchDoc::read(path) else { continue };
        let pairs = pairs(&doc);
        println!("{:<44} {}", label(path), summary(&pairs));
        if verbose {
            for p in &pairs {
                println!(
                    "    {:<7} {:<9} {:<6} → {:<10} net={:<18} {:>6.1}mm {}",
                    p.kind.name(),
                    if p.together { "in-block" } else { "across" },
                    p.part,
                    p.served,
                    p.net,
                    p.dist,
                    if p.wired { "wire" } else { "LABEL" }
                );
            }
        }
        all.extend(pairs);
    }
    println!("{:<44} {}", "TOTAL", summary(&all));
}

/// A fixture is named by its file, except a corpus of `<case>/input/reference.kicad_sch`,
/// where the case is the name and the file is not.
fn label(path: &std::path::Path) -> String {
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    match stem.as_str() {
        "reference" => path
            .ancestors()
            .nth(2)
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(stem),
        _ => stem,
    }
}

fn walk(dir: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        match path.is_dir() {
            true => out.extend(walk(&path)),
            false => out.push(path),
        }
    }
    out
}

fn summary(pairs: &[Pair]) -> String {
    if pairs.is_empty() {
        return "pairs=0".into();
    }
    let mut d: Vec<f64> = pairs.iter().map(|p| p.dist).collect();
    d.sort_by(f64::total_cmp);
    let at = |q: f64| d[((d.len() as f64 - 1.0) * q).round() as usize];
    let wired = pairs.iter().filter(|p| p.wired).count();
    let same: Vec<&Pair> = pairs.iter().filter(|p| p.together).collect();
    let mut sd: Vec<f64> = same.iter().map(|p| p.dist).collect();
    sd.sort_by(f64::total_cmp);
    let together = match sd.is_empty() {
        true => "in-block none".to_string(),
        false => format!(
            "in-block={:<4} median={:>6.1} wired={:>5.1}%",
            sd.len(),
            sd[(sd.len() - 1) / 2],
            100.0 * same.iter().filter(|p| p.wired).count() as f64 / sd.len() as f64
        ),
    };
    format!(
        "pairs={:<4} median={:>6.1} p90={:>6.1} wired={:>5.1}%   {together}",
        pairs.len(),
        at(0.5),
        at(0.9),
        100.0 * wired as f64 / pairs.len() as f64
    )
}

/// Every serving pair on the sheet.
fn pairs(doc: &SchDoc) -> Vec<Pair> {
    let placed = sch_doc::placed_pins(doc);
    let netlist = connect::extract(doc);
    let at: HashMap<(String, u32, String), Point2> = placed
        .iter()
        .map(|p| ((p.refdes.clone(), p.unit, p.number.clone()), p.at))
        .collect();
    let mut pin_count: BTreeMap<&str, usize> = BTreeMap::new();
    for pin in &placed {
        if !pin.refdes.starts_with('#') {
            *pin_count.entry(pin.refdes.as_str()).or_default() += 1;
        }
    }
    // Net of every pin, and the pins of every net, with power symbols left out: a rail's
    // glyph is not a part anyone seats a cap beside.
    let mut net_of: HashMap<&PinRef, &str> = HashMap::new();
    let mut pins_of: HashMap<&str, Vec<&PinRef>> = HashMap::new();
    for net in &netlist.nets {
        for pin in &net.pins {
            if pin.refdes.starts_with('#') {
                continue;
            }
            net_of.insert(pin, net.name.as_str());
            pins_of.entry(net.name.as_str()).or_default().push(pin);
        }
    }
    let mut drawn = Drawn::new(doc, &placed);
    // The block each symbol joined, as `place_parts` tagged it. A human original carries
    // no such tag: its whole sheet is one drawing, and every pair on it is in-block.
    let block: HashMap<&str, &str> = doc
        .symbols()
        .filter_map(|s| {
            let field = s.fields.get(sch_model::result::AP_BLOCK)?;
            Some((s.refdes(), field.value.as_str()))
        })
        .collect();

    let mut out = Vec::new();
    for (refdes, count) in &pin_count {
        if *count != 2 {
            continue;
        }
        let mine: Vec<&PinRef> = net_of
            .keys()
            .filter(|p| p.refdes == *refdes)
            .copied()
            .collect();
        if mine.len() != 2 {
            continue;
        }
        let rails = mine
            .iter()
            .filter(|p| is_power_net(net_of[*p]))
            .count();
        let kind = match rails {
            1 => Kind::Shunt,
            0 => Kind::Series,
            _ => continue,
        };
        for pin in mine.iter().filter(|p| !is_power_net(net_of[*p])) {
            let net = net_of[*pin];
            let Some(served) = served_pin(&pins_of[net], pin, &pin_count) else {
                continue;
            };
            let (Some(a), Some(b)) = (
                at.get(&(pin.refdes.clone(), pin.unit, pin.pin.clone())),
                at.get(&(served.refdes.clone(), served.unit, served.pin.clone())),
            ) else {
                continue;
            };
            out.push(Pair {
                kind,
                together: block.get(refdes) == block.get(served.refdes.as_str()),
                part: refdes.to_string(),
                served: format!("{}.{}", served.refdes, served.pin),
                net: net.to_string(),
                dist: a.dist(*b),
                wired: drawn.connected(*a, *b),
            });
        }
    }
    out.sort_by(|x, y| (x.kind, &x.part, &x.served).cmp(&(y.kind, &y.part, &y.served)));
    out
}

/// The one pin `net` exists to serve, seen from `mine`: the single pin on a part with
/// three or more pins, or — a net between two two-pin parts — the single other pin.
///
/// A net with two candidates is a bus stop, not a service: nobody can seat one cap beside
/// two devices, and the humans do not try.
fn served_pin<'a>(
    net: &[&'a PinRef],
    mine: &PinRef,
    pin_count: &BTreeMap<&str, usize>,
) -> Option<&'a PinRef> {
    let others: Vec<&&PinRef> = net.iter().filter(|p| p.refdes != mine.refdes).collect();
    let big: Vec<&&PinRef> = others
        .iter()
        .filter(|p| pin_count.get(p.refdes.as_str()).copied().unwrap_or(0) >= 3)
        .copied()
        .collect();
    match (big.len(), others.len()) {
        (1, _) => Some(big[0]),
        (0, 1) => Some(others[0]),
        _ => None,
    }
}

/// The sheet's DRAWN connectivity: what a wire path joins, ignoring every net name.
///
/// Two pins a human seated beside each other are joined by wire. Two pins our blocks put
/// in different rows are joined by a name — the same net, and a reader who has to search
/// the sheet for the other end.
struct Drawn {
    node: HashMap<(i64, i64), usize>,
    sets: UnionFind,
}

impl Drawn {
    fn new(doc: &SchDoc, placed: &[PlacedPin]) -> Drawn {
        let key = |p: Point2| ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64);
        let segments: Vec<Segment> = doc
            .wires()
            .flat_map(|w| w.points.windows(2).map(|p| Segment::new(p[0], p[1])))
            .collect();
        let mut node: HashMap<(i64, i64), usize> = HashMap::new();
        let mut points: Vec<Point2> = Vec::new();
        let mut intern = |p: Point2, node: &mut HashMap<(i64, i64), usize>| {
            *node.entry(key(p)).or_insert_with(|| {
                points.push(p);
                points.len() - 1
            })
        };
        for seg in &segments {
            intern(seg.a, &mut node);
            intern(seg.b, &mut node);
        }
        for pin in placed {
            intern(pin.at, &mut node);
        }
        let mut sets = UnionFind::new(node.len());
        for seg in &segments {
            sets.union(node[&key(seg.a)], node[&key(seg.b)]);
        }
        // A pin or wire end landing ON another segment is a tee the sheet draws with a
        // junction dot; measuring the geometry alone counts it either way.
        for (k, i) in &node {
            let p = Point2::new(k.0 as f64 / 1000.0, k.1 as f64 / 1000.0);
            for seg in &segments {
                if seg.dist_to_point(p) < EPS {
                    sets.union(*i, node[&key(seg.a)]);
                }
            }
        }
        Drawn { node, sets }
    }

    fn connected(&mut self, a: Point2, b: Point2) -> bool {
        let key = |p: Point2| ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64);
        match (self.node.get(&key(a)), self.node.get(&key(b))) {
            (Some(a), Some(b)) => self.sets.find(*a) == self.sets.find(*b),
            _ => false,
        }
    }
}
