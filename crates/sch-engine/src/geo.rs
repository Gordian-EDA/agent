//! Structured design -> raw design geometry.
//!
//! Cells generate tidy geometry in local grid units, cells are flowed into their block and blocks
//! are packed onto the sheet. Output is the raw design JSON understood by `check::build`
//! (parts/power/wires/labels/nc/texts/rects in absolute grid units).

use crate::model::{GRID, rot_point};
use crate::symlib::{SymbolInfo, index};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// Label stub length.
pub const STUB: f64 = 3.0;
/// Power stub length.
pub const PSTUB: f64 = 2.0;
pub const BLOCK_GAP_X: f64 = 8.0;
pub const BLOCK_GAP_Y: f64 = 8.0;

/// paper -> (xmax, ymax, titleblock x0, titleblock y0) in grid units.
pub const PAPERS: [(&str, [f64; 4]); 4] = [
    ("A5", [157.0, 96.0, 70.0, 80.0]),
    ("A4", [226.0, 145.0, 140.0, 126.0]),
    ("A3", [322.0, 214.0, 232.0, 194.0]),
    ("A2", [459.0, 311.0, 370.0, 292.0]),
];

pub fn paper(name: &str) -> Option<[f64; 4]> {
    PAPERS.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

/// Two-pin parts drawn vertically in the library: the model's `rot` follows the R/C convention.
pub const TWO_PIN_HORIZONTAL_ROT: [(&str, i32); 12] = [
    ("Device:R", 90),
    ("Device:C", 90),
    ("Device:C_Polarized", 90),
    ("Device:L", 90),
    ("Device:Fuse", 90),
    ("Device:Ferrite_Bead", 90),
    ("Device:R_Small", 90),
    ("Device:C_Small", 90),
    ("Device:Polyfuse", 90),
    ("Device:Jumper", 0),
    ("Device:Thermistor", 90),
    ("Device:Varistor", 90),
];

pub fn two_pin_horizontal_rot(lib: &str) -> Option<i32> {
    TWO_PIN_HORIZONTAL_ROT.iter().find(|(n, _)| *n == lib).map(|(_, v)| *v)
}

#[derive(Debug, Clone)]
pub struct LayoutError(pub String);

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for LayoutError {}

pub type Dir = (i32, i32);

/// Outward direction of a pin on an instance with the given rotation/mirror.
pub fn dir_of(pin: &crate::symlib::Pin, rot: i32, mirror: &str) -> Dir {
    let mut ang = (pin.angle + rot).rem_euclid(360);
    if mirror == "y" {
        ang = (180 - ang).rem_euclid(360);
    }
    if mirror == "x" {
        ang = (-ang).rem_euclid(360);
    }
    match ang {
        0 => (-1, 0),
        180 => (1, 0),
        90 => (0, 1),
        _ => (0, -1),
    }
}

pub fn rot_label(d: Dir) -> i32 {
    match d {
        (1, 0) => 0,
        (-1, 0) => 180,
        (0, -1) => 90,
        _ => 270,
    }
}

thread_local! {
    /// Symbols embedded in an edited file (custom libs).
    static EXTRA_INFOS: RefCell<HashMap<String, Arc<SymbolInfo>>> = RefCell::new(HashMap::new());
    /// net -> false until a PWR_FLAG was attached next to one of its power symbols.
    static FLAG_AT_SYMBOL: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
}

pub fn extra_infos_get(lib: &str) -> Option<Arc<SymbolInfo>> {
    EXTRA_INFOS.with(|m| m.borrow().get(lib).cloned())
}

pub fn extra_infos_set(lib: &str, info: Arc<SymbolInfo>) {
    EXTRA_INFOS.with(|m| {
        m.borrow_mut().insert(lib.to_string(), info);
    });
}

pub fn extra_infos_clear() {
    EXTRA_INFOS.with(|m| m.borrow_mut().clear());
}

pub fn flag_at_symbol_get(net: &str) -> Option<bool> {
    FLAG_AT_SYMBOL.with(|m| m.borrow().get(net).copied())
}

pub fn flag_at_symbol_set(net: &str, v: bool) {
    FLAG_AT_SYMBOL.with(|m| {
        m.borrow_mut().insert(net.to_string(), v);
    });
}

pub fn flag_at_symbol_clear() {
    FLAG_AT_SYMBOL.with(|m| m.borrow_mut().clear());
}

/// Symbol lookup: custom libs first, then the stock index.
pub fn info(lib: &str) -> Result<Arc<SymbolInfo>, LayoutError> {
    extra_infos_get(lib)
        .or_else(|| index().get(lib))
        .ok_or_else(|| LayoutError(format!("unknown symbol '{lib}' - use search_symbols to find the right lib id")))
}

pub fn pin_pos(pin: &crate::symlib::Pin, at: [f64; 2], rot: i32, mirror: &str) -> [f64; 2] {
    let (dx, dy) = rot_point(pin.x, pin.y, rot, mirror);
    [at[0] + dx / GRID, at[1] + dy / GRID]
}

/// Instance extent in grid units, pins included.
pub fn extent(info: &SymbolInfo, at: [f64; 2], rot: i32, mirror: &str, unit: i32) -> [f64; 4] {
    let bb = info.bbox_for(unit);
    let pts = [
        rot_point(bb.0, bb.1, rot, mirror),
        rot_point(bb.2, bb.3, rot, mirror),
        rot_point(bb.0, bb.3, rot, mirror),
        rot_point(bb.2, bb.1, rot, mirror),
    ];
    let xs: Vec<f64> = pts.iter().map(|p| at[0] + p.0 / GRID).collect();
    let ys: Vec<f64> = pts.iter().map(|p| at[1] + p.1 / GRID).collect();
    [fmin(&xs), fmin(&ys), fmax(&xs), fmax(&ys)]
}

fn fmin(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::INFINITY, f64::min)
}
fn fmax(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
}

