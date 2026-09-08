//! Build pipeline: resolve pin references, compile, check geometry/connectivity, run ERC.
//!
//! Everything reported back to the LLM is in grid units (1 unit = 1.27 mm). Faithful port of
//! `schagent/check.py`: same checks, same thresholds, same message wording (the agent's prompt
//! quotes these messages).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::compile::{Compiler, Key, PinsAt, on_segment, round_py};
use crate::model::{Design, GRID, Part, gu, rot_point};
use crate::symlib::{Pin, SymbolInfo, index, parse_symbol_node};

pub const PAPER_MM: [(&str, (f64, f64)); 8] = [
    ("A5", (210.0, 148.0)),
    ("A4", (297.0, 210.0)),
    ("A3", (420.0, 297.0)),
    ("A2", (594.0, 420.0)),
    ("A", (279.4, 215.9)),
    ("B", (431.8, 279.4)),
    ("USLetter", (279.4, 215.9)),
    ("USLegal", (355.6, 215.9)),
];

pub fn paper_mm(paper: &str) -> (f64, f64) {
    PAPER_MM
        .iter()
        .find(|(k, _)| *k == paper)
        .map(|(_, v)| *v)
        .unwrap_or((297.0, 210.0))
}

/// KiCad ERC kinds the agent is never asked to fix.
pub const IGNORED_ERC: [&str; 4] = [
    "lib_symbol_issues",
    "footprint_link_issues",
    "lib_symbol_mismatch",
    "undefined_netclass",
];

// ---------------------------------------------------------------- formatting helpers

/// Python's `repr` for the JSON values that appear in error messages.
fn repr(v: &Value) -> String {
    match v {
        Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => format!("[{}]", a.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(_) => v.to_string(),
    }
}

/// mm -> grid units, printed the way Python prints an int or a float.
fn g(v: f64) -> String {
    let u = gu(v);
    if u == 0.0 { "0".into() } else { format!("{u}") }
}

fn fmt_pt(x: f64, y: f64) -> String {
    format!("({},{})", g(x), g(y))
}

// ---------------------------------------------------------------- pin reference resolution

/// Outward unit vector (away from the body) of a pin after the instance transform, sheet coords.
pub fn pin_dir(pin: &Pin, rot: i32, mirror: &str) -> (i32, i32) {
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

/// pin number or name -> (x, y) in grid units and the pin's outward direction.
#[derive(Debug, Clone, Default)]
pub struct PinMap {
    order: Vec<String>,
    by_key: HashMap<String, (f64, f64, (i32, i32))>,
}

impl PinMap {
    fn insert(&mut self, k: String, v: (f64, f64, (i32, i32))) {
        if !self.by_key.contains_key(&k) {
            self.order.push(k.clone());
            self.by_key.insert(k, v);
        }
    }
    pub fn get(&self, k: &str) -> Option<&(f64, f64, (i32, i32))> {
        self.by_key.get(k)
    }
    pub fn keys(&self) -> &[String] {
        &self.order
    }
}

fn pin_map(info: &SymbolInfo, unit: i32, rot: i32, mirror: &str, ax: f64, ay: f64) -> PinMap {
    let mut m = PinMap::default();
    for pin in info.pins_for_unit(unit) {
        let (dx, dy) = rot_point(pin.x, pin.y, rot, mirror);
        let pt = (
            round_py(ax + dx / GRID, 3),
            round_py(ay + dy / GRID, 3),
            pin_dir(pin, rot, mirror),
        );
        m.insert(pin.number.clone(), pt);
        if !pin.name.is_empty() {
            m.insert(pin.name.clone(), pt);
        }
    }
    m
}

/// Insertion-ordered `ref -> PinMap` built from the JSON parts (before compile).
#[derive(Debug, Default)]
pub struct PinTable {
    order: Vec<String>,
    by_ref: HashMap<String, PinMap>,
}

impl PinTable {
    fn insert(&mut self, k: String, v: PinMap) {
        if !self.by_ref.contains_key(&k) {
            self.order.push(k.clone());
        }
        self.by_ref.insert(k, v);
    }
    pub fn get(&self, k: &str) -> Option<&PinMap> {
        self.by_ref.get(k)
    }
    pub fn contains(&self, k: &str) -> bool {
        self.by_ref.contains_key(k)
    }
    pub fn refs(&self) -> &[String] {
        &self.order
    }
}

fn part_field<'a>(p: &'a Value, k: &str) -> Option<&'a Value> {
    p.as_object()?.get(k)
}

fn part_rot(p: &Value) -> i32 {
    part_field(p, "rot").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32
}

fn part_mirror(p: &Value) -> String {
    part_field(p, "mirror")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn part_unit(p: &Value) -> i32 {
    part_field(p, "unit")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0) as i32
}

fn part_at(p: &Value) -> (f64, f64) {
    let a = part_field(p, "at")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    (
        a.first().and_then(|v| v.as_f64()).unwrap_or(0.0),
        a.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
    )
}

fn part_id(p: &Value) -> String {
    part_field(p, "id")
        .map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default()
}

