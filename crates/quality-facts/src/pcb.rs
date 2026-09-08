//! The parts of a `.kicad_pcb` the quality harness measures: where every
//! footprint sits, what it is, what the outline encloses, and the copper.

use std::collections::{BTreeMap, BTreeSet};

use crate::geom::{Point, Rect};
use crate::sexp::{Sexp, parse};

#[derive(Debug, Clone)]
pub struct Footprint {
    pub reference: String,
    pub value: String,
    pub lib_id: String,
    pub at: Point,
    pub rotation: f64,
    /// Pad number to net name, for the pads that carry one.
    pub pad_nets: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub path: Vec<Point>,
}

#[derive(Debug, Clone, Default)]
pub struct Board {
    pub footprints: Vec<Footprint>,
    pub nets: Vec<String>,
    pub outline: Option<Rect>,
    pub tracks: Vec<Track>,
    pub via_count: usize,
}

impl Board {
    pub fn read(path: &std::path::Path) -> Result<Board, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{e}"))?;
        Board::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Board, String> {
        let root = parse(text)?;
        if root.head() != Some("kicad_pcb") {
            return Err("not a kicad_pcb document".into());
        }
        let mut board = Board::default();
        let codes: BTreeMap<i64, String> = root
            .children("net")
            .filter_map(|net| Some((net.number(1)? as i64, net_name(net)?)))
            .collect();
        let mut nets: BTreeSet<String> = codes.values().cloned().collect();
        let mut edges = Vec::new();
        for item in root.items() {
            match item.head() {
                Some("footprint") => board.footprints.push(read_footprint(item, &codes)),
                Some("segment" | "arc") => {
                    if let Some(track) = read_track(item) {
                        board.tracks.push(track);
                    }
                }
                Some("via") => board.via_count += 1,
                Some("gr_line" | "gr_arc" | "gr_rect" | "gr_poly" | "gr_circle") => {
                    if item.text("layer") == Some("Edge.Cuts") {
                        collect_points(item, &mut edges);
                    }
                }
                _ => {}
            }
        }
        board.outline = Rect::bounding(&edges);
        nets.extend(
            board
                .footprints
                .iter()
                .flat_map(|fp| fp.pad_nets.values().cloned()),
        );
        board.nets = nets.into_iter().collect();
        board
            .footprints
            .sort_by(|a, b| a.reference.cmp(&b.reference));
        Ok(board)
    }
}

fn read_footprint(node: &Sexp, codes: &BTreeMap<i64, String>) -> Footprint {
    let at = node.child("at");
    let origin = Point::new(
        at.and_then(|n| n.number(1)).unwrap_or(0.0),
        at.and_then(|n| n.number(2)).unwrap_or(0.0),
    );
    let mut pad_nets = BTreeMap::new();
    for pad in node.children("pad") {
        let number = pad.items().get(1).and_then(Sexp::as_atom).unwrap_or("");
        let net = pad
            .child("net")
            .and_then(|net| net_name(net).or_else(|| codes.get(&(net.number(1)? as i64)).cloned()));
        if let Some(net) = net {
            pad_nets.insert(number.to_string(), net);
        }
    }
    Footprint {
        reference: property(node, "Reference"),
        value: property(node, "Value"),
        lib_id: node
            .items()
            .get(1)
            .and_then(Sexp::as_atom)
            .unwrap_or("")
            .into(),
        at: origin,
        rotation: at.and_then(|n| n.number(3)).unwrap_or(0.0),
        pad_nets,
    }
}

/// A footprint's field, written as `(property "Reference" "R1" …)` in KiCad 8+
/// and as `(fp_text reference "R1" …)` before it.
fn property(node: &Sexp, name: &str) -> String {
    for property in node.children("property") {
        if property.items().get(1).and_then(Sexp::as_atom) == Some(name) {
            return property
                .items()
                .get(2)
                .and_then(Sexp::as_atom)
                .unwrap_or("")
                .into();
        }
    }
    let legacy = name.to_lowercase();
    for text in node.children("fp_text") {
        if text.items().get(1).and_then(Sexp::as_atom) == Some(legacy.as_str()) {
            return text
                .items()
                .get(2)
                .and_then(Sexp::as_atom)
                .unwrap_or("")
                .into();
        }
    }
    String::new()
}

/// The name inside a `(net …)` node, if it carries one. KiCad's net table is
/// `(net <code> "NAME")` and a pad is `(net <code>)`, which resolves through the
/// table; a board written with the name inline on the pad resolves here.
fn net_name(node: &Sexp) -> Option<String> {
    let named = node
        .items()
        .iter()
        .skip(1)
        .filter_map(Sexp::as_atom)
        .find(|atom| !atom.is_empty() && atom.parse::<i64>().is_err())?;
    Some(named.to_string())
}

fn read_track(node: &Sexp) -> Option<Track> {
    let point = |name: &str| {
        node.child(name)
            .map(|n| Point::new(n.number(1).unwrap_or(0.0), n.number(2).unwrap_or(0.0)))
    };
    Some(Track {
        path: vec![point("start")?, point("end")?],
    })
}

fn collect_points(node: &Sexp, out: &mut Vec<Point>) {
    if node.head() == Some("gr_circle") {
        let at = |name: &str| {
            node.child(name)
                .map(|n| Point::new(n.number(1).unwrap_or(0.0), n.number(2).unwrap_or(0.0)))
        };
        if let (Some(centre), Some(edge)) = (at("center"), at("end")) {
            let radius = ((edge.x - centre.x).powi(2) + (edge.y - centre.y).powi(2)).sqrt();
            out.push(Point::new(centre.x - radius, centre.y - radius));
            out.push(Point::new(centre.x + radius, centre.y + radius));
            return;
        }
    }
    for tag in ["start", "end", "mid", "center"] {
        if let Some(child) = node.child(tag) {
            out.push(Point::new(
                child.number(1).unwrap_or(0.0),
                child.number(2).unwrap_or(0.0),
            ));
        }
    }
    if let Some(pts) = node.child("pts") {
        out.extend(
            pts.children("xy")
                .map(|xy| Point::new(xy.number(1).unwrap_or(0.0), xy.number(2).unwrap_or(0.0))),
        );
    }
}