/// Raw geometry accumulator (local coordinates, grid units).
#[derive(Debug, Clone, Default)]
pub struct Geo {
    pub parts: Vec<Map<String, Value>>,
    pub power: Vec<Map<String, Value>>,
    pub wires: Vec<Vec<[f64; 2]>>,
    pub labels: Vec<Map<String, Value>>,
    pub nc: Vec<[f64; 2]>,
    pub texts: Vec<Map<String, Value>>,
    pub rects: Vec<Map<String, Value>>,
    /// Extents used for bbox estimation.
    pub boxes: Vec<[f64; 4]>,
}

/// Optional arguments of [`Geo::add_part`], mirroring the Python keyword arguments.
#[derive(Debug, Clone, Default)]
pub struct PartOpts {
    pub rot: i32,
    pub value: String,
    pub footprint: String,
    pub fields: Option<Map<String, Value>>,
    pub mirror: String,
    pub unit: i32,
    pub dnp: bool,
}

impl PartOpts {
    pub fn new(rot: i32) -> PartOpts {
        PartOpts { rot, unit: 1, ..Default::default() }
    }
}

impl Geo {
    pub fn new() -> Geo {
        Geo::default()
    }

    pub fn add_part(
        &mut self,
        id: &str,
        lib: &str,
        at: [f64; 2],
        opts: &PartOpts,
    ) -> Result<Arc<SymbolInfo>, LayoutError> {
        let unit = if opts.unit == 0 { 1 } else { opts.unit };
        let sym = info(lib)?;
        let mut p = Map::new();
        p.insert("id".into(), json!(id));
        p.insert("lib".into(), json!(lib));
        p.insert("at".into(), json!([at[0], at[1]]));
        p.insert("rot".into(), json!(opts.rot));
        if !opts.mirror.is_empty() {
            p.insert("mirror".into(), json!(opts.mirror));
        }
        if !opts.value.is_empty() {
            p.insert("value".into(), json!(opts.value));
        }
        if !opts.footprint.is_empty() {
            p.insert("footprint".into(), json!(opts.footprint));
        }
        if let Some(f) = &opts.fields
            && !f.is_empty()
        {
            p.insert("fields".into(), Value::Object(f.clone()));
        }
        if unit != 1 {
            p.insert("unit".into(), json!(unit));
        }
        if opts.dnp {
            p.insert("dnp".into(), json!(true));
        }
        self.parts.push(p);
        let mut ext = extent(&sym, at, opts.rot, &opts.mirror, unit);
        // reference/value text room: ~5 units on the right for vertical passives, above/below otherwise
        if sym.pins_for_unit(unit).len() <= 2 {
            if opts.rot == 0 || opts.rot == 180 {
                ext[2] += 5.0;
            } else {
                ext[1] -= 1.5;
                ext[3] += 1.5;
            }
        } else {
            ext[1] -= 1.5;
            ext[3] += 1.5;
        }
        self.boxes.push(ext);
        Ok(sym)
    }

    pub fn label_box(&self, text: &str, at: [f64; 2], rot: i32, type_: &str) -> [f64; 4] {
        let w = 0.95 * text.chars().count() as f64 + if type_ == "local" { 1.2 } else { 3.5 };
        label_rect(at, rot, w)
    }

    pub fn add_label(&mut self, text: &str, at: [f64; 2], rot: i32, type_: &str, shape: &str) {
        let mut d = Map::new();
        d.insert("text".into(), json!(text));
        d.insert("at".into(), json!([at[0], at[1]]));
        d.insert("rot".into(), json!(rot));
        if type_ != "local" {
            d.insert("type".into(), json!(type_));
            d.insert("shape".into(), json!(shape));
        }
        self.labels.push(d);
        let w = 0.95 * text.chars().count() as f64 + 1.2;
        self.boxes.push(label_rect(at, rot, w));
    }

    pub fn add_power(&mut self, net: &str, at: [f64; 2], rot: i32) {
        let mut d = Map::new();
        d.insert("net".into(), json!(net));
        d.insert("at".into(), json!([at[0], at[1]]));
        d.insert("rot".into(), json!(rot));
        self.power.push(d);
        let w = f64::max(3.0, 0.5 * net.chars().count() as f64 + 1.0);
        let (x, y) = (at[0], at[1]);
        let box_ = if is_gnd(net) {
            match rot {
                0 => [x - w, y, x + w, y + 4.0],
                180 => [x - w, y - 4.0, x + w, y],
                90 => [x - 4.0, y - w, x, y + w],
                _ => [x, y - w, x + 4.0, y + w],
            }
        } else {
            match rot {
                0 => [x - w, y - 4.0, x + w, y],
                180 => [x - w, y, x + w, y + 4.0],
                90 => [x, y - w, x + 4.0, y + w],
                _ => [x - 4.0, y - w, x, y + w],
            }
        };
        self.boxes.push(box_);
    }