fn parts_of(d: &Value) -> Vec<Value> {
    d.get("parts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn pin_lookup_table(d: &Value) -> PinTable {
    let mut table = PinTable::default();
    for pj in parts_of(d) {
        let lib_id = part_field(&pj, "lib")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let Some(info) = index().get(lib_id) else {
            continue;
        };
        let (ax, ay) = part_at(&pj);
        table.insert(
            part_id(&pj),
            pin_map(
                &info,
                part_unit(&pj),
                part_rot(&pj),
                &part_mirror(&pj),
                ax,
                ay,
            ),
        );
    }
    table
}

fn lib_infos_from_base(base: Option<&Design>) -> HashMap<String, SymbolInfo> {
    base.map(|b| {
        b.lib_symbols
            .iter()
            .map(|(k, v)| (k.clone(), parse_symbol_node(v, k)))
            .collect()
    })
    .unwrap_or_default()
}

/// Replace `"REF.PIN"` strings in wires/labels/power/nc with coordinates (grid units).
pub fn resolve_pin_refs(d: &mut Value, base: Option<&Design>) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    let mut table = pin_lookup_table(d);
    // also parts from the base file's embedded libs (custom libs)
    let infos = lib_infos_from_base(base);
    if !infos.is_empty() {
        for pj in parts_of(d) {
            let key = part_field(&pj, "lib_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    part_field(&pj, "lib")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                });
            let Some(info) = infos.get(&key) else {
                continue;
            };
            if table.contains(&part_id(&pj)) {
                continue;
            }
            let (ax, ay) = part_at(&pj);
            table.insert(
                part_id(&pj),
                pin_map(
                    info,
                    part_unit(&pj),
                    part_rot(&pj),
                    &part_mirror(&pj),
                    ax,
                    ay,
                ),
            );
        }
    }

    let resolve =
        |errors: &mut Vec<String>, v: &Value, ctx: &str| -> (f64, f64, Option<(i32, i32)>) {
            if let Some(s) = v.as_str() {
                let vs = s.trim();
                if !vs.contains('.') {
                    errors.push(format!(
                        "{ctx}: bad point {} (use [x,y] or \"REF.PIN\"; net names are not points - \
                     use a power symbol or label at a coordinate instead)",
                        repr(v)
                    ));
                    return (0.0, 0.0, None);
                }
                // split at the first '.' that yields a known part reference (refs may contain odd characters)
                let mut found: Option<(String, String)> = None;
                let b: Vec<char> = vs.chars().collect();
                for k in 0..b.len() {
                    if b[k] == '.' {
                        let head: String = b[..k].iter().collect();
                        if table.contains(&head) {
                            found = Some((head, b[k + 1..].iter().collect()));
                            break;
                        }
                    }
                }
                let Some((r, pin)) = found else {
                    let (r, _pin) = vs.split_once('.').unwrap();
                    let near = table
                        .refs()
                        .iter()
                        .find(|x| x.to_lowercase() == r.to_lowercase());
                    errors.push(format!(
                        "{ctx}: unknown part {} in pin reference {}{}",
                        repr(&json!(r)),
                        repr(v),
                        near.map(|n| format!(" (did you mean {n}?)"))
                            .unwrap_or_default()
                    ));
                    return (0.0, 0.0, None);
                };
                let m = table.get(&r).unwrap();
                match m.get(&pin) {
                    Some(e) => (e.0, e.1, Some(e.2)),
                    None => {
                        let mut avail: Vec<String> = m.keys().to_vec();
                        avail.sort();
                        avail.truncate(30);
                        errors.push(format!(
                            "{ctx}: part {r} has no pin {} (available: {})",
                            repr(&json!(pin)),
                            avail.join(", ")
                        ));
                        (0.0, 0.0, Some((0, 0)))
                    }
                }
            } else if let Some(a) = v.as_array().filter(|a| a.len() == 2) {
                match (a[0].as_f64(), a[1].as_f64()) {
                    (Some(x), Some(y)) => (x, y, None),
                    _ => {
                        errors.push(format!("{ctx}: bad point {}", repr(v)));
                        (0.0, 0.0, None)
                    }
                }
            } else {
                errors.push(format!("{ctx}: bad point {}", repr(v)));
                (0.0, 0.0, None)
            }
        };

    let obj = d.as_object_mut().unwrap();
    let mut extra_wires: Vec<Value> = Vec::new();

    let old_wires = obj
        .get("wires")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut wires: Vec<Value> = Vec::new();
    for (wi, w) in old_wires.iter().enumerate() {
        match w.as_array() {
            Some(pts) if pts.len() >= 2 => {
                let ctx = format!("wire {wi}");
                wires.push(Value::Array(
                    pts.iter()
                        .map(|p| {
                            let (x, y, _) = resolve(&mut errors, p, &ctx);
                            json!([x, y])
                        })
                        .collect(),
                ));
            }
            _ => errors.push(format!("wire {wi}: needs at least 2 points")),
        }
    }

    for key in ["labels", "power", "texts"] {
        let items = obj
            .get(key)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out: Vec<Value> = Vec::new();
        for (i, it) in items.into_iter().enumerate() {
            let mut m = it.as_object().cloned().unwrap_or_default();
            let ctx = format!("{key}[{i}]");
            if m.contains_key("pin") && !m.contains_key("at") {
                let (mut x, mut y, dirv) = resolve(&mut errors, &m["pin"].clone(), &ctx);
                let default_stub = match key {
                    "labels" => 3,
                    "power" => 2,
                    _ => 0,
                };
                let stub = match m.get("stub") {
                    Some(v) => v.as_f64().map(|f| f as i32).unwrap_or(0),
                    None => default_stub,
                };
                if let Some(dv) = dirv
                    && stub != 0
                    && dv != (0, 0)
                {
                    let (ex, ey) = (x + dv.0 as f64 * stub as f64, y + dv.1 as f64 * stub as f64);
                    extra_wires.push(json!([[x, y], [ex, ey]]));
                    x = ex;
                    y = ey;
                }
                m.insert("at".into(), json!([x, y]));
                if key == "labels"
                    && !m.contains_key("rot")
                    && let Some(dv) = dirv
                    && dv != (0, 0)
                {
                    let rot = match dv {
                        (1, 0) => 0,
                        (-1, 0) => 180,
                        (0, -1) => 90,
                        _ => 270,
                    };
                    m.insert("rot".into(), json!(rot));
                }
            } else if m.contains_key("at") {
                let (x, y, _) = resolve(&mut errors, &m["at"].clone(), &ctx);
                m.insert("at".into(), json!([x, y]));
            }
            out.push(Value::Object(m));
        }
        obj.insert(key.into(), Value::Array(out));
    }

    let old_nc = obj
        .get("nc")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let nc: Vec<Value> = old_nc
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (x, y, _) = resolve(&mut errors, p, &format!("nc[{i}]"));
            json!([x, y])
        })
        .collect();

    wires.extend(extra_wires);

    // snap everything to half grid units (0.635 mm) so existing off-grid-but-valid geometry
    // survives; new designs use integers
    fn h(v: f64) -> Value {
        let r = round_py(v * 2.0, 0) / 2.0;
        if r == r.trunc() {
            json!(r as i64)
        } else {
            json!(r)
        }
    }
    fn snap_pt(p: &Value) -> Value {
        let a = p.as_array().cloned().unwrap_or_default();
        json!([
            h(a.first().and_then(|v| v.as_f64()).unwrap_or(0.0)),
            h(a.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0))
        ])
    }
    for key in ["parts", "power", "labels"] {
        let items = obj
            .get(key)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let out: Vec<Value> = items
            .into_iter()
            .map(|it| {
                let mut m = it.as_object().cloned().unwrap_or_default();
                if let Some(at) = m.get("at").cloned() {
                    m.insert("at".into(), snap_pt(&at));
                }
                Value::Object(m)
            })
            .collect();
        obj.insert(key.into(), Value::Array(out));
    }
    // drop zero-length wires / duplicate consecutive points
    let mut cleaned: Vec<Value> = Vec::new();
    for w in &wires {
        let Some(pts) = w.as_array() else { continue };
        if pts.len() < 2 {
            continue;
        }
        let snapped: Vec<Value> = pts.iter().map(snap_pt).collect();
        let mut out = vec![snapped[0].clone()];
        for p in &snapped[1..] {
            if *p != *out.last().unwrap() {
                out.push(p.clone());
            }
        }
        if out.len() >= 2 {
            cleaned.push(Value::Array(out));
        }
    }
    obj.insert("wires".into(), Value::Array(cleaned));
    obj.insert("nc".into(), Value::Array(nc.iter().map(snap_pt).collect()));
    errors
}

