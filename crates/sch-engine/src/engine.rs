//! Layout engine: parts with pin geometry and text room ([`PartInst`]), routing / net labels /
//! power symbols for one block ([`GroupLayout`]), and insertion of a new circuit into an existing
//! sheet (edit mode).
//!
//! Input is a "netlist design": parts with a pin -> net map (`"pins": {"1": "GND", "PA5": "LED"}`,
//! `"pins_default": "nc"|"label"|"float"`), optional `"power"` (extra rail names), `"flags"` (nets
//! that get a PWR_FLAG), and the `"layout"` trees consumed by [`crate::flexlayout`], which places
//! the parts and then calls [`GroupLayout::route`] / [`GroupLayout::emit`]. No coordinates anywhere
//! in the input; the output is raw design JSON.
//!
//! A faithful port of `schagent/engine.py`: same algorithm, same costs and thresholds, same
//! tie-breaking. Python dicts iterate in insertion order, so ordered containers are used wherever
//! that order reaches the result.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::geo::{
    self, Geo, LayoutError, PAPERS, PartOpts, STUB, dir_of, extent, flag_at_symbol_clear,
    flag_at_symbol_get, flag_at_symbol_set, is_gnd, pack_blocks, pin_pos, rot_label, snap_even,
};
use crate::model::{GRID, rot_point};
use crate::symlib::{Pin, SymbolInfo, index};

pub type Pt = [f64; 2];
pub type Dir = (i32, i32);
pub type Box4 = [f64; 4];
pub type Path = Vec<Pt>;
/// `(part id, pin number)`.
pub type PinKey = (String, String);

type Res<T> = Result<T, LayoutError>;

/// `(x, y, rot, justify)` of a library property, in lib coordinates.
type LibPropXform = (f64, f64, i32, String);

/// A grid cell key: its coordinate paired with the direction wires leave it in.
type CellDir = ((i64, i64), Dir);

/// A net queued for routing: its name, pin-index pairs to connect, and whether it is a supply net.
type RoutableNet = (String, Vec<(usize, usize)>, bool);

// ---------------------------------------------------------------------------- small utilities

/// Python's `round`: half to even.
fn py_round(v: f64) -> f64 {
    let f = v.floor();
    let diff = v - f;
    if diff > 0.5 || (diff == 0.5 && (f as i64).rem_euclid(2) != 0) {
        f + 1.0
    } else {
        f
    }
}

fn ri(v: f64) -> i64 {
    py_round(v) as i64
}

fn cmpf(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

fn fmin(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::INFINITY, f64::min)
}
fn fmax(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

/// Insertion-ordered string-keyed map (mirrors a Python `dict`).
#[derive(Clone, Debug)]
pub struct OrderMap<V> {
    keys: Vec<String>,
    vals: Vec<V>,
    idx: HashMap<String, usize>,
}

impl<V> Default for OrderMap<V> {
    fn default() -> Self {
        OrderMap {
            keys: Vec::new(),
            vals: Vec::new(),
            idx: HashMap::new(),
        }
    }
}

impl<V> OrderMap<V> {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self, k: &str) -> Option<&V> {
        self.idx.get(k).map(|&i| &self.vals[i])
    }
    pub fn get_mut(&mut self, k: &str) -> Option<&mut V> {
        let i = *self.idx.get(k)?;
        Some(&mut self.vals[i])
    }
    pub fn contains_key(&self, k: &str) -> bool {
        self.idx.contains_key(k)
    }
    pub fn insert(&mut self, k: impl Into<String>, v: V) {
        let k = k.into();
        match self.idx.get(&k) {
            Some(&i) => self.vals[i] = v,
            None => {
                self.idx.insert(k.clone(), self.keys.len());
                self.keys.push(k);
                self.vals.push(v);
            }
        }
    }
    /// `d[k]` on a `defaultdict`.
    pub fn entry_or(&mut self, k: &str, default: V) -> &mut V {
        if !self.idx.contains_key(k) {
            self.insert(k.to_string(), default);
        }
        self.get_mut(k).unwrap()
    }
    pub fn len(&self) -> usize {
        self.keys.len()
    }
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.keys.iter()
    }
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.vals.iter()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.keys.iter().zip(self.vals.iter())
    }
}

/// Iteration order of a CPython `set` of small ints, which `route_trunk` relies on to break score
/// ties the same way.
///
/// CPython hashes an int to itself and walks the open-addressing table in slot order, so the order
/// is `value & mask` for the table the set grew to. Exact whenever no two candidates collide modulo
/// the table size (always, for the few dozen block-local coordinates involved); a collision falls
/// back to insertion order.
fn py_int_set_order(vals: &[i64]) -> Vec<i64> {
    let mut size: i64 = 8;
    let mut fill: i64 = 0;
    for _ in vals {
        fill += 1;
        if fill * 5 >= (size - 1) * 3 {
            let minused = fill * 4;
            let mut ns = 8i64;
            while ns <= minused {
                ns <<= 1;
            }
            size = ns;
        }
    }
    let mask = size - 1;
    let mut idx: Vec<usize> = (0..vals.len()).collect();
    idx.sort_by_key(|&i| (vals[i] & mask, i));
    idx.into_iter().map(|i| vals[i]).collect()
}

/// Distinct values in insertion order.
fn uniq(vals: &[i64]) -> Vec<i64> {
    let mut seen = HashSet::new();
    vals.iter().copied().filter(|v| seen.insert(*v)).collect()
}

/// `str(v)` of a JSON scalar, as Python writes it.
fn json_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// Stand-in for `difflib.get_close_matches` (error-message hints only, not a parity surface).
fn close_matches(word: &str, candidates: &[String], n: usize, cutoff: f64) -> Vec<String> {
    fn ratio(a: &str, b: &str) -> f64 {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let mut prev = vec![0usize; b.len() + 1];
        for &ca in &a {
            let mut cur = vec![0usize; b.len() + 1];
            for (j, &cb) in b.iter().enumerate() {
                cur[j + 1] = if ca == cb {
                    prev[j] + 1
                } else {
                    cur[j].max(prev[j + 1])
                };
            }
            prev = cur;
        }
        if a.is_empty() && b.is_empty() {
            return 1.0;
        }
        2.0 * prev[b.len()] as f64 / (a.len() + b.len()) as f64
    }
    let mut scored: Vec<(f64, &String)> = candidates
        .iter()
        .map(|c| (ratio(word, c), c))
        .filter(|(r, _)| *r >= cutoff)
        .collect();
    scored.sort_by(|a, b| cmpf(b.0, a.0));
    scored.into_iter().take(n).map(|(_, c)| c.clone()).collect()
}

// ---------------------------------------------------------------------------- symbol geometry

thread_local! {
    /// lib_id -> property name -> (x, y, rot, justify), in library coordinates.
    static LIB_PROPS: RefCell<HashMap<String, HashMap<String, LibPropXform>>> =
        RefCell::new(HashMap::new());
}

/// `(x, y, rot, justify)` of a library property, in lib coordinates (read lazily, cached).
fn lib_prop(info: &SymbolInfo, name: &str) -> Option<LibPropXform> {
    LIB_PROPS.with(|c| {
        let mut c = c.borrow_mut();
        let cache = c.entry(info.lib_id.clone()).or_insert_with(|| {
            let mut out = HashMap::new();
            if let Some(node) = index().raw_symbol(&info.lib_id) {
                for ch in node.as_list().unwrap_or(&[]).iter().skip(1) {
                    let Some(items) = ch.as_list() else { continue };
                    if items.is_empty() || ch.tag() != "property" || items.len() < 2 {
                        continue;
                    }
                    let Some(at) = ch.child("at").and_then(|a| a.as_list()) else {
                        continue;
                    };
                    let justify = ch
                        .child("effects")
                        .and_then(|e| e.child("justify"))
                        .and_then(|j| j.as_list())
                        .map(|l| {
                            l[1..]
                                .iter()
                                .map(|a| a.text())
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    out.insert(
                        items[1].text(),
                        (
                            at.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
                            at.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0),
                            at.get(3).and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
                            justify,
                        ),
                    );
                }
            }
            out
        });
        cache.get(name).cloned()
    })
}

/// Graphics-only bbox of a symbol after rot/mirror, as `(half_x, half_y)` in mm around the anchor.
pub fn rot_body(info: &SymbolInfo, unit: i32, rot: i32, mirror: &str) -> (f64, f64) {
    let bb = info.body_for(unit);
    let mut hx: f64 = 0.0;
    let mut hy: f64 = 0.0;
    for x in [bb.0, bb.2] {
        for y in [bb.1, bb.3] {
            let q = rot_point(x, y, rot, mirror);
            hx = hx.max(q.0.abs());
            hy = hy.max(q.1.abs());
        }
    }
    (hx, hy)
}

/// `"h"` when a 2-pin part's pins end up left/right of each other after the transform, else `"v"`.
pub fn two_pin_axis(info: &SymbolInfo, unit: i32, rot: i32, mirror: &str) -> &'static str {
    let pins = info.pins_for_unit(unit);
    if pins.len() != 2 {
        return "v";
    }
    let a = rot_point(pins[0].x, pins[0].y, rot, mirror);
    let b = rot_point(pins[1].x, pins[1].y, rot, mirror);
    if (a.1 - b.1).abs() < 1e-6 { "h" } else { "v" }
}

/// Where a connector's reference/value pair goes (after rot/mirror): the side opposite its pins when
/// they are all on one side (so it never crosses a wire), otherwise the first pin-free side.
pub fn connector_text_slot(info: &SymbolInfo, unit: i32, rot: i32, mirror: &str) -> &'static str {
    let mut sides: Vec<&'static str> = Vec::new();
    for pin in info.pins_for_unit(unit) {
        let s = match dir_of(pin, rot, mirror) {
            (0, 1) => "bottom",
            (1, 0) => "right",
            (-1, 0) => "left",
            _ => "top",
        };
        if !sides.contains(&s) {
            sides.push(s);
        }
    }
    if sides.len() == 1 {
        return match sides[0] {
            "right" => "left",
            "left" => "right",
            "top" => "below",
            _ => "above",
        };
    }
    for (slot, side) in [
        ("above", "top"),
        ("below", "bottom"),
        ("right", "right"),
        ("left", "left"),
    ] {
        if !sides.contains(&side) {
            return slot;
        }
    }
    "above"
}

/// Connections longer than this become net labels.
pub const MAX_WIRE: f64 = 32.0;

/// `[+-]?\d+(\.\d+)?V\d*[A-Z]*`
fn is_voltage_name(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let d0 = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == d0 {
        return false;
    }
    if i < b.len() && b[i] == b'.' {
        let j = i + 1;
        let mut k = j;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        if k == j {
            return false;
        }
        i = k;
    }
    if i >= b.len() || b[i] != b'V' {
        return false;
    }
    i += 1;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_uppercase() {
        i += 1;
    }
    i == b.len()
}