    pub fn add_wire(&mut self, pts: &[[f64; 2]]) {
        self.wires.push(pts.to_vec());
    }

    pub fn add_nc(&mut self, at: [f64; 2]) {
        self.nc.push(at);
    }

    pub fn add_text(&mut self, text: &str, at: [f64; 2], size: f64, bold: bool) {
        let text = if !bold && size <= 1.5 {
            text.split('\n')
                .map(|par| if par.chars().count() > 60 { fill(par, 60) } else { par.to_string() })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            text.to_string()
        };
        let mut d = Map::new();
        d.insert("text".into(), json!(text));
        d.insert("at".into(), json!([at[0], at[1]]));
        d.insert("size".into(), json!(size));
        d.insert("bold".into(), json!(bold));
        self.texts.push(d);
        let lines: Vec<&str> = text.split('\n').collect();
        let w = 0.95 * size / 1.27 * lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f64;
        let h = 1.5 * size / 1.27 * lines.len() as f64;
        self.boxes.push([at[0], at[1] - h, at[0] + w, at[1]]);
    }

    pub fn bbox(&self) -> [f64; 4] {
        if self.boxes.is_empty() {
            return [0.0, 0.0, 0.0, 0.0];
        }
        [
            fmin(&self.boxes.iter().map(|b| b[0]).collect::<Vec<_>>()),
            fmin(&self.boxes.iter().map(|b| b[1]).collect::<Vec<_>>()),
            fmax(&self.boxes.iter().map(|b| b[2]).collect::<Vec<_>>()),
            fmax(&self.boxes.iter().map(|b| b[3]).collect::<Vec<_>>()),
        ]
    }

    pub fn translate(&mut self, dx: f64, dy: f64) {
        let t = |p: &Value| -> Value {
            let a = p.as_array().unwrap();
            json!([a[0].as_f64().unwrap_or(0.0) + dx, a[1].as_f64().unwrap_or(0.0) + dy])
        };
        for group in [&mut self.parts, &mut self.power, &mut self.labels, &mut self.texts] {
            for it in group.iter_mut() {
                let v = t(&it["at"]);
                it.insert("at".into(), v);
            }
        }
        for r in self.rects.iter_mut() {
            let (s, e) = (t(&r["start"]), t(&r["end"]));
            r.insert("start".into(), s);
            r.insert("end".into(), e);
        }
        for w in self.wires.iter_mut() {
            for p in w.iter_mut() {
                *p = [p[0] + dx, p[1] + dy];
            }
        }
        for p in self.nc.iter_mut() {
            *p = [p[0] + dx, p[1] + dy];
        }
        for b in self.boxes.iter_mut() {
            *b = [b[0] + dx, b[1] + dy, b[2] + dx, b[3] + dy];
        }
    }

    pub fn merge(&mut self, other: &Geo) {
        self.parts.extend(other.parts.iter().cloned());
        self.power.extend(other.power.iter().cloned());
        self.wires.extend(other.wires.iter().cloned());
        self.labels.extend(other.labels.iter().cloned());
        self.nc.extend(other.nc.iter().cloned());
        self.texts.extend(other.texts.iter().cloned());
        self.rects.extend(other.rects.iter().cloned());
        self.boxes.extend(other.boxes.iter().cloned());
    }

    /// The raw design JSON. Coordinates that landed on whole grid units are written as integers,
    /// the way the Python engine's integer arithmetic does.
    pub fn to_json(&self) -> Value {
        let objs = |v: &Vec<Map<String, Value>>| Value::Array(v.iter().cloned().map(Value::Object).collect());
        let mut out = json!({
            "parts": objs(&self.parts),
            "power": objs(&self.power),
            "wires": self.wires,
            "labels": objs(&self.labels),
            "nc": self.nc,
            "texts": objs(&self.texts),
            "rects": objs(&self.rects),
        });
        narrow_ints(&mut out);
        out
    }
}

/// Rewrite every integral float in the tree as an integer.
fn narrow_ints(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(narrow_ints),
        Value::Object(o) => o.values_mut().for_each(narrow_ints),
        Value::Number(n) => {
            if let Some(f) = n.as_f64()
                && n.as_i64().is_none()
                && f.fract() == 0.0
                && f.abs() < 9e15
            {
                *v = Value::from(f as i64);
            }
        }
        _ => {}
    }
}

fn label_rect(at: [f64; 2], rot: i32, w: f64) -> [f64; 4] {
    match rot {
        0 => [at[0], at[1] - 1.6, at[0] + w, at[1] + 0.3],
        180 => [at[0] - w, at[1] - 1.6, at[0], at[1] + 0.3],
        90 => [at[0] - 1.6, at[1] - w, at[0] + 0.3, at[1]],
        _ => [at[0] - 1.6, at[1], at[0] + 0.3, at[1] + w],
    }
}