// ---------------------------------------------------------------- geometry / connectivity

fn overlap(a: [f64; 4], b: [f64; 4], shrink: f64) -> bool {
    !(a[2] - shrink <= b[0] + shrink
        || b[2] - shrink <= a[0] + shrink
        || a[3] - shrink <= b[1] + shrink
        || b[3] - shrink <= a[1] + shrink)
}

fn seg_crosses_box(a: (f64, f64), b: (f64, f64), bb: [f64; 4], eps: f64) -> bool {
    let (x0, y0, x1, y1) = (bb[0] + eps, bb[1] + eps, bb[2] - eps, bb[3] - eps);
    if x0 >= x1 || y0 >= y1 {
        return false;
    }
    if (a.0 - b.0).abs() < 0.01 {
        return x0 < a.0 && a.0 < x1 && a.1.max(b.1) > y0 && a.1.min(b.1) < y1;
    }
    if (a.1 - b.1).abs() < 0.01 {
        return y0 < a.1 && a.1 < y1 && a.0.max(b.0) > x0 && a.0.min(b.0) < x1;
    }
    false
}

/// True when the axis-aligned segments cross at an interior point of both (an X, not a T).
fn segments_cross(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    const EPS: f64 = 0.05;
    let va = (a.0 - b.0).abs() < EPS;
    let vc = (c.0 - d.0).abs() < EPS;
    if va == vc {
        return false;
    }
    let (a, b, c, d) = if va { (a, b, c, d) } else { (c, d, a, b) };
    let (x, y) = (a.0, c.1);
    a.1.min(b.1) + EPS < y
        && y < a.1.max(b.1) - EPS
        && c.0.min(d.0) + EPS < x
        && x < c.0.max(d.0) - EPS
}

type TextBox = (String, String, [f64; 4], (f64, f64));

pub struct Checker<'a> {
    pub comp: &'a Compiler,
    pub issues: Vec<String>,
    pub warnings: Vec<String>,
    /// net name -> pin members (`"U1.12"`), in discovery order.
    pub nets: Vec<(String, Vec<String>)>,
}

