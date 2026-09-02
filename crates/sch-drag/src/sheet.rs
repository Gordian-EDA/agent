//! The sheet as both the drag primitive and the evaluator need to see it:
//! bodies, pins, and the wire graph with the net every node and segment carries.

use std::collections::{HashMap, HashSet};

use geom::{Point2, Rect};
use sch_doc::{Item, LabelKind, PlacedPin, SchDoc, connect};

/// Connection points are compared at 1 µm, matching the extractor's own
/// quantisation — float dust must never split a node here either.
pub type NodeKey = (i64, i64);

/// Quantise a point to its connection-node key.
pub fn key(p: Point2) -> NodeKey {
    p.quantized_key(1000.0)
}

/// The point a connection-node key stands for.
pub fn point_of(k: &NodeKey) -> Point2 {
    Point2::new(k.0 as f64 / 1000.0, k.1 as f64 / 1000.0)
}

/// A placed symbol's drawn outline.
#[derive(Debug, Clone)]
pub struct Body {
    pub uuid: String,
    pub refdes: String,
    pub lib_id: String,
    pub rect: Rect,
    /// A power symbol is a rail marker, not a part: it may sit anywhere and its
    /// tiny body is not an obstacle worth routing around.
    pub power: bool,
}

/// One drawn wire segment, with the wire item it belongs to.
#[derive(Debug, Clone)]
pub struct WireSeg {
    pub uuid: String,
    pub a: Point2,
    pub b: Point2,
    pub net: String,
    /// A wire item KiCAD wrote with more than two points cannot be retracted
    /// piecewise, so the drag primitive treats it as immovable scenery.
    pub polyline: bool,
}

impl WireSeg {
    pub fn horizontal(&self) -> bool {
        (self.a.y - self.b.y).abs() < geom::EPS
    }

    pub fn vertical(&self) -> bool {
        (self.a.x - self.b.x).abs() < geom::EPS
    }

    pub fn length(&self) -> f64 {
        self.a.manhattan(self.b)
    }
}

/// Everything derived once per sheet state.
///
/// Building it costs one [`connect::scene`] pass; every query below is then a
/// hash lookup, which is what keeps a search over thousands of poses viable.
#[derive(Debug, Clone)]
pub struct Sheet {
    pub pins: Vec<PlacedPin>,
    pub bodies: Vec<Body>,
    pub wires: Vec<WireSeg>,
    /// Text runs drawn on the sheet, for the collision term.
    pub texts: Vec<Rect>,
    /// Net carried by each connection point on the sheet.
    pub node_net: HashMap<NodeKey, String>,
    /// Wire indices incident to a node.
    pub incident: HashMap<NodeKey, Vec<usize>>,
    /// Pin indices sitting on a node.
    pub pins_at: HashMap<NodeKey, Vec<usize>>,
    /// Nodes a junction, no-connect, label or sheet pin pins in place.
    pub fixtures: HashSet<NodeKey>,
    /// Connection dots, which also attach part-way along a wire.
    pub junctions: HashSet<NodeKey>,
    /// Hierarchical sheet pins, which attach part-way along a wire too.
    pub sheet_pins: HashSet<NodeKey>,
    /// Local/global/hierarchical label anchors, by node.
    pub label_names: HashMap<NodeKey, String>,
    /// Of those, the ones whose scope is this sheet alone — the only names this
    /// crate may rewrite, since a global or hierarchical one speaks elsewhere.
    pub local_labels: HashSet<NodeKey>,
}

/// KiCAD's default 1.27 mm text: roughly 0.72 mm of advance per glyph.
fn glyph_span(text: &str) -> f64 {
    0.72 * text.chars().count().max(1) as f64
}

/// The box a label's text occupies. A label reads *away* from its anchor, in
/// the direction its rotation points — modelling it as always running to the
/// right puts half the boxes on the wrong side of the sheet.
fn label_extent(text: &str, at: Point2, rot: f64) -> Rect {
    let (span, half) = (glyph_span(text), 1.27 / 2.0);
    match rot.rem_euclid(360.0).round() as i64 {
        90 => Rect::new(at.x - half, at.y - span, at.x + half, at.y),
        180 => Rect::new(at.x - span, at.y - half, at.x, at.y + half),
        270 => Rect::new(at.x - half, at.y, at.x + half, at.y + span),
        _ => Rect::new(at.x, at.y - half, at.x + span, at.y + half),
    }
}