/// Greedy word wrap, matching `textwrap.fill(text, width)` for the plain text we emit.
fn fill(text: &str, width: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let space = if cur.is_empty() { 0 } else { 1 };
            if cur.chars().count() + space + word.chars().count() <= width {
                if space == 1 {
                    cur.push(' ');
                }
                cur.push_str(word);
                break;
            }
            if cur.is_empty() {
                // break a word longer than the width
                let cut = word.char_indices().nth(width).map(|(i, _)| i).unwrap_or(word.len());
                lines.push(word[..cut].to_string());
                word = &word[cut..];
                if word.is_empty() {
                    break;
                }
                continue;
            }
            lines.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.join("\n")
}

pub fn is_gnd(net: &str) -> bool {
    let n = net.to_uppercase();
    n.ends_with("GND")
        || n.starts_with("GND")
        || matches!(n.as_str(), "VSS" | "VSSA" | "AGND" | "DGND" | "PGND" | "EARTH" | "VEE" | "0V")
}

pub fn snap_even(v: f64) -> f64 {
    crate::model::round_py(v / 2.0) * 2.0
}

pub fn overlap_box(a: [f64; 4], b: [f64; 4], m: f64) -> bool {
    !(a[2] + m <= b[0] || b[2] + m <= a[0] || a[3] + m <= b[1] || b[3] + m <= a[1])
}

pub fn rects_overlap(a: [f64; 4], b: [f64; 4]) -> bool {
    !(a[2] <= b[0] || b[2] <= a[0] || a[3] <= b[1] || b[3] <= a[1])
}

/// Attach a PWR_FLAG beside a power symbol whose connection point is `pt` (stub direction `d`).
pub fn flag_next_to(g: &mut Geo, net: &str, pt: [f64; 2], d: Dir) {
    if flag_at_symbol_get(net) != Some(false) {
        return;
    }
    let mut pt = pt;
    if d.0 == 0 {
        pt = [pt[0], pt[1] - d.1 as f64 * 2.0];
    }
    let sides: Vec<Dir> = if d.0 == 0 { vec![(1, 0), (-1, 0)] } else { vec![(d.0, 0)] };
    for side in sides {
        let far = [pt[0] + side.0 as f64 * 8.0, pt[1]];
        let box_ = [pt[0].min(far[0]) - 4.5, pt[1] - 6.0, pt[0].max(far[0]) + 4.5, pt[1] + 1.0];
        let n = g.boxes.len().saturating_sub(1);
        if g.boxes[..n].iter().any(|b| overlap_box(box_, *b, 0.0)) {
            continue;
        }
        g.add_wire(&[pt, far]);
        g.add_power("PWR_FLAG", far, 0);
        flag_at_symbol_set(net, true);
        return;
    }
}

/// Rotation for a power symbol attached to a stub pointing in direction `d` (outward from the pin).
pub fn power_rot(net: &str, d: Dir) -> i32 {
    let up_kind = !is_gnd(net);
    match d {
        (0, -1) => {
            if up_kind {
                0
            } else {
                180
            }
        }
        (0, 1) => {
            if up_kind {
                180
            } else {
                0
            }
        }
        (-1, 0) => {
            if up_kind {
                270
            } else {
                90
            }
        }
        _ => {
            if up_kind {
                90
            } else {
                270
            }
        }
    }
}

fn key2(p: [f64; 2]) -> (i64, i64) {
    (((p[0] * 100.0).round()) as i64, ((p[1] * 100.0).round()) as i64)
}

fn spec_str(v: &Value) -> Option<&str> {
    v.as_str()
}