pub fn is_power_net(net: &str, extra: &HashSet<String>) -> bool {
    if extra.contains(net) || net == "PWR_FLAG" || is_gnd(net) {
        return true;
    }
    if index().get(&format!("power:{net}")).is_some() {
        return true;
    }
    is_voltage_name(net)
        || matches!(
            net,
            "VCC" | "VDD" | "VBUS" | "VBAT" | "VEE" | "VSS" | "VIN" | "VOUT"
        )
}

// ---------------------------------------------------------------------------- PartInst

/// One part of a block: symbol, pin -> net map, placement, and the room its texts and attachments
/// need.
#[derive(Clone)]
pub struct PartInst {
    pub id: String,
    pub lib: String,
    pub info: Arc<SymbolInfo>,
    pub unit: i32,
    pub value: String,
    pub footprint: String,
    pub fields: Option<Map<String, Value>>,
    /// Visible pins of this unit.
    pub pins: Vec<Pin>,
    /// pin number -> net name / "nc" / "float".
    pub pinmap: OrderMap<String>,
    pub at: Pt,
    pub rot: i32,
    pub mirror: String,
    pub two_pin: bool,
    /// Set by [`GroupLayout`]: all connected pins are on power nets (decoupling caps etc.).
    pub power_only: bool,
    pub is_connector: bool,
    /// Parts humans freely rotate/mirror: passives, diodes, transistors, small switches.
    pub rotatable: bool,
    /// Expected attachment reach per pin, set by [`GroupLayout`].
    pub attach: OrderMap<f64>,
    /// `id`, or `id#unit` for units other than 1 (set by [`crate::flexlayout`]).
    pub key: String,
}

