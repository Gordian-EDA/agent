//! Design model: the JSON-ish intermediate representation the LLM reads/writes.
//!
//! All coordinates are schematic mm (Y down), snapped to the 1.27 mm grid; the JSON form
//! carries grid units instead.

use serde_json::{Map, Value, json};

pub const GRID: f64 = 1.27;

/// Python's `round()`: half-to-even, unlike Rust's half-away-from-zero.
pub fn round_py(v: f64) -> f64 {
    let r = v.round();
    if (v - v.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        r - v.signum()
    } else {
        r
    }
}

/// Python's `round(v, dp)`: half-to-even on the value's exact decimal expansion.
pub fn round_py_dp(v: f64, dp: i32) -> f64 {
    if !v.is_finite() {
        return v;
    }
    let dp = dp.max(0) as usize;
    let s = format!("{:.*}", dp + 25, v.abs());
    let (int_part, frac) = s.split_once('.').unwrap();
    let (keep, rest) = frac.split_at(dp);
    let mut digits: Vec<u8> = int_part.bytes().chain(keep.bytes()).collect();
    let first = rest.as_bytes()[0];
    let up = first > b'5'
        || (first == b'5'
            && (rest[1..].bytes().any(|c| c != b'0')
                || (digits[digits.len() - 1] - b'0') % 2 == 1));
    if up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let text = String::from_utf8(digits).unwrap();
    let cut = text.len() - dp;
    let out: f64 = format!("{}.{}0", &text[..cut], &text[cut..])
        .parse()
        .unwrap();
    if v < 0.0 { -out } else { out }
}

fn round4(v: f64) -> f64 {
    round_py_dp(v, 4)
}

pub fn snap(v: f64, g: f64) -> f64 {
    round4(round_py(v / g) * g)
}

/// grid units -> mm (snapped to grid).
pub fn mm(u: f64) -> f64 {
    round4(u * GRID)
}

/// mm -> grid units (integral when on grid).
pub fn gu(v: f64) -> f64 {
    let r = round_py_dp(v / GRID, 3);
    if (r - round_py(r)).abs() < 1e-6 {
        r.trunc()
    } else {
        r
    }
}

/// JSON number that stays an integer when the value is integral (matching Python's `gu`).
pub fn num(v: f64) -> Value {
    if v.is_finite() && (v - v.round()).abs() < 1e-9 {
        json!(v.round() as i64)
    } else {
        json!(v)
    }
}

pub fn pt_mm(p: &Value) -> [f64; 2] {
    let a = p.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    [
        mm(a.first().and_then(|v| v.as_f64()).unwrap_or(0.0)),
        mm(a.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0)),
    ]
}

pub fn pt_gu(p: [f64; 2]) -> Value {
    Value::Array(vec![num(gu(p[0])), num(gu(p[1]))])
}

/// `[x, y, ...rest]` in grid units -> mm, rest untouched.
pub fn at_mm(a: Option<&Value>) -> Option<Vec<Value>> {
    let a = a?.as_array()?;
    if a.is_empty() {
        return None;
    }
    let mut out = vec![
        json!(mm(a[0].as_f64().unwrap_or(0.0))),
        json!(mm(a.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0))),
    ];
    out.extend(a.iter().skip(2).cloned());
    Some(out)
}

pub fn at_gu(a: &[Value]) -> Option<Value> {
    if a.is_empty() {
        return None;
    }
    let mut out = vec![
        num(gu(a[0].as_f64().unwrap_or(0.0))),
        num(gu(a.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0))),
    ];
    out.extend(a.iter().skip(2).cloned());
    Some(Value::Array(out))
}

pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Map a point in symbol-library coordinates (Y up) to a schematic-relative offset (Y down)
/// for an instance with the given rotation/mirror.
///
/// Matches KiCad: rotate in lib space, then mirror, then flip Y.
pub fn rot_point(x: f64, y: f64, rot: i32, mirror: &str) -> (f64, f64) {
    let (mut x, mut y) = (x, y);
    match rot.rem_euclid(360) {
        90 => (x, y) = (-y, x),
        180 => (x, y) = (-x, -y),
        270 => (x, y) = (y, -x),
        _ => {}
    }
    match mirror {
        "x" => y = -y,
        "y" => x = -x,
        _ => {}
    }
    (round4(x), round4(-y))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Part {
    /// Reference designator, e.g. `U1`.
    pub id: String,
    /// lib_id, e.g. `Device:R`.
    pub lib: String,
    pub at: [f64; 2],
    pub rot: i32,
    /// "", "x" or "y".
    pub mirror: String,
    pub value: String,
    pub unit: i32,
    pub footprint: String,
    pub dnp: bool,
    /// Extra properties (Datasheet, MPN, ...).
    pub fields: Map<String, Value>,
    /// `[x, y, rot, "justify"]` override for the Reference text.
    pub ref_at: Option<Vec<Value>>,
    pub val_at: Option<Vec<Value>>,
    pub hide_value: bool,
    pub uuid: String,
    /// Local `lib_symbols` key when the instance uses a modified copy.
    pub lib_name: String,
}

impl Part {
    pub fn new(id: impl Into<String>, lib: impl Into<String>, at: [f64; 2]) -> Part {
        Part {
            id: id.into(),
            lib: lib.into(),
            at,
            unit: 1,
            ..Default::default()
        }
    }

    pub fn to_json(&self) -> Value {
        let mut d = Map::new();
        d.insert("id".into(), json!(self.id));
        d.insert("lib".into(), json!(self.lib));
        d.insert("at".into(), pt_gu(self.at));
        d.insert("rot".into(), json!(self.rot));
        if !self.lib_name.is_empty() {
            d.insert("lib_name".into(), json!(self.lib_name));
        }
        if !self.mirror.is_empty() {
            d.insert("mirror".into(), json!(self.mirror));
        }
        if !self.value.is_empty() {
            d.insert("value".into(), json!(self.value));
        }
        if self.unit != 1 {
            d.insert("unit".into(), json!(self.unit));
        }
        if !self.footprint.is_empty() {
            d.insert("footprint".into(), json!(self.footprint));
        }
        if self.dnp {
            d.insert("dnp".into(), json!(true));
        }
        if !self.fields.is_empty() {
            d.insert("fields".into(), Value::Object(self.fields.clone()));
        }
        if let Some(a) = self.ref_at.as_ref().and_then(|a| at_gu(a)) {
            d.insert("ref_at".into(), a);
        }
        if let Some(a) = self.val_at.as_ref().and_then(|a| at_gu(a)) {
            d.insert("val_at".into(), a);
        }
        if self.hide_value {
            d.insert("hide_value".into(), json!(true));
        }
        Value::Object(d)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Power {
    /// "GND", "+3V3", "VBUS" ...
    pub net: String,
    /// Connection point (the pin).
    pub at: [f64; 2],
    pub rot: i32,
    /// e.g. `power:GND`; defaults to `power:<net>`.
    pub lib: String,
    pub reference: String,
    pub uuid: String,
    /// Displayed net name if different from the symbol name.
    pub value: String,
    pub val_at: Option<Vec<Value>>,
    pub mirror: String,
}

impl Power {
    pub fn new(net: impl Into<String>, at: [f64; 2], rot: i32) -> Power {
        let net = net.into();
        Power {
            lib: format!("power:{net}"),
            net,
            at,
            rot,
            ..Default::default()
        }
    }

    pub fn to_json(&self) -> Value {
        let mut d = Map::new();
        d.insert("net".into(), json!(self.net));
        d.insert("at".into(), pt_gu(self.at));
        if self.rot != 0 {
            d.insert("rot".into(), json!(self.rot));
        }
        if !self.lib.is_empty() && self.lib != format!("power:{}", self.net) {
            d.insert("lib".into(), json!(self.lib));
        }
        if !self.mirror.is_empty() {
            d.insert("mirror".into(), json!(self.mirror));
        }
        Value::Object(d)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub text: String,
    pub at: [f64; 2],
    pub rot: i32,
    /// local | global | hier
    pub kind: String,
    /// For global/hier: input|output|bidirectional|tri_state|passive
    pub shape: String,
    pub uuid: String,
    pub justify: String,
}

impl Default for Label {
    fn default() -> Self {
        Label {
            text: String::new(),
            at: [0.0, 0.0],
            rot: 0,
            kind: "local".into(),
            shape: "input".into(),
            uuid: String::new(),
            justify: String::new(),
        }
    }
}

impl Label {
    pub fn to_json(&self) -> Value {
        let mut d = Map::new();
        d.insert("text".into(), json!(self.text));
        d.insert("at".into(), pt_gu(self.at));
        if self.rot != 0 {
            d.insert("rot".into(), json!(self.rot));
        }
        if self.kind != "local" {
            d.insert("type".into(), json!(self.kind));
        }
        if self.kind != "local" && self.shape != "input" {
            d.insert("shape".into(), json!(self.shape));
        }
        Value::Object(d)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    pub text: String,
    pub at: [f64; 2],
    pub rot: i32,
    pub size: f64,
    pub bold: bool,
    pub justify: String,
    pub uuid: String,
}

impl Default for Text {
    fn default() -> Self {
        Text {
            text: String::new(),
            at: [0.0, 0.0],
            rot: 0,
            size: 1.27,
            bold: false,
            justify: "left bottom".into(),
            uuid: String::new(),
        }
    }
}

impl Text {
    pub fn to_json(&self) -> Value {
        let mut d = Map::new();
        d.insert("text".into(), json!(self.text));
        d.insert("at".into(), pt_gu(self.at));
        if self.rot != 0 {
            d.insert("rot".into(), json!(self.rot));
        }
        if self.size != 1.27 {
            d.insert("size".into(), json!(self.size));
        }
        if self.bold {
            d.insert("bold".into(), json!(true));
        }
        if self.justify != "left bottom" {
            d.insert("justify".into(), json!(self.justify));
        }
        Value::Object(d)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rect {
    pub start: [f64; 2],
    pub end: [f64; 2],
    pub uuid: String,
}

impl Rect {
    pub fn to_json(&self) -> Value {
        json!({"start": pt_gu(self.start), "end": pt_gu(self.end)})
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Design {
    pub title: String,
    pub paper: String,
    pub rev: String,
    pub company: String,
    pub date: String,
    pub comments: Vec<String>,
    pub parts: Vec<Part>,
    pub power: Vec<Power>,
    pub wires: Vec<Vec<[f64; 2]>>,
    pub labels: Vec<Label>,
    pub nc: Vec<[f64; 2]>,
    pub junctions: Vec<[f64; 2]>,
    pub texts: Vec<Text>,
    pub rects: Vec<Rect>,
    /// lib_id -> raw node from the source file (preserved, not editable).
    pub lib_symbols: std::collections::BTreeMap<String, crate::sexp::Sexp>,
    /// Raw nodes we preserve verbatim: sheet, bus, bus_entry, image, polyline...
    pub extra_nodes: Vec<crate::sexp::Sexp>,
    pub wire_uuids: Vec<String>,
    pub uuid: String,
    pub sheet_path_uuid: String,
    pub project: String,
}

impl Default for Design {
    fn default() -> Self {
        Design {
            title: String::new(),
            paper: "A4".into(),
            rev: String::new(),
            company: String::new(),
            date: String::new(),
            comments: Vec::new(),
            parts: Vec::new(),
            power: Vec::new(),
            wires: Vec::new(),
            labels: Vec::new(),
            nc: Vec::new(),
            junctions: Vec::new(),
            texts: Vec::new(),
            rects: Vec::new(),
            lib_symbols: Default::default(),
            extra_nodes: Vec::new(),
            wire_uuids: Vec::new(),
            uuid: String::new(),
            sheet_path_uuid: String::new(),
            project: "schagent".into(),
        }
    }
}

impl Design {
    pub fn part(&self, reference: &str) -> Option<&Part> {
        self.parts.iter().find(|p| p.id == reference)
    }

    /// JSON for the LLM. `with_ids` tags wires/labels/power/nc/texts/rects with ids
    /// (`w1`, `l1`, `p1`, `n1`, `t1`, `r1`) so an edit patch can address them.
    pub fn to_json(&self, with_ids: bool) -> Value {
        let mut d = self.to_json_plain();
        if !with_ids {
            return d;
        }
        let o = d.as_object_mut().unwrap();
        let wires: Vec<Value> = o["wires"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, w)| json!({"id": format!("w{}", i + 1), "pts": w}))
            .collect();
        o.insert("wires".into(), Value::Array(wires));
        let nc: Vec<Value> = o["nc"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, p)| json!({"id": format!("n{}", i + 1), "at": p}))
            .collect();
        o.insert("nc".into(), Value::Array(nc));
        for (key, pre) in [
            ("labels", "l"),
            ("power", "p"),
            ("texts", "t"),
            ("rects", "r"),
        ] {
            let items: Vec<Value> = o[key]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    let mut m = Map::new();
                    m.insert("id".into(), json!(format!("{pre}{}", i + 1)));
                    for (k, v) in it.as_object().unwrap() {
                        m.insert(k.clone(), v.clone());
                    }
                    Value::Object(m)
                })
                .collect();
            o.insert(key.into(), Value::Array(items));
        }
        d
    }

    fn to_json_plain(&self) -> Value {
        let mut d = Map::new();
        d.insert("title".into(), json!(self.title));
        d.insert("paper".into(), json!(self.paper));
        if !self.rev.is_empty() {
            d.insert("rev".into(), json!(self.rev));
        }
        if !self.company.is_empty() {
            d.insert("company".into(), json!(self.company));
        }
        if !self.date.is_empty() {
            d.insert("date".into(), json!(self.date));
        }
        if !self.comments.is_empty() {
            d.insert("comments".into(), json!(self.comments));
        }
        d.insert(
            "parts".into(),
            Value::Array(self.parts.iter().map(|p| p.to_json()).collect()),
        );
        d.insert(
            "power".into(),
            Value::Array(self.power.iter().map(|p| p.to_json()).collect()),
        );
        d.insert(
            "wires".into(),
            Value::Array(
                self.wires
                    .iter()
                    .map(|w| Value::Array(w.iter().map(|p| pt_gu(*p)).collect()))
                    .collect(),
            ),
        );
        d.insert(
            "labels".into(),
            Value::Array(self.labels.iter().map(|l| l.to_json()).collect()),
        );
        d.insert(
            "nc".into(),
            Value::Array(self.nc.iter().map(|p| pt_gu(*p)).collect()),
        );
        d.insert(
            "texts".into(),
            Value::Array(self.texts.iter().map(|t| t.to_json()).collect()),
        );
        d.insert(
            "rects".into(),
            Value::Array(self.rects.iter().map(|r| r.to_json()).collect()),
        );
        if !self.extra_nodes.is_empty() {
            let mut tags: Vec<String> = self
                .extra_nodes
                .iter()
                .map(|n| n.tag().to_string())
                .collect();
            tags.sort();
            tags.dedup();
            d.insert("_preserved".into(), json!(tags));
        }
        Value::Object(d)
    }
}

/// Accept the id-tagged forms (`{"id","pts"}` wires, `{"id","at"}` nc) and return the plain form.
pub fn normalize_json(d: &Value) -> Value {
    let mut d = d.as_object().cloned().unwrap_or_default();
    let wires: Vec<Value> = arr(&d, "wires")
        .iter()
        .map(|w| {
            if w.is_object() {
                w["pts"].clone()
            } else {
                w.clone()
            }
        })
        .collect();
    d.insert("wires".into(), Value::Array(wires));
    let nc: Vec<Value> = arr(&d, "nc")
        .iter()
        .map(|n| {
            if n.is_object() {
                n["at"].clone()
            } else {
                n.clone()
            }
        })
        .collect();
    d.insert("nc".into(), Value::Array(nc));
    for key in ["labels", "power", "texts", "rects", "parts"] {
        let items: Vec<Value> = arr(&d, key)
            .iter()
            .map(|it| {
                let mut m = it.as_object().cloned().unwrap_or_default();
                if key != "parts" {
                    m.remove("id");
                }
                Value::Object(m)
            })
            .collect();
        d.insert(key.into(), Value::Array(items));
    }
    Value::Object(d)
}

fn arr(d: &Map<String, Value>, key: &str) -> Vec<Value> {
    d.get(key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

const PATCH_KEYS: [&str; 7] = ["parts", "power", "wires", "labels", "nc", "texts", "rects"];

fn ident(it: &Value) -> Option<&str> {
    it.as_object()?.get("id")?.as_str()
}

/// Apply an edit patch to an id-tagged design JSON. Returns (new id-tagged JSON, errors).
pub fn apply_patch(base_json: &Value, patch: &Value) -> (Value, Vec<String>) {
    let mut d = base_json.as_object().cloned().unwrap_or_default();
    let mut errors: Vec<String> = Vec::new();
    let empty = Map::new();
    let patch = patch.as_object().unwrap_or(&empty);

    let known: std::collections::BTreeSet<String> = PATCH_KEYS
        .iter()
        .flat_map(|k| arr(&d, k))
        .filter_map(|it| ident(&it).map(|s| s.to_string()))
        .collect();
    let remove: std::collections::BTreeSet<String> = patch
        .get("remove")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    for r in &remove {
        if !known.contains(r) {
            errors.push(format!(
                "note: remove: id '{r}' does not exist in the original design (patches are always relative \
                 to the ORIGINAL; ids you added in an earlier patch do not exist) - ignored"
            ));
        }
    }
    for k in PATCH_KEYS {
        let kept: Vec<Value> = arr(&d, k)
            .into_iter()
            .filter(|it| !ident(it).map(|i| remove.contains(i)).unwrap_or(false))
            .collect();
        d.insert(k.into(), Value::Array(kept));
    }

    if let Some(updates) = patch.get("update").and_then(|v| v.as_object()) {
        for (pid, changes) in updates {
            if remove.contains(pid) {
                errors.push(format!("update: '{pid}' is also removed"));
                continue;
            }
            let mut found = false;
            for k in PATCH_KEYS {
                let mut items = arr(&d, k);
                if let Some(hit) = items.iter_mut().find(|it| ident(it) == Some(pid.as_str())) {
                    found = true;
                    match changes.as_object() {
                        None => errors.push(format!(
                            "update: '{pid}' needs an object of fields to change"
                        )),
                        Some(ch) => {
                            let obj = hit.as_object_mut().unwrap();
                            for (ck, cv) in ch {
                                if ck != "id" {
                                    obj.insert(ck.clone(), cv.clone());
                                }
                            }
                        }
                    }
                    d.insert(k.into(), Value::Array(items));
                    break;
                }
            }
            if !found {
                errors.push(format!("update: unknown id '{pid}'"));
            }
        }
    }

    let add = patch
        .get("add")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let mut counters: std::collections::BTreeMap<&str, usize> =
        PATCH_KEYS.iter().map(|k| (*k, arr(&d, k).len())).collect();
    let prefix: std::collections::BTreeMap<&str, &str> = [
        ("wires", "w"),
        ("labels", "l"),
        ("power", "p"),
        ("nc", "n"),
        ("texts", "t"),
        ("rects", "r"),
    ]
    .into_iter()
    .collect();
    for k in PATCH_KEYS {
        for it in add
            .get(k)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let mut it = match (k, it.is_object()) {
                ("wires", false) => json!({ "pts": it }),
                ("nc", false) => json!({ "at": it }),
                _ => it,
            };
            if k == "parts" {
                let id = it.get("id").cloned().unwrap_or(Value::Null);
                let unit = it.get("unit").and_then(|v| v.as_i64()).unwrap_or(1);
                let parts = arr(&d, "parts");
                let dup = parts.iter().any(|x| x.get("id") == Some(&id));
                let multi = parts.iter().any(|x| {
                    x.get("id") == Some(&id)
                        && x.get("unit").and_then(|v| v.as_i64()).unwrap_or(1) != 1
                });
                if dup && unit == 1 && !multi {
                    errors.push(format!(
                        "add: part id '{}' already exists (use update or a new id)",
                        id.as_str().unwrap_or_default()
                    ));
                    continue;
                }
            } else {
                let c = counters.get_mut(k).unwrap();
                *c += 1;
                let mut m = Map::new();
                m.insert("id".into(), json!(format!("{}{}", prefix[k], c)));
                for (kk, vv) in it.as_object().unwrap() {
                    if kk != "id" {
                        m.insert(kk.clone(), vv.clone());
                    }
                }
                it = Value::Object(m);
            }
            let mut items = arr(&d, k);
            items.push(it);
            d.insert(k.into(), Value::Array(items));
        }
    }
    for k in ["title", "rev", "date", "company", "comments", "paper"] {
        if let Some(v) = patch.get(k) {
            d.insert(k.into(), v.clone());
        }
    }
    (Value::Object(d), errors)
}

/// Build a [`Design`] from LLM/user JSON. `base` supplies preserved data (lib symbols, uuids, extras).
pub fn design_from_json(d: &Value, base: Option<&Design>) -> Design {
    let d = normalize_json(d);
    let d = d.as_object().unwrap();
    let mut des = Design::default();
    if let Some(b) = base {
        des.lib_symbols = b.lib_symbols.clone();
        des.extra_nodes = b.extra_nodes.clone();
        des.uuid = b.uuid.clone();
        des.sheet_path_uuid = b.sheet_path_uuid.clone();
        des.project = b.project.clone();
    }
    let s = |k: &str, fallback: &str| -> String {
        d.get(k)
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
            .unwrap_or_else(|| fallback.to_string())
    };
    des.title = s("title", base.map(|b| b.title.as_str()).unwrap_or(""));
    des.paper = s("paper", base.map(|b| b.paper.as_str()).unwrap_or("A4"));
    des.rev = s("rev", base.map(|b| b.rev.as_str()).unwrap_or(""));
    des.company = s("company", base.map(|b| b.company.as_str()).unwrap_or(""));
    des.date = s("date", base.map(|b| b.date.as_str()).unwrap_or(""));
    des.comments = match d.get("comments").and_then(|v| v.as_array()) {
        Some(a) => a
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect(),
        None => base.map(|b| b.comments.clone()).unwrap_or_default(),
    };

    for pj in arr(d, "parts") {
        let o = pj.as_object().cloned().unwrap_or_default();
        let mut p = Part {
            id: str_of(&o, "id"),
            lib: str_of(&o, "lib"),
            at: pt_mm(o.get("at").unwrap_or(&Value::Null)),
            rot: o.get("rot").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
            mirror: str_of(&o, "mirror"),
            value: str_of(&o, "value"),
            unit: o.get("unit").and_then(|v| v.as_f64()).unwrap_or(1.0) as i32,
            footprint: str_of(&o, "footprint"),
            dnp: o.get("dnp").and_then(|v| v.as_bool()).unwrap_or(false),
            fields: o
                .get("fields")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default(),
            ref_at: at_mm(o.get("ref_at")),
            val_at: at_mm(o.get("val_at")),
            hide_value: o
                .get("hide_value")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            uuid: String::new(),
            lib_name: str_of(&o, "lib_name"),
        };
        if let Some(old) = base.and_then(|b| b.parts.iter().find(|x| x.id == p.id))
            && old.lib == p.lib
            && old.unit == p.unit
        {
            p.uuid = old.uuid.clone();
            if p.ref_at.is_none() && old.at == p.at && old.rot == p.rot && old.mirror == p.mirror {
                p.ref_at = old.ref_at.clone();
                p.val_at = old.val_at.clone();
                p.hide_value = old.hide_value;
            }
        }
        des.parts.push(p);
    }

    for pj in arr(d, "power") {
        let o = pj.as_object().cloned().unwrap_or_default();
        let net = str_of(&o, "net");
        let lib = {
            let l = str_of(&o, "lib");
            if l.is_empty() {
                format!("power:{net}")
            } else {
                l
            }
        };
        let mut p = Power {
            net: net.clone(),
            at: pt_mm(o.get("at").unwrap_or(&Value::Null)),
            rot: o.get("rot").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
            lib,
            mirror: str_of(&o, "mirror"),
            ..Default::default()
        };
        if let Some(old) =
            base.and_then(|b| b.power.iter().find(|x| x.net == p.net && x.at == p.at))
            && old.rot == p.rot
        {
            p.uuid = old.uuid.clone();
            p.reference = old.reference.clone();
            p.val_at = old.val_at.clone();
        }
        des.power.push(p);
    }

    des.wires = arr(d, "wires")
        .iter()
        .filter_map(|w| w.as_array())
        .filter(|w| w.len() >= 2)
        .map(|w| w.iter().map(pt_mm).collect())
        .collect();
    for lj in arr(d, "labels") {
        let o = lj.as_object().cloned().unwrap_or_default();
        des.labels.push(Label {
            text: str_of(&o, "text"),
            at: pt_mm(o.get("at").unwrap_or(&Value::Null)),
            rot: o.get("rot").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
            kind: opt_str(&o, "type", "local"),
            shape: opt_str(&o, "shape", "input"),
            ..Default::default()
        });
    }
    des.nc = arr(d, "nc").iter().map(pt_mm).collect();
    for tj in arr(d, "texts") {
        let o = tj.as_object().cloned().unwrap_or_default();
        des.texts.push(Text {
            text: str_of(&o, "text"),
            at: pt_mm(o.get("at").unwrap_or(&Value::Null)),
            rot: o.get("rot").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
            size: o.get("size").and_then(|v| v.as_f64()).unwrap_or(1.27),
            bold: o.get("bold").and_then(|v| v.as_bool()).unwrap_or(false),
            justify: opt_str(&o, "justify", "left bottom"),
            ..Default::default()
        });
    }
    for rj in arr(d, "rects") {
        let o = rj.as_object().cloned().unwrap_or_default();
        des.rects.push(Rect {
            start: pt_mm(o.get("start").unwrap_or(&Value::Null)),
            end: pt_mm(o.get("end").unwrap_or(&Value::Null)),
            uuid: String::new(),
        });
    }
    des
}

fn str_of(o: &Map<String, Value>, k: &str) -> String {
    match o.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

fn opt_str(o: &Map<String, Value>, k: &str, default: &str) -> String {
    match o.get(k).and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => default.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        assert_eq!(mm(3.0), 3.81);
        assert_eq!(gu(3.81), 3.0);
        assert_eq!(snap(3.9, GRID), 3.81);
        assert_eq!(num(3.0), json!(3));
    }

    #[test]
    fn python_banker_rounding() {
        assert_eq!(round_py(0.5), 0.0);
        assert_eq!(round_py(1.5), 2.0);
        assert_eq!(round_py(2.5), 2.0);
        assert_eq!(round_py(-0.5), 0.0);
        assert_eq!(round_py(-1.5), -2.0);
        assert_eq!(round_py(3.2), 3.0);
        assert_eq!(round_py_dp(0.125, 2), 0.12);
        assert_eq!(round_py_dp(2.675, 2), 2.67);
    }

    #[test]
    fn rotations() {
        assert_eq!(rot_point(2.54, 0.0, 0, ""), (2.54, 0.0));
        assert_eq!(rot_point(2.54, 0.0, 90, ""), (0.0, -2.54));
        assert_eq!(rot_point(2.54, 0.0, 180, ""), (-2.54, 0.0));
        assert_eq!(rot_point(2.54, 0.0, 270, ""), (0.0, 2.54));
        assert_eq!(rot_point(2.54, 1.0, 0, "y"), (-2.54, -1.0));
        assert_eq!(rot_point(2.54, 1.0, 0, "x"), (2.54, 1.0));
    }

    #[test]
    fn patch_add_and_remove() {
        let base = json!({"parts": [{"id": "R1", "lib": "Device:R", "at": [0, 0]}],
                          "wires": [{"id": "w1", "pts": [[0, 0], [1, 0]]}],
                          "labels": [], "power": [], "nc": [], "texts": [], "rects": []});
        let patch = json!({"remove": ["w1", "zz"], "add": {"wires": [[[2, 2], [3, 2]]]}});
        let (out, errs) = apply_patch(&base, &patch);
        assert_eq!(out["wires"].as_array().unwrap().len(), 1);
        assert_eq!(out["wires"][0]["id"], "w1");
        assert!(errs.iter().any(|e| e.contains("'zz'")));
    }
}
