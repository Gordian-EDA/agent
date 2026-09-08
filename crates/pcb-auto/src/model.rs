//! Typed views and guarded edits over a `.kicad_pcb` S-expression tree.
//!
//! The tree is the truth; views are computed on demand. Coordinates are millimetres in the board
//! frame (y down); angles degrees counter-clockwise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::geom::{
    add, arc_points, chain_segments, circle_points, norm_angle, polygon_area, rotate, BBox, Point,
};
use crate::sexp::{self, l, lxy, Node, SList};

pub const COPPER_SUFFIX: &str = ".Cu";
const DEFAULT_COPPER: [&str; 2] = ["F.Cu", "B.Cu"];

pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn xy(node: Option<&SList>) -> Point {
    node.map(|n| (n.arg_f64(0).unwrap_or(0.0), n.arg_f64(1).unwrap_or(0.0)))
        .unwrap_or((0.0, 0.0))
}

fn at3(node: Option<&SList>) -> (f64, f64, f64) {
    node.map(|n| {
        (
            n.arg_f64(0).unwrap_or(0.0),
            n.arg_f64(1).unwrap_or(0.0),
            n.arg_f64(2).unwrap_or(0.0),
        )
    })
    .unwrap_or((0.0, 0.0, 0.0))
}

/// Expand KiCad layer wildcards: `*.Cu` -> every copper layer, `*.Mask`/`F&B.Mask` -> both sides.
pub fn expand_layers(tokens: &[String], copper: &[String]) -> Vec<String> {
    let copper: Vec<String> = if copper.is_empty() {
        DEFAULT_COPPER.iter().map(|s| s.to_string()).collect()
    } else {
        copper.to_vec()
    };
    let mut out: Vec<String> = Vec::new();
    for t in tokens {
        if t.starts_with("*.") || t.starts_with("F&B.") {
            let suffix = t.split_once('.').map(|(_, s)| s).unwrap_or("");
            if t.starts_with("*.") && suffix == "Cu" {
                out.extend(copper.iter().cloned());
            } else {
                out.push(format!("F.{suffix}"));
                out.push(format!("B.{suffix}"));
            }
        } else {
            out.push(t.clone());
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.into_iter().filter(|l| seen.insert(l.clone())).collect()
}

#[derive(Debug, Clone)]
pub struct Net {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Pad {
    pub number: String,
    /// `smd` | `thru_hole` | `np_thru_hole` | `connect`
    pub kind: String,
    pub shape: String,
    pub pos: Point,
    pub rot: f64,
    pub size: (f64, f64),
    pub drill: Option<f64>,
    pub layers: Vec<String>,
    pub net_id: i64,
    pub net_name: String,
    pub roundrect_ratio: Option<f64>,
    /// Index of the pad node inside its footprint node's `items`.
    pub node_key: (usize, usize),
    /// Custom-pad primitive vertices in the pad's local frame, if any.
    pub custom_points: Vec<Point>,
}

impl Pad {
    pub fn copper_layers(&self) -> Vec<String> {
        self.layers
            .iter()
            .filter(|l| l.ends_with(COPPER_SUFFIX))
            .cloned()
            .collect()
    }

    pub fn bbox(&self) -> BBox {
        let (w, h) = self.size;
        BBox::of_points([-1.0f64, 1.0].iter().flat_map(|sx| {
            [-1.0f64, 1.0]
                .iter()
                .map(move |sy| (sx * w / 2.0, sy * h / 2.0))
        }).map(|p| add(self.pos, rotate(p, self.rot))).collect::<Vec<_>>())
    }

    pub fn is_through(&self) -> bool {
        self.kind == "thru_hole" || self.kind == "np_thru_hole"
    }
}

#[derive(Debug, Clone)]
pub struct Footprint {
    pub ref_: String,
    pub value: String,
    pub lib_id: String,
    pub pos: Point,
    pub rot: f64,
    pub layer: String,
    pub locked: bool,
    pub attrs: Vec<String>,
    /// Index of this footprint's node inside the board tree's `items`.
    pub index: usize,
    pub pads: Vec<Pad>,
    courtyard: BBox,
}

impl Footprint {
    pub fn side(&self) -> &'static str {
        if self.layer.starts_with("B.") {
            "back"
        } else {
            "front"
        }
    }
    pub fn is_dnp(&self) -> bool {
        self.attrs.iter().any(|a| a == "dnp")
    }
    /// Courtyard in board coordinates, falling back to pads plus fab/silk graphics.
    pub fn courtyard_bbox(&self) -> BBox {
        self.courtyard
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub start: Point,
    pub end: Point,
    pub width: f64,
    pub layer: String,
    pub net_id: i64,
}

#[derive(Debug, Clone)]
pub struct Via {
    pub pos: Point,
    pub size: f64,
    pub drill: f64,
    pub layers: (String, String),
    pub net_id: i64,
}

#[derive(Debug, Clone)]
pub struct Zone {
    pub net_id: i64,
    pub net_name: String,
    pub layers: Vec<String>,
    pub polygon: Vec<Point>,
    /// item -> allowed, for a rule area; `None` for a copper zone.
    pub keepout: Option<HashMap<String, bool>>,
    pub name: String,
    /// Filled islands, as KiCad wrote them after a refill.
    pub filled: Vec<Vec<Point>>,
}

/// Design rules the board states, over KiCad's own defaults.
#[derive(Debug, Clone)]
pub struct Rules {
    pub track_width: f64,
    pub clearance: f64,
    pub via_size: f64,
    pub via_drill: f64,
    pub edge_clearance: f64,
    pub hole_clearance: f64,
    pub pad_clearance: Option<f64>,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            track_width: 0.2,
            clearance: 0.2,
            via_size: 0.6,
            via_drill: 0.3,
            edge_clearance: 0.5,
            hole_clearance: 0.25,
            pad_clearance: None,
        }
    }
}

/// A stated trace width above this is a rail, not the width to route signals at.
const MAX_SIGNAL_TRACK_WIDTH: f64 = 0.5;

pub struct Board {
    pub tree: SList,
    pub path: Option<PathBuf>,
}

impl Board {
    pub fn load(path: &Path) -> anyhow::Result<Board> {
        let tree = sexp::load(path)?;
        anyhow::ensure!(tree.is("kicad_pcb"), "not a kicad_pcb file");
        Ok(Board {
            tree,
            path: Some(path.to_path_buf()),
        })
    }

    pub fn parse(text: &str) -> anyhow::Result<Board> {
        let tree = sexp::parse(text)?;
        anyhow::ensure!(tree.is("kicad_pcb"), "not a kicad_pcb file");
        Ok(Board { tree, path: None })
    }

    pub fn empty(copper_layers: usize) -> Board {
        let mut b = Board::parse(EMPTY_BOARD).expect("built-in empty board parses");
        b.with_copper_layers(copper_layers);
        b
    }

    pub fn save(&mut self, path: Option<&Path>) -> anyhow::Result<PathBuf> {
        let p = path
            .map(|p| p.to_path_buf())
            .or_else(|| self.path.clone())
            .ok_or_else(|| anyhow::anyhow!("no path to save to"))?;
        sexp::save(&self.tree, &p)?;
        self.path = Some(p.clone());
        Ok(p)
    }

    pub fn dumps(&self) -> String {
        sexp::dumps(&self.tree)
    }

    // ---- structure ------------------------------------------------------
    pub fn copper_layers(&self) -> Vec<String> {
        self.tree
            .find("layers")
            .map(|n| {
                n.lists(None)
                    .iter()
                    .filter_map(|l| l.items.get(1).map(|a| a.text().to_string()))
                    .filter(|s| s.ends_with(COPPER_SUFFIX))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// F.Cu, In1.Cu .. In(n-2).Cu, B.Cu, keeping every non-copper layer.
    pub fn with_copper_layers(&mut self, n: usize) {
        let Some(node) = self.tree.find_mut("layers") else {
            return;
        };
        let keep: Vec<Node> = node
            .items
            .iter()
            .filter(|c| {
                c.as_list()
                    .and_then(|s| s.items.get(1))
                    .map(|a| !a.text().ends_with(COPPER_SUFFIX))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        let mut copper = vec![Node::List(SList {
            items: vec![Node::int(0), Node::str("F.Cu"), Node::sym("signal")],
        })];
        for i in 1..n.saturating_sub(1) {
            copper.push(Node::List(SList {
                items: vec![
                    Node::int(2 * i as i64 + 2),
                    Node::str(format!("In{i}.Cu")),
                    Node::sym("signal"),
                ],
            }));
        }
        copper.push(Node::List(SList {
            items: vec![Node::int(2), Node::str("B.Cu"), Node::sym("signal")],
        }));
        let head = node.items[0].clone();
        node.items = std::iter::once(head).chain(copper).chain(keep).collect();
    }

    pub fn nets(&self) -> Vec<Net> {
        self.tree
            .lists(Some("net"))
            .iter()
            .map(|n| Net {
                id: n.arg_f64(0).unwrap_or(0.0) as i64,
                name: n.arg_text(1).unwrap_or("").to_string(),
            })
            .collect()
    }

    pub fn net_by_name(&self, name: &str) -> Option<Net> {
        self.nets().into_iter().find(|n| n.name == name)
    }

    pub fn ensure_net(&mut self, name: &str) -> Net {
        if let Some(n) = self.net_by_name(name) {
            return n;
        }
        let id = self.nets().iter().map(|n| n.id).max().unwrap_or(0) + 1;
        let node = l("net", vec![Node::int(id), Node::str(name)]);
        self.insert_after_last(node, &["net"], &["setup"]);
        Net {
            id,
            name: name.to_string(),
        }
    }

    fn net_ref(&self, net: Option<&SList>, index: &HashMap<String, i64>) -> i64 {
        let Some(net) = net else { return 0 };
        match net.arg(0) {
            Some(Node::Num(s)) => s.parse::<f64>().unwrap_or(0.0) as i64,
            Some(other) => *index.get(other.text()).unwrap_or(&0),
            None => 0,
        }
    }

    fn net_index(&self) -> HashMap<String, i64> {
        self.nets().into_iter().map(|n| (n.name, n.id)).collect()
    }

    /// Rules the board file itself states, over KiCad's defaults.
    pub fn design_rules(&self) -> Rules {
        let mut r = Rules::default();
        let take = |node: Option<&SList>, pairs: &[(&str, &str)], out: &mut Rules| {
            let Some(node) = node else { return };
            for (spelling, key) in pairs {
                let Some(v) = node.find(spelling).and_then(|c| c.arg_f64(0)) else {
                    continue;
                };
                if v <= 0.0 {
                    continue;
                }
                match *key {
                    // a stated width is a pen, not a floor: keep the narrowest, drop rail widths
                    "track_width" if v <= MAX_SIGNAL_TRACK_WIDTH => {
                        out.track_width = out.track_width.min(v)
                    }
                    "clearance" => out.clearance = out.clearance.max(v),
                    "via_size" => out.via_size = out.via_size.max(v),
                    "via_drill" => out.via_drill = out.via_drill.max(v),
                    _ => {}
                }
            }
        };
        const SETUP: &[(&str, &str)] = &[
            ("clearance", "clearance"),
            ("trace_clearance", "clearance"),
            ("segment_width", "track_width"),
            ("trace_width", "track_width"),
            ("via_size", "via_size"),
            ("via_dia", "via_size"),
            ("via_drill", "via_drill"),
        ];
        const NETCLASS: &[(&str, &str)] = &[
            ("clearance", "clearance"),
            ("trace_width", "track_width"),
            ("via_dia", "via_size"),
            ("via_size", "via_size"),
            ("via_drill", "via_drill"),
        ];
        let setup = self.tree.find("setup").cloned();
        let classes = self.tree.lists(Some("net_class"));
        let default_class = classes
            .iter()
            .find(|c| {
                c.arg_text(0)
                    .map(|s| s.trim().eq_ignore_ascii_case("default"))
                    .unwrap_or(false)
            })
            .or(classes.first())
            .map(|c| (*c).clone());
        take(setup.as_ref(), SETUP, &mut r);
        take(default_class.as_ref(), NETCLASS, &mut r);
        r.pad_clearance = self.pad_clearance();
        r
    }

    fn pad_clearance(&self) -> Option<f64> {
        let mut best: Option<f64> = None;
        for f in self.footprint_nodes() {
            let node = &self.tree.items[f];
            let Some(fl) = node.as_list() else { continue };
            for n in std::iter::once(fl).chain(fl.lists(Some("pad"))) {
                if let Some(v) = n.find("clearance").and_then(|c| c.arg_f64(0)) {
                    best = Some(best.map_or(v, |b: f64| b.max(v)));
                }
            }
        }
        best
    }

    // ---- footprints -----------------------------------------------------
    pub fn footprint_nodes(&self) -> Vec<usize> {
        self.tree
            .items
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.as_list()
                    .map(|s| s.is("footprint") || s.is("module"))
                    .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn footprints(&self) -> Vec<Footprint> {
        let nets = self.net_index();
        let copper = self.copper_layers();
        self.footprint_nodes()
            .into_iter()
            .map(|i| self.footprint_at(i, &nets, &copper))
            .collect()
    }

    pub fn footprint(&self, ref_: &str) -> Option<Footprint> {
        self.footprints().into_iter().find(|f| f.ref_ == ref_)
    }

    fn footprint_at(&self, index: usize, nets: &HashMap<String, i64>, copper: &[String]) -> Footprint {
        let n = self.tree.items[index].as_list().expect("footprint node");
        let (x, y, rot) = at3(n.find("at"));
        let layer = n
            .find("layer")
            .and_then(|l| l.arg_text(0))
            .unwrap_or("F.Cu")
            .to_string();
        let attrs = n
            .find("attr")
            .map(|a| a.args().iter().map(|v| v.text().to_string()).collect())
            .unwrap_or_default();
        let locked = n.find("locked").is_some()
            || n.items
                .iter()
                .any(|i| matches!(i, Node::Sym(s) if s == "locked"));
        let mut fp = Footprint {
            ref_: prop(n, "Reference"),
            value: prop(n, "Value"),
            lib_id: n.arg_text(0).unwrap_or("").to_string(),
            pos: (x, y),
            rot: norm_angle(rot),
            layer,
            locked,
            attrs,
            index,
            pads: Vec::new(),
            courtyard: BBox::empty(),
        };
        fp.pads = read_pads(n, index, &fp, nets, copper);
        fp.courtyard = courtyard_of(n, &fp);
        fp
    }

    pub fn set_footprint_pose(&mut self, ref_: &str, pos: Option<Point>, rot: Option<f64>) {
        let Some(f) = self.footprint(ref_) else { return };
        let new_pos = pos.unwrap_or(f.pos);
        let new_rot = rot.map(norm_angle).unwrap_or(f.rot);
        let delta = norm_angle(new_rot - f.rot);
        let node = self.tree.items[f.index].as_list_mut();
        let mut args = vec![Node::num(new_pos.0), Node::num(new_pos.1)];
        if new_rot != 0.0 {
            args.push(Node::num(new_rot));
        }
        node.set("at", args);
        if delta != 0.0 {
            for child in node.items.iter_mut() {
                if let Node::List(c) = child
                    && matches!(c.head(), Some("pad") | Some("property") | Some("fp_text")) {
                        set_child_at(c, |a| a + delta, false);
                    }
            }
        }
    }

    /// Move a footprint to the other side, mirroring about its own X axis as KiCad does.
    pub fn flip_footprint(&mut self, ref_: &str) {
        let Some(f) = self.footprint(ref_) else { return };
        let swap = |name: &str| -> String {
            if let Some(rest) = name.strip_prefix("F.") {
                format!("B.{rest}")
            } else if let Some(rest) = name.strip_prefix("B.") {
                format!("F.{rest}")
            } else {
                name.to_string()
            }
        };
        let node = self.tree.items[f.index].as_list_mut();
        node.set("layer", vec![Node::str(swap(&f.layer))]);
        for child in node.items.iter_mut() {
            let Node::List(c) = child else { continue };
            let head = c.head().unwrap_or("").to_string();
            if let Some(lay) = c.find_mut("layer") {
                let v = swap(lay.arg_text(0).unwrap_or(""));
                lay.set_args(vec![Node::str(v)]);
            }
            if let Some(lays) = c.find_mut("layers") {
                let vs: Vec<Node> = lays
                    .args()
                    .iter()
                    .map(|a| Node::str(swap(a.text())))
                    .collect();
                lays.set_args(vs);
            }
            if matches!(head.as_str(), "pad" | "property" | "fp_text") {
                set_child_at(c, |a| -a, true);
            }
            if head.starts_with("fp_") {
                mirror_graphic_y(c);
            }
        }
        set_child_at_self(node, |a| -a);
    }

    pub fn set_locked(&mut self, ref_: &str, locked: bool) {
        let Some(f) = self.footprint(ref_) else { return };
        let node = self.tree.items[f.index].as_list_mut();
        node.remove("locked");
        node.items
            .retain(|i| !matches!(i, Node::Sym(s) if s == "locked"));
        if locked {
            let idx = node
                .items
                .iter()
                .position(|c| c.as_list().map(|s| s.is("layer")).unwrap_or(false))
                .unwrap_or(0);
            node.items.insert(idx + 1, l("locked", vec![Node::flag(true)]));
        }
    }

    /// Insert a footprint parsed from a `.kicad_mod`, assigning reference and value.
    // every argument is a distinct field of the node being written; grouping them would only
    // move the same list one call further out
    #[allow(clippy::too_many_arguments)]
    pub fn add_footprint(
        &mut self,
        module: &SList,
        ref_: &str,
        value: &str,
        pos: Point,
        rot: f64,
        side: &str,
        lib_id: &str,
    ) {
        let mut node = module.clone();
        node.items[0] = Node::sym("footprint");
        if node.items.len() > 1 {
            node.items[1] = Node::str(lib_id);
        }
        node.remove("version");
        node.remove("generator");
        node.remove("generator_version");
        node.set("layer", vec![Node::str("F.Cu")]);
        let lay_idx = node
            .items
            .iter()
            .position(|c| c.as_list().map(|s| s.is("layer")).unwrap_or(false))
            .unwrap_or(0);
        node.items
            .insert(lay_idx + 1, l("uuid", vec![Node::str(new_uuid())]));
        node.items.insert(lay_idx + 2, lxy("at", pos.0, pos.1));
        for child in node.items.iter_mut() {
            let Node::List(c) = child else { continue };
            let head = c.head().unwrap_or("").to_string();
            if head == "property" {
                match c.arg_text(0) {
                    Some("Reference") => c.set_args(vec![Node::str("Reference"), Node::str(ref_)]),
                    Some("Value") => c.set_args(vec![Node::str("Value"), Node::str(value)]),
                    _ => {}
                }
            }
            if head == "pad" {
                c.remove("net");
            }
            if c.find("uuid").is_none()
                && matches!(
                    head.as_str(),
                    "pad" | "property"
                        | "fp_text"
                        | "fp_text_box"
                        | "fp_line"
                        | "fp_rect"
                        | "fp_circle"
                        | "fp_arc"
                        | "fp_poly"
                )
            {
                c.push(l("uuid", vec![Node::str(new_uuid())]));
            }
        }
        self.tree.push(Node::List(node));
        if side == "back" {
            self.flip_footprint(ref_);
        }
        if rot != 0.0 {
            self.set_footprint_pose(ref_, None, Some(rot));
        }
    }

    pub fn remove_footprint(&mut self, ref_: &str) {
        if let Some(f) = self.footprint(ref_) {
            self.tree.items.remove(f.index);
        }
    }

    /// Set a pad's net by name, creating the net if needed.
    pub fn set_pad_net(&mut self, ref_: &str, pad: &str, net_name: &str) -> bool {
        let net = self.ensure_net(net_name);
        let Some(f) = self.footprint(ref_) else {
            return false;
        };
        let node = self.tree.items[f.index].as_list_mut();
        let mut done = false;
        for child in node.items.iter_mut() {
            let Node::List(c) = child else { continue };
            if c.is("pad") && c.arg_text(0) == Some(pad) {
                c.set("net", vec![Node::int(net.id), Node::str(&net.name)]);
                done = true;
            }
        }
        done
    }

    // ---- copper ---------------------------------------------------------
    pub fn tracks(&self) -> Vec<Track> {
        let index = self.net_index();
        let default_w = self.design_rules().track_width;
        self.tree
            .lists(Some("segment"))
            .iter()
            .map(|n| Track {
                start: xy(n.find("start")),
                end: xy(n.find("end")),
                width: n.find("width").and_then(|w| w.arg_f64(0)).unwrap_or(default_w),
                layer: n
                    .find("layer")
                    .and_then(|l| l.arg_text(0))
                    .unwrap_or("F.Cu")
                    .to_string(),
                net_id: self.net_ref(n.find("net"), &index),
            })
            .collect()
    }

    pub fn vias(&self) -> Vec<Via> {
        let index = self.net_index();
        let rules = self.design_rules();
        let copper = self.copper_layers();
        self.tree
            .lists(Some("via"))
            .iter()
            .map(|n| {
                let toks: Vec<String> = n
                    .find("layers")
                    .map(|l| l.args().iter().map(|a| a.text().to_string()).collect())
                    .unwrap_or_default();
                let mut lays = expand_layers(&toks, &copper);
                if lays.len() < 2 {
                    lays = vec![
                        copper.first().cloned().unwrap_or("F.Cu".into()),
                        copper.last().cloned().unwrap_or("B.Cu".into()),
                    ];
                }
                Via {
                    pos: xy(n.find("at")),
                    size: n.find("size").and_then(|s| s.arg_f64(0)).unwrap_or(rules.via_size),
                    drill: n.find("drill").and_then(|s| s.arg_f64(0)).unwrap_or(rules.via_drill),
                    layers: (lays[0].clone(), lays[1].clone()),
                    net_id: self.net_ref(n.find("net"), &index),
                }
            })
            .collect()
    }

    pub fn add_track(&mut self, start: Point, end: Point, width: f64, layer: &str, net_id: i64) {
        let n = l(
            "segment",
            vec![
                lxy("start", start.0, start.1),
                lxy("end", end.0, end.1),
                l("width", vec![Node::num(width)]),
                l("layer", vec![Node::str(layer)]),
                l("net", vec![Node::int(net_id)]),
                l("uuid", vec![Node::str(new_uuid())]),
            ],
        );
        self.insert_copper(n);
    }

    pub fn add_via(&mut self, pos: Point, size: f64, drill: f64, net_id: i64, layers: (&str, &str)) {
        let n = l(
            "via",
            vec![
                lxy("at", pos.0, pos.1),
                l("size", vec![Node::num(size)]),
                l("drill", vec![Node::num(drill)]),
                l("layers", vec![Node::str(layers.0), Node::str(layers.1)]),
                l("net", vec![Node::int(net_id)]),
                l("uuid", vec![Node::str(new_uuid())]),
            ],
        );
        self.insert_copper(n);
    }

    fn insert_after_last(&mut self, node: Node, primary: &[&str], fallback: &[&str]) {
        let last = |heads: &[&str], tree: &SList| -> Option<usize> {
            tree.items
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.as_list()
                        .and_then(|s| s.head())
                        .map(|h| heads.contains(&h))
                        .unwrap_or(false)
                })
                .map(|(i, _)| i)
                .next_back()
        };
        let idx = last(primary, &self.tree)
            .or_else(|| last(fallback, &self.tree))
            .unwrap_or(self.tree.items.len().saturating_sub(1));
        self.tree.items.insert(idx + 1, node);
    }

    fn insert_copper(&mut self, node: Node) {
        self.insert_after_last(
            node,
            &["segment", "via", "arc"],
            &[
                "footprint", "module", "gr_line", "gr_rect", "gr_arc", "gr_circle", "gr_poly",
                "gr_text",
            ],
        );
    }

    /// Delete tracks and vias, optionally only on the given nets; returns the count.
    pub fn remove_copper(&mut self, net_ids: Option<&std::collections::HashSet<i64>>) -> usize {
        let index = self.net_index();
        let before = self.tree.items.len();
        let items = std::mem::take(&mut self.tree.items);
        self.tree.items = items
            .into_iter()
            .filter(|c| {
                let Some(s) = c.as_list() else { return true };
                let head = s.head().unwrap_or("");
                let nid = self.net_ref(s.find("net"), &index);
                if matches!(head, "segment" | "via" | "arc")
                    && net_ids.is_none_or(|ids| ids.contains(&nid))
                {
                    return false;
                }
                if head.starts_with("gr_") && nid != 0 {
                    let on_copper = s
                        .find("layer")
                        .and_then(|l| l.arg_text(0))
                        .map(|l| l.ends_with(".Cu"))
                        .unwrap_or(false);
                    if on_copper && net_ids.is_none_or(|ids| ids.contains(&nid)) {
                        return false;
                    }
                }
                true
            })
            .collect();
        before - self.tree.items.len()
    }

    // ---- zones ----------------------------------------------------------
    pub fn zones(&self) -> Vec<Zone> {
        let index = self.net_index();
        self.tree
            .lists(Some("zone"))
            .iter()
            .map(|n| zone_of(n, |net| self.net_ref(net, &index)))
            .collect()
    }

    pub fn add_zone(
        &mut self,
        net_name: &str,
        layer: &str,
        polygon: &[Point],
        connect_clearance: f64,
        solid_pads: bool,
    ) {
        let net = if net_name.is_empty() {
            Net {
                id: 0,
                name: String::new(),
            }
        } else {
            self.ensure_net(net_name)
        };
        let mut connect = l("connect_pads", vec![l("clearance", vec![Node::num(connect_clearance)])]);
        if solid_pads
            && let Node::List(c) = &mut connect {
                c.items.insert(1, Node::sym("yes"));
            }
        let n = l(
            "zone",
            vec![
                l("net", vec![Node::int(net.id)]),
                l("net_name", vec![Node::str(&net.name)]),
                l("layer", vec![Node::str(layer)]),
                l("uuid", vec![Node::str(new_uuid())]),
                l("hatch", vec![Node::sym("edge"), Node::num(0.5)]),
                connect,
                l("min_thickness", vec![Node::num(0.25)]),
                l("filled_areas_thickness", vec![Node::flag(false)]),
                l(
                    "fill",
                    vec![
                        Node::flag(true),
                        l("thermal_gap", vec![Node::num(0.5)]),
                        l("thermal_bridge_width", vec![Node::num(0.5)]),
                        // MEASURED AND REJECTED: island removal at any limit strands the ground
                        // pads whose only pour contact is a small piece -- 0.5 mm2 cost completion
                        // 0.976 -> 0.968 and took DRC warnings from 5 to 41.
                    ],
                ),
                l(
                    "polygon",
                    vec![l(
                        "pts",
                        polygon.iter().map(|p| lxy("xy", p.0, p.1)).collect(),
                    )],
                ),
            ],
        );
        self.tree.push(n);
    }

    /// Drop every `(filled_polygon ..)` a refill left behind, so the file stays the model's truth.
    pub fn strip_zone_fills(&mut self) {
        for item in self.tree.items.iter_mut() {
            if let Node::List(z) = item
                && z.is("zone") {
                    z.remove("filled_polygon");
                    z.remove("fill_segments");
                }
        }
    }

    // ---- outline --------------------------------------------------------
    fn outline_items(&self) -> Vec<&SList> {
        self.tree
            .lists(None)
            .into_iter()
            .filter(|c| {
                c.head().map(|h| h.starts_with("gr_")).unwrap_or(false)
                    && c.find("layer").and_then(|l| l.arg_text(0)) == Some("Edge.Cuts")
            })
            .collect()
    }

    /// The board outline as one closed polygon: the largest loop on Edge.Cuts.
    pub fn outline_polygon(&self) -> Option<Vec<Point>> {
        let mut segs: Vec<(Point, Point)> = Vec::new();
        let mut candidates: Vec<Vec<Point>> = Vec::new();
        for g in self.outline_items() {
            match g.head().unwrap_or("") {
                "gr_line" => segs.push((xy(g.find("start")), xy(g.find("end")))),
                "gr_rect" => {
                    let ((x0, y0), (x1, y1)) = (xy(g.find("start")), xy(g.find("end")));
                    candidates.push(vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]);
                }
                "gr_circle" => {
                    let (c, e) = (xy(g.find("center")), xy(g.find("end")));
                    let r = (c.0 - e.0).hypot(c.1 - e.1);
                    candidates.push(circle_points(
                        c,
                        r,
                        ((std::f64::consts::TAU * r / 0.5) as usize).max(24),
                    ));
                }
                "gr_poly" => {
                    if let Some(pts) = g.find("pts") {
                        candidates.push(pts.lists(Some("xy")).iter().map(|p| xy(Some(p))).collect());
                    }
                }
                "gr_arc" => {
                    let pts = arc_points(
                        xy(g.find("start")),
                        xy(g.find("mid")),
                        xy(g.find("end")),
                        0.5,
                    );
                    segs.extend(pts.windows(2).map(|w| (w[0], w[1])));
                }
                _ => {}
            }
        }
        for chain in chain_segments(&segs, 0.01) {
            if chain.len() >= 3 && crate::geom::dist(chain[0], *chain.last().unwrap()) <= 0.05 {
                candidates.push(chain[..chain.len() - 1].to_vec());
            }
        }
        candidates.into_iter().max_by(|a, b| {
            polygon_area(a)
                .abs()
                .partial_cmp(&polygon_area(b).abs())
                .unwrap()
        })
    }

    pub fn outline_bbox(&self) -> Option<BBox> {
        self.outline_polygon().map(BBox::of_points)
    }

    pub fn clear_outline(&mut self) -> usize {
        let before = self.tree.items.len();
        let items = std::mem::take(&mut self.tree.items);
        self.tree.items = items
            .into_iter()
            .filter(|c| {
                let Some(s) = c.as_list() else { return true };
                !(s.head().map(|h| h.starts_with("gr_")).unwrap_or(false)
                    && s.find("layer").and_then(|l| l.arg_text(0)) == Some("Edge.Cuts"))
            })
            .collect();
        before - self.tree.items.len()
    }

    pub fn set_outline_rect(&mut self, x0: f64, y0: f64, w: f64, h: f64, radius: f64) {
        self.clear_outline();
        let stroke = || {
            l(
                "stroke",
                vec![
                    l("width", vec![Node::num(0.05)]),
                    l("type", vec![Node::sym("default")]),
                ],
            )
        };
        if radius <= 0.0 {
            let n = l(
                "gr_rect",
                vec![
                    lxy("start", x0, y0),
                    lxy("end", x0 + w, y0 + h),
                    stroke(),
                    l("fill", vec![Node::flag(false)]),
                    l("layer", vec![Node::str("Edge.Cuts")]),
                    l("uuid", vec![Node::str(new_uuid())]),
                ],
            );
            self.add_graphic(n);
            return;
        }
        let r = radius.min(w / 2.0).min(h / 2.0);
        let (x1, y1) = (x0 + w, y0 + h);
        for (a, b) in [
            ((x0 + r, y0), (x1 - r, y0)),
            ((x1, y0 + r), (x1, y1 - r)),
            ((x1 - r, y1), (x0 + r, y1)),
            ((x0, y1 - r), (x0, y0 + r)),
        ] {
            let n = l(
                "gr_line",
                vec![
                    lxy("start", a.0, a.1),
                    lxy("end", b.0, b.1),
                    stroke(),
                    l("layer", vec![Node::str("Edge.Cuts")]),
                    l("uuid", vec![Node::str(new_uuid())]),
                ],
            );
            self.add_graphic(n);
        }
        let k = 0.29289321881_f64;
        for (s, m, e) in [
            ((x1 - r, y0), (x1 - r * k, y0 + r * k), (x1, y0 + r)),
            ((x1, y1 - r), (x1 - r * k, y1 - r * k), (x1 - r, y1)),
            ((x0 + r, y1), (x0 + r * k, y1 - r * k), (x0, y1 - r)),
            ((x0, y0 + r), (x0 + r * k, y0 + r * k), (x0 + r, y0)),
        ] {
            let n = l(
                "gr_arc",
                vec![
                    lxy("start", s.0, s.1),
                    lxy("mid", m.0, m.1),
                    lxy("end", e.0, e.1),
                    stroke(),
                    l("layer", vec![Node::str("Edge.Cuts")]),
                    l("uuid", vec![Node::str(new_uuid())]),
                ],
            );
            self.add_graphic(n);
        }
    }

    fn add_graphic(&mut self, node: Node) {
        self.insert_after_last(
            node,
            &["gr_line", "gr_rect", "gr_arc", "gr_circle", "gr_poly", "gr_text"],
            &["footprint", "module"],
        );
    }

    // ---- connectivity ---------------------------------------------------
    pub fn pads_by_net(&self) -> HashMap<i64, Vec<(Footprint, Pad)>> {
        let mut out: HashMap<i64, Vec<(Footprint, Pad)>> = HashMap::new();
        for f in self.footprints() {
            for p in &f.pads {
                if p.net_id != 0 {
                    out.entry(p.net_id).or_default().push((f.clone(), p.clone()));
                }
            }
        }
        out
    }
}

impl Node {
    fn as_list_mut(&mut self) -> &mut SList {
        match self {
            Node::List(l) => l,
            _ => panic!("not a list"),
        }
    }
}

fn prop(n: &SList, name: &str) -> String {
    for p in n.lists(Some("property")) {
        if p.arg_text(0) == Some(name) {
            return p.arg_text(1).unwrap_or("").to_string();
        }
    }
    for t in n.lists(Some("fp_text")) {
        if t.arg_text(0) == Some(&name.to_lowercase()) {
            return t.arg_text(1).unwrap_or("").to_string();
        }
    }
    String::new()
}

fn read_pads(
    n: &SList,
    fp_index: usize,
    fp: &Footprint,
    nets: &HashMap<String, i64>,
    copper: &[String],
) -> Vec<Pad> {
    let mut out = Vec::new();
    for (i, child) in n.items.iter().enumerate() {
        let Some(p) = child.as_list() else { continue };
        if !p.is("pad") {
            continue;
        }
        let args = p.args();
        let number = args.first().map(|a| a.text().to_string()).unwrap_or_default();
        let kind = args.get(1).map(|a| a.text().to_string()).unwrap_or_default();
        let shape = args.get(2).map(|a| a.text().to_string()).unwrap_or_default();
        let (lx, ly, lrot) = at3(p.find("at"));
        let size = p
            .find("size")
            .map(|s| (s.arg_f64(0).unwrap_or(0.0), s.arg_f64(1).unwrap_or(0.0)))
            .unwrap_or((0.0, 0.0));
        let drill = p.find("drill").and_then(|d| {
            let nums: Vec<f64> = d.args().iter().filter_map(|a| a.as_f64()).collect();
            nums.iter().take(2).cloned().fold(None, |acc, v| {
                Some(acc.map_or(v, |a: f64| a.max(v)))
            })
        });
        let toks: Vec<String> = p
            .find("layers")
            .map(|l| l.args().iter().map(|a| a.text().to_string()).collect())
            .unwrap_or_default();
        let layers = expand_layers(&toks, copper);
        let (mut net_id, mut net_name) = (0i64, String::new());
        if let Some(netn) = p.find("net") {
            match netn.arg(0) {
                Some(Node::Num(s)) => {
                    net_id = s.parse::<f64>().unwrap_or(0.0) as i64;
                    net_name = netn.arg_text(1).unwrap_or("").to_string();
                }
                Some(other) => {
                    net_name = other.text().to_string();
                    net_id = *nets.get(&net_name).unwrap_or(&0);
                }
                None => {}
            }
        }
        let rr = p.find("roundrect_rratio").and_then(|r| r.arg_f64(0));
        out.push(Pad {
            number,
            kind,
            shape,
            pos: add(fp.pos, rotate((lx, ly), fp.rot)),
            // KiCad writes a pad angle ABSOLUTELY: it already carries the footprint orientation
            rot: norm_angle(lrot),
            size,
            drill,
            layers,
            net_id,
            net_name,
            roundrect_ratio: rr,
            node_key: (fp_index, i),
            custom_points: custom_pad_points(p),
        });
    }
    out
}

/// Anchor box plus every primitive vertex of a custom pad, in the pad's local frame.
fn custom_pad_points(p: &SList) -> Vec<Point> {
    let Some(prims) = p.find("primitives") else {
        return vec![];
    };
    let mut pts = Vec::new();
    for g in prims.lists(None) {
        let half = g.find("width").and_then(|w| w.arg_f64(0)).unwrap_or(0.0) / 2.0;
        let mut raw: Vec<Point> = Vec::new();
        match g.head().unwrap_or("") {
            "gr_poly" => {
                if let Some(ps) = g.find("pts") {
                    raw = ps.lists(Some("xy")).iter().map(|q| xy(Some(q))).collect();
                }
            }
            "gr_rect" => {
                let ((x0, y0), (x1, y1)) = (xy(g.find("start")), xy(g.find("end")));
                raw = vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
            }
            "gr_circle" => {
                let (c, e) = (xy(g.find("center")), xy(g.find("end")));
                let r = (c.0 - e.0).hypot(c.1 - e.1);
                raw = vec![
                    (c.0 + r, c.1 + r),
                    (c.0 + r, c.1 - r),
                    (c.0 - r, c.1 + r),
                    (c.0 - r, c.1 - r),
                ];
            }
            "gr_line" | "gr_arc" => {
                raw = ["start", "mid", "end"]
                    .iter()
                    .filter_map(|k| g.find(k))
                    .map(|q| xy(Some(q)))
                    .collect();
            }
            _ => {}
        }
        for (x, y) in raw {
            if half > 0.0 {
                for (dx, dy) in [(1.0, 1.0), (1.0, -1.0), (-1.0, 1.0), (-1.0, -1.0)] {
                    pts.push((x + dx * half, y + dy * half));
                }
            } else {
                pts.push((x, y));
            }
        }
    }
    pts
}

fn graphic_points(g: &SList) -> Vec<Point> {
    let h = g.head().unwrap_or("");
    if h.ends_with("_line") {
        return vec![xy(g.find("start")), xy(g.find("end"))];
    }
    if h.ends_with("_rect") {
        let ((x0, y0), (x1, y1)) = (xy(g.find("start")), xy(g.find("end")));
        return vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
    }
    if h.ends_with("_circle") {
        let (c, e) = (xy(g.find("center")), xy(g.find("end")));
        let r = (c.0 - e.0).hypot(c.1 - e.1);
        return circle_points(c, r, 16);
    }
    if h.ends_with("_arc") {
        return arc_points(
            xy(g.find("start")),
            xy(g.find("mid")),
            xy(g.find("end")),
            1.0,
        );
    }
    if h.ends_with("_poly")
        && let Some(pts) = g.find("pts") {
            return pts.lists(Some("xy")).iter().map(|p| xy(Some(p))).collect();
        }
    vec![]
}

fn graphics_bbox(n: &SList, fp: &Footprint, layers: &[&str]) -> BBox {
    let mut b = BBox::empty();
    for g in n.items.iter().filter_map(|c| c.as_list()) {
        if !g.head().map(|h| h.starts_with("fp_")).unwrap_or(false) {
            continue;
        }
        let Some(lay) = g.find("layer").and_then(|l| l.arg_text(0)) else {
            continue;
        };
        if !layers.contains(&lay) {
            continue;
        }
        for p in graphic_points(g) {
            b.add_point(add(fp.pos, rotate(p, fp.rot)));
        }
    }
    b
}

fn courtyard_of(n: &SList, fp: &Footprint) -> BBox {
    let want = if fp.side() == "front" {
        "F.CrtYd"
    } else {
        "B.CrtYd"
    };
    let b = graphics_bbox(n, fp, &[want]);
    if b.valid() {
        return b;
    }
    let mut b = BBox::empty();
    for p in &fp.pads {
        b.add_bbox(&p.bbox());
    }
    b.add_bbox(&graphics_bbox(
        n,
        fp,
        &["F.Fab", "B.Fab", "F.SilkS", "B.SilkS"],
    ));
    if b.valid() {
        b
    } else {
        BBox::new(fp.pos.0 - 0.5, fp.pos.1 - 0.5, fp.pos.0 + 0.5, fp.pos.1 + 0.5)
    }
}

fn zone_of(n: &SList, net_ref: impl Fn(Option<&SList>) -> i64) -> Zone {
    let layers = if let Some(lay) = n.find("layer") {
        vec![lay.arg_text(0).unwrap_or("").to_string()]
    } else if let Some(lays) = n.find("layers") {
        lays.args().iter().map(|a| a.text().to_string()).collect()
    } else {
        vec![]
    };
    let polygon = n
        .find("polygon")
        .and_then(|p| p.find("pts"))
        .map(|pts| pts.lists(Some("xy")).iter().map(|p| xy(Some(p))).collect())
        .unwrap_or_default();
    let keepout = n.find("keepout").map(|ko| {
        ko.lists(None)
            .iter()
            .map(|c| {
                (
                    c.head().unwrap_or("").to_string(),
                    c.arg_text(0) == Some("allowed"),
                )
            })
            .collect()
    });
    let filled = n
        .lists(Some("filled_polygon"))
        .iter()
        .filter_map(|fp| fp.find("pts"))
        .map(|pts| pts.lists(Some("xy")).iter().map(|p| xy(Some(p))).collect())
        .collect();
    let net_id = net_ref(n.find("net"));
    let net_name = n
        .find("net_name")
        .and_then(|nn| nn.arg_text(0))
        .map(str::to_string)
        .or_else(|| match n.find("net").and_then(|x| x.arg(0)) {
            Some(Node::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Zone {
        net_id,
        net_name,
        layers,
        polygon,
        keepout,
        name: n
            .find("name")
            .and_then(|x| x.arg_text(0))
            .unwrap_or("")
            .to_string(),
        filled,
    }
}

fn set_child_at(c: &mut SList, angle: impl Fn(f64) -> f64, flip_y: bool) {
    let Some(at) = c.find_mut("at") else { return };
    let x = at.arg_f64(0).unwrap_or(0.0);
    let y = at.arg_f64(1).unwrap_or(0.0);
    let ang = norm_angle(angle(at.arg_f64(2).unwrap_or(0.0)));
    let mut args = vec![Node::num(x), Node::num(if flip_y { -y } else { y })];
    if ang != 0.0 {
        args.push(Node::num(ang));
    }
    at.set_args(args);
}

fn set_child_at_self(node: &mut SList, angle: impl Fn(f64) -> f64) {
    let Some(at) = node.find_mut("at") else { return };
    let x = at.arg_f64(0).unwrap_or(0.0);
    let y = at.arg_f64(1).unwrap_or(0.0);
    let ang = norm_angle(angle(at.arg_f64(2).unwrap_or(0.0)));
    let mut args = vec![Node::num(x), Node::num(y)];
    if ang != 0.0 {
        args.push(Node::num(ang));
    }
    at.set_args(args);
}

fn mirror_graphic_y(c: &mut SList) {
    for key in ["start", "end", "center", "mid"] {
        if let Some(p) = c.find_mut(key) {
            let (x, y) = (p.arg_f64(0).unwrap_or(0.0), p.arg_f64(1).unwrap_or(0.0));
            p.set_args(vec![Node::num(x), Node::num(-y)]);
        }
    }
    if let Some(pts) = c.find_mut("pts") {
        for item in pts.items.iter_mut() {
            if let Node::List(p) = item
                && p.is("xy") {
                    let (x, y) = (p.arg_f64(0).unwrap_or(0.0), p.arg_f64(1).unwrap_or(0.0));
                    p.set_args(vec![Node::num(x), Node::num(-y)]);
                }
        }
    }
}

/// Parse a `.kicad_mod` library footprint.
pub fn parse_footprint_module(text: &str) -> anyhow::Result<SList> {
    let m = sexp::parse(text)?;
    anyhow::ensure!(m.is("footprint") || m.is("module"), "not a footprint");
    Ok(m)
}

pub const EMPTY_BOARD: &str = r#"(kicad_pcb (version 20241229) (generator "pcb-auto") (generator_version "0.1")
 (general (thickness 1.6) (legacy_teardrops no))
 (paper "A4")
 (layers
  (0 "F.Cu" signal) (2 "B.Cu" signal)
  (9 "F.Adhes" user "F.Adhesive") (11 "B.Adhes" user "B.Adhesive")
  (13 "F.Paste" user) (15 "B.Paste" user)
  (5 "F.SilkS" user "F.Silkscreen") (7 "B.SilkS" user "B.Silkscreen")
  (1 "F.Mask" user) (3 "B.Mask" user)
  (17 "Dwgs.User" user "User.Drawings") (19 "Cmts.User" user "User.Comments")
  (21 "Eco1.User" user "User.Eco1") (23 "Eco2.User" user "User.Eco2")
  (25 "Edge.Cuts" user) (27 "Margin" user)
  (31 "F.CrtYd" user "F.Courtyard") (29 "B.CrtYd" user "B.Courtyard")
  (35 "F.Fab" user) (33 "B.Fab" user))
 (setup (pad_to_mask_clearance 0) (allow_soldermask_bridges_in_footprints no))
 (net 0 ""))
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_board_has_two_copper_layers_and_an_outline_api() {
        let mut b = Board::empty(2);
        assert_eq!(b.copper_layers(), vec!["F.Cu", "B.Cu"]);
        assert!(b.outline_polygon().is_none());
        b.set_outline_rect(0.0, 0.0, 50.0, 30.0, 0.0);
        let bb = b.outline_bbox().unwrap();
        assert!((bb.w() - 50.0).abs() < 1e-9 && (bb.h() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn four_layer_stack_names_inner_layers() {
        let b = Board::empty(4);
        assert_eq!(b.copper_layers(), vec!["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"]);
    }
}