impl PartInst {
    pub fn new(pj: &Value) -> Res<PartInst> {
        let id = pj
            .get("id")
            .map(json_str)
            .ok_or_else(|| LayoutError("part is missing field 'id'".into()))?;
        let lib_id = pj
            .get("lib")
            .map(json_str)
            .ok_or_else(|| LayoutError("part is missing field 'lib'".into()))?;
        let info = geo::info(&lib_id)?;
        if info.power || lib_id.starts_with("power:") {
            return Err(LayoutError(format!(
                "{id}: {lib_id} is a power symbol, not a part. Remove it from parts; connect pins to the net name \
                 (e.g. \"GND\", \"+9V\") and list nets needing a PWR_FLAG in \"flags\""
            )));
        }
        let unit = pj.get("unit").and_then(|v| v.as_i64()).unwrap_or(1) as i32;
        let pins: Vec<Pin> = info
            .pins_for_unit(unit)
            .into_iter()
            .filter(|p| !p.hidden)
            .cloned()
            .collect();
        let mut pinmap: OrderMap<String> = OrderMap::new();
        let default = pj
            .get("pins_default")
            .map(json_str)
            .unwrap_or_else(|| "nc".to_string());
        for p in &pins {
            let v = if default != "label" {
                default.clone()
            } else {
                let head = p.name.split('/').next().unwrap_or("").to_string();
                if head.is_empty() {
                    p.number.clone()
                } else {
                    head
                }
            };
            pinmap.insert(p.number.clone(), v);
        }
        if let Some(Value::Object(map)) = pj.get("pins") {
            for (k, v) in map {
                let matched: Vec<&Pin> = {
                    let by_num: Vec<&Pin> = pins.iter().filter(|p| &p.number == k).collect();
                    if by_num.is_empty() {
                        pins.iter().filter(|p| &p.name == k).collect()
                    } else {
                        by_num
                    }
                };
                if matched.is_empty() {
                    let mut names: Vec<String> = pins.iter().map(|p| p.name.clone()).collect();
                    names.extend(pins.iter().map(|p| p.number.clone()));
                    let near = close_matches(k, &names, 3, 0.5);
                    let hint = if near.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " (did you mean {}?)",
                            near.iter()
                                .map(|x| format!("'{x}'"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let mut listed: String = pins
                        .iter()
                        .map(|p| format!("{}={}", p.number, p.name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    listed.truncate(500);
                    return Err(LayoutError(format!(
                        "{id} ({lib_id}): no pin '{k}'{hint}; pins: {listed}"
                    )));
                }
                let val = if v.is_null() {
                    "nc".to_string()
                } else {
                    json_str(v)
                };
                for p in matched {
                    pinmap.insert(p.number.clone(), val.clone());
                }
            }
        }
        let fam = lib_id.split(':').next().unwrap_or("").to_string();
        let name = lib_id.rsplit(':').next().unwrap_or("").to_string();
        let two_pin = pins.len() == 2;
        let is_connector = info.ref_prefix == "J" || fam.starts_with("Connector");
        let rotatable = (two_pin && !is_connector)
            || fam.starts_with("Transistor")
            || name.starts_with("Q_")
            || name.starts_with("D_")
            || name.starts_with("LED")
            || matches!(
                info.ref_prefix.as_str(),
                "Q" | "D" | "SW" | "L" | "FB" | "TP"
            );
        Ok(PartInst {
            key: id.clone(),
            id,
            lib: lib_id,
            info,
            unit,
            value: pj.get("value").map(json_str).unwrap_or_default(),
            footprint: pj.get("footprint").map(json_str).unwrap_or_default(),
            fields: pj.get("fields").and_then(|f| f.as_object().cloned()),
            pins,
            pinmap,
            at: [0.0, 0.0],
            rot: 0,
            mirror: String::new(),
            two_pin,
            power_only: false,
            is_connector,
            rotatable,
            attach: OrderMap::new(),
        })
    }

    pub fn pin_pos(&self, pin: &Pin) -> Pt {
        pin_pos(pin, self.at, self.rot, &self.mirror)
    }

    pub fn pin_pos_at(&self, pin: &Pin, at: Pt, rot: i32, mirror: &str) -> Pt {
        pin_pos(pin, at, rot, mirror)
    }

    pub fn pin_dir(&self, pin: &Pin) -> Dir {
        dir_of(pin, self.rot, &self.mirror)
    }

    pub fn pin_dir_at(&self, pin: &Pin, rot: i32, mirror: &str) -> Dir {
        dir_of(pin, rot, mirror)
    }

    /// Extent including text room and attachment zones.
    pub fn extent(&self) -> Box4 {
        self.ext(self.at, self.rot, &self.mirror, true).0
    }

    pub fn extent_no_attach(&self) -> Box4 {
        self.ext(self.at, self.rot, &self.mirror, false).0
    }

    /// Extent for a hypothetical placement (`flexlayout::_leaf_box`).
    pub fn extent_of(&self, at: Pt, rot: i32, mirror: &str) -> Box4 {
        self.ext(at, rot, mirror, true).0
    }

    /// Component boxes of the extent: `[symbol bbox, text boxes.., attachment zones..]`. The router
    /// blocks these individually so a wide value text does not wall off the pin row next to it.
    pub fn boxes(&self, attachments: bool) -> Vec<Box4> {
        self.ext(self.at, self.rot, &self.mirror, attachments).1
    }

    fn ext(&self, at: Pt, rot: i32, mirror: &str, attachments: bool) -> (Box4, Vec<Box4>) {
        let mut e = extent(&self.info, at, rot, mirror, self.unit);
        let mut boxes: Vec<Box4> = vec![e];
        macro_rules! grow {
            ($b:expr) => {{
                let b: Box4 = $b;
                boxes.push(b);
                e[0] = e[0].min(b[0]);
                e[1] = e[1].min(b[1]);
                e[2] = e[2].max(b[2]);
                e[3] = e[3].max(b[3]);
            }};
        }
        let libname = self.lib.rsplit(':').next().unwrap_or("").to_string();
        let val_text = if self.value.is_empty() {
            libname
        } else {
            self.value.clone()
        };
        let tw = 0.95 * self.id.chars().count().max(val_text.chars().count()) as f64 + 0.6;
        if self.two_pin
            && !self.is_connector
            && two_pin_axis(&self.info, self.unit, rot, mirror) == "v"
        {
            let right = at[0] + rot_body(&self.info, self.unit, rot, mirror).0 / GRID + 0.7;
            grow!([right, at[1] - 2.2, right + tw, at[1] + 2.2]);
        } else if self.is_connector || (!self.two_pin && (rot == 90 || rot == 270)) {
            // connectors and rotated multi-pin parts (transistors lying sideways): reference + value
            // stacked in the pin-free slot (compile::emit_part does the same)
            let (hx, hy) = rot_body(&self.info, self.unit, rot, mirror);
            let (bl, br) = (at[0] - hx / GRID, at[0] + hx / GRID);
            let (bt, bbm) = (at[1] - hy / GRID, at[1] + hy / GRID);
            let slot = connector_text_slot(&self.info, self.unit, rot, mirror);
            let dirs: Vec<Dir> = self.pins.iter().map(|p| dir_of(p, rot, mirror)).collect();
            // texts above/below keep clear of the pin side: they end at the body edge away from
            // lateral pins
            let (x0_, x1_) = if dirs.contains(&(1, 0)) {
                (br - tw, br)
            } else if dirs.contains(&(-1, 0)) {
                (bl, bl + tw)
            } else {
                ((bl + br) / 2.0 - tw / 2.0, (bl + br) / 2.0 + tw / 2.0)
            };
            let bx: Box4 = match slot {
                "above" => [x0_, bt - 4.8, x1_, bt - 0.2],
                "below" => [x0_, bbm + 0.2, x1_, bbm + 4.8],
                "right" => [br + 0.6, bt + 0.2, br + 0.6 + tw, bt + 3.8],
                _ => [bl - 0.6 - tw, bt + 0.2, bl - 0.6, bt + 3.8],
            };
            grow!(bx);
        } else if self.two_pin && !self.is_connector {
            // horizontal passive: reference above / value below the body
            let bh = rot_body(&self.info, self.unit, rot, mirror).1 / GRID;
            let half = bh + 1.4 / GRID + 0.8;
            grow!([at[0] - tw / 2.0, at[1] - half, at[0] + tw / 2.0, at[1] - bh]);
            grow!([at[0] - tw / 2.0, at[1] + bh, at[0] + tw / 2.0, at[1] + half]);
        }
        // exact reference/value text boxes, from the library's property positions
        if !(self.two_pin || self.is_connector || rot == 90 || rot == 270) {
            for which in ["Reference", "Value"] {
                let text = if which == "Reference" {
                    self.id.clone()
                } else {
                    val_text.clone()
                };
                let Some((lx, ly, lrot, justify)) = lib_prop(&self.info, which) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                let (dx, dy) = rot_point(lx, ly, rot, mirror);
                let (tx, ty) = (at[0] + dx / GRID, at[1] + dy / GRID);
                let trot = (lrot + rot).rem_euclid(180);
                let (mut w, mut h) = (0.95 * text.chars().count() as f64 + 0.6, 1.5);
                if trot == 90 {
                    std::mem::swap(&mut w, &mut h);
                }
                let mut hj = if justify.contains("left") {
                    "left"
                } else if justify.contains("right") {
                    "right"
                } else {
                    "center"
                };
                if rot == 180 || mirror == "y" {
                    hj = match hj {
                        "left" => "right",
                        "right" => "left",
                        o => o,
                    };
                }
                let (x0, x1, y0, y1) = if trot == 90 {
                    (tx - w / 2.0, tx + w / 2.0, ty - h / 2.0, ty + h / 2.0)
                } else {
                    let x0 = match hj {
                        "left" => tx,
                        "right" => tx - w,
                        _ => tx - w / 2.0,
                    };
                    (x0, x0 + w, ty - h / 2.0, ty + h / 2.0)
                };
                grow!([x0, y0, x1, y1]);
            }
        }
        if attachments && !self.attach.is_empty() {
            for pin in &self.pins {
                let reach = *self.attach.get(&pin.number).unwrap_or(&0.0);
                if reach == 0.0 {
                    continue;
                }
                let pos = pin_pos(pin, at, rot, mirror);
                let d = dir_of(pin, rot, mirror);
                if d.1 != 0 && reach != 6.0 {
                    // label on a vertical pin: placed as a short L-stub with horizontal text, so
                    // reserve 3 units along the pin and the text length sideways
                    let fy = pos[1] + d.1 as f64 * 3.0;
                    grow!([
                        pos[0] - (reach - 1.0),
                        fy - 1.6,
                        pos[0] + (reach - 1.0),
                        fy + 1.6
                    ]);
                    continue;
                }
                let (fx, fy) = (pos[0] + d.0 as f64 * reach, pos[1] + d.1 as f64 * reach);
                let hw = if reach >= 6.0 { 3.5 } else { 2.0 };
                grow!([fx - hw, fy - hw, fx + hw, fy + hw]);
                if reach >= 6.0 && d.1 == 0 {
                    // side power pins turn up/down by 4 units before the symbol: reserve that too
                    grow!([fx - hw, fy - 8.0, fx + hw, fy + 8.0]);
                }
            }
        }
        (e, boxes)
    }
}

// ---------------------------------------------------------------------------- segment helpers

fn overlap(a: &Box4, b: &Box4, m: f64) -> bool {
    !(a[2] + m <= b[0] || b[2] + m <= a[0] || a[3] + m <= b[1] || b[3] + m <= a[1])
}

fn seg_hits_box(a: Pt, b: Pt, bx: &Box4, eps: f64) -> bool {
    let (x0, y0, x1, y1) = (bx[0] + eps, bx[1] + eps, bx[2] - eps, bx[3] - eps);
    if x0 >= x1 || y0 >= y1 {
        return false;
    }
    if (a[0] - b[0]).abs() < 1e-6 {
        return x0 < a[0] && a[0] < x1 && a[1].max(b[1]) > y0 && a[1].min(b[1]) < y1;
    }
    if (a[1] - b[1]).abs() < 1e-6 {
        return y0 < a[1] && a[1] < y1 && a[0].max(b[0]) > x0 && a[0].min(b[0]) < x1;
    }
    true
}

fn segs_cross(a: Pt, b: Pt, c: Pt, d: Pt) -> bool {
    let va = (a[0] - b[0]).abs() < 1e-6;
    let vc = (c[0] - d[0]).abs() < 1e-6;
    if va == vc {
        // collinear overlap counts as a crossing (ugly)
        if va && (a[0] - c[0]).abs() < 1e-6 {
            return a[1].min(b[1]).max(c[1].min(d[1])) < a[1].max(b[1]).min(c[1].max(d[1]));
        }
        if !va && (a[1] - c[1]).abs() < 1e-6 {
            return a[0].min(b[0]).max(c[0].min(d[0])) < a[0].max(b[0]).min(c[0].max(d[0]));
        }
        return false;
    }
    let (a, b, c, d) = if !va { (c, d, a, b) } else { (a, b, c, d) };
    let (x, y) = (a[0], c[1]);
    a[1].min(b[1]) < y && y < a[1].max(b[1]) && c[0].min(d[0]) < x && x < c[0].max(d[0])
}

/// True if collinear segment `a`-`b` contains segment `p`-`q`.
pub fn covers(a: Pt, b: Pt, p: Pt, q: Pt) -> bool {
    if (a[0] - b[0]).abs() < 1e-6 && (p[0] - q[0]).abs() < 1e-6 && (a[0] - p[0]).abs() < 1e-6 {
        return a[1].min(b[1]) - 1e-6 <= p[1].min(q[1]) && p[1].max(q[1]) <= a[1].max(b[1]) + 1e-6;
    }
    if (a[1] - b[1]).abs() < 1e-6 && (p[1] - q[1]).abs() < 1e-6 && (a[1] - p[1]).abs() < 1e-6 {
        return a[0].min(b[0]) - 1e-6 <= p[0].min(q[0]) && p[0].max(q[0]) <= a[0].max(b[0]) + 1e-6;
    }
    false
}

fn seg_pairs(path: &[Pt]) -> impl Iterator<Item = (Pt, Pt)> + '_ {
    path.windows(2).map(|w| (w[0], w[1]))
}

// ---------------------------------------------------------------------------- GroupLayout

/// One endpoint of a net.
#[derive(Clone, Copy)]
struct End {
    /// Index into [`GroupLayout::parts`].
    p: usize,
    /// Index into that part's `pins`.
    pin: usize,
    pos: Pt,
    d: Dir,
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// A Dijkstra queue entry ordered exactly like Python's `(cost, pos, dir)` heap tuples (reversed,
/// because `BinaryHeap` is a max-heap and `heapq` is a min-heap).
struct QItem {
    cost: f64,
    pos: (i64, i64),
    d: Dir,
}
impl PartialEq for QItem {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for QItem {}
impl PartialOrd for QItem {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for QItem {
    fn cmp(&self, o: &Self) -> Ordering {
        cmpf(o.cost, self.cost)
            .then(o.pos.cmp(&self.pos))
            .then(o.d.cmp(&self.d))
    }
}

/// Places and wires the parts of one functional group.
pub struct GroupLayout {
    pub parts: Vec<PartInst>,
    /// net -> total pin count in the whole design (to detect cross-group nets).
    pub all_nets: HashMap<String, usize>,
    /// `(part id, pin number)` known to need a net label (from a previous pass).
    pub label_pins: HashSet<PinKey>,
    /// Extra rail names.
    pub power: HashSet<String>,
    /// Nets that get a PWR_FLAG in this group.
    pub flags: Vec<String>,
    pub nets: OrderMap<Vec<(usize, usize)>>,
    pub signal_nets: OrderMap<Vec<(usize, usize)>>,
    /// Non-ground power nets: routed locally with short wires when possible.
    pub supply_nets: OrderMap<Vec<(usize, usize)>>,
    pub pure_power_group: bool,
    pub wires: Vec<Path>,
    /// net -> indices into [`GroupLayout::wires`].
    pub net_wires: OrderMap<Vec<usize>>,
    pub labelled_pins: HashSet<PinKey>,
    pub wired_pins: HashSet<PinKey>,
    pub geo: Geo,
    /// Connections longer than this become labels (`flexlayout` raises it to 44).
    pub max_wire: f64,
    /// Draw long connections whose path is straight or single-bend instead of labelling them.
    pub long_simple: bool,
    reserved: Vec<(Box4, Option<String>)>,
    bus_pairs: HashSet<(String, String)>,
    power_symbol_pins: HashSet<PinKey>,
    symbol_on_wire: Vec<(String, Pt)>,
}

impl GroupLayout {
    pub fn new(
        parts: Vec<PartInst>,
        power: HashSet<String>,
        flags: Vec<String>,
        all_nets: HashMap<String, usize>,
        label_pins: HashSet<PinKey>,
    ) -> GroupLayout {
        let mut gl = GroupLayout {
            parts,
            all_nets,
            label_pins,
            power,
            flags,
            nets: OrderMap::new(),
            signal_nets: OrderMap::new(),
            supply_nets: OrderMap::new(),
            pure_power_group: false,
            wires: Vec::new(),
            net_wires: OrderMap::new(),
            labelled_pins: HashSet::new(),
            wired_pins: HashSet::new(),
            geo: Geo::default(),
            max_wire: MAX_WIRE,
            long_simple: true,
            reserved: Vec::new(),
            bus_pairs: HashSet::new(),
            power_symbol_pins: HashSet::new(),
            symbol_on_wire: Vec::new(),
        };
        for pi in 0..gl.parts.len() {
            for qi in 0..gl.parts[pi].pins.len() {
                let p = &gl.parts[pi];
                let net = p
                    .pinmap
                    .get(&p.pins[qi].number)
                    .cloned()
                    .unwrap_or_else(|| "nc".into());
                if net == "nc" || net == "float" || net.is_empty() {
                    continue;
                }
                gl.nets.entry_or(&net, Vec::new()).push((pi, qi));
            }
        }
        let (mut sig, mut sup) = (OrderMap::new(), OrderMap::new());
        for (net, v) in gl.nets.iter() {
            if !is_power_net(net, &gl.power) {
                sig.insert(net.clone(), v.clone());
            } else if !is_gnd(net) && net != "PWR_FLAG" {
                sup.insert(net.clone(), v.clone());
            }
        }
        gl.signal_nets = sig;
        gl.supply_nets = sup;
        let power_names = gl.power.clone();
        for p in gl.parts.iter_mut() {
            let mut nets_of: Vec<String> = Vec::new();
            for pin in &p.pins {
                if let Some(n) = p.pinmap.get(&pin.number)
                    && n != "nc"
                    && n != "float"
                    && !n.is_empty()
                    && !nets_of.contains(n)
                {
                    nets_of.push(n.clone());
                }
            }
            p.power_only =
                !nets_of.is_empty() && nets_of.iter().all(|n| is_power_net(n, &power_names));
        }
        gl.pure_power_group = gl.parts.iter().all(|p| p.power_only);
        if gl.pure_power_group {
            // in a power block the supply chain is the circuit: wire it
            for p in gl.parts.iter_mut() {
                p.power_only = false;
            }
        }
        // expected attachment reach per pin (for collision-aware placement)
        let label_pins = gl.label_pins.clone();
        let no_label_pins = label_pins.is_empty();
        for pi in 0..gl.parts.len() {
            let mut attach = OrderMap::new();
            for qi in 0..gl.parts[pi].pins.len() {
                let p = &gl.parts[pi];
                let number = p.pins[qi].number.clone();
                let net = p
                    .pinmap
                    .get(&number)
                    .cloned()
                    .unwrap_or_else(|| "nc".into());
                if net == "nc" || net == "float" || net.is_empty() {
                    continue;
                }
                if let Some(here) = gl.signal_nets.get(&net) {
                    // pins that will certainly carry a net label reserve room for it: IC pins, and
                    // pins of nets that continue outside this group (or are the only pin here).
                    // First pass: every pin of a cross-block net reserves label room; later passes
                    // know which pin actually carries the label and reserve only there.
                    let cross =
                        *gl.all_nets.get(&net).unwrap_or(&0) > here.len() || here.len() == 1;
                    let labelled = label_pins.contains(&(p.id.clone(), number.clone()))
                        || (cross && (no_label_pins || here.len() == 1));
                    attach.insert(
                        number,
                        if labelled {
                            3.0 + 0.9 * net.chars().count() as f64
                        } else {
                            0.0
                        },
                    );
                } else {
                    attach.insert(number, 6.0); // power symbol stub
                }
            }
            gl.parts[pi].attach = attach;
        }
        gl
    }

    fn key_of(&self, e: &End) -> PinKey {
        (
            self.parts[e.p].id.clone(),
            self.parts[e.p].pins[e.pin].number.clone(),
        )
    }

    fn is_bus_pair(&self, a: usize, b: usize) -> bool {
        let (x, y) = (&self.parts[a].id, &self.parts[b].id);
        let key = if x <= y {
            (x.clone(), y.clone())
        } else {
            (y.clone(), x.clone())
        };
        self.bus_pairs.contains(&key)
    }

    fn join_targets(&self, parent: &mut [usize], wire_owner: &[usize], j: usize) -> HashSet<usize> {
        let tgt = find(parent, j);
        (0..wire_owner.len())
            .filter(|&wi| find(parent, wire_owner[wi]) == tgt)
            .collect()
    }

    // ------------------------------------------------------------ routing

    /// Wires the group's nets and places net labels where a wire would snake. Returns the nets that
    /// got a label.
    pub fn route(&mut self) -> Vec<String> {
        // reserve power stub zones so wires don't run through them
        let mut reserved: Vec<(Box4, Option<String>)> = Vec::new();
        for p in &self.parts {
            for pin in &p.pins {
                let Some(net) = p.pinmap.get(&pin.number) else {
                    continue;
                };
                if net.is_empty()
                    || !self.nets.contains_key(net)
                    || self.signal_nets.contains_key(net)
                {
                    continue;
                }
                let pos = p.pin_pos(pin);
                let d = p.pin_dir(pin);
                let (ex, ey) = (pos[0] + d.0 as f64 * 5.0, pos[1] + d.1 as f64 * 5.0);
                reserved.push((
                    [
                        pos[0].min(ex) - 1.5,
                        pos[1].min(ey) - 1.5,
                        pos[0].max(ex) + 1.5,
                        pos[1].max(ey) + 1.5,
                    ],
                    Some(net.clone()),
                ));
            }
        }
        self.reserved = reserved;
        let mut labelled_nets: Vec<String> = Vec::new();

        // pairs of multi-pin parts sharing many nets (MCU <-> header): humans use net labels, not
        // wire bundles
        let mut shared: HashMap<(String, String), usize> = HashMap::new();
        for (_, lst) in self.signal_nets.iter() {
            let mut big: Vec<String> = lst
                .iter()
                .filter(|(pi, _)| self.parts[*pi].pins.len() > 3)
                .map(|(pi, _)| self.parts[*pi].id.clone())
                .collect();
            big.sort();
            big.dedup();
            for i in 0..big.len() {
                for j in i + 1..big.len() {
                    *shared.entry((big[i].clone(), big[j].clone())).or_insert(0) += 1;
                }
            }
        }
        self.bus_pairs = shared
            .into_iter()
            .filter(|(_, v)| *v >= 4)
            .map(|(k, _)| k)
            .collect();
        self.power_symbol_pins.clear();
        self.symbol_on_wire.clear();

        let mut routable: Vec<RoutableNet> = self
            .signal_nets
            .iter()
            .map(|(n, v)| (n.clone(), v.clone(), false))
            .chain(
                self.supply_nets
                    .iter()
                    .map(|(n, v)| (n.clone(), v.clone(), true)),
            )
            .collect();
        // nets sorted: 2-pin nets first (short local connections), then bigger
        routable.sort_by(|a, b| a.1.len().cmp(&b.1.len()).then(a.0.cmp(&b.0)));

        for (net, lst, is_supply) in routable {
            let ends: Vec<End> = lst
                .iter()
                .map(|&(pi, qi)| {
                    let p = &self.parts[pi];
                    End {
                        p: pi,
                        pin: qi,
                        pos: p.pin_pos(&p.pins[qi]),
                        d: p.pin_dir(&p.pins[qi]),
                    }
                })
                .collect();
            let n = ends.len();
            if n < 2 {
                continue;
            }
            let mut parent: Vec<usize> = (0..n).collect();
            let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
            for i in 0..n {
                for j in i + 1..n {
                    let (a, b) = (ends[i].pos, ends[j].pos);
                    pairs.push(((a[0] - b[0]).abs() + (a[1] - b[1]).abs(), i, j));
                }
            }
            pairs.sort_by(|a, b| cmpf(a.0, b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
            let mut wire_owner: Vec<usize> = Vec::new();
            let limit = self.max_wire;
            if is_supply {
                // decoupling caps (power-only parts) are wired to a supply only along a horizontal
                // rail right next to them; otherwise each gets its own power symbol
                pairs.retain(|&(dist, i, j)| {
                    (dist <= 8.0 && (ends[i].pos[1] - ends[j].pos[1]).abs() < 0.01)
                        || !(self.parts[ends[i].p].power_only || self.parts[ends[j].p].power_only)
                });
            }
            if !self.bus_pairs.is_empty() {
                pairs.retain(|&(_, i, j)| !self.is_bus_pair(ends[i].p, ends[j].p));
            }
            let xs: Vec<f64> = ends.iter().map(|e| e.pos[0]).collect();
            let ys: Vec<f64> = ends.iter().map(|e| e.pos[1]).collect();
            let span = (fmax(&xs) - fmin(&xs)) + (fmax(&ys) - fmin(&ys));
            let mut any_bus = false;
            for (ai, a) in ends.iter().enumerate() {
                for (bi, b) in ends.iter().enumerate() {
                    if ai != bi {
                        any_bus |= self.is_bus_pair(a.p, b.p);
                    }
                }
            }
            if n >= 3 && !is_supply && span <= 44f64.max(self.max_wire + 4.0) && !any_bus {
                if let Some(trunk) = self.route_trunk(&ends) {
                    for path in trunk {
                        let wi = self.wires.len();
                        self.wires.push(path);
                        self.net_wires.entry_or(&net, Vec::new()).push(wi);
                        wire_owner.push(0);
                    }
                    for (k, end) in ends.iter().enumerate() {
                        let key = self.key_of(end);
                        self.wired_pins.insert(key);
                        let (a, b) = (find(&mut parent, k), find(&mut parent, 0));
                        parent[a] = b;
                    }
                }
            }
            for &(dist, i, j) in &pairs {
                if find(&mut parent, i) == find(&mut parent, j) {
                    continue;
                }
                // beyond the wire limit a connection is still drawn when its path is simple (straight
                // or one bend, no crossings) - humans draw long clean wires, labels only replace
                // convoluted ones
                if limit < dist && dist <= 1.8 * limit && !is_supply && self.long_simple {
                    let allowed = self.join_targets(&mut parent, &wire_owner, j);
                    if let Some((path, joined)) =
                        self.route_pair(&ends[i], &ends[j], &allowed, &net, true)
                    {
                        let wi = self.wires.len();
                        self.wires.push(path);
                        self.net_wires.entry_or(&net, Vec::new()).push(wi);
                        wire_owner.push(i);
                        let ki = self.key_of(&ends[i]);
                        self.wired_pins.insert(ki);
                        match joined {
                            None => {
                                let kj = self.key_of(&ends[j]);
                                self.wired_pins.insert(kj);
                                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                                parent[a] = b;
                            }
                            Some(w) => {
                                let (a, b) =
                                    (find(&mut parent, i), find(&mut parent, wire_owner[w]));
                                parent[a] = b;
                            }
                        }
                        continue;
                    }
                }
                if dist > limit {
                    continue;
                }
                if dist < 1e-6 {
                    // pin tips touch: already connected (the compiler infers the junction)
                    let (ki, kj) = (self.key_of(&ends[i]), self.key_of(&ends[j]));
                    self.wired_pins.insert(ki);
                    self.wired_pins.insert(kj);
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    parent[a] = b;
                    continue;
                }
                let (pi, pj) = (ends[i], ends[j]);
                if pi.p == pj.p
                    && pi.d == pj.d
                    && ((pi.pos[0] - pj.pos[0]).abs() < 1e-6
                        || (pi.pos[1] - pj.pos[1]).abs() < 1e-6)
                {
                    // two pins of the same part on the same side: bridge the tips directly (unless a
                    // pin lies between)
                    let (a_, b_) = (pi.pos, pj.pos);
                    let part = &self.parts[pi.p];
                    let mut between = false;
                    for (qi, q) in part.pins.iter().enumerate() {
                        if qi == pi.pin || qi == pj.pin {
                            continue;
                        }
                        let qp = part.pin_pos(q);
                        if (a_[0] - b_[0]).abs() < 1e-6
                            && (qp[0] - a_[0]).abs() < 1e-6
                            && a_[1].min(b_[1]) < qp[1]
                            && qp[1] < a_[1].max(b_[1])
                        {
                            between = true;
                        }
                        if (a_[1] - b_[1]).abs() < 1e-6
                            && (qp[1] - a_[1]).abs() < 1e-6
                            && a_[0].min(b_[0]) < qp[0]
                            && qp[0] < a_[0].max(b_[0])
                        {
                            between = true;
                        }
                    }
                    if !between {
                        let wi = self.wires.len();
                        self.wires.push(vec![a_, b_]);
                        self.net_wires.entry_or(&net, Vec::new()).push(wi);
                        wire_owner.push(i);
                        let (ki, kj) = (self.key_of(&pi), self.key_of(&pj));
                        self.wired_pins.insert(ki);
                        self.wired_pins.insert(kj);
                        let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                        parent[a] = b;
                        continue;
                    }
                }
                // only wires of the target's component are join targets (joining our own wire
                // connects nothing)
                let allowed = self.join_targets(&mut parent, &wire_owner, j);
                let Some((path, joined)) =
                    self.route_pair(&ends[i], &ends[j], &allowed, &net, false)
                else {
                    continue;
                };
                let wi = self.wires.len();
                self.wires.push(path);
                self.net_wires.entry_or(&net, Vec::new()).push(wi);
                wire_owner.push(i);
                let ki = self.key_of(&ends[i]);
                self.wired_pins.insert(ki);
                match joined {
                    None => {
                        let kj = self.key_of(&ends[j]);
                        self.wired_pins.insert(kj);
                        let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                        parent[a] = b;
                    }
                    Some(w) => {
                        let (a, b) = (find(&mut parent, i), find(&mut parent, wire_owner[w]));
                        parent[a] = b;
                    }
                }
            }
            // components, in the order their first member appears
            let mut comps: Vec<(usize, Vec<usize>)> = Vec::new();
            for i in 0..n {
                let r = find(&mut parent, i);
                match comps.iter_mut().find(|(k, _)| *k == r) {
                    Some((_, v)) => v.push(i),
                    None => comps.push((r, vec![i])),
                }
            }
            if is_supply {
                // every fully wired component needs one power symbol: on its longest horizontal wire
                // if possible
                for (_, comp) in &comps {
                    let unwired = comp
                        .iter()
                        .any(|&i| !self.wired_pins.contains(&self.key_of(&ends[i])));
                    if comp.len() == 1 || unwired {
                        continue; // unwired pins get their own symbols in emit()
                    }
                    let comp_pins: HashSet<PinKey> =
                        comp.iter().map(|&k| self.key_of(&ends[k])).collect();
                    let mut segs: Vec<(f64, Pt, Pt)> = Vec::new();
                    let widx: Vec<usize> = self.net_wires.get(&net).cloned().unwrap_or_default();
                    for (wi, &w) in widx.iter().enumerate() {
                        if !comp_pins.contains(&self.key_of(&ends[wire_owner[wi]])) {
                            continue;
                        }
                        for (c, d) in seg_pairs(&self.wires[w]) {
                            if (c[1] - d[1]).abs() < 1e-6 && (c[0] - d[0]).abs() >= 4.0 {
                                segs.push(((c[0] - d[0]).abs(), c, d));
                            }
                        }
                    }
                    if !segs.is_empty() {
                        let mut best = segs[0];
                        for s in &segs[1..] {
                            if gt_lex(&seg_key(s), &seg_key(&best)) {
                                best = *s;
                            }
                        }
                        let (_, c, d) = best;
                        let mx = snap_even((c[0] + d[0]) / 2.0);
                        self.symbol_on_wire.push((net.clone(), [mx, c[1]]));
                    } else {
                        let i = *comp
                            .iter()
                            .min_by_key(|&&k| (if ends[k].d == (0, -1) { 0 } else { 1 }, k))
                            .unwrap();
                        let key = self.key_of(&ends[i]);
                        self.power_symbol_pins.insert(key);
                    }
                }
                continue;
            }
            // the net continues in another block: it needs a label here
            let cross = *self.all_nets.get(&net).unwrap_or(&0) > n;
            if comps.len() > 1 || cross {
                labelled_nets.push(net.clone());
                // one label per component, on the pin with the freest surroundings
                for (_, comp) in &comps {
                    let keys: Vec<(bool, i64)> = comp
                        .iter()
                        .map(|&k| {
                            (
                                self.wired_pins.contains(&self.key_of(&ends[k])),
                                self.label_crowding(&ends[k], &net),
                            )
                        })
                        .collect();
                    let mut order: Vec<usize> = (0..comp.len()).collect();
                    order.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
                    let cand: Vec<usize> = order.into_iter().map(|i| comp[i]).collect();
                    let mut done = false;
                    for &i in &cand {
                        if self.label_pin(&ends[i], &net, false) {
                            done = true;
                            break;
                        }
                    }
                    if !done {
                        self.label_pin(&ends[cand[0]], &net, true);
                    }
                }
            }
        }
        labelled_nets
    }

    fn label_crowding(&self, e: &End, net: &str) -> i64 {
        let (pos, d) = (e.pos, e.d);
        let far = [pos[0] + d.0 as f64 * STUB, pos[1] + d.1 as f64 * STUB];
        let w = 0.95 * net.chars().count() as f64 + 1.2;
        let bx: Box4 = match d {
            (1, 0) => [far[0], far[1] - 1.6, far[0] + w, far[1] + 0.3],
            (-1, 0) => [far[0] - w, far[1] - 1.6, far[0], far[1] + 0.3],
            (0, -1) => [far[0] - 1.6, far[1] - w, far[0] + 0.3, far[1]],
            _ => [far[0] - 1.6, far[1], far[0] + 0.3, far[1] + w],
        };
        let mut n = 0i64;
        for (qi, q) in self.parts.iter().enumerate() {
            if qi != e.p && overlap(&bx, &q.extent(), 0.0) {
                n += 10;
            }
        }
        for wv in &self.wires {
            for (a, b) in seg_pairs(wv) {
                if seg_hits_box(a, b, &bx, 0.1) {
                    n += 3;
                }
            }
        }
        n
    }

    /// Dijkstra on a unit grid: cost = length + bends*5 + wire crossings*10; the path leaves/enters
    /// the pins along their outward directions and never enters a part body.
    ///
    /// Returns the path and, when it ended on an existing wire of the same net, that wire's index
    /// within `net_wires[routing_net]`.
    fn route_pair(
        &self,
        e1: &End,
        e2: &End,
        join_allowed: &HashSet<usize>,
        routing_net: &str,
        simple_only: bool,
    ) -> Option<(Path, Option<usize>)> {
        let (da, db) = (e1.d, e2.d);
        let a = (ri(e1.pos[0]), ri(e1.pos[1]));
        let b = (ri(e2.pos[0]), ri(e2.pos[1]));
        let x0 = a.0.min(b.0) - 12;
        let x1 = a.0.max(b.0) + 12;
        let y0 = a.1.min(b.1) - 12;
        let y1 = a.1.max(b.1) + 12;
        let mut blocked: HashSet<(i64, i64)> = HashSet::new();
        let cells = |bx: &Box4, out: &mut HashSet<(i64, i64)>| {
            let xa = (bx[0] + 0.3).ceil() as i64;
            let xb = (bx[2] - 0.3).floor() as i64;
            let ya = (bx[1] + 0.3).ceil() as i64;
            let yb = (bx[3] - 0.3).floor() as i64;
            for x in xa.max(x0)..=xb.min(x1) {
                for y in ya.max(y0)..=yb.min(y1) {
                    out.insert((x, y));
                }
            }
        };
        for q in &self.parts {
            for bx in q.boxes(false) {
                cells(&bx, &mut blocked);
            }
        }
        for (bx, net) in &self.reserved {
            if net.as_deref() == Some(routing_net) {
                continue;
            }
            cells(bx, &mut blocked);
        }
        // own pin stubs are free (4 cells outward from each tip)
        let mut free: HashSet<(i64, i64)> = HashSet::new();
        for (pt, d) in [(a, da), (b, db)] {
            for k in 0..4i64 {
                free.insert((pt.0 + d.0 as i64 * k, pt.1 + d.1 as i64 * k));
            }
        }
        // existing wires: cells with direction (other nets = obstacles/crossings; same net = joins)
        let same_net: Vec<usize> = self.net_wires.get(routing_net).cloned().unwrap_or_default();
        let same_idx: HashSet<usize> = same_net.iter().copied().collect();
        let mut join_cells: HashMap<(i64, i64), (bool, bool)> = HashMap::new();
        let mut join_owner: HashMap<(i64, i64), usize> = HashMap::new();
        for (wi, &w) in same_net.iter().enumerate() {
            if !join_allowed.contains(&wi) {
                continue;
            }
            for (c, d) in seg_pairs(&self.wires[w]) {
                if (c[0] - d[0]).abs() < 1e-6 {
                    let x = ri(c[0]);
                    for y in ri(c[1].min(d[1]))..=ri(c[1].max(d[1])) {
                        join_cells.entry((x, y)).or_default().0 = true;
                        join_owner.insert((x, y), wi);
                    }
                } else {
                    let y = ri(c[1]);
                    for x in ri(c[0].min(d[0]))..=ri(c[0].max(d[0])) {
                        join_cells.entry((x, y)).or_default().1 = true;
                        join_owner.insert((x, y), wi);
                    }
                }
            }
        }
        let mut wire_cells: HashMap<(i64, i64), (bool, bool)> = HashMap::new();
        for (wi, w) in self.wires.iter().enumerate() {
            if same_idx.contains(&wi) {
                continue; // same net: free to run along (shared segments merge), never an obstacle
            }
            for (c, d) in seg_pairs(w) {
                if (c[0] - d[0]).abs() < 1e-6 {
                    let x = ri(c[0]);
                    for y in ri(c[1].min(d[1]))..=ri(c[1].max(d[1])) {
                        wire_cells.entry((x, y)).or_default().0 = true;
                    }
                } else {
                    let y = ri(c[1]);
                    for x in ri(c[0].min(d[0]))..=ri(c[0].max(d[0])) {
                        wire_cells.entry((x, y)).or_default().1 = true;
                    }
                }
            }
        }
        const DIRS: [Dir; 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
        let goal_dir = (-db.0, -db.1); // we must arrive at b moving opposite to b's outward direction
        let mut heap: BinaryHeap<QItem> = BinaryHeap::new();
        heap.push(QItem {
            cost: 0.0,
            pos: a,
            d: da,
        });
        let mut best: HashMap<CellDir, f64> = HashMap::new();
        let mut prev: HashMap<CellDir, CellDir> = HashMap::new();
        let manhattan = (a.0 - b.0).abs() + (a.1 - b.1).abs();
        let limit = 60f64.max(2.5 * manhattan as f64 + 60.0);
        let mut end_key: Option<CellDir> = None;
        while let Some(QItem { cost, pos, d }) = heap.pop() {
            let key = (pos, d);
            if best.contains_key(&key) {
                continue;
            }
            best.insert(key, cost);
            if pos == b {
                end_key = Some(key);
                break;
            }
            if let Some(&(v, h)) = join_cells.get(&pos)
                && pos != a
            {
                // joining an existing wire of the same net: arrive perpendicular to it (T junction)
                let mine_v = d.0 == 0;
                if (mine_v && !v) || (!mine_v && !h) {
                    end_key = Some(key);
                    break;
                }
            }
            if cost > limit {
                continue;
            }
            for nd in DIRS {
                if nd == (-d.0, -d.1) {
                    continue;
                }
                if nd != da && pos == (a.0 + da.0 as i64, a.1 + da.1 as i64) {
                    continue; // a wire may meet the pin tip at a right angle (T), but no 1-unit stub
                }
                let nxt = (pos.0 + nd.0 as i64, pos.1 + nd.1 as i64);
                if nd != goal_dir && nxt == (b.0 - goal_dir.0 as i64, b.1 - goal_dir.1 as i64) {
                    continue; // same at the target: straight in, or a T at the tip - never a stub
                }
                if !(x0 <= nxt.0 && nxt.0 <= x1 && y0 <= nxt.1 && nxt.1 <= y1) {
                    continue;
                }
                if blocked.contains(&nxt) && !free.contains(&nxt) {
                    continue;
                }
                let mut step = 1.0 + if nd != d { 6.0 } else { 0.0 };
                if nd != d
                    && ((pos.0 - a.0).abs() + (pos.1 - a.1).abs() < 4
                        || (pos.0 - b.0).abs() + (pos.1 - b.1).abs() < 4)
                {
                    step += 4.0;
                }
                if !free.contains(&nxt)
                    && [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)]
                        .iter()
                        .any(|(ox, oy)| blocked.contains(&(nxt.0 + ox, nxt.1 + oy)))
                {
                    step += 2.0; // hugging a part body: keep clearance
                }
                if let Some(&(v, h)) = wire_cells.get(&nxt) {
                    let same = if nd.0 == 0 { v } else { h };
                    if same && nxt != b {
                        continue; // running along an existing wire
                    }
                    step += 20.0;
                }
                let nk = (nxt, nd);
                if best.contains_key(&nk) {
                    continue;
                }
                prev.insert(nk, key);
                heap.push(QItem {
                    cost: cost + step,
                    pos: nxt,
                    d: nd,
                });
            }
        }
        let end_key = end_key?;
        let joined = if end_key.0 != b {
            join_owner.get(&end_key.0).copied()
        } else {
            None
        };
        if simple_only && end_key.0 != b {
            return None; // no joins for long wires
        }
        // reconstruct and compress collinear points
        let mut pts: Vec<Pt> = Vec::new();
        let mut k = end_key;
        loop {
            if !(prev.contains_key(&k) || k == (a, da)) {
                break;
            }
            pts.push([k.0.0 as f64, k.0.1 as f64]);
            if k == (a, da) {
                break;
            }
            k = prev[&k];
        }
        pts.reverse();
        if pts.is_empty() {
            return None;
        }
        let mut out: Vec<Pt> = vec![pts[0]];
        for i in 1..pts.len().saturating_sub(1) {
            let (u, v, w) = (pts[i - 1], pts[i], pts[i + 1]);
            if ((u[0] - v[0]).abs() < 1e-6 && (v[0] - w[0]).abs() < 1e-6)
                || ((u[1] - v[1]).abs() < 1e-6 && (v[1] - w[1]).abs() < 1e-6)
            {
                continue;
            }
            out.push(v);
        }
        out.push(pts[pts.len() - 1]);
        // accept by shape, not by raw search cost: detour + bends + crossings (hugging is fine)
        let length: f64 = seg_pairs(&out)
            .map(|(u, v)| (u[0] - v[0]).abs() + (u[1] - v[1]).abs())
            .sum();
        let bends = out.len().saturating_sub(2) as f64;
        let mut crossings = 0.0;
        for (u, v) in seg_pairs(&out) {
            if (u[1] - v[1]).abs() < 1e-6 {
                let y = ri(u[1]);
                for x in ri(u[0].min(v[0])) + 1..ri(u[0].max(v[0])) {
                    if wire_cells.get(&(x, y)).is_some_and(|c| c.0) {
                        crossings += 1.0;
                    }
                }
            } else {
                let x = ri(u[0]);
                for y in ri(u[1].min(v[1])) + 1..ri(u[1].max(v[1])) {
                    if wire_cells.get(&(x, y)).is_some_and(|c| c.1) {
                        crossings += 1.0;
                    }
                }
            }
        }
        let last = pts[pts.len() - 1];
        // manhattan to where we actually ended (a join may stop short of b)
        let target = (last[0] - a.0 as f64).abs() + (last[1] - a.1 as f64).abs();
        let shape = (length - target) + 6.0 * bends + 20.0 * crossings;
        let limit_shape = if simple_only { 8.5 } else { 30.5 };
        if shape > limit_shape {
            return None; // a net label is cleaner than a snake
        }
        Some((out, joined))
    }

    /// Straight node wire (horizontal or vertical) with every pin dropping onto it by a straight or
    /// L path. Returns the wire paths, or `None` when no trunk works.
    fn route_trunk(&self, ends: &[End]) -> Option<Vec<Path>> {
        let mut best: Option<Vec<Path>> = None;
        let mut best_s = f64::INFINITY;
        // per box: a wide value text must not wall off pins
        let obstacles: Vec<(usize, Box4)> = self
            .parts
            .iter()
            .enumerate()
            .flat_map(|(qi, q)| q.boxes(false).into_iter().map(move |b| (qi, b)))
            .collect();
        let others: Vec<(Pt, Pt)> = self
            .wires
            .iter()
            .flat_map(|w| seg_pairs(w).collect::<Vec<_>>())
            .collect();

        // `None` = blocked, `Some(n)` = crosses n existing segments
        let clear = |u: Pt, v: Pt, own: Option<usize>| -> Option<f64> {
            for (qi, ob) in &obstacles {
                if seg_hits_box(u, v, ob, 0.3) {
                    // leaving own part outward along the pin direction is fine
                    if own == Some(*qi) {
                        continue;
                    }
                    return None;
                }
            }
            Some(
                others
                    .iter()
                    .filter(|(c, d)| segs_cross(u, v, *c, *d))
                    .count() as f64,
            )
        };

        for horizontal in [true, false] {
            let mut coords: Vec<i64> = ends
                .iter()
                .map(|e| ri(if horizontal { e.pos[1] } else { e.pos[0] }))
                .collect();
            coords.sort_unstable();
            coords.dedup();
            let mut cand_raw: Vec<i64> = coords;
            for e in ends {
                let (pos, d) = (e.pos, e.d);
                for k in [2.0, 4.0, 6.0, 8.0] {
                    cand_raw.push(ri(if horizontal {
                        pos[1] + d.1 as f64 * k
                    } else {
                        pos[0] + d.0 as f64 * k
                    }));
                }
                // pins pointing along the trunk direction: lines beside them (a bus past a resistor)
                if (horizontal && d.1 == 0) || (!horizontal && d.0 == 0) {
                    for k in [-6.0, -4.0, 4.0, 6.0] {
                        cand_raw.push(ri(if horizontal { pos[1] + k } else { pos[0] + k }));
                    }
                }
            }
            for t in py_int_set_order(&uniq(&cand_raw)) {
                let t = t as f64;
                let mut paths: Vec<Path> = Vec::new();
                let mut total = 0.0;
                let mut ok = true;
                let mut ends_on_trunk: Vec<f64> = Vec::new();
                for e in ends {
                    let own = Some(e.p);
                    let (pos, d) = (e.pos, e.d);
                    let (px, py) = (ri(pos[0]) as f64, ri(pos[1]) as f64);
                    if horizontal {
                        if d.1 != 0 {
                            if py == t {
                                ends_on_trunk.push(px);
                                continue; // tip on the line: T junction
                            }
                            if (t - py) * (d.1 as f64) < 2.0 {
                                ok = false;
                                break;
                            }
                            let (s0, s1) = ([px, py], [px, t]);
                            let Some(c) = clear(s0, s1, own) else {
                                ok = false;
                                break;
                            };
                            paths.push(vec![s0, s1]);
                            total += (t - py).abs() + 20.0 * c;
                            ends_on_trunk.push(px);
                        } else {
                            if py == t {
                                ends_on_trunk.push(px); // lies on the trunk line already
                                continue;
                            }
                            // go outward 2, then vertical to the trunk
                            let mx = px + d.0 as f64 * 2.0;
                            let (a0, a1, a2) = ([px, py], [mx, py], [mx, t]);
                            let (Some(c1), Some(c2)) = (clear(a0, a1, own), clear(a1, a2, None))
                            else {
                                ok = false;
                                break;
                            };
                            paths.push(vec![a0, a1, a2]);
                            total += 2.0 + (t - py).abs() + 6.0 + 20.0 * (c1 + c2);
                            ends_on_trunk.push(mx);
                        }
                    } else if d.0 != 0 {
                        if px == t {
                            ends_on_trunk.push(py);
                            continue;
                        }
                        if (t - px) * (d.0 as f64) < 2.0 {
                            ok = false;
                            break;
                        }
                        let (s0, s1) = ([px, py], [t, py]);
                        let Some(c) = clear(s0, s1, own) else {
                            ok = false;
                            break;
                        };
                        paths.push(vec![s0, s1]);
                        total += (t - px).abs() + 20.0 * c;
                        ends_on_trunk.push(py);
                    } else {
                        if px == t {
                            ends_on_trunk.push(py);
                            continue;
                        }
                        let my = py + d.1 as f64 * 2.0;
                        let (a0, a1, a2) = ([px, py], [px, my], [t, my]);
                        let (Some(c1), Some(c2)) = (clear(a0, a1, own), clear(a1, a2, None)) else {
                            ok = false;
                            break;
                        };
                        paths.push(vec![a0, a1, a2]);
                        total += 2.0 + (t - px).abs() + 6.0 + 20.0 * (c1 + c2);
                        ends_on_trunk.push(my);
                    }
                }
                if !ok || ends_on_trunk.len() < 2 {
                    continue;
                }
                let (lo, hi) = (fmin(&ends_on_trunk), fmax(&ends_on_trunk));
                if hi - lo > 0.0 {
                    let (t0, t1) = if horizontal {
                        ([lo, t], [hi, t])
                    } else {
                        ([t, lo], [t, hi])
                    };
                    let Some(c) = clear(t0, t1, None) else {
                        continue;
                    };
                    paths.push(vec![t0, t1]);
                    total += (hi - lo) + 20.0 * c;
                }
                // pins lying on the trunk line pointing along it are connected by the trunk itself
                let score = total + 3.0 * paths.len() as f64;
                if best.is_none() || score < best_s {
                    best_s = score;
                    best = Some(paths);
                }
            }
        }
        best
    }

    /// Wire segments of `net` connected (through shared points / T junctions) to `pos`.
    fn component_segments(&self, net: &str, pos: Pt) -> Vec<(Pt, Pt)> {
        let segs: Vec<(Pt, Pt)> = self
            .net_wires
            .get(net)
            .map(|v| {
                v.iter()
                    .flat_map(|&w| seg_pairs(&self.wires[w]).collect::<Vec<_>>())
                    .collect()
            })
            .unwrap_or_default();
        fn on(pt: Pt, a: Pt, b: Pt) -> bool {
            ((b[0] - a[0]) * (pt[1] - a[1]) - (b[1] - a[1]) * (pt[0] - a[0])).abs() < 1e-6
                && a[0].min(b[0]) - 1e-6 <= pt[0]
                && pt[0] <= a[0].max(b[0]) + 1e-6
                && a[1].min(b[1]) - 1e-6 <= pt[1]
                && pt[1] <= a[1].max(b[1]) + 1e-6
        }
        let mut got: Vec<(Pt, Pt)> = Vec::new();
        let mut frontier: Vec<Pt> = vec![pos];
        let mut left = segs;
        while let Some(pt) = frontier.pop() {
            let mut keep = Vec::new();
            for (a, b) in left {
                if on(pt, a, b) {
                    got.push((a, b));
                    frontier.push(a);
                    frontier.push(b);
                } else {
                    keep.push((a, b));
                }
            }
            left = keep;
            // segments whose endpoint lies on an already collected segment
            let mut keep = Vec::new();
            for (a, b) in left {
                if got.iter().any(|&(c, e)| on(a, c, e) || on(b, c, e)) {
                    got.push((a, b));
                    frontier.push(a);
                    frontier.push(b);
                } else {
                    keep.push((a, b));
                }
            }
            left = keep;
        }
        got
    }

    /// Attach a net label for `net` at pin `e`. Returns true when a collision-free spot was found;
    /// with `force = false` nothing is placed otherwise (the caller then tries another pin of the
    /// same component).
    fn label_pin(&mut self, e: &End, net: &str, force: bool) -> bool {
        let (pos, d) = (e.pos, e.d);
        let mut perp: Vec<Dir> = if d.0 == 0 {
            vec![(d.1, d.0), (-d.1, -d.0)]
        } else {
            vec![(0, 1), (0, -1)]
        };
        // bent stubs: try the side facing the rest of the net first (the label then points where
        // the signal goes)
        let others: Vec<Pt> = self
            .nets
            .get(net)
            .map(|v| {
                v.iter()
                    .filter(|&&(qi, qp)| !(qi == e.p && qp == e.pin))
                    .map(|&(qi, qp)| {
                        let q = &self.parts[qi];
                        q.pin_pos(&q.pins[qp])
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !others.is_empty() {
            let cx = others.iter().map(|o| o[0]).sum::<f64>() / others.len() as f64;
            let cy = others.iter().map(|o| o[1]).sum::<f64>() / others.len() as f64;
            perp.sort_by(|a, b| {
                let ka = -((cx - pos[0]) * a.0 as f64 + (cy - pos[1]) * a.1 as f64);
                let kb = -((cx - pos[0]) * b.0 as f64 + (cy - pos[1]) * b.1 as f64);
                cmpf(ka, kb)
            });
        }
        // straight stubs first, then L-shaped stubs (outward 2, then sideways) as a last resort
        let mut straight: Vec<(Path, Pt, i32)> = Vec::new();
        for stub in [4.0, 6.0, 8.0, 10.0, 12.0, 14.0] {
            let far = [pos[0] + d.0 as f64 * stub, pos[1] + d.1 as f64 * stub];
            straight.push((vec![pos, far], far, rot_label(d)));
        }
        let mut bent: Vec<(Path, Pt, i32)> = Vec::new();
        for side in &perp {
            for k in [4.0, 6.0, 8.0, 10.0, 12.0] {
                let mid = [pos[0] + d.0 as f64 * 2.0, pos[1] + d.1 as f64 * 2.0];
                let far = [mid[0] + side.0 as f64 * k, mid[1] + side.1 as f64 * k];
                bent.push((vec![pos, mid, far], far, rot_label(*side)));
            }
        }
        // vertical pins: a short L-stub with horizontal text reads better than a vertical label
        let mut cands: Vec<(Path, Pt, i32)> = if d.1 != 0 {
            let mut v = bent[..2.min(bent.len())].to_vec();
            v.extend(straight.clone());
            v.extend(bent[2.min(bent.len())..].to_vec());
            v
        } else {
            let mut v = straight.clone();
            v.extend(bent.clone());
            v
        };
        if self.wired_pins.contains(&self.key_of(e)) {
            // an already wired pin: the label reads best sitting on one of its wires (horizontal
            // run, text above)
            let mut onwire: Vec<(Path, Pt, i32)> = Vec::new();
            for (a, b) in self.component_segments(net, pos) {
                if (a[1] - b[1]).abs() < 1e-6 && (a[0] - b[0]).abs() >= 4.0 {
                    let mx = snap_even((a[0] + b[0]) / 2.0);
                    onwire.push((Vec::new(), [mx, a[1]], 0));
                }
            }
            onwire.extend(cands);
            cands = onwire;
        }
        let mut chosen: Option<(Path, Pt, i32)> = None;
        for (path, far, rot) in &cands {
            let bx = self.geo.label_box(net, *far, *rot, "local");
            if self.label_spot_free(e.p, pos, &bx, path) {
                chosen = Some((path.clone(), *far, *rot));
                break;
            }
        }
        let chosen = match chosen {
            Some(c) => c,
            None if force => cands[0].clone(),
            None => return false,
        };
        let (path, far, rot) = chosen;
        let key = self.key_of(e);
        self.labelled_pins.insert(key.clone());
        if path.len() >= 2 {
            self.wires.push(path);
        }
        self.wired_pins.insert(key);
        self.geo.add_label(net, far, rot, "local", "input");
        self.reserved.push((*self.geo.boxes.last().unwrap(), None));
        true
    }

    fn label_spot_free(&self, p_idx: usize, pos: Pt, bx: &Box4, path: &[Pt]) -> bool {
        for (qi, q) in self.parts.iter().enumerate() {
            if qi != p_idx && overlap(bx, &q.extent(), 0.0) {
                return false;
            }
        }
        if overlap(
            bx,
            &self.parts[p_idx].extent(),
            if path.is_empty() { 0.8 } else { -0.5 },
        ) {
            return false;
        }
        if self.geo.boxes.iter().any(|b| overlap(bx, b, -0.2)) {
            return false; // labels placed earlier
        }
        for w in &self.wires {
            for (a, b) in seg_pairs(w) {
                if a == pos || b == pos {
                    continue;
                }
                if seg_hits_box(a, b, bx, 0.1) {
                    return false;
                }
                for (u, v) in seg_pairs(path) {
                    if segs_cross(u, v, a, b) {
                        return false;
                    }
                }
            }
        }
        true
    }

    // ------------------------------------------------------------ emit

    /// Parts, wires, labels, power symbols and PWR_FLAGs of the block, in local coordinates.
    pub fn emit(&mut self) -> Res<Geo> {
        flag_at_symbol_clear();
        for net in &self.flags {
            if is_gnd(net) || self.net_wires.get(net).is_none_or(|v| v.is_empty()) {
                flag_at_symbol_set(net, false);
            }
        }
        for p in &self.parts {
            let opts = PartOpts {
                rot: p.rot,
                value: p.value.clone(),
                footprint: p.footprint.clone(),
                fields: p.fields.clone(),
                mirror: p.mirror.clone(),
                unit: p.unit,
                dnp: false,
            };
            self.geo.add_part(&p.id, &p.lib, p.at, &opts)?;
            // include the reference/value text room in the block bbox
            *self.geo.boxes.last_mut().unwrap() = p.extent_no_attach();
        }
        let wires = self.wires.clone();
        for w in &wires {
            self.geo.add_wire(w);
        }
        // connectors first: a PWR_FLAG placed beside a symbol lands on the input connector, where
        // humans put it
        let mut order: Vec<usize> = (0..self.parts.len()).collect();
        order.sort_by_key(|&i| if self.parts[i].is_connector { 0 } else { 1 });
        for pi in order {
            let mut pinmap: Map<String, Value> = Map::new();
            let mut skip: BTreeSet<String> = BTreeSet::new();
            for qi in 0..self.parts[pi].pins.len() {
                let p = &self.parts[pi];
                let pin = &p.pins[qi];
                let number = pin.number.clone();
                let net = p
                    .pinmap
                    .get(&number)
                    .cloned()
                    .unwrap_or_else(|| "nc".into());
                let key = (p.id.clone(), number.clone());
                if self.power_symbol_pins.contains(&key) {
                    // wired supply pin that carries the net's power symbol: put the symbol on a
                    // branch of the stub
                    let pos = p.pin_pos(pin);
                    let d = p.pin_dir(pin);
                    let mid = [pos[0] + d.0 as f64 * 2.0, pos[1] + d.1 as f64 * 2.0];
                    let covered = self
                        .wires
                        .iter()
                        .any(|w| seg_pairs(w).any(|(a, b)| covers(a, b, pos, mid)));
                    if !covered {
                        self.geo.add_wire(&[pos, mid]);
                    }
                    if d.1 == 0 {
                        let far = [mid[0], mid[1] - 2.0];
                        self.geo.add_wire(&[mid, far]);
                        self.geo.add_power(&net, far, 0);
                    } else {
                        let far = [mid[0], mid[1] + d.1 as f64 * 2.0];
                        self.geo.add_wire(&[mid, far]);
                        self.geo
                            .add_power(&net, far, if d == (0, -1) { 0 } else { 180 });
                    }
                    skip.insert(number);
                    continue;
                }
                let spec = if self.labelled_pins.contains(&key) {
                    json!("labelled") // has a label: crowds neighbouring power symbols
                } else if self.wired_pins.contains(&key) {
                    json!("wired")
                } else if net == "nc" || net.is_empty() {
                    json!("nc")
                } else if net == "float" {
                    json!("float")
                } else if self.nets.contains_key(&net) && !self.signal_nets.contains_key(&net) {
                    json!({ "power": net })
                } else {
                    json!({ "net": net })
                };
                pinmap.insert(number, spec);
            }
            let p = &self.parts[pi];
            geo::connect_pins(
                &mut self.geo,
                &p.info,
                p.at,
                p.rot,
                &p.mirror,
                p.unit,
                &pinmap,
                &json!("nc"),
                &skip,
            )?;
        }
        for (net, pt) in self.symbol_on_wire.clone() {
            self.geo.add_wire(&[pt, [pt[0], pt[1] - 3.0]]);
            self.geo.add_power(&net, [pt[0], pt[1] - 3.0], 0);
        }
        // PWR_FLAGs: on the longest horizontal wire of the net if it has one, else beside a pin's
        // power symbol
        let all_pins: Vec<Pt> = self
            .parts
            .iter()
            .flat_map(|q| q.pins.iter().map(|pin| q.pin_pos(pin)).collect::<Vec<_>>())
            .collect();
        for net in self.flags.clone() {
            let mut placed = flag_at_symbol_get(&net) == Some(true);
            if placed {
                continue;
            }
            let mut segs: Vec<(f64, Pt, Pt)> = Vec::new();
            for &w in self
                .net_wires
                .get(&net)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
            {
                for (c, d) in seg_pairs(&self.wires[w]) {
                    if (c[1] - d[1]).abs() < 1e-6 && (c[0] - d[0]).abs() >= 4.0 {
                        segs.push(((c[0] - d[0]).abs(), c, d));
                    }
                }
            }
            let used: Vec<Pt> = self.symbol_on_wire.iter().map(|(_, pt)| *pt).collect();
            let bodies: Vec<Box4> = self
                .parts
                .iter()
                .map(|q| extent(&q.info, q.at, q.rot, &q.mirror, q.unit))
                .collect();
            let mut best_c: Option<(f64, f64, f64)> = None;
            for &(_, c, d) in &segs {
                let (lo, hi) = (c[0].min(d[0]), c[0].max(d[0]));
                let li = lo.trunc() as i64;
                let mut x = li + li.rem_euclid(2);
                while (x as f64) < hi {
                    let cand = x as f64;
                    x += 2;
                    let clear = used
                        .iter()
                        .all(|u| (cand - u[0]).abs() >= 7.0 || (c[1] - u[1]).abs() > 0.5);
                    if !(lo < cand && cand < hi && clear) {
                        continue;
                    }
                    let tbox: Box4 = [cand - 4.5, c[1] - 7.0, cand + 4.5, c[1] - 0.5];
                    if self.wires.iter().any(|w| {
                        seg_pairs(w).any(|(e, f)| {
                            !((e == c && f == d) || (e == d && f == c))
                                && seg_hits_box(e, f, &tbox, 0.1)
                        })
                    }) {
                        continue;
                    }
                    if self.parts.iter().any(|q| overlap(&tbox, &q.extent(), 0.0))
                        || self.geo.boxes.iter().any(|b| overlap(&tbox, b, 0.0))
                    {
                        continue;
                    }
                    // prefer the spot farthest from any part body (clear, uncluttered)
                    let dist = bodies
                        .iter()
                        .map(|bx| {
                            (bx[0] - cand)
                                .max(cand - bx[2])
                                .max(bx[1] - c[1])
                                .max(c[1] - bx[3])
                        })
                        .fold(f64::INFINITY, f64::min);
                    let dist = if bodies.is_empty() { 99.0 } else { dist };
                    if best_c.is_none() || dist > best_c.unwrap().0 {
                        best_c = Some((dist, cand, c[1]));
                    }
                }
            }
            if let Some((_, cand, y)) = best_c {
                self.geo.add_wire(&[[cand, y], [cand, y - 3.0]]);
                self.geo.add_power("PWR_FLAG", [cand, y - 3.0], 0);
                placed = true;
            }
            if placed {
                continue;
            }
            // no wire: branch sideways from a pin's power stub where no other pin is nearby
            // (connectors first)
            let mut members: Vec<(usize, usize)> = self.nets.get(&net).cloned().unwrap_or_default();
            members.sort_by_key(|&(qi, _)| {
                (
                    if self.parts[qi].info.ref_prefix == "J" {
                        0
                    } else {
                        1
                    },
                    self.parts[qi].pins.len(),
                )
            });
            for (qi, qp) in members {
                let q = &self.parts[qi];
                let pin = &q.pins[qp];
                let pos = q.pin_pos(pin);
                let d = q.pin_dir(pin);
                let mid = [pos[0] + d.0 as f64 * 2.0, pos[1] + d.1 as f64 * 2.0];
                let sides: [Dir; 2] = if d.0 == 0 {
                    [(-1, 0), (1, 0)]
                } else {
                    [(0, -1), (0, 1)]
                };
                for side in sides {
                    let far = [mid[0] + side.0 as f64 * 10.0, mid[1] + side.1 as f64 * 10.0];
                    let bx: Box4 = [
                        mid[0].min(far[0]) - 3.0,
                        mid[1].min(far[1]) - 3.0,
                        mid[0].max(far[0]) + 3.0,
                        mid[1].max(far[1]) + 3.0,
                    ];
                    if all_pins.iter().any(|pp| {
                        bx[0] <= pp[0]
                            && pp[0] <= bx[2]
                            && bx[1] <= pp[1]
                            && pp[1] <= bx[3]
                            && *pp != pos
                    }) {
                        continue;
                    }
                    if self.wires.iter().any(|w| {
                        seg_pairs(w).any(|(c, d2)| {
                            !(c == pos || d2 == pos || c == mid || d2 == mid)
                                && seg_hits_box(c, d2, &bx, 0.1)
                        })
                    }) {
                        continue;
                    }
                    let fbox: Box4 = [far[0] - 4.5, far[1] - 4.0, far[0] + 4.5, far[1] + 1.0];
                    if self
                        .parts
                        .iter()
                        .enumerate()
                        .any(|(oi, qq)| oi != qi && overlap(&fbox, &qq.extent(), -0.5))
                    {
                        continue;
                    }
                    if overlap(&fbox, &self.parts[qi].extent_no_attach(), -0.5) {
                        continue;
                    }
                    if self.geo.boxes.iter().any(|b| {
                        !(b[0] <= pos[0] && pos[0] <= b[2] && b[1] <= pos[1] && pos[1] <= b[3])
                            && !(b[0] <= mid[0]
                                && mid[0] <= b[2]
                                && b[1] <= mid[1]
                                && mid[1] <= b[3])
                            && overlap(&fbox, b, -0.3)
                    }) {
                        continue;
                    }
                    let drawn: Vec<(Pt, Pt)> = self
                        .wires
                        .iter()
                        .flat_map(|w| seg_pairs(w).collect::<Vec<_>>())
                        .chain(
                            self.geo
                                .wires
                                .iter()
                                .filter(|w| w.len() >= 2)
                                .map(|w| (w[0], w[1])),
                        )
                        .collect();
                    if !drawn.iter().any(|&(c, d2)| covers(c, d2, pos, mid)) {
                        // the pin's wire may leave sideways at the tip: draw the stub
                        self.geo.add_wire(&[pos, mid]);
                    }
                    self.geo.add_wire(&[mid, far]);
                    self.geo.add_power("PWR_FLAG", far, 0);
                    placed = true;
                    break;
                }
                if placed {
                    break;
                }
            }
        }
        Ok(std::mem::take(&mut self.geo))
    }
}

/// Python's tuple comparison key for `(length, c, d)` segment triples.
fn seg_key(s: &(f64, Pt, Pt)) -> [f64; 5] {
    [s.0, s.1[0], s.1[1], s.2[0], s.2[1]]
}

fn gt_lex(a: &[f64; 5], b: &[f64; 5]) -> bool {
    for i in 0..5 {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    false
}

// ---------------------------------------------------------------------------- design helpers

/// Power symbols listed as parts are converted: PWR_FLAG -> `flags` entry, GND/VCC symbols ->
/// dropped (their nets are drawn as power symbols anyway). Mutates `d`; returns notes.
pub fn strip_power_parts(d: &mut Value) -> Vec<String> {
    let mut notes: Vec<String> = Vec::new();
    let parts: Vec<Value> = d
        .get("parts")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut keep: Vec<Value> = Vec::new();
    for pj in parts {
        let lib = pj.get("lib").map(json_str).unwrap_or_default();
        let info = index().get(&lib);
        let is_power = info.as_ref().map(|i| i.power).unwrap_or(false) || lib.starts_with("power:");
        if !is_power {
            keep.push(pj);
            continue;
        }
        let id = pj.get("id").map(json_str).unwrap_or_default();
        let nets: Vec<String> = pj
            .get("pins")
            .and_then(|p| p.as_object())
            .map(|m| {
                m.values()
                    .filter(|v| !v.is_null() && json_str(v) != "nc" && json_str(v) != "float")
                    .map(json_str)
                    .collect()
            })
            .unwrap_or_default();
        if lib.contains("PWR_FLAG") && !nets.is_empty() {
            let flags = d
                .as_object_mut()
                .unwrap()
                .entry("flags")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .unwrap();
            for n in &nets {
                if !flags.iter().any(|f| json_str(f) == *n) {
                    flags.push(json!(n));
                }
            }
            notes.push(format!(
                "note: {id} (PWR_FLAG) is not a part - moved to flags: [{}]",
                nets.iter()
                    .map(|n| format!("'{n}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else {
            notes.push(format!(
                "note: {id} ({lib}) is a power symbol, not a part - dropped (nets get power symbols automatically)"
            ));
        }
        // remove it from the layout trees too
        if let Some(layout) = d.get_mut("layout").and_then(|l| l.as_array_mut()) {
            for blk in layout.iter_mut() {
                if let Some(tree) = blk.get_mut("tree") {
                    prune_tree(tree, &id);
                }
            }
        }
    }
    d.as_object_mut()
        .unwrap()
        .insert("parts".into(), Value::Array(keep));
    notes
}

fn prune_tree(node: &mut Value, id: &str) {
    for k in ["row", "col"] {
        let Some(children) = node.get_mut(k).and_then(|c| c.as_array_mut()) else {
            continue;
        };
        children.retain(|c| c.get("part").map(json_str).unwrap_or_default() != id);
        for c in children.iter_mut() {
            prune_tree(c, id);
        }
    }
}

/// Edit mode: lay out a netlist circuit and place its groups in the free space of an existing raw
/// design.
pub fn add_circuit_to_raw(raw: &Value, circuit: &Value, paper: &str) -> (Value, Vec<String>) {
    let mut errors: Vec<String> = Vec::new();
    let mut d = circuit.clone();
    if d.get("paper").is_none() {
        d.as_object_mut()
            .unwrap()
            .insert("paper".into(), json!(paper));
    }
    if d.get("layout")
        .and_then(|l| l.as_array())
        .is_none_or(|l| l.is_empty())
    {
        return (
            raw.clone(),
            vec!["the added circuit needs a \"layout\" (blocks with row/col trees) like a new design".into()],
        );
    }
    let (geos, errs) = crate::flexlayout::circuit_geos(&d);
    if errs.iter().any(|e| !e.starts_with("note:")) {
        return (raw.clone(), errs);
    }
    errors.extend(errs.into_iter().filter(|e| e.starts_with("note:")));
    let obstacles = geo::raw_boxes(raw);
    let base = geo::paper(paper).unwrap_or_else(|| geo::paper("A4").unwrap());
    let mut tries: Vec<String> = vec![paper.to_string()];
    for (name, dims) in PAPERS {
        if name != paper && ["A4", "A3", "A2"].contains(&name) && dims[0] >= base[0] {
            tries.push(name.to_string());
        }
    }
    let mut sheet: Option<Geo> = None;
    let mut paper_used = paper.to_string();
    for paper_try in tries {
        let Some(p) = geo::paper(&paper_try) else {
            continue;
        };
        sheet = pack_blocks(
            &geos,
            14.0,
            14.0,
            p[0],
            p[1],
            (p[2], p[3]),
            &obstacles,
            true,
        );
        if sheet.is_some() {
            paper_used = paper_try;
            break;
        }
    }
    let Some(sheet) = sheet else {
        return (
            raw.clone(),
            vec![
                "no free space for the new circuit even after enlarging the sheet; remove more of the old circuit \
                 first"
                    .into(),
            ],
        );
    };
    if paper_used != paper {
        errors.push(format!(
            "note: no free space on {paper}; paper enlarged to {paper_used} (existing content kept in place)"
        ));
    }
    let mut out = raw.clone();
    let obj = out.as_object_mut().unwrap();
    obj.insert("paper".into(), json!(paper_used));
    let add = sheet.to_json();
    for k in ["parts", "power", "wires", "labels", "nc", "texts", "rects"] {
        let mut v: Vec<Value> = obj
            .get(k)
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        v.extend(
            add.get(k)
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default(),
        );
        obj.insert(k.into(), Value::Array(v));
    }
    (out, errors)
}