impl<'a> Checker<'a> {
    pub fn new(comp: &'a Compiler) -> Checker<'a> {
        Checker {
            comp,
            issues: Vec::new(),
            warnings: Vec::new(),
            nets: Vec::new(),
        }
    }

    fn des(&self) -> &Design {
        &self.comp.des
    }

    /// Transformed bbox in mm (schematic coords).
    fn bbox(&self, p: &Part, body_only: bool) -> [f64; 4] {
        let Some(info) = self.comp.resolve_lib(&p.lib, &p.lib_name) else {
            return [0.0; 4];
        };
        let bb = if body_only {
            info.body_for(p.unit)
        } else {
            info.bbox_for(p.unit)
        };
        let pts = [
            rot_point(bb.0, bb.1, p.rot, &p.mirror),
            rot_point(bb.2, bb.3, p.rot, &p.mirror),
            rot_point(bb.0, bb.3, p.rot, &p.mirror),
            rot_point(bb.2, bb.1, p.rot, &p.mirror),
        ];
        let xs: Vec<f64> = pts.iter().map(|q| p.at[0] + q.0).collect();
        let ys: Vec<f64> = pts.iter().map(|q| p.at[1] + q.1).collect();
        [
            xs.iter().cloned().fold(f64::INFINITY, f64::min),
            ys.iter().cloned().fold(f64::INFINITY, f64::min),
            xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        ]
    }

    pub fn run(&mut self) {
        let des_pins = self.comp.all_pin_positions();
        let pin_at: HashMap<Key, usize> = des_pins
            .iter()
            .enumerate()
            .map(|(i, (k, _))| (*k, i))
            .collect();
        let segs: Vec<(Key, Key)> = self
            .des()
            .wires
            .iter()
            .flat_map(|w| {
                w.windows(2)
                    .map(|ab| (Key::of(ab[0][0], ab[0][1]), Key::of(ab[1][0], ab[1][1])))
            })
            .collect();
        let mut wire_pts: HashMap<Key, i32> = HashMap::new();
        for (a, b) in &segs {
            *wire_pts.entry(*a).or_insert(0) += 1;
            *wire_pts.entry(*b).or_insert(0) += 1;
        }
        let mut label_order: Vec<Key> = Vec::new();
        let mut label_pts: HashMap<Key, usize> = HashMap::new();
        for (i, l) in self.des().labels.iter().enumerate() {
            let k = Key::of(l.at[0], l.at[1]);
            if label_pts.insert(k, i).is_none() {
                label_order.push(k);
            }
        }
        let nc_pts: BTreeSet<Key> = self.des().nc.iter().map(|p| Key::of(p[0], p[1])).collect();

        let on_seg = |pt: Key| {
            segs.iter()
                .any(|(a, b)| on_segment(pt.xy(), a.xy(), b.xy(), 0.01))
        };
        let touches_wire = |pt: Key| wire_pts.contains_key(&pt) || on_seg(pt);

        // paper bounds
        let (w_mm, h_mm) = paper_mm(&self.des().paper);
        let parts = self.des().parts.clone();
        for p in &parts {
            let b = self.bbox(p, false);
            if b[0] < 10.0 || b[1] < 10.0 || b[2] > w_mm - 10.0 || b[3] > h_mm - 25.0 {
                self.issues.push(format!(
                    "{} at {} extends outside the drawable page area (keep x in 10..{}, y in 10..{} grid units)",
                    p.id,
                    fmt_pt(p.at[0], p.at[1]),
                    ((w_mm - 10.0) / GRID) as i64,
                    ((h_mm - 25.0) / GRID) as i64
                ));
            }
        }
        let mut wire_end_list: Vec<Key> = Vec::new();
        for w in &self.des().wires {
            wire_end_list.push(Key::of(w[0][0], w[0][1]));
            wire_end_list.push(Key::of(w[w.len() - 1][0], w[w.len() - 1][1]));
        }
        let nearest = |pt: Key, cands: &[Key]| -> Option<Key> {
            let (mut best, mut bd) = (None, 6.0 * GRID);
            let (px, py) = pt.xy();
            for c in cands {
                let (cx, cy) = c.xy();
                let d = (cx - px).abs() + (cy - py).abs();
                if d > 0.0 && d < bd {
                    best = Some(*c);
                    bd = d;
                }
            }
            best
        };

        // unconnected pins
        for (pt, lst) in &des_pins {
            for (r, num, et) in lst {
                if r.starts_with('#') || et == "no_connect" {
                    continue;
                }
                let connected = touches_wire(*pt) || label_pts.contains_key(pt) || lst.len() > 1;
                if nc_pts.contains(pt) {
                    if connected {
                        let (x, y) = pt.xy();
                        self.issues.push(format!(
                            "{r}.{num} at {} has both a no-connect flag and a connection",
                            fmt_pt(x, y)
                        ));
                    }
                    continue;
                }
                if !connected {
                    let (x, y) = pt.xy();
                    let hint = match nearest(*pt, &wire_end_list) {
                        Some(ne) => {
                            let (nx, ny) = ne.xy();
                            format!(
                                "; a wire end at {} is nearby - probably meant to reach this pin: \
                                 use \"{r}.{num}\" as the wire point",
                                fmt_pt(nx, ny)
                            )
                        }
                        None => " (add a wire/label/power symbol at exactly this point, or an nc marker)".into(),
                    };
                    self.issues.push(format!(
                        "pin {r}.{num} ({et}) at {} is not connected to anything{hint}",
                        fmt_pt(x, y)
                    ));
                }
            }
        }
        // dangling wire ends
        let pin_keys: Vec<Key> = des_pins.iter().map(|(k, _)| *k).collect();
        for w in &self.des().wires.clone() {
            for end in [w[0], w[w.len() - 1]] {
                let pt = Key::of(end[0], end[1]);
                if pin_at.contains_key(&pt) || label_pts.contains_key(&pt) || nc_pts.contains(&pt) {
                    continue;
                }
                if wire_pts.get(&pt).copied().unwrap_or(0) >= 2 || on_seg(pt) {
                    continue;
                }
                let (x, y) = pt.xy();
                let hint = match nearest(pt, &pin_keys) {
                    Some(np) => {
                        let lst = &des_pins[pin_at[&np]].1;
                        let names: Vec<String> =
                            lst.iter().map(|(r, n, _)| format!("{r}.{n}")).collect();
                        let (nx, ny) = np.xy();
                        format!(
                            "; nearest pin {} is at {} - use \"{}\" as the wire point",
                            names.join(", "),
                            fmt_pt(nx, ny),
                            names[0]
                        )
                    }
                    None => String::new(),
                };
                self.issues.push(format!(
                    "wire end at {} is dangling (connects to nothing){hint}",
                    fmt_pt(x, y)
                ));
            }
        }
        // labels must sit on a wire end or pin
        for pt in &label_order {
            if pin_at.contains_key(pt) || touches_wire(*pt) {
                continue;
            }
            let l = &self.des().labels[label_pts[pt]];
            let (x, y) = pt.xy();
            self.issues.push(format!(
                "label '{}' at {} is floating - it must sit exactly on a wire end or pin",
                l.text,
                fmt_pt(x, y)
            ));
        }
        for pw in &self.des().power.clone() {
            let pt = Key::of(pw.at[0], pw.at[1]);
            let n = pin_at.get(&pt).map(|i| des_pins[*i].1.len()).unwrap_or(0);
            if n > 1 || touches_wire(pt) || label_pts.contains_key(&pt) {
                continue;
            }
            let (x, y) = pt.xy();
            self.issues.push(format!(
                "power symbol {} at {} is not connected to anything",
                pw.net,
                fmt_pt(x, y)
            ));
        }
        for pt in &nc_pts {
            if !pin_at.contains_key(pt) {
                let (x, y) = pt.xy();
                self.issues.push(format!(
                    "no-connect marker at {} is not on a pin",
                    fmt_pt(x, y)
                ));
            }
        }
        // diagonal wires
        for (a, b) in &segs {
            let (ax, ay) = a.xy();
            let (bx, by) = b.xy();
            if (ax - bx).abs() > 0.01 && (ay - by).abs() > 0.01 {
                self.warnings.push(format!(
                    "diagonal wire {}-{}: use only horizontal/vertical segments",
                    fmt_pt(ax, ay),
                    fmt_pt(bx, by)
                ));
            }
        }
        // symbol overlaps (full extents incl. pins)
        let mut allb: Vec<(String, [f64; 4], [f64; 4])> = Vec::new();
        for p in &parts {
            allb.push((p.id.clone(), self.bbox(p, false), self.bbox(p, true)));
        }
        for pw in &self.des().power.clone() {
            let Some(info) = self.comp.resolve_lib(&pw.lib, "") else {
                continue;
            };
            if pw.net == "PWR_FLAG" {
                continue;
            }
            let id = if pw.reference.is_empty() {
                pw.net.clone()
            } else {
                pw.reference.clone()
            };
            let mut fake = Part {
                id: id.clone(),
                lib: pw.lib.clone(),
                at: [pw.at[0], pw.at[1]],
                rot: pw.rot,
                mirror: pw.mirror.clone(),
                unit: 1,
                ..Default::default()
            };
            if let Some(pin) = info.pins.first() {
                let (dx, dy) = rot_point(pin.x, pin.y, pw.rot, &pw.mirror);
                fake.at = [pw.at[0] - dx, pw.at[1] - dy];
            }
            let full = self.bbox(&fake, true);
            allb.push((id, full, self.bbox(&fake, true)));
        }
        for i in 0..allb.len() {
            for j in i + 1..allb.len() {
                let (ref ia, a, ba) = allb[i];
                let (ref ib, b, bb_) = allb[j];
                // bodies may not overlap, and no body may overlap the other's pins; pin tips touching is fine
                if overlap(ba, bb_, 0.3) || overlap(ba, b, 0.3) || overlap(a, bb_, 0.3) {
                    self.issues.push(format!(
                        "{ia} (x {}..{}, y {}..{}) and {ib} (x {}..{}, y {}..{}) overlap - move them apart",
                        g(a[0]),
                        g(a[2]),
                        g(a[1]),
                        g(a[3]),
                        g(b[0]),
                        g(b[2]),
                        g(b[1]),
                        g(b[3])
                    ));
                }
            }
        }
        // wires crossing symbol bodies
        let bodies: Vec<(String, [f64; 4])> = parts
            .iter()
            .map(|p| (p.id.clone(), self.bbox(p, true)))
            .collect();
        for (a, b) in &segs {
            for (id, bb) in &bodies {
                if seg_crosses_box(a.xy(), b.xy(), *bb, 1.0) {
                    let (ax, ay) = a.xy();
                    let (bx, by) = b.xy();
                    self.issues.push(format!(
                        "wire {}-{} passes through the body of {id} (body x {}..{}, y {}..{})",
                        fmt_pt(ax, ay),
                        fmt_pt(bx, by),
                        g(bb[0]),
                        g(bb[2]),
                        g(bb[1]),
                        g(bb[3])
                    ));
                    break;
                }
            }
        }
        // labels overlapping symbol bodies (rough: label text extends from its anchor)
        for pt in &label_order {
            let l = self.des().labels[label_pts[pt]].clone();
            let (x, y) = pt.xy();
            let len = 1.27 * 0.9 * l.text.chars().count() as f64 + 1.5;
            let lb = if l.rot == 0 || l.rot == 180 {
                [
                    x - if l.rot == 180 { len } else { 0.0 },
                    y - 1.6,
                    x + if l.rot == 180 { 0.0 } else { len },
                    y + 0.2,
                ]
            } else {
                [
                    x - 1.6,
                    y - if l.rot == 90 { len } else { 0.0 },
                    x + 0.2,
                    y + if l.rot == 90 { 0.0 } else { len },
                ]
            };
            for (id, bb) in &bodies {
                if overlap(lb, *bb, 0.2) {
                    self.warnings.push(format!(
                        "label '{}' at {} (rot {}) overlaps the body of {id}; rotate it to point away from the symbol or move it",
                        l.text,
                        fmt_pt(x, y),
                        l.rot
                    ));
                    break;
                }
            }
        }
        // ---- text collisions (readability) ----
        self.text_checks(&bodies, &segs, &des_pins);
        // net connectivity
        self.nets = self.netlist(&des_pins, &segs, &label_pts, &label_order);
        let nets = self.nets.clone();
        for (name, members) in &nets {
            let real: Vec<&String> = members.iter().filter(|m| !m.starts_with('#')).collect();
            if real.len() == 1 && name.starts_with("N$") {
                self.warnings.push(format!(
                    "net with single pin {} (wire leads nowhere)",
                    real[0]
                ));
            }
        }
        // label used only once (typo?)
        let mut order: Vec<(String, String)> = Vec::new();
        let mut cnt: HashMap<(String, String), i32> = HashMap::new();
        for l in &self.des().labels {
            let k = (l.kind.clone(), l.text.clone());
            if !cnt.contains_key(&k) {
                order.push(k.clone());
            }
            *cnt.entry(k).or_insert(0) += 1;
        }
        for (t, txt) in order {
            if t == "local" && cnt[&(t.clone(), txt.clone())] == 1 {
                self.warnings.push(format!(
                    "net label '{txt}' appears only once - if it is meant to connect elsewhere, add the matching label; \
                     if it is a port/net name for documentation, that's fine"
                ));
            }
        }
    }

    /// Approximate bounding boxes (mm) of all visible texts: labels, ref/value, power names, notes.
    fn text_boxes(&self) -> Vec<TextBox> {
        const CW: f64 = 0.95;
        const CH: f64 = 1.5;
        fn box_from(x: f64, y: f64, rot: i32, justify: &str, w: f64, h: f64) -> [f64; 4] {
            let mut hj = "center";
            let mut vj = "center";
            for j in justify.split_whitespace() {
                if j == "left" || j == "right" {
                    hj = if j == "left" { "left" } else { "right" };
                }
                if j == "top" || j == "bottom" {
                    vj = if j == "top" { "top" } else { "bottom" };
                }
            }
            if rot.rem_euclid(180) == 90 {
                let (w, h) = (h, w);
                // for vertical text KiCad's left/right refer to the reading direction
                let (y0, y1) = match hj {
                    "left" => (y - h, y),
                    "right" => (y, y + h),
                    _ => (y - h / 2.0, y + h / 2.0),
                };
                let (x0, x1) = match vj {
                    "bottom" => (x - w, x),
                    "top" => (x, x + w),
                    _ => (x - w / 2.0, x + w / 2.0),
                };
                return [x0, y0, x1, y1];
            }
            let (x0, x1) = match hj {
                "left" => (x, x + w),
                "right" => (x - w, x),
                _ => (x - w / 2.0, x + w / 2.0),
            };
            let (y0, y1) = match vj {
                "bottom" => (y - h, y),
                "top" => (y, y + h),
                _ => (y - h / 2.0, y + h / 2.0),
            };
            [x0, y0, x1, y1]
        }
        let mut boxes: Vec<TextBox> = Vec::new();
        for t in self.comp.text_items.borrow().iter() {
            let (w, h) = (CW * t.size * t.text.chars().count() as f64, CH * t.size);
            boxes.push((
                t.owner.clone(),
                format!("{} text '{}' of {}", t.kind, t.text, t.owner),
                box_from(t.x, t.y, t.rot, &t.justify, w, h),
                (t.x, t.y),
            ));
        }
        for l in &self.des().labels {
            let w = CW * 1.27 * l.text.chars().count() as f64
                + if l.kind != "local" { 3.0 } else { 0.6 };
            let h = CH * 1.27;
            let (x, y) = (l.at[0], l.at[1]);
            let bb = if l.kind == "local" {
                // text sits above the anchor line, extending in the rot direction
                match l.rot {
                    0 => [x, y - h, x + w, y],
                    180 => [x - w, y - h, x, y],
                    90 => [x - h, y - w, x, y],
                    _ => [x - h, y, x, y + w],
                }
            } else {
                match l.rot {
                    0 => [x, y - h / 2.0, x + w, y + h / 2.0],
                    180 => [x - w, y - h / 2.0, x, y + h / 2.0],
                    90 => [x - h / 2.0, y - w, x + h / 2.0, y],
                    _ => [x - h / 2.0, y, x + h / 2.0, y + w],
                }
            };
            boxes.push((
                format!("label:{}", l.text),
                format!("label '{}'", l.text),
                bb,
                (x, y),
            ));
        }
        for t in &self.des().texts {
            let lines: Vec<&str> = t.text.split('\n').collect();
            let w = CW * t.size * lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f64;
            let h = CH * t.size * lines.len() as f64;
            let head: String = t.text.chars().take(20).collect();
            let head25: String = t.text.chars().take(25).collect();
            boxes.push((
                format!("text:{head}"),
                format!("note '{head25}'"),
                box_from(t.at[0], t.at[1], t.rot, &t.justify, w, h),
                (t.at[0], t.at[1]),
            ));
        }
        boxes
    }

    fn text_checks(&mut self, bodies: &[(String, [f64; 4])], segs: &[(Key, Key)], pins: &PinsAt) {
        let boxes = self.text_boxes();
        for (owner, desc, bb, anchor) in &boxes {
            for (id, body) in bodies {
                if owner == id {
                    continue;
                }
                if overlap(*bb, *body, 0.25) {
                    self.issues.push(format!(
                        "{desc} at {} overlaps the body of {id} - move/rotate it",
                        fmt_pt(anchor.0, anchor.1)
                    ));
                    break;
                }
            }
        }
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        for i in 0..boxes.len() {
            for j in i + 1..boxes.len() {
                let (a, b) = (&boxes[i], &boxes[j]);
                if a.0 == b.0 {
                    continue;
                }
                if overlap(a.2, b.2, 0.3) {
                    let key = (a.1.clone(), b.1.clone());
                    if !seen.insert(key) {
                        continue;
                    }
                    self.issues.push(format!(
                        "{} at {} overlaps {} at {} - texts must not overlap",
                        a.1,
                        fmt_pt(a.3.0, a.3.1),
                        b.1,
                        fmt_pt(b.3.0, b.3.1)
                    ));
                }
            }
        }
        // texts crossed by wires (excluding wires touching the text's own anchor / owner pins)
        let mut own_pts: HashMap<String, BTreeSet<Key>> = HashMap::new();
        for (pt, lst) in pins {
            for (r, _, _) in lst {
                own_pts.entry(r.clone()).or_default().insert(*pt);
            }
        }
        let mut n = 0;
        for (owner, desc, bb, anchor) in &boxes {
            let ak = Key::of(anchor.0, anchor.1);
            let own = own_pts.get(owner);
            for (a, b) in segs {
                if *a == ak
                    || *b == ak
                    || own.map(|s| s.contains(a) || s.contains(b)).unwrap_or(false)
                {
                    continue;
                }
                if seg_crosses_box(a.xy(), b.xy(), *bb, 0.2) {
                    n += 1;
                    if n <= 12 {
                        let (ax, ay) = a.xy();
                        let (bx, by) = b.xy();
                        self.warnings.push(format!(
                            "wire {}-{} runs through {desc} at {}",
                            fmt_pt(ax, ay),
                            fmt_pt(bx, by),
                            fmt_pt(anchor.0, anchor.1)
                        ));
                    }
                    break;
                }
            }
        }
        // wire-wire crossings (not connections)
        let mut cross = 0;
        for i in 0..segs.len() {
            for j in i + 1..segs.len() {
                if segments_cross(
                    segs[i].0.xy(),
                    segs[i].1.xy(),
                    segs[j].0.xy(),
                    segs[j].1.xy(),
                ) {
                    cross += 1;
                }
            }
        }
        if cross >= 3 {
            self.warnings
                .push(format!("{cross} wire-wire crossings without connection - reduce by rerouting or using net labels"));
        }
    }

    fn netlist(
        &mut self,
        pins: &PinsAt,
        segs: &[(Key, Key)],
        label_pts: &HashMap<Key, usize>,
        label_order: &[Key],
    ) -> Vec<(String, Vec<String>)> {
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        enum Node {
            Pt(Key),
            Pin(String, String),
            Net(String),
        }
        let mut order: Vec<Node> = Vec::new();
        let mut parent: HashMap<Node, Node> = HashMap::new();
        /// Union-find with path halving; a new node is appended to `order`, mirroring the
        /// insertion order of Python's `parent` dict (which decides the `N$n` numbering).
        fn find(parent: &mut HashMap<Node, Node>, order: &mut Vec<Node>, mut x: Node) -> Node {
            if !parent.contains_key(&x) {
                order.push(x.clone());
                parent.insert(x.clone(), x.clone());
                return x;
            }
            while parent[&x] != x {
                let grand = parent[&parent[&x]].clone();
                parent.insert(x.clone(), grand.clone());
                x = grand;
            }
            x
        }
        let union = |parent: &mut HashMap<Node, Node>, order: &mut Vec<Node>, a: Node, b: Node| {
            let ra = find(parent, order, a);
            let rb = find(parent, order, b);
            parent.insert(ra, rb);
        };

        // `pts` mirrors Python's `set(pins) | set(label_pts)` plus the segment endpoints
        let mut pts: Vec<Key> = Vec::new();
        let mut seen: BTreeSet<Key> = BTreeSet::new();
        for (k, _) in pins {
            if seen.insert(*k) {
                pts.push(*k);
            }
        }
        for k in label_order {
            if seen.insert(*k) {
                pts.push(*k);
            }
        }
        for (a, b) in segs {
            union(&mut parent, &mut order, Node::Pt(*a), Node::Pt(*b));
            for k in [a, b] {
                if seen.insert(*k) {
                    pts.push(*k);
                }
            }
        }
        for pt in &pts {
            for (a, b) in segs {
                if on_segment(pt.xy(), a.xy(), b.xy(), 0.01) {
                    union(&mut parent, &mut order, Node::Pt(*pt), Node::Pt(*a));
                }
            }
        }
        let ncset: BTreeSet<Key> = self.des().nc.iter().map(|p| Key::of(p[0], p[1])).collect();
        for (pt, lst) in pins {
            if ncset.contains(pt) {
                continue;
            }
            for (r, num, _) in lst {
                union(
                    &mut parent,
                    &mut order,
                    Node::Pt(*pt),
                    Node::Pin(r.clone(), num.clone()),
                );
            }
        }
        for pt in label_order {
            let l = &self.des().labels[label_pts[pt]];
            union(
                &mut parent,
                &mut order,
                Node::Pt(*pt),
                Node::Net(l.text.clone()),
            );
        }
        for pw in &self.des().power.clone() {
            if pw.net == "PWR_FLAG" {
                continue;
            }
            union(
                &mut parent,
                &mut order,
                Node::Pt(Key::of(pw.at[0], pw.at[1])),
                Node::Net(pw.net.clone()),
            );
        }

        let mut group_order: Vec<Node> = Vec::new();
        let mut groups: HashMap<Node, Vec<Node>> = HashMap::new();
        for k in order.clone() {
            let r = find(&mut parent, &mut order, k.clone());
            if !groups.contains_key(&r) {
                group_order.push(r.clone());
            }
            groups.entry(r).or_default().push(k);
        }
        let mut nets: Vec<(String, Vec<String>)> = Vec::new();
        let mut n = 0;
        for r in group_order {
            let gr = &groups[&r];
            let mut names: Vec<String> = gr
                .iter()
                .filter_map(|k| {
                    if let Node::Net(s) = k {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect();
            names.sort();
            let mut members: Vec<String> = gr
                .iter()
                .filter_map(|k| match k {
                    Node::Pin(r, num) if !r.starts_with('#') => Some(format!("{r}.{num}")),
                    _ => None,
                })
                .collect();
            members.sort();
            if members.is_empty() {
                continue;
            }
            let name = if !names.is_empty() {
                if names.len() > 1 {
                    self.warnings.push(format!(
                        "nets [{}] are shorted together",
                        names
                            .iter()
                            .map(|s| format!("'{s}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                names[0].clone()
            } else {
                n += 1;
                format!("N${n}")
            };
            nets.push((name, members));
        }
        nets
    }

    pub fn netlist_text(&self, limit: usize) -> String {
        let mut items = self.nets.clone();
        items.sort_by(|a, b| (a.0.starts_with("N$"), &a.0).cmp(&(b.0.starts_with("N$"), &b.0)));
        let mut lines: Vec<String> = items
            .iter()
            .map(|(name, members)| format!("{name}: {}", members.join(" ")))
            .collect();
        if lines.len() > limit {
            let more = self.nets.len() - limit;
            lines.truncate(limit);
            lines.push(format!("... ({more} more nets)"));
        }
        lines.join("\n")
    }

    /// `net -> {"U1.12", ...}` for [`crate::BuildReport`].
    pub fn netlist_map(&self) -> BTreeMap<String, BTreeSet<String>> {
        self.nets
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
            .collect()
    }
}

// ---------------------------------------------------------------- ERC

/// KiCad ERC violations of a sheet as text lines, ignoring the kinds in [`IGNORED_ERC`].
pub fn run_erc(kicad_cli: &Path, sch: &Path) -> Result<Vec<String>> {
    let dir = tempfile::tempdir()?;
    let rep = dir.path().join("erc.json");
    let out = std::process::Command::new(kicad_cli)
        .args(["sch", "erc", "-o"])
        .arg(&rep)
        .args(["--format", "json", "--severity-all"])
        .arg(sch)
        .output()
        .with_context(|| format!("running {}", kicad_cli.display()))?;
    if !rep.exists() {
        anyhow::bail!(
            "erc failed: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let data: Value = serde_json::from_str(&std::fs::read_to_string(&rep)?)?;
    let mut errors: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    for sh in data
        .get("sheets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        for v in sh
            .get("violations")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if IGNORED_ERC.contains(&kind) {
                continue;
            }
            let sev = v.get("severity").and_then(|t| t.as_str()).unwrap_or("");
            let desc = v.get("description").and_then(|t| t.as_str()).unwrap_or("");
            let items: Vec<String> = v
                .get("items")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .map(|it| {
                            let d = it.get("description").and_then(|t| t.as_str()).unwrap_or("");
                            let pos = it.get("pos");
                            let x = pos
                                .and_then(|p| p.get("x"))
                                .map(|v| v.to_string())
                                .unwrap_or("?".into());
                            let y = pos
                                .and_then(|p| p.get("y"))
                                .map(|v| v.to_string())
                                .unwrap_or("?".into());
                            erc_item(&format!("{d} @({x},{y})"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let line = format!("[{sev}] {kind}: {desc} -- {}", items.join("; "));
            if sev == "error" {
                errors.push(line)
            } else {
                rest.push(line)
            }
        }
    }
    errors.extend(rest);
    Ok(errors)
}

/// Rewrite the `@(x,y)` coordinates KiCad reports into grid units.
pub fn erc_item(it: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"@\(([-\d.]+),([-\d.]+)\)").unwrap());
    re.replace_all(it, |c: &regex::Captures| {
        match (c[1].parse::<f64>(), c[2].parse::<f64>()) {
            (Ok(x), Ok(y)) => format!("@({},{})", g(x * 100.0), g(y * 100.0)),
            _ => c[0].to_string(),
        }
    })
    .into_owned()
}

// ---------------------------------------------------------------- edit-mode helpers

/// Add `"touches": ["C1.2", "label:RST", "power:GND", "w4"]` to every id-tagged wire.
pub fn annotate_wires(d: &mut Value, base: Option<&Design>) {
    let mut plain = crate::model::normalize_json(d);
    resolve_pin_refs(&mut plain, base);
    let mut table = pin_lookup_table(&plain);
    let infos = lib_infos_from_base(base);
    for pj in parts_of(&plain) {
        let key = part_field(&pj, "lib_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                part_field(&pj, "lib")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            });
        if !table.contains(&part_id(&pj))
            && let Some(info) = infos.get(&key)
        {
            let (ax, ay) = part_at(&pj);
            table.insert(
                part_id(&pj),
                pin_map(
                    info,
                    part_unit(&pj),
                    part_rot(&pj),
                    &part_mirror(&pj),
                    ax,
                    ay,
                ),
            );
        }
    }
    let mut at_pt: HashMap<Key, Vec<String>> = HashMap::new();
    for r in table.refs() {
        let m = table.get(r).unwrap();
        let mut seen: BTreeSet<Key> = BTreeSet::new();
        for k in m.keys() {
            let v = m.get(k).unwrap();
            let key = Key::of(v.0, v.1);
            if !seen.insert(key) {
                continue;
            }
            at_pt.entry(key).or_default().push(format!("{r}.{k}"));
        }
    }
    for it in d
        .get("labels")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let at = it
            .get("at")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let (x, y) = (
            at.first().and_then(|v| v.as_f64()).unwrap_or(0.0),
            at.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
        );
        at_pt
            .entry(Key::of(x, y))
            .or_default()
            .push(format!("label:{}", it["text"].as_str().unwrap_or("")));
    }
    for it in d
        .get("power")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let at = it
            .get("at")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let (x, y) = (
            at.first().and_then(|v| v.as_f64()).unwrap_or(0.0),
            at.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
        );
        at_pt
            .entry(Key::of(x, y))
            .or_default()
            .push(format!("power:{}", it["net"].as_str().unwrap_or("")));
    }
    let wires = d
        .get("wires")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut ends: HashMap<Key, Vec<String>> = HashMap::new();
    for w in &wires {
        if let Some(o) = w.as_object() {
            let pts = o["pts"].as_array().cloned().unwrap_or_default();
            for pt in [pts.first(), pts.last()].into_iter().flatten() {
                let a = pt.as_array().cloned().unwrap_or_default();
                let k = Key::of(
                    a.first().and_then(|v| v.as_f64()).unwrap_or(0.0),
                    a.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
                );
                ends.entry(k)
                    .or_default()
                    .push(o["id"].as_str().unwrap_or("").to_string());
            }
        }
    }
    let mut out: Vec<Value> = Vec::new();
    for w in wires {
        let Some(o) = w.as_object() else {
            out.push(w);
            continue;
        };
        let mut o = o.clone();
        let id = o["id"].as_str().unwrap_or("").to_string();
        let pts = o["pts"].as_array().cloned().unwrap_or_default();
        let mut touches: BTreeSet<String> = BTreeSet::new();
        for pt in [pts.first(), pts.last()].into_iter().flatten() {
            let a = pt.as_array().cloned().unwrap_or_default();
            let k = Key::of(
                a.first().and_then(|v| v.as_f64()).unwrap_or(0.0),
                a.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
            );
            touches.extend(at_pt.get(&k).cloned().unwrap_or_default());
            touches.extend(
                ends.get(&k)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|x| *x != id),
            );
        }
        o.insert(
            "touches".into(),
            json!(touches.into_iter().collect::<Vec<_>>()),
        );
        out.push(Value::Object(o));
    }
    d.as_object_mut()
        .unwrap()
        .insert("wires".into(), Value::Array(out));
}

/// Every intended net of a netlist design must come out as exactly one connected net.
pub fn netlist_mismatch(d: &Value, nets: &BTreeMap<String, BTreeSet<String>>) -> Vec<String> {
    let mut intended: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for p in parts_of(d) {
        let lib_id = part_field(&p, "lib").and_then(|v| v.as_str()).unwrap_or("");
        let Some(info) = index().get(lib_id) else {
            continue;
        };
        if info.power || lib_id.starts_with("power:") || lib_id.ends_with(":PWR_FLAG") {
            continue;
        }
        let mut canon: HashMap<String, String> = HashMap::new();
        for pin in &info.pins {
            canon
                .entry(pin.number.clone())
                .or_insert_with(|| pin.number.clone());
            if !pin.name.is_empty() {
                canon
                    .entry(pin.name.clone())
                    .or_insert_with(|| pin.number.clone());
            }
        }
        let pins: Map<String, Value> = part_field(&p, "pins")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        for (num, spec) in pins {
            let spec = match &spec {
                Value::Object(o) => o
                    .get("net")
                    .or_else(|| o.get("power"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                Value::String(s) => s.as_str(),
                _ => "",
            };
            if spec.is_empty() || spec == "nc" || spec == "float" {
                continue;
            }
            let n = canon.get(&num).cloned().unwrap_or(num.clone());
            intended
                .entry(spec.to_string())
                .or_default()
                .insert(format!("{}.{n}", part_id(&p)));
        }
    }
    let mut where_: HashMap<&String, &String> = HashMap::new();
    for (name, members) in nets {
        for m in members {
            where_.insert(m, name);
        }
    }
    let unknown = "?".to_string();
    let mut issues = Vec::new();
    for (net, pins) in &intended {
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        for pin in pins {
            let g = where_.get(pin).copied().unwrap_or(&unknown).clone();
            if !groups.contains_key(&g) {
                order.push(g.clone());
            }
            groups.entry(g).or_default().push(pin.clone());
        }
        if order.len() > 1 {
            let parts: Vec<String> = order
                .iter()
                .map(|g| {
                    let mut v = groups[g].clone();
                    v.sort();
                    v.join(",")
                })
                .collect();
            issues.push(format!(
                "net {net} is split into {} unconnected pieces: {} - add a net label to each piece",
                order.len(),
                parts.join(" | ")
            ));
        }
    }
    issues
}

// ---------------------------------------------------------------- the build pipeline

/// What one compile + check produced.
pub struct Built {
    pub issues: Vec<String>,
    pub warnings: Vec<String>,
    pub nets: BTreeMap<String, BTreeSet<String>>,
    pub netlist_text: String,
    /// The design JSON after pin-reference resolution and grid snapping.
    pub resolved: Value,
    pub design: Design,
}

/// Resolve pin references, compile `raw` to `out_sch` and check it.
///
/// Unresolvable references and unknown symbols come back as issues and nothing is written.
pub fn build(raw: &Value, out_sch: &Path, base: Option<&Design>) -> Result<Built> {
    let mut d = crate::model::normalize_json(raw);
    let mut errs = resolve_pin_refs(&mut d, base);
    let des = crate::model::design_from_json(&d, base);
    let (text, comp) = crate::compile::compile_design(des);
    errs.extend(comp.errors.borrow().iter().cloned());
    if !errs.is_empty() {
        return Ok(Built {
            issues: errs,
            warnings: Vec::new(),
            nets: BTreeMap::new(),
            netlist_text: String::new(),
            resolved: d,
            design: comp.des,
        });
    }
    if let Some(dir) = out_sch.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(out_sch, &text).with_context(|| format!("writing {}", out_sch.display()))?;
    let mut chk = Checker::new(&comp);
    chk.run();
    Ok(Built {
        issues: chk.issues.clone(),
        warnings: chk.warnings.clone(),
        nets: chk.netlist_map(),
        netlist_text: chk.netlist_text(80),
        resolved: d,
        design: comp.des,
    })
}
