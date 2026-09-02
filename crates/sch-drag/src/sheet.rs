//! The sheet as both the drag primitive and the evaluator need to see it:
//! bodies, pins, and the wire graph with the net every node and segment carries.

use std::collections::{HashMap, HashSet};

use geom::{Point2, Rect};
use sch_doc::{Item, PlacedPin, SchDoc, connect};

/// Connection points are compared at 1 µm, matching the extractor's own
/// quantisation — float dust must never split a node here either.
pub type NodeKey = (i64, i64);

/// Quantise a point to its connection-node key.
pub fn key(p: Point2) -> NodeKey {
    p.quantized_key(1000.0)
}

/// A placed symbol's drawn outline.
#[derive(Debug, Clone)]
pub struct Body {
    pub uuid: String,
    pub refdes: String,
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

/// A text run drawn on the sheet, for the collision term.
#[derive(Debug, Clone)]
pub struct TextBox {
    pub rect: Rect,
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
    pub texts: Vec<TextBox>,
    /// Net carried by each connection point on the sheet.
    pub node_net: HashMap<NodeKey, String>,
    /// Wire indices incident to a node.
    pub incident: HashMap<NodeKey, Vec<usize>>,
    /// Pin indices sitting on a node.
    pub pins_at: HashMap<NodeKey, Vec<usize>>,
    /// Nodes a junction, no-connect, label or sheet pin pins in place.
    pub fixtures: HashSet<NodeKey>,
    /// Local/global/hierarchical label anchors, by node.
    pub label_names: HashMap<NodeKey, String>,
}

fn text_extent(text: &str, at: Point2, rot: f64) -> Rect {
    // KiCAD's default 1.27 mm text: roughly 0.7 mm advance per glyph.
    let w = 0.72 * text.chars().count().max(1) as f64;
    let h = 1.27;
    let (w, h) = if (rot - 90.0).abs() < 1.0 || (rot - 270.0).abs() < 1.0 {
        (h, w)
    } else {
        (w, h)
    };
    Rect::new(at.x, at.y - h / 2.0, at.x + w, at.y + h / 2.0)
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
        let mut label_names = HashMap::new();
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
                }
                Item::NoConnect(n) => {
                    fixtures.insert(key(n.at));
                }
                Item::Label(l) => {
                    fixtures.insert(key(l.at.point()));
                    label_names.insert(key(l.at.point()), l.text.clone());
                    texts.push(TextBox {
                        rect: text_extent(&l.text, l.at.point(), l.at.rot),
                    });
                }
                Item::Text(t) => texts.push(TextBox {
                    rect: text_extent(&t.text, t.at.point(), t.at.rot),
                }),
                Item::Sheet(s) => {
                    for pin in &s.pins {
                        fixtures.insert(key(pin.at.point()));
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
                rect,
                power,
            });
            for name in ["Reference", "Value"] {
                if let Some(field) = symbol.fields.get(name)
                    && !field.hidden
                    && let Some(at) = field.at
                {
                    texts.push(TextBox {
                        rect: text_extent(&field.value, at.point(), at.rot),
                    });
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
            label_names,
        }
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