/// Connect the pins of one instance according to `pinmap`.
///
/// spec: `"nc"` | `{"net": X}` | `{"power": X}` | `{"label": X, ...}` | `"float"` (leave
/// unconnected). Power pins on one side with the same net and adjacent positions share a rail.
pub fn connect_pins(
    g: &mut Geo,
    sym: &SymbolInfo,
    at: [f64; 2],
    rot: i32,
    mirror: &str,
    unit: i32,
    pinmap: &Map<String, Value>,
    default: &Value,
    skip: &std::collections::BTreeSet<String>,
) -> Result<(), LayoutError> {
    let pins: Vec<&crate::symlib::Pin> = sym.pins_for_unit(unit).into_iter().filter(|p| !p.hidden).collect();
    let mut spec_of: HashMap<String, Value> = HashMap::new();
    for (key, spec) in pinmap {
        let matched: Vec<&&crate::symlib::Pin> = {
            let by_num: Vec<&&crate::symlib::Pin> = pins.iter().filter(|p| p.number == *key).collect();
            if by_num.is_empty() { pins.iter().filter(|p| p.name == *key).collect() } else { by_num }
        };
        if matched.is_empty() {
            let all: String =
                pins.iter().map(|p| format!("{}={}", p.number, p.name)).collect::<Vec<_>>().join(", ");
            return Err(LayoutError(format!(
                "{}: no pin '{}' (pins: {})",
                sym.lib_id,
                key,
                all.chars().take(500).collect::<String>()
            )));
        }
        for p in matched {
            spec_of.insert(p.number.clone(), spec.clone());
        }
    }

    // (dir, net) -> [(pos, dir)] in first-seen order
    let mut groups: Vec<((Dir, String), Vec<([f64; 2], Dir)>)> = Vec::new();
    for p in &pins {
        if skip.contains(&p.number) {
            continue;
        }
        let mut spec = spec_of.get(&p.number).cloned().unwrap_or_else(|| default.clone());
        if spec.is_null() {
            spec = default.clone();
        }
        let pos = pin_pos(p, at, rot, mirror);
        let d = dir_of(p, rot, mirror);
        if matches!(spec_str(&spec), Some("float" | "wired" | "labelled")) {
            continue;
        }
        if spec_str(&spec) == Some("nc") || spec.get("nc").and_then(|v| v.as_bool()).unwrap_or(false) {
            g.add_nc(pos);
            continue;
        }
        if spec_str(&spec) == Some("label") {
            let n = p.name.split('/').next().unwrap_or("").to_string();
            spec = json!({ "net": if n.is_empty() { p.number.clone() } else { n } });
        }
        if let Some(s) = spec_str(&spec) {
            spec = json!({ "net": s });
        }
        if let Some(pw) = spec.get("power").and_then(|v| v.as_str()) {
            let key = (d, pw.to_string());
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => v.push((pos, d)),
                None => groups.push((key, vec![(pos, d)])),
            }
            continue;
        }
        let net = spec
            .get("net")
            .and_then(|v| v.as_str())
            .or_else(|| spec.get("label").and_then(|v| v.as_str()))
            .ok_or_else(|| LayoutError(format!("{} pin {}: bad spec {}", sym.lib_id, p.number, spec)))?
            .to_string();
        let stub = spec.get("stub").and_then(|v| v.as_f64()).unwrap_or(STUB).trunc();
        let end = [pos[0] + d.0 as f64 * stub, pos[1] + d.1 as f64 * stub];
        g.add_wire(&[pos, end]);
        g.add_label(
            &net,
            end,
            rot_label(d),
            spec.get("type").and_then(|v| v.as_str()).unwrap_or("local"),
            spec.get("shape").and_then(|v| v.as_str()).unwrap_or("input"),
        );
    }

    // occupied positions along each side (connected pins) decide where a power stub may turn
    let mut occupied: std::collections::HashSet<(i64, i64)> = Default::default();
    for p in &pins {
        let spec = spec_of.get(&p.number).cloned().unwrap_or_else(|| default.clone());
        if matches!(spec_str(&spec), Some("float" | "nc" | "wired"))
            || spec.is_null()
            || spec.get("nc").and_then(|v| v.as_bool()).unwrap_or(false)
        {
            continue; // a plain routed wire next door does not crowd a power symbol; labels/symbols do
        }
        occupied.insert(key2(pin_pos(p, at, rot, mirror)));
    }

    if sym.ref_prefix == "J" {
        // the connector's reference/value slot must stay clear of power stubs
        let slot = crate::engine::connector_text_slot(sym, unit, rot, mirror);
        let dirs: std::collections::HashSet<Dir> = pins.iter().map(|p| dir_of(p, rot, mirror)).collect();
        let lateral = dirs.contains(&(1, 0)) || dirs.contains(&(-1, 0));
        for p in &pins {
            let pos = pin_pos(p, at, rot, mirror);
            for k in [2.0, 4.0] {
                if slot == "above" && !lateral {
                    occupied.insert(key2([pos[0], pos[1] - k]));
                } else if slot == "below" && !lateral {
                    occupied.insert(key2([pos[0], pos[1] + k]));
                }
            }
        }
    }
    let free = |occupied: &std::collections::HashSet<(i64, i64)>, pt: [f64; 2]| !occupied.contains(&key2(pt));

    for ((d, net), mut lst) in groups {
        lst.sort_by(|a, b| {
            (a.0[0], a.0[1]).partial_cmp(&(b.0[0], b.0[1])).unwrap_or(std::cmp::Ordering::Equal)
        });
        // split into runs of adjacent pins (distance 2 along the side)
        let mut runs: Vec<Vec<([f64; 2], Dir)>> = Vec::new();
        let mut cur = vec![lst[0]];
        for item in lst.iter().skip(1) {
            let prev = cur.last().unwrap().0;
            let dist = (item.0[0] - prev[0]).abs() + (item.0[1] - prev[1]).abs();
            if dist <= 4.01 {
                cur.push(*item);
            } else {
                runs.push(std::mem::take(&mut cur));
                cur = vec![*item];
            }
        }
        runs.push(cur);

        for run in runs {
            let stub = if d.1 != 0 {
                if pins.len() <= 4 { PSTUB } else { PSTUB * 2.0 }
            } else {
                PSTUB * 2.0
            };
            let ends: Vec<[f64; 2]> =
                run.iter().map(|(pos, _)| [pos[0] + d.0 as f64 * stub, pos[1] + d.1 as f64 * stub]).collect();
            for (i, (pos, _)) in run.iter().enumerate() {
                g.add_wire(&[*pos, ends[i]]);
            }
            if run.len() > 1 {
                g.add_wire(&[ends[0], *ends.last().unwrap()]);
            }
            if d.1 != 0 {
                let (first, last) = (run[0].0, run.last().unwrap().0);
                let crowded = !free(&occupied, [first[0] - 2.0, first[1]])
                    || !free(&occupied, [last[0] + 2.0, last[1]])
                    || !free(&occupied, [first[0] - 4.0, first[1]])
                    || !free(&occupied, [last[0] + 4.0, last[1]]);
                if run.len() == 1 {
                    if crowded {
                        let far = [ends[0][0], ends[0][1] + d.1 as f64 * (STUB - PSTUB)];
                        g.add_wire(&[ends[0], far]);
                        g.add_label(&net, far, rot_label(d), "global", "input");
                    } else {
                        g.add_power(&net, ends[0], power_rot(&net, d));
                        flag_next_to(g, &net, ends[0], d);
                    }
                } else {
                    let mid = if ends.len() % 2 == 1 { ends[ends.len() / 2] } else { ends[ends.len() / 2 - 1] };
                    let far = [mid[0] + d.0 as f64 * PSTUB, mid[1] + d.1 as f64 * PSTUB];
                    g.add_wire(&[mid, far]);
                    if crowded {
                        g.add_label(&net, far, rot_label(d), "global", "input");
                    } else {
                        g.add_power(&net, far, power_rot(&net, d));
                    }
                }
                continue;
            }
            // horizontal stub: turn up for supplies / down for grounds when free
            let want_up = !is_gnd(&net);
            let (first, last) = (run[0].0, run.last().unwrap().0);
            let mut turn_pt = if want_up { ends[0] } else { *ends.last().unwrap() };
            let mut check_pin = if want_up { first } else { last };
            let mut dy: f64 = if want_up { -1.0 } else { 1.0 };
            if !(free(&occupied, [check_pin[0], check_pin[1] + 2.0 * dy])
                && free(&occupied, [check_pin[0], check_pin[1] + 4.0 * dy]))
            {
                let (alt_pt, alt_pin) =
                    if want_up { (*ends.last().unwrap(), last) } else { (ends[0], first) };
                if free(&occupied, [alt_pin[0], alt_pin[1] - 2.0 * dy])
                    && free(&occupied, [alt_pin[0], alt_pin[1] - 4.0 * dy])
                {
                    turn_pt = alt_pt;
                    check_pin = alt_pin;
                    dy = -dy;
                }
            }
            if free(&occupied, [check_pin[0], check_pin[1] + 2.0 * dy])
                && free(&occupied, [check_pin[0], check_pin[1] + 4.0 * dy])
            {
                let far = [turn_pt[0], turn_pt[1] + 4.0 * dy];
                g.add_wire(&[turn_pt, far]);
                g.add_power(&net, far, 0);
                flag_next_to(g, &net, far, (0, dy as i32));
            } else {
                for e in &ends {
                    let far = [e[0] + d.0 as f64 * (STUB - PSTUB), e[1]];
                    g.add_wire(&[*e, far]);
                    g.add_label(&net, far, rot_label(d), "global", "input");
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// arrangement
// ---------------------------------------------------------------------------

/// Try several block orderings with the bottom-left heuristic; return the most compact layout that fits.
pub fn pack_blocks(
    geos: &[(String, Geo)],
    x0: f64,
    y0: f64,
    xmax: f64,
    ymax: f64,
    tb: (f64, f64),
    obstacles: &[[f64; 4]],
    spread_ok: bool,
) -> Option<Geo> {
    let n = geos.len();
    let h = |i: usize| {
        let b = geos[i].1.bbox();
        b[3] - b[1]
    };
    let w = |i: usize| {
        let b = geos[i].1.bbox();
        b[2] - b[0]
    };
    let by = |key: &dyn Fn(usize) -> f64| {
        let mut v: Vec<usize> = (0..n).collect();
        v.sort_by(|a, b| key(*a).partial_cmp(&key(*b)).unwrap_or(std::cmp::Ordering::Equal));
        v
    };
    let orders: Vec<Vec<usize>> = vec![
        (0..n).collect(),
        by(&|i| -h(i)),
        by(&|i| -w(i) * h(i)),
        by(&|i| -w(i)),
    ];
    let mut best: Option<Geo> = None;
    let mut best_score = f64::INFINITY;
    let mut best_blocks: Option<Vec<(String, Geo)>> = None;
    let sheet_aspect = (xmax - x0) / f64::max(1.0, ymax - y0);
    let fracs: &[f64] = if obstacles.is_empty() { &[1.0, 0.85, 0.7, 0.55] } else { &[1.0] };
    for (oi, order) in orders.iter().enumerate() {
        for frac in fracs {
            let mut trial: Vec<(String, Geo)> = geos.to_vec();
            let Some(res) =
                pack_blocks_ordered(&mut trial, order, x0, y0, x0 + (xmax - x0) * frac, ymax, tb, obstacles)
            else {
                continue;
            };
            let bb = res.bbox();
            let (bw, bh) = (bb[2] - x0, bb[3] - y0);
            let aspect = bw / f64::max(1.0, bh);
            let mut score = (aspect / sheet_aspect).ln().abs() * 0.5
                + (bw * bh) / ((xmax - x0) * (ymax - y0)) * 0.5
                + 0.8 * f64::max(0.0, 1.0 - bw / (xmax - x0))
                + 0.3 * f64::max(0.0, 1.0 - bh / (ymax - y0));
            score += if oi == 0 { 0.0 } else { 0.35 };
            if best.is_none() || score < best_score {
                best = Some(res);
                best_score = score;
                best_blocks = Some(trial);
            }
        }
    }
    if let Some(blocks) = best_blocks.as_ref()
        && best.is_some()
        && obstacles.is_empty()
        && spread_ok
    {
        // spread the arrangement to use ~80% of the usable area (keeps the relative layout)
        let bbs: Vec<[f64; 4]> = blocks.iter().map(|(_, g)| g.bbox()).collect();
        let bx0 = fmin(&bbs.iter().map(|b| b[0]).collect::<Vec<_>>());
        let by0 = fmin(&bbs.iter().map(|b| b[1]).collect::<Vec<_>>());
        let bx1 = fmax(&bbs.iter().map(|b| b[2]).collect::<Vec<_>>());
        let by1 = fmax(&bbs.iter().map(|b| b[3]).collect::<Vec<_>>());
        let (bw, bh) = (bx1 - bx0, by1 - by0);
        let cap = if bbs.len() <= 3 { 1.35 } else { 1.7 };
        let kx = if bbs.len() > 1 { f64::min(cap, 0.80 * (xmax - x0) / f64::max(1.0, bw)) } else { 1.0 };
        let ky = if bbs.len() > 1 { f64::min(cap, 0.80 * (ymax - y0) / f64::max(1.0, bh)) } else { 1.0 };
        for (kx_, ky_) in [(kx, ky), (kx, 1.0), (1.0, ky), ((1.0 + kx) / 2.0, (1.0 + ky) / 2.0)] {
            let (kx_, ky_) = (kx_.max(1.0), ky_.max(1.0));
            if kx_ < 1.02 && ky_ < 1.02 {
                continue;
            }
            let mut spread = Geo::new();
            for ((_, g), b) in blocks.iter().zip(bbs.iter()) {
                let mut g2 = g.clone();
                let nx = bx0 + (b[0] - bx0) * kx_;
                let ny = by0 + (b[1] - by0) * ky_;
                g2.translate(snap_even(nx - b[0]), snap_even(ny - b[1]));
                spread.merge(&g2);
            }
            if !spread.boxes.iter().any(|b| b[2] > tb.0 - 4.0 && b[3] > tb.1 - 4.0)
                && spread.boxes.iter().all(|b| b[2] <= xmax && b[3] <= ymax)
            {
                best = Some(spread);
                break;
            }
        }
    }
    if let Some(b) = best.as_mut()
        && obstacles.is_empty()
    {
        // centre the content in the usable area, as long as it doesn't hit the title block
        let bb = b.bbox();
        let (bw, bh) = (bb[2] - bb[0], bb[3] - bb[1]);
        for (fx, fy) in [(0.5, 0.5), (0.5, 0.0), (0.0, 0.5), (0.25, 0.25), (0.0, 0.0)] {
            let dx = snap_even((xmax - x0 - bw) * fx);
            let dy = snap_even((ymax - y0 - bh) * fy);
            if dx < 0.0 || dy < 0.0 {
                continue;
            }
            let boxes: Vec<[f64; 4]> =
                b.boxes.iter().map(|x| [x[0] + dx, x[1] + dy, x[2] + dx, x[3] + dy]).collect();
            if !boxes.iter().any(|x| x[2] > tb.0 - 4.0 && x[3] > tb.1 - 4.0)
                && boxes.iter().all(|x| x[2] <= xmax && x[3] <= ymax)
            {
                b.translate(dx, dy);
                break;
            }
        }
    }
    best
}

/// Bottom-left packing of block bboxes into `[x0,xmax] x [y0,ymax]`, avoiding the title block
/// corner `tb` and any extra obstacle rects.
pub fn pack_blocks_ordered(
    geos: &mut [(String, Geo)],
    order: &[usize],
    x0: f64,
    y0: f64,
    xmax: f64,
    ymax: f64,
    tb: (f64, f64),
    obstacles: &[[f64; 4]],
) -> Option<Geo> {
    let mut placed: Vec<[f64; 4]> = Vec::new();
    let mut corners: Vec<[f64; 4]> = Vec::new();
    let obst = [tb.0 - 4.0, tb.1 - 4.0, xmax + 100.0, ymax + 100.0];
    for ob in obstacles {
        placed.push([
            ob[0] - BLOCK_GAP_X + 0.5,
            ob[1] - BLOCK_GAP_Y + 0.5,
            ob[2] + BLOCK_GAP_X - 0.5,
            ob[3] + BLOCK_GAP_Y - 0.5,
        ]);
        corners.push(*ob);
    }
    let mut result = Geo::new();
    for i in order {
        let bb = geos[*i].1.bbox();
        let (w, h) = (bb[2] - bb[0], bb[3] - bb[1]);
        let mut cands: Vec<(f64, f64)> = vec![(x0, y0)];
        for r in &corners {
            for c in [(r[2] + BLOCK_GAP_X, r[1]), (r[0], r[3] + BLOCK_GAP_Y), (r[2] + BLOCK_GAP_X, y0), (x0, r[3] + BLOCK_GAP_Y)] {
                if !cands.iter().any(|e| *e == c) {
                    cands.push(c);
                }
            }
        }
        cands.sort_by(|a, b| (a.1, a.0).partial_cmp(&(b.1, b.0)).unwrap_or(std::cmp::Ordering::Equal));
        let mut best: Option<(f64, f64)> = None;
        for (cx, cy) in cands {
            let rect = [cx, cy, cx + w, cy + h];
            if rect[2] > xmax || rect[3] > ymax {
                continue;
            }
            if placed.iter().any(|r| rects_overlap(rect, *r)) || rects_overlap(rect, obst) {
                continue;
            }
            best = Some((cx, cy));
            break;
        }
        let (bx, by) = best?;
        let g = &mut geos[*i].1;
        g.translate(snap_even(bx - bb[0]), snap_even(by - bb[1]));
        let nb = g.bbox();
        placed.push([
            nb[0] - BLOCK_GAP_X + 0.5,
            nb[1] - BLOCK_GAP_Y + 0.5,
            nb[2] + BLOCK_GAP_X - 0.5,
            nb[3] + BLOCK_GAP_Y - 0.5,
        ]);
        corners.push(nb);
        result.merge(g);
    }
    Some(result)
}

/// Occupied rectangles (grid units) of a raw design — obstacles when adding a circuit to a sheet.
pub fn raw_boxes(raw: &Value) -> Vec<[f64; 4]> {
    let mut boxes: Vec<[f64; 4]> = Vec::new();
    let list = |k: &str| raw.get(k).and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for p in list("parts") {
        let lib = p.get("lib").and_then(|v| v.as_str()).unwrap_or("");
        let lib_name = p.get("lib_name").and_then(|v| v.as_str()).unwrap_or(lib);
        let sym = index().get(lib).or_else(|| extra_infos_get(lib_name));
        if let Some(sym) = sym {
            let at = p.get("at").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let at = [
                at.first().and_then(|v| v.as_f64()).unwrap_or(0.0),
                at.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
            ];
            let e = extent(
                &sym,
                at,
                p.get("rot").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
                p.get("mirror").and_then(|v| v.as_str()).unwrap_or(""),
                p.get("unit").and_then(|v| v.as_f64()).unwrap_or(1.0) as i32,
            );
            boxes.push([e[0] - 4.0, e[1] - 3.0, e[2] + 4.0, e[3] + 3.0]);
        }
    }
    let num_pt = |pt: &Value| -> Option<[f64; 2]> {
        let a = pt.as_array()?;
        Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?])
    };
    for w in list("wires") {
        let pts_v = if w.is_object() { w.get("pts").cloned().unwrap_or(Value::Null) } else { w.clone() };
        let pts: Vec<[f64; 2]> =
            pts_v.as_array().map(|a| a.iter().filter_map(num_pt).collect()).unwrap_or_default();
        if !pts.is_empty() {
            let xs: Vec<f64> = pts.iter().map(|p| p[0]).collect();
            let ys: Vec<f64> = pts.iter().map(|p| p[1]).collect();
            boxes.push([fmin(&xs) - 0.5, fmin(&ys) - 0.5, fmax(&xs) + 0.5, fmax(&ys) + 0.5]);
        }
    }
    for l in list("labels").into_iter().chain(list("power")) {
        if let Some(p) = l.get("at").and_then(num_pt) {
            boxes.push([p[0] - 5.0, p[1] - 4.0, p[0] + 5.0, p[1] + 4.0]);
        }
    }
    for t in list("texts") {
        if let Some(p) = t.get("at").and_then(num_pt) {
            let text = t.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let lines: Vec<&str> = text.split('\n').collect();
            let w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f64;
            boxes.push([p[0], p[1] - 2.0, p[0] + 0.95 * w + 1.0, p[1] + 2.0 * lines.len() as f64]);
        }
    }
    boxes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnd_names() {
        assert!(is_gnd("GND"));
        assert!(is_gnd("AGND"));
        assert!(is_gnd("VSS"));
        assert!(!is_gnd("+3V3"));
    }

    #[test]
    fn power_rotations() {
        assert_eq!(power_rot("+3V3", (0, -1)), 0);
        assert_eq!(power_rot("GND", (0, -1)), 180);
        assert_eq!(power_rot("GND", (0, 1)), 0);
        assert_eq!(power_rot("+3V3", (-1, 0)), 270);
    }

    #[test]
    fn snap_even_is_bankers_like_python() {
        for (v, want) in [(1.0, 0.0), (3.0, 4.0), (5.0, 4.0), (7.0, 8.0), (-1.0, 0.0), (-3.0, -4.0), (2.0, 2.0)] {
            assert_eq!(snap_even(v), want, "snap_even({v})");
        }
    }

    #[test]
    fn geo_translate_and_bbox() {
        let mut g = Geo::new();
        g.add_label("NET", [0.0, 0.0], 0, "local", "input");
        let b0 = g.bbox();
        g.translate(2.0, 3.0);
        let b1 = g.bbox();
        assert_eq!(b1[0] - b0[0], 2.0);
        assert_eq!(b1[1] - b0[1], 3.0);
    }

    #[test]
    fn wrap_matches_textwrap() {
        assert_eq!(fill("aaa bbb ccc", 7), "aaa bbb\nccc");
    }
}