/// The box a symbol field occupies. KiCAD centres a property on its point.
fn field_extent(text: &str, at: Point2, rot: f64) -> Rect {
    let (span, thick) = (glyph_span(text), 1.27);
    let half = if (rot - 90.0).abs() < 1.0 || (rot - 270.0).abs() < 1.0 {
        (thick / 2.0, span / 2.0)
    } else {
        (span / 2.0, thick / 2.0)
    };
    Rect::from_center_half(at, half)
}

impl Sheet {
    /// Derive the view. `doc` is not modified.
    pub fn of(doc: &SchDoc) -> Sheet {
        let scene = connect::scene(doc);
        let mut node_net: HashMap<NodeKey, String> = HashMap::new();
        for (p, net) in &scene.points {
            node_net.insert(key(*p), net.clone());
        }

        let mut wires = Vec::new();
        let mut fixtures = HashSet::new();
        let mut junctions = HashSet::new();
        let mut sheet_pins = HashSet::new();
        let mut label_names = HashMap::new();
        let mut local_labels = HashSet::new();
        let mut texts = Vec::new();
        for item in doc.items() {
            match item {
                Item::Wire(w) => {
                    let polyline = w.points.len() > 2;
                    for pair in w.points.windows(2) {
                        let net = node_net.get(&key(pair[0])).cloned().unwrap_or_default();
                        wires.push(WireSeg {
                            uuid: w.uuid.clone(),
                            a: pair[0],
                            b: pair[1],
                            net,
                            polyline,
                        });
                    }
                }
                Item::Junction(j) => {
                    fixtures.insert(key(j.at));
                    junctions.insert(key(j.at));
                }
                Item::NoConnect(n) => {
                    fixtures.insert(key(n.at));
                }
                Item::Label(l) => {
                    fixtures.insert(key(l.at.point()));
                    label_names.insert(key(l.at.point()), l.text.clone());
                    if l.kind == LabelKind::Local {
                        local_labels.insert(key(l.at.point()));
                    }
                    texts.push(label_extent(&l.text, l.at.point(), l.at.rot));
                }
                Item::Text(t) => texts.push(label_extent(&t.text, t.at.point(), t.at.rot)),
                Item::Sheet(s) => {
                    for pin in &s.pins {
                        fixtures.insert(key(pin.at.point()));
                        sheet_pins.insert(key(pin.at.point()));
                    }
                }
                _ => {}
            }
        }

        let mut bodies = Vec::new();
        for symbol in doc.symbols() {
            let Some(rect) = sch_doc::body_rect(doc, symbol) else {
                continue;
            };
            let power = symbol.refdes().starts_with('#');
            bodies.push(Body {
                uuid: symbol.uuid.clone(),
                refdes: symbol.refdes().to_string(),
                lib_id: symbol.lib_id.clone(),
                rect,
                power,
            });
            for name in ["Reference", "Value"] {
                if let Some(field) = symbol.fields.get(name)
                    && !field.hidden
                    && let Some(at) = field.at
                {
                    texts.push(field_extent(&field.value, at.point(), at.rot));
                }
            }
        }

        let pins = sch_doc::placed_pins(doc);
        let mut incident: HashMap<NodeKey, Vec<usize>> = HashMap::new();
        for (index, seg) in wires.iter().enumerate() {
            incident.entry(key(seg.a)).or_default().push(index);
            incident.entry(key(seg.b)).or_default().push(index);
        }
        let mut pins_at: HashMap<NodeKey, Vec<usize>> = HashMap::new();
        for (index, pin) in pins.iter().enumerate() {
            pins_at.entry(key(pin.at)).or_default().push(index);
        }

        Sheet {
            pins,
            bodies,
            wires,
            texts,
            node_net,
            incident,
            pins_at,
            fixtures,
            junctions,
            sheet_pins,
            label_names,
            local_labels,
        }
    }

    /// Wires whose *interior* the point lies on — where a junction or a sheet
    /// pin makes a connection and a bare wire end does not.
    pub fn wires_through(&self, p: Point2) -> impl Iterator<Item = usize> {
        self.wires.iter().enumerate().filter_map(move |(i, w)| {
            let inside = if w.horizontal() {
                (w.a.y - p.y).abs() < geom::EPS
                    && p.x > w.a.x.min(w.b.x) + geom::EPS
                    && p.x < w.a.x.max(w.b.x) - geom::EPS
            } else {
                (w.a.x - p.x).abs() < geom::EPS
                    && p.y > w.a.y.min(w.b.y) + geom::EPS
                    && p.y < w.a.y.max(w.b.y) - geom::EPS
            };
            inside.then_some(i)
        })
    }

    /// Wire ends at a point, and wires running through it.
    pub fn incidence(&self, p: Point2) -> (usize, usize) {
        (
            self.incident.get(&key(p)).map_or(0, Vec::len),
            self.wires_through(p).count(),
        )
    }

    /// Whether KiCAD needs a connection dot here: three or more wire ends
    /// meeting, or an end landing part-way along another wire. Two wires merely
    /// crossing need none — and a dot there would mean the opposite.
    pub fn junction_needed(&self, p: Point2) -> bool {
        let (ends, through) = self.incidence(p);
        ends >= 3 || (ends >= 1 && through >= 1)
    }

    /// Whether a dot here connects nothing at all.
    pub fn junction_inert(&self, p: Point2) -> bool {
        let (ends, through) = self.incidence(p);
        ends <= 1 && through == 0
    }

    /// Wire ends left hanging in space — no pin, no dot, no label, not even
    /// resting on another wire. A reader reads one as a mistake, and it is.
    pub fn dangling_ends(&self) -> usize {
        self.incident
            .iter()
            .filter(|(node, wires)| {
                wires.len() == 1
                    && !self.fixtures.contains(*node)
                    && !self.pins_at.contains_key(*node)
                    && self.wires_through(point_of(node)).next().is_none()
            })
            .count()
    }

    /// Whether a label at this point is speaking for nothing: no wire end, no
    /// pin, not even a wire running past. KiCAD calls it "label not connected";
    /// a reader calls it a name floating in space.
    pub fn label_stranded(&self, p: Point2) -> bool {
        let k = key(p);
        !self.incident.contains_key(&k)
            && !self.pins_at.contains_key(&k)
            && self.wires_through(p).next().is_none()
    }

    /// Labels speaking for nothing.
    pub fn stranded_labels(&self) -> usize {
        self.label_names
            .keys()
            .filter(|k| self.label_stranded(point_of(k)))
            .count()
    }

    /// Dots that connect nothing, and points that need one and have none.
    pub fn junction_faults(&self) -> usize {
        let stray = self
            .junctions
            .iter()
            .filter(|k| self.junction_inert(point_of(k)))
            .count();
        let missing = self
            .incident
            .keys()
            .filter(|k| !self.junctions.contains(*k) && self.junction_needed(point_of(k)))
            .count();
        stray + missing
    }

    /// The net at a connection point, if the sheet has one there.
    pub fn net_at(&self, p: Point2) -> Option<&str> {
        self.node_net.get(&key(p)).map(String::as_str)
    }

    /// Pins of one symbol, by its UUID.
    pub fn pins_of(&self, uuid: &str) -> impl Iterator<Item = &PlacedPin> {
        self.pins.iter().filter(move |p| p.owner == uuid)
    }

    /// Bodies that are real parts — power rail markers excluded.
    pub fn part_bodies(&self) -> impl Iterator<Item = &Body> {
        self.bodies.iter().filter(|b| !b.power)
    }

    /// Names already claimed by a label, so a fallback label can pick a fresh one.
    pub fn label_texts(&self) -> HashSet<&str> {
        self.label_names.values().map(String::as_str).collect()
    }
}
