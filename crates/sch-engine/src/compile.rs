//! Compile a [`Design`] into the `.kicad_sch` text KiCad 10 loads.
//!
//! A faithful port of `schagent/compile.py`: stock symbols are embedded flattened, pins are
//! transformed (rotate -> mirror -> flip Y), reference/value texts are placed clear of wires and
//! power symbols, junctions are inferred and the sheet is written as s-expressions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::engine::{connector_text_slot, rot_body, two_pin_axis};
use crate::geo::dir_of;
use crate::model::{Design, Label, Part, Power, new_uuid, rot_point};
use crate::sexp::{self, Sexp};
use crate::symlib::{SymbolInfo, index, parse_symbol_node};

pub const VERSION: i64 = 20250114;

/// Python's `round(v, n)`: correct rounding of the exact binary value, ties to even.
pub fn round_py(v: f64, n: usize) -> f64 {
    format!("{v:.n$}", n = n).parse().unwrap_or(v)
}

fn r2(v: f64) -> f64 {
    round_py(v, 2)
}

fn r4(v: f64) -> f64 {
    round_py(v, 4)
}

/// A point rounded to 2 decimals, usable as a map key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(i64, i64);

impl Key {
    pub fn of(x: f64, y: f64) -> Key {
        Key((r2(x) * 100.0).round() as i64, (r2(y) * 100.0).round() as i64)
    }
    pub fn xy(self) -> (f64, f64) {
        (self.0 as f64 / 100.0, self.1 as f64 / 100.0)
    }
}

fn num(v: f64) -> Sexp {
    Sexp::Float(v)
}

fn font(size: f64, bold: bool, hide: bool, justify: &str) -> Sexp {
    let mut f = vec![Sexp::sym("font"), Sexp::List(vec![Sexp::sym("size"), num(size), num(size)])];
    if bold {
        f.push(Sexp::List(vec![Sexp::sym("bold"), Sexp::sym("yes")]));
    }
    let mut eff = vec![Sexp::sym("effects"), Sexp::List(f)];
    if !justify.is_empty() {
        let mut j = vec![Sexp::sym("justify")];
        j.extend(justify.split_whitespace().map(Sexp::sym));
        eff.push(Sexp::List(j));
    }
    if hide {
        eff.push(Sexp::List(vec![Sexp::sym("hide"), Sexp::sym("yes")]));
    }
    Sexp::List(eff)
}

fn property(name: &str, value: &str, x: f64, y: f64, rot: f64, hide: bool, justify: &str) -> Sexp {
    Sexp::List(vec![
        Sexp::sym("property"),
        Sexp::str(name),
        Sexp::str(value),
        Sexp::List(vec![Sexp::sym("at"), num(x), num(y), num(rot)]),
        font(1.27, false, hide, justify),
    ])
}

pub fn ground_like(net: &str) -> bool {
    let n = net.to_uppercase();
    n.ends_with("GND")
        || n.starts_with("GND")
        || matches!(n.as_str(), "VSS" | "VSSA" | "AGND" | "DGND" | "PGND" | "0V" | "EARTH" | "VEE")
}

/// The power symbol that draws `net`: its own symbol when one exists, else a generic GND/VCC.
pub fn power_lib_for(net: &str, lib_symbols: &std::collections::BTreeMap<String, Sexp>) -> String {
    let cand = format!("power:{net}");
    if lib_symbols.contains_key(&cand) || index().get(&cand).is_some() {
        return cand;
    }
    if ground_like(net) { "power:GND".into() } else { "power:VCC".into() }
}

/// A property anchor: `[x, y, stored angle, justify]`.
#[derive(Debug, Clone, PartialEq)]
pub struct At {
    pub x: f64,
    pub y: f64,
    pub rot: i32,
    pub justify: String,
}

impl At {
    fn new(x: f64, y: f64, rot: i32, justify: &str) -> At {
        At { x, y, rot, justify: justify.into() }
    }
    fn from_json(v: &[Value]) -> At {
        At {
            x: v.first().and_then(|a| a.as_f64()).unwrap_or(0.0),
            y: v.get(1).and_then(|a| a.as_f64()).unwrap_or(0.0),
            rot: v.get(2).and_then(|a| a.as_f64()).unwrap_or(0.0) as i32,
            justify: v.get(3).and_then(|a| a.as_str()).unwrap_or("").to_string(),
        }
    }
    fn round4(&self) -> At {
        At { x: r4(self.x), y: r4(self.y), rot: self.rot, justify: self.justify.clone() }
    }
}

fn swap_lr(j: &str) -> String {
    j.replace("left", "\u{1}").replace("right", "left").replace('\u{1}', "right")
}

fn swap_tb(j: &str) -> String {
    j.replace("top", "\u{1}").replace("bottom", "top").replace('\u{1}', "bottom")
}

/// A visible text the checker measures (the rendered angle and justification, not the stored ones).
#[derive(Debug, Clone)]
pub struct TextItem {
    pub owner: String,
    pub kind: String,
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub rot: i32,
    pub justify: String,
    pub size: f64,
}

type Box4 = [f64; 4];
type Seg = ([f64; 2], [f64; 2]);
/// `(ref, pin number, electrical type)` of every pin sitting at one point.
pub type PinsAt = Vec<(Key, Vec<(String, String, String)>)>;

pub struct Compiler {
    pub des: Design,
    /// lib key -> raw symbol node to embed, in first-use order.
    libs: RefCell<Vec<(String, Sexp)>>,
    infos: RefCell<HashMap<String, Arc<SymbolInfo>>>,
    pub errors: RefCell<Vec<String>>,
    pub text_items: RefCell<Vec<TextItem>>,
    segs: RefCell<Option<Vec<Seg>>>,
    pboxes: RefCell<Option<Vec<Box4>>>,
}

impl Compiler {
    pub fn new(des: Design) -> Compiler {
        Compiler {
            des,
            libs: RefCell::new(Vec::new()),
            infos: RefCell::new(HashMap::new()),
            errors: RefCell::new(Vec::new()),
            text_items: RefCell::new(Vec::new()),
            segs: RefCell::new(None),
            pboxes: RefCell::new(None),
        }
    }

    fn lib_node(&self, key: &str) -> Option<Sexp> {
        self.libs.borrow().iter().find(|(k, _)| k == key).map(|(_, n)| n.clone())
    }

    // ---- library resolution ------------------------------------------

    /// The symbol definition behind an instance, embedding its raw node on first use.
    pub fn resolve_lib(&self, lib_id: &str, lib_name: &str) -> Option<Arc<SymbolInfo>> {
        let key = if lib_name.is_empty() { lib_id.to_string() } else { lib_name.to_string() };
        if let Some(i) = self.infos.borrow().get(&key) {
            return Some(i.clone());
        }
        let mut lib_id = lib_id.to_string();
        let mut node = self.des.lib_symbols.get(&key).cloned();
        if node.is_none() && lib_name.is_empty() {
            node = index().raw_symbol(&lib_id);
        }
        if node.is_none() && !lib_name.is_empty() {
            lib_id = key.clone();
            node = index().raw_symbol(&lib_id);
        }
        let Some(node) = node else {
            self.errors.borrow_mut().push(format!("unknown symbol {lib_id}"));
            return None;
        };
        if !self.libs.borrow().iter().any(|(k, _)| *k == key) {
            self.libs.borrow_mut().push((key.clone(), node.clone()));
        }
        let mut info = parse_symbol_node(&node, &lib_id);
        // a symbol whose definition only `extends` another carries no pins: take them from the index
        if info.pins.is_empty()
            && let Some(idx) = index().get(&lib_id)
        {
            info.pins = idx.pins.clone();
            info.bbox = idx.bbox;
        }
        let info = Arc::new(info);
        self.infos.borrow_mut().insert(key, info.clone());
        Some(info)
    }

    pub fn pin_pos(&self, part: &Part, number: &str) -> Option<(f64, f64)> {
        let info = self.resolve_lib(&part.lib, &part.lib_name)?;
        let pin = info.pin(number, Some(part.unit))?;
        let (dx, dy) = rot_point(pin.x, pin.y, part.rot, &part.mirror);
        Some((r4(part.at[0] + dx), r4(part.at[1] + dy)))
    }

    /// `(x, y) -> [(ref, pin number, electrical type)]`, parts first then power symbols.
    pub fn all_pin_positions(&self) -> PinsAt {
        let mut out: PinsAt = Vec::new();
        let mut at: HashMap<Key, usize> = HashMap::new();
        let mut push = |k: Key, v: (String, String, String)| match at.get(&k) {
            Some(&i) => out[i].1.push(v),
            None => {
                at.insert(k, out.len());
                out.push((k, vec![v]));
            }
        };
        for p in &self.des.parts {
            let Some(info) = self.resolve_lib(&p.lib, &p.lib_name) else { continue };
            for pin in info.pins_for_unit(p.unit) {
                if pin.hidden {
                    continue;
                }
                let (dx, dy) = rot_point(pin.x, pin.y, p.rot, &p.mirror);
                push(Key::of(p.at[0] + dx, p.at[1] + dy), (p.id.clone(), pin.number.clone(), pin.etype.clone()));
            }
        }
        for pw in &self.des.power {
            let r = if pw.reference.is_empty() { "#PWR".to_string() } else { pw.reference.clone() };
            push(Key::of(pw.at[0], pw.at[1]), (r, "1".into(), "power_in".into()));
        }
        out
    }

    // ---- junction inference -----------------------------------------

    /// Points where three or more connections meet.
    pub fn compute_junctions(&self) -> Vec<[f64; 2]> {
        let segs: Vec<(Key, Key)> = self
            .des
            .wires
            .iter()
            .flat_map(|w| w.windows(2).map(|ab| (Key::of(ab[0][0], ab[0][1]), Key::of(ab[1][0], ab[1][1]))))
            .collect();
        let mut order: Vec<Key> = Vec::new();
        let mut deg: HashMap<Key, i32> = HashMap::new();
        let bump = |order: &mut Vec<Key>, deg: &mut HashMap<Key, i32>, k: Key, n: i32| {
            if !deg.contains_key(&k) {
                order.push(k);
            }
            *deg.entry(k).or_insert(0) += n;
        };
        for (a, b) in &segs {
            bump(&mut order, &mut deg, *a, 1);
            bump(&mut order, &mut deg, *b, 1);
        }
        for (pt, lst) in self.all_pin_positions() {
            bump(&mut order, &mut deg, pt, lst.len() as i32);
        }
        for l in &self.des.labels {
            bump(&mut order, &mut deg, Key::of(l.at[0], l.at[1]), 0);
        }
        for pt in order.clone() {
            for (a, b) in &segs {
                if pt == *a || pt == *b {
                    continue;
                }
                if on_segment(pt.xy(), a.xy(), b.xy(), 0.01) {
                    *deg.get_mut(&pt).unwrap() += 2;
                }
            }
        }
        order
            .into_iter()
            .filter(|k| deg[k] >= 3)
            .map(|k| {
                let (x, y) = k.xy();
                [x, y]
            })
            .collect()
    }

    // ---- geometry the text placement avoids ---------------------------

    fn wire_segs(&self) -> Vec<Seg> {
        let mut cache = self.segs.borrow_mut();
        cache
            .get_or_insert_with(|| self.des.wires.iter().flat_map(|w| w.windows(2).map(|ab| (ab[0], ab[1]))).collect())
            .clone()
    }

    /// Power symbols (body + text): roughly 6 x 9 mm around their anchor.
    fn power_boxes(&self) -> Vec<Box4> {
        let mut cache = self.pboxes.borrow_mut();
        cache
            .get_or_insert_with(|| {
                self.des
                    .power
                    .iter()
                    .map(|pw| [pw.at[0] - 3.0, pw.at[1] - 4.5, pw.at[0] + 3.0, pw.at[1] + 4.5])
                    .collect()
            })
            .clone()
    }

    fn box_hit(&self, b: Box4, segs: &[Seg], pboxes: &[Box4]) -> bool {
        segs.iter().any(|(a, c)| seg_in_box(*a, *c, b, 0.2))
            || pboxes.iter().any(|q| !(b[2] <= q[0] || q[2] <= b[0] || b[3] <= q[1] || q[3] <= b[1]))
    }

    // ---- symbol instance emit --------------------------------------

    pub fn emit_part(&self, p: &Part) -> Option<Sexp> {
        let info = self.resolve_lib(&p.lib, &p.lib_name)?;
        let key = if p.lib_name.is_empty() { p.lib.clone() } else { p.lib_name.clone() };
        let libnode = self.lib_node(&key)?;
        let uuid = if p.uuid.is_empty() { new_uuid() } else { p.uuid.clone() };
        let mut node = vec![Sexp::sym("symbol"), Sexp::List(vec![Sexp::sym("lib_id"), Sexp::str(&p.lib)])];
        if !p.lib_name.is_empty() {
            node.push(Sexp::List(vec![Sexp::sym("lib_name"), Sexp::str(&p.lib_name)]));
        }
        node.push(Sexp::List(vec![Sexp::sym("at"), num(p.at[0]), num(p.at[1]), num(p.rot as f64)]));
        if !p.mirror.is_empty() {
            node.push(Sexp::List(vec![Sexp::sym("mirror"), Sexp::sym(&p.mirror)]));
        }
        node.extend([
            Sexp::List(vec![Sexp::sym("unit"), num(p.unit as f64)]),
            Sexp::List(vec![Sexp::sym("exclude_from_sim"), Sexp::sym("no")]),
            Sexp::List(vec![Sexp::sym("in_bom"), Sexp::sym("yes")]),
            Sexp::List(vec![Sexp::sym("on_board"), Sexp::sym("yes")]),
            Sexp::List(vec![Sexp::sym("dnp"), Sexp::sym(if p.dnp { "yes" } else { "no" })]),
            Sexp::List(vec![Sexp::sym("uuid"), Sexp::str(&uuid)]),
        ]);

        let mut ref_at = p
            .ref_at
            .as_ref()
            .map(|v| At::from_json(v))
            .unwrap_or_else(|| self.auto_prop_pos(p, libnode.prop("Reference")));
        let mut val_at =
            p.val_at.as_ref().map(|v| At::from_json(v)).unwrap_or_else(|| self.auto_prop_pos(p, libnode.prop("Value")));

        let pins_u = info.pins_for_unit(p.unit);
        let npins = pins_u.len();
        // multi-pin parts: give the reference/value a little more air above/below the body
        if p.ref_at.is_none() && npins > 3 && p.rot == 0 && p.mirror.is_empty() {
            let bb = info.body_for(p.unit);
            let (top, bot) = (p.at[1] - bb.1.max(bb.3), p.at[1] - bb.1.min(bb.3));
            let powered = |side: &str| {
                pins_u.iter().any(|pin| pin.side() == side && matches!(pin.etype.as_str(), "power_in" | "power_out"))
            };
            // power symbols above the pins need ~9 units of clearance
            let top_air = if powered("top") { 4.5 } else { 2.0 };
            let bot_air = if powered("bottom") { 4.5 } else { 2.0 };
            if ref_at.y < top {
                ref_at.y = r4(ref_at.y - top_air);
            }
            if val_at.y > bot {
                val_at.y = r4(val_at.y + bot_air);
            }
        }
        let axis2 = if npins == 2 { two_pin_axis(&info, p.unit, p.rot, &p.mirror) } else { "" };
        let mut passive_h = (0.0f64, 0i32);
        if p.ref_at.is_none() && npins == 2 && info.ref_prefix != "J" && axis2 == "h" {
            // horizontal passive: reference above and value below the body, both drawn horizontally
            // (KiCad renders a property at lib angle + instance rotation, so 90 + 90 = horizontal)
            let half = rot_body(&info, p.unit, p.rot, &p.mirror).1 + 1.4;
            let ang = (-p.rot).rem_euclid(180);
            ref_at = At::new(p.at[0], r4(p.at[1] - half), ang, "");
            val_at = At::new(p.at[0], r4(p.at[1] + half), ang, "");
            passive_h = (half, ang);
        }

        // a wire or power symbol running through the value/reference text moves the text away
        let segs = self.wire_segs();
        let pboxes = self.power_boxes();
        let value_text = if p.value.is_empty() { info.value.clone() } else { p.value.clone() };
        let anchored = |txt: &str, at_: &At| -> Box4 {
            let w = 1.27 * 0.95 * txt.chars().count().max(1) as f64 + 1.0;
            let h = 2.6;
            let x0 = if at_.justify.contains("left") {
                at_.x
            } else if at_.justify.contains("right") {
                at_.x - w
            } else {
                at_.x - w / 2.0
            };
            [x0, at_.y - h / 2.0, x0 + w, at_.y + h / 2.0]
        };
        let hit = |at_: &At| self.box_hit(anchored(&value_text, at_), &segs, &pboxes);
        let hit_ref = |at_: &At| self.box_hit(anchored(&p.id, at_), &segs, &pboxes);
        let hit_centred = |txt: &str, at_: &At| {
            let w = 1.27 * 0.95 * txt.chars().count().max(1) as f64 + 1.0;
            let h = 2.6;
            self.box_hit([at_.x - w / 2.0, at_.y - h / 2.0, at_.x + w / 2.0, at_.y + h / 2.0], &segs, &pboxes)
        };

        let sideways = npins > 2 && (p.rot == 90 || p.rot == 270);
        if p.ref_at.is_none() && npins == 2 && info.ref_prefix != "J" && axis2 == "h" {
            // lying passive: if the reference (above) or the value (below) sits on a wire or power
            // symbol, stack both texts on the clear side
            let (half, ang) = passive_h;
            let up_bad = hit_centred(&p.id, &ref_at);
            let down_bad = hit_centred(&value_text, &val_at);
            if up_bad && !down_bad {
                ref_at = At::new(p.at[0], r4(p.at[1] + half), ang, "");
                val_at = At::new(p.at[0], r4(p.at[1] + half + 2.4), ang, "");
            } else if down_bad && !up_bad {
                val_at = At::new(p.at[0], r4(p.at[1] - half), ang, "");
                ref_at = At::new(p.at[0], r4(p.at[1] - half - 2.4), ang, "");
            }
        }

        if p.ref_at.is_none()
            && p.val_at.is_none()
            && ((npins > 3 && p.rot == 0 && p.mirror.is_empty()) || info.ref_prefix == "J" || sideways)
        {
            // reference + value as a stacked pair; try above, beside (either side), below - the
            // first position whose texts are clear of wires wins
            let (hx, hy) = rot_body(&info, p.unit, p.rot, &p.mirror);
            let (bl, br) = (p.at[0] - hx, p.at[0] + hx);
            let (bt, bbm) = (p.at[1] - hy, p.at[1] + hy);
            let sides: Vec<&str> = ["top", "bottom", "right", "left"]
                .into_iter()
                .filter(|s| pins_u.iter().any(|pin| side_of(dir_of(pin, p.rot, &p.mirror)) == *s))
                .collect();
            let has = |s: &str| sides.contains(&s);
            // texts above/below end at the body edge away from lateral pins (so power stubs stay clear)
            let (cx, cj) = if has("right") && !has("left") {
                (br, "right")
            } else if has("left") && !has("right") {
                (bl, "left")
            } else {
                ((bl + br) / 2.0, "")
            };
            let sa = (-p.rot).rem_euclid(180);
            let slot = |name: &str| -> (At, At) {
                match name {
                    "above" => (At::new(cx, bt - 4.4, sa, cj), At::new(cx, bt - 1.8, sa, cj)),
                    "below" => (At::new(cx, bbm + 1.8, sa, cj), At::new(cx, bbm + 4.4, sa, cj)),
                    "right" => (At::new(br + 1.0, bt + 1.0, sa, "left"), At::new(br + 1.0, bt + 3.6, sa, "left")),
                    _ => (At::new(bl - 1.0, bt + 1.0, sa, "right"), At::new(bl - 1.0, bt + 3.6, sa, "right")),
                }
            };
            let mut cands: Vec<(At, At)> = Vec::new();
            if info.ref_prefix == "J" || sideways {
                // connectors / sideways parts: the pin-free slot the placement engine reserved comes first
                cands.push(slot(connector_text_slot(&info, p.unit, p.rot, &p.mirror)));
            }
            cands.push((ref_at.clone(), val_at.clone()));
            if !has("right") {
                cands.push(slot("right"));
            }
            if !has("left") {
                cands.push(slot("left"));
            }
            cands.push(slot("above"));
            cands.push(slot("below"));
            for (r_, v_) in cands {
                if !hit_ref(&r_) && !hit(&v_) {
                    let (mut r_, mut v_) = (r_.round4(), v_.round4());
                    // KiCad mirrors the justification of a mirrored symbol's text (and of 180-degree text)
                    if ((r_.rot + p.rot).rem_euclid(360) == 180) != (p.mirror == "y") {
                        r_.justify = swap_lr(&r_.justify);
                        v_.justify = swap_lr(&v_.justify);
                    }
                    ref_at = r_;
                    val_at = v_;
                    break;
                }
            }
        }

        if p.val_at.is_none() && hit(&val_at) {
            let bb = info.body_for(p.unit);
            let body_top = p.at[1] - bb.1.max(bb.3);
            let body_bot = p.at[1] - bb.1.min(bb.3);
            for cand in [
                At::new(val_at.x, r4(body_top - 2.6), 0, &val_at.justify),
                At::new(val_at.x, r4(body_bot + 2.6), 0, &val_at.justify),
                At::new(val_at.x, r4(body_top - 4.2), 0, &val_at.justify),
            ] {
                if !hit(&cand) && (cand.y - ref_at.y).abs() > 1.5 {
                    val_at = cand;
                    break;
                }
            }
        }

        // vertical 2-pin parts: horizontal reference/value beside the body (easier to read)
        if p.ref_at.is_none() && npins == 2 && info.ref_prefix != "J" && axis2 == "v" {
            let right = p.at[0] + rot_body(&info, p.unit, p.rot, &p.mirror).0 + 0.9;
            let ang = (-p.rot).rem_euclid(180);
            // KiCad flips the justification back for 180-degree / mirrored text
            let j = if ((ang + p.rot).rem_euclid(360) == 180) != (p.mirror == "y") { "right" } else { "left" };
            ref_at = At::new(r4(right), r4(p.at[1] - 1.3), ang, j);
            val_at = At::new(r4(right), r4(p.at[1] + 1.3), ang, j);
        }

        node.push(property("Reference", &p.id, ref_at.x, ref_at.y, ref_at.rot as f64, false, &ref_at.justify));
        let value = if !p.value.is_empty() {
            p.value.clone()
        } else if !info.value.is_empty() {
            info.value.clone()
        } else {
            p.lib.split_once(':').map(|(_, n)| n.to_string()).unwrap_or_default()
        };
        node.push(property("Value", &value, val_at.x, val_at.y, val_at.rot as f64, p.hide_value, &val_at.justify));

        let rendered_justify = |at_: &At| {
            if ((at_.rot + p.rot).rem_euclid(360) == 180) != (p.mirror == "y") {
                swap_lr(&at_.justify)
            } else {
                at_.justify.clone()
            }
        };
        self.text_items.borrow_mut().push(TextItem {
            owner: p.id.clone(),
            kind: "ref".into(),
            text: p.id.clone(),
            x: ref_at.x,
            y: ref_at.y,
            rot: (ref_at.rot + p.rot).rem_euclid(180),
            justify: rendered_justify(&ref_at),
            size: 1.27,
        });
        if !p.hide_value {
            self.text_items.borrow_mut().push(TextItem {
                owner: p.id.clone(),
                kind: "value".into(),
                text: value.clone(),
                x: val_at.x,
                y: val_at.y,
                rot: (val_at.rot + p.rot).rem_euclid(180),
                justify: rendered_justify(&val_at),
                size: 1.27,
            });
        }

        let fp = if p.footprint.is_empty() { info.footprint.clone() } else { p.footprint.clone() };
        node.push(property("Footprint", &fp, p.at[0], p.at[1], 0.0, true, ""));
        let field = |k: &str| p.fields.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
        let ds = field("Datasheet")
            .unwrap_or_else(|| if info.datasheet.is_empty() { "~".into() } else { info.datasheet.clone() });
        node.push(property("Datasheet", &ds, p.at[0], p.at[1], 0.0, true, ""));
        let desc = field("Description").unwrap_or_else(|| info.description.clone());
        node.push(property("Description", &desc, p.at[0], p.at[1], 0.0, true, ""));
        for (k, v) in p.fields.iter() {
            if k == "Datasheet" || k == "Description" {
                continue;
            }
            let s = v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string());
            node.push(property(k, &s, p.at[0], p.at[1], 0.0, true, ""));
        }
        for pin in info.pins_for_unit(p.unit) {
            node.push(Sexp::List(vec![
                Sexp::sym("pin"),
                Sexp::str(&pin.number),
                Sexp::List(vec![Sexp::sym("uuid"), Sexp::str(new_uuid())]),
            ]));
        }
        node.push(instances(&self.des, &p.id, p.unit));
        Some(Sexp::List(node))
    }

    /// The library's own property anchor, transformed onto the instance.
    fn auto_prop_pos(&self, p: &Part, libprop: Option<&Sexp>) -> At {
        let (lx, ly, mut justify) = match libprop {
            Some(lp) => {
                let at = lp.child("at");
                let x = at.and_then(|a| a.as_list()?.get(1)?.as_f64()).unwrap_or(0.0);
                let y = at.and_then(|a| a.as_list()?.get(2)?.as_f64()).unwrap_or(0.0);
                let j = lp
                    .child("effects")
                    .and_then(|e| e.child("justify"))
                    .map(|j| j.as_list().unwrap()[1..].iter().map(|a| a.text()).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                (x, y, j)
            }
            None => (0.0, 0.0, String::new()),
        };
        let (dx, dy) = rot_point(lx, ly, p.rot, &p.mirror);
        // KiCad stores property angles in symbol space and adds the instance rotation when drawing,
        // so the library angle is written unchanged; `rendered` only decides the justification.
        let ang = (-p.rot).rem_euclid(180);
        let rendered = (ang + p.rot).rem_euclid(360);
        // Measured with KiCad 10: it mirrors the horizontal justification when
        // (rendered == 180) xor (mirror y), the vertical one when (rendered == 180) xor (mirror x).
        // For rot 0/180 the geometric side flip of the anchor coincides with that, so only sideways
        // parts need the swap.
        if !justify.is_empty() && (p.rot == 90 || p.rot == 270) {
            if (rendered == 180) != (p.mirror == "y") {
                justify = swap_lr(&justify);
            }
            if (rendered == 180) != (p.mirror == "x") {
                justify = swap_tb(&justify);
            }
        }
        At::new(r4(p.at[0] + dx), r4(p.at[1] + dy), ang, &justify)
    }

    pub fn emit_power(&self, pw: &mut Power, n: usize) -> Option<Sexp> {
        let mut lib_id = if pw.lib.is_empty() { power_lib_for(&pw.net, &self.des.lib_symbols) } else { pw.lib.clone() };
        if !self.des.lib_symbols.contains_key(&lib_id) && index().get(&lib_id).is_none() {
            // generic bar/ground symbol showing the net name
            lib_id = power_lib_for(&pw.net, &self.des.lib_symbols);
        }
        let info = self.resolve_lib(&lib_id, "")?;
        pw.lib = lib_id.clone();
        let pin = info.pins.first().cloned();
        let (px, py) = match &pin {
            Some(pin) => rot_point(pin.x, pin.y, pw.rot, &pw.mirror),
            None => (0.0, 0.0),
        };
        let (ax, ay) = (r4(pw.at[0] - px), r4(pw.at[1] - py));
        let reference = if pw.reference.is_empty() { format!("#PWR{n:03}") } else { pw.reference.clone() };
        pw.reference = reference.clone();
        let mut node = vec![
            Sexp::sym("symbol"),
            Sexp::List(vec![Sexp::sym("lib_id"), Sexp::str(&lib_id)]),
            Sexp::List(vec![Sexp::sym("at"), num(ax), num(ay), num(pw.rot as f64)]),
        ];
        if !pw.mirror.is_empty() {
            node.push(Sexp::List(vec![Sexp::sym("mirror"), Sexp::sym(&pw.mirror)]));
        }
        node.extend([
            Sexp::List(vec![Sexp::sym("unit"), num(1.0)]),
            Sexp::List(vec![Sexp::sym("exclude_from_sim"), Sexp::sym("no")]),
            Sexp::List(vec![Sexp::sym("in_bom"), Sexp::sym("yes")]),
            Sexp::List(vec![Sexp::sym("on_board"), Sexp::sym("yes")]),
            Sexp::List(vec![Sexp::sym("dnp"), Sexp::sym("no")]),
            Sexp::List(vec![
                Sexp::sym("uuid"),
                Sexp::str(if pw.uuid.is_empty() { new_uuid() } else { pw.uuid.clone() }),
            ]),
        ]);
        let libnode = self.lib_node(&lib_id)?;
        let fake = Part {
            id: reference.clone(),
            lib: lib_id.clone(),
            at: [ax, ay],
            rot: pw.rot,
            mirror: pw.mirror.clone(),
            unit: 1,
            ..Default::default()
        };
        let rp = self.auto_prop_pos(&fake, libnode.prop("Reference"));
        let vp = pw
            .val_at
            .as_ref()
            .map(|v| At::from_json(v))
            .unwrap_or_else(|| self.auto_prop_pos(&fake, libnode.prop("Value")));
        let shown = if pw.value.is_empty() { pw.net.clone() } else { pw.value.clone() };
        node.push(property("Reference", &reference, rp.x, rp.y, 0.0, true, ""));
        node.push(property("Value", &shown, vp.x, vp.y, vp.rot as f64, false, &vp.justify));
        self.text_items.borrow_mut().push(TextItem {
            owner: reference.clone(),
            kind: "power".into(),
            text: shown,
            x: vp.x,
            y: vp.y,
            rot: vp.rot,
            justify: vp.justify.clone(),
            size: 1.27,
        });
        node.push(property("Footprint", "", ax, ay, 0.0, true, ""));
        node.push(property("Datasheet", "", ax, ay, 0.0, true, ""));
        node.push(property("Description", &info.description, ax, ay, 0.0, true, ""));
        node.push(Sexp::List(vec![
            Sexp::sym("pin"),
            Sexp::str(pin.map(|p| p.number).unwrap_or_else(|| "1".into())),
            Sexp::List(vec![Sexp::sym("uuid"), Sexp::str(new_uuid())]),
        ]));
        node.push(instances(&self.des, &reference, 1));
        Some(Sexp::List(node))
    }

    // ---- top level ---------------------------------------------------

    pub fn compile(&mut self) -> String {
        if self.des.uuid.is_empty() {
            self.des.uuid = new_uuid();
        }
        if self.des.sheet_path_uuid.is_empty() {
            self.des.sheet_path_uuid = format!("/{}", self.des.uuid);
        }
        let mut root = {
            let des = &self.des;
            vec![
                Sexp::sym("kicad_sch"),
                Sexp::List(vec![Sexp::sym("version"), Sexp::Int(VERSION)]),
                Sexp::List(vec![Sexp::sym("generator"), Sexp::str("eeschema")]),
                Sexp::List(vec![Sexp::sym("generator_version"), Sexp::str("9.0")]),
                Sexp::List(vec![Sexp::sym("uuid"), Sexp::str(&des.uuid)]),
                Sexp::List(vec![Sexp::sym("paper"), Sexp::str(&des.paper)]),
            ]
        };
        let trunc = |s: &str, n: usize| s.chars().take(n).collect::<String>();
        {
            let des = &self.des;
            let mut tb = vec![Sexp::sym("title_block")];
            if !des.title.is_empty() {
                tb.push(Sexp::List(vec![Sexp::sym("title"), Sexp::str(trunc(&des.title, 70))]));
            }
            if !des.date.is_empty() {
                tb.push(Sexp::List(vec![Sexp::sym("date"), Sexp::str(&des.date)]));
            }
            if !des.rev.is_empty() {
                tb.push(Sexp::List(vec![Sexp::sym("rev"), Sexp::str(&des.rev)]));
            }
            if !des.company.is_empty() {
                tb.push(Sexp::List(vec![Sexp::sym("company"), Sexp::str(&des.company)]));
            }
            for (i, c) in des.comments.iter().take(9).enumerate() {
                tb.push(Sexp::List(vec![Sexp::sym("comment"), Sexp::Int(i as i64 + 1), Sexp::str(trunc(c, 75))]));
            }
            if tb.len() > 1 {
                root.push(Sexp::List(tb));
            }
        }

        let parts = self.des.parts.clone();
        let part_nodes: Vec<Option<Sexp>> = parts.iter().map(|p| self.emit_part(p)).collect();
        let mut power = std::mem::take(&mut self.des.power);
        let mut used: std::collections::BTreeSet<String> =
            power.iter().filter(|pw| !pw.reference.is_empty()).map(|pw| pw.reference.clone()).collect();
        let mut n = 1usize;
        let mut pwr_nodes: Vec<Option<Sexp>> = Vec::new();
        for pw in power.iter_mut() {
            if pw.reference.is_empty() {
                while used.contains(&format!("#PWR{n:03}")) {
                    n += 1;
                }
                used.insert(format!("#PWR{n:03}"));
            }
            pwr_nodes.push(self.emit_power(pw, n));
        }
        self.des.power = power;

        let mut keys: Vec<String> = self.libs.borrow().iter().map(|(k, _)| k.clone()).collect();
        keys.sort();
        let mut lib_syms = vec![Sexp::sym("lib_symbols")];
        for k in keys {
            lib_syms.push(self.lib_node(&k).unwrap());
        }
        root.push(Sexp::List(lib_syms));

        for j in self.compute_junctions() {
            root.push(Sexp::List(vec![
                Sexp::sym("junction"),
                Sexp::List(vec![Sexp::sym("at"), num(j[0]), num(j[1])]),
                Sexp::List(vec![Sexp::sym("diameter"), Sexp::Int(0)]),
                Sexp::List(vec![Sexp::sym("color"), Sexp::Int(0), Sexp::Int(0), Sexp::Int(0), Sexp::Int(0)]),
                Sexp::List(vec![Sexp::sym("uuid"), Sexp::str(new_uuid())]),
            ]));
        }
        let des = &self.des;
        for pt in &des.nc {
            root.push(Sexp::List(vec![
                Sexp::sym("no_connect"),
                Sexp::List(vec![Sexp::sym("at"), num(pt[0]), num(pt[1])]),
                Sexp::List(vec![Sexp::sym("uuid"), Sexp::str(new_uuid())]),
            ]));
        }
        for w in &des.wires {
            for ab in w.windows(2) {
                root.push(Sexp::List(vec![
                    Sexp::sym("wire"),
                    Sexp::List(vec![
                        Sexp::sym("pts"),
                        Sexp::List(vec![Sexp::sym("xy"), num(ab[0][0]), num(ab[0][1])]),
                        Sexp::List(vec![Sexp::sym("xy"), num(ab[1][0]), num(ab[1][1])]),
                    ]),
                    stroke(),
                    Sexp::List(vec![Sexp::sym("uuid"), Sexp::str(new_uuid())]),
                ]));
            }
        }
        for r in &des.rects {
            root.push(Sexp::List(vec![
                Sexp::sym("rectangle"),
                Sexp::List(vec![Sexp::sym("start"), num(r.start[0]), num(r.start[1])]),
                Sexp::List(vec![Sexp::sym("end"), num(r.end[0]), num(r.end[1])]),
                stroke(),
                Sexp::List(vec![Sexp::sym("fill"), Sexp::List(vec![Sexp::sym("type"), Sexp::sym("none")])]),
                Sexp::List(vec![
                    Sexp::sym("uuid"),
                    Sexp::str(if r.uuid.is_empty() { new_uuid() } else { r.uuid.clone() }),
                ]),
            ]));
        }
        for t in &des.texts {
            root.push(Sexp::List(vec![
                Sexp::sym("text"),
                Sexp::str(&t.text),
                Sexp::List(vec![Sexp::sym("exclude_from_sim"), Sexp::sym("no")]),
                Sexp::List(vec![Sexp::sym("at"), num(t.at[0]), num(t.at[1]), num(t.rot as f64)]),
                font(t.size, t.bold, false, &t.justify),
                Sexp::List(vec![
                    Sexp::sym("uuid"),
                    Sexp::str(if t.uuid.is_empty() { new_uuid() } else { t.uuid.clone() }),
                ]),
            ]));
        }
        for l in &des.labels {
            let tag = match l.kind.as_str() {
                "global" => "global_label",
                "hier" => "hierarchical_label",
                _ => "label",
            };
            let mut node = vec![Sexp::sym(tag), Sexp::str(&l.text)];
            if l.kind != "local" {
                node.push(Sexp::List(vec![Sexp::sym("shape"), Sexp::sym(&l.shape)]));
            }
            node.push(Sexp::List(vec![Sexp::sym("at"), num(l.at[0]), num(l.at[1]), num(l.rot as f64)]));
            let just = if l.justify.is_empty() { label_justify(l) } else { l.justify.clone() };
            node.push(font(1.27, false, false, &just));
            node.push(Sexp::List(vec![
                Sexp::sym("uuid"),
                Sexp::str(if l.uuid.is_empty() { new_uuid() } else { l.uuid.clone() }),
            ]));
            root.push(Sexp::List(node));
        }
        for nd in part_nodes.into_iter().chain(pwr_nodes).flatten() {
            root.push(nd);
        }
        root.extend(des.extra_nodes.iter().cloned());
        root.push(Sexp::List(vec![
            Sexp::sym("sheet_instances"),
            Sexp::List(vec![Sexp::sym("path"), Sexp::str("/"), Sexp::List(vec![Sexp::sym("page"), Sexp::str("1")])]),
        ]));
        root.push(Sexp::List(vec![Sexp::sym("embedded_fonts"), Sexp::sym("no")]));
        sexp::dumps(&Sexp::List(root), 0) + "\n"
    }
}

fn stroke() -> Sexp {
    Sexp::List(vec![
        Sexp::sym("stroke"),
        Sexp::List(vec![Sexp::sym("width"), Sexp::Int(0)]),
        Sexp::List(vec![Sexp::sym("type"), Sexp::sym("default")]),
    ])
}

fn instances(des: &Design, reference: &str, unit: i32) -> Sexp {
    Sexp::List(vec![
        Sexp::sym("instances"),
        Sexp::List(vec![
            Sexp::sym("project"),
            Sexp::str(&des.project),
            Sexp::List(vec![
                Sexp::sym("path"),
                Sexp::str(&des.sheet_path_uuid),
                Sexp::List(vec![Sexp::sym("reference"), Sexp::str(reference)]),
                Sexp::List(vec![Sexp::sym("unit"), num(unit as f64)]),
            ]),
        ]),
    ])
}

fn side_of(d: (i32, i32)) -> &'static str {
    match d {
        (0, -1) => "top",
        (0, 1) => "bottom",
        (1, 0) => "right",
        (-1, 0) => "left",
        _ => "top",
    }
}

/// True when an axis-aligned segment runs through the interior of `bx`.
pub fn seg_in_box(a: [f64; 2], b: [f64; 2], bx: Box4, eps: f64) -> bool {
    let (x0, y0, x1, y1) = (bx[0] + eps, bx[1] + eps, bx[2] - eps, bx[3] - eps);
    if (a[0] - b[0]).abs() < 1e-6 {
        return x0 < a[0] && a[0] < x1 && a[1].max(b[1]) > y0 && a[1].min(b[1]) < y1;
    }
    if (a[1] - b[1]).abs() < 1e-6 {
        return y0 < a[1] && a[1] < y1 && a[0].max(b[0]) > x0 && a[0].min(b[0]) < x1;
    }
    false
}

/// KiCad anchors label text at the point, extending away along the rotation direction.
pub fn label_justify(l: &Label) -> String {
    let r = l.rot.rem_euclid(360);
    if l.kind == "local" {
        if r == 0 || r == 90 { "left bottom".into() } else { "right bottom".into() }
    } else if r == 0 || r == 90 {
        "left".into()
    } else {
        "right".into()
    }
}

/// True when `p` lies strictly inside the segment `a`-`b`.
pub fn on_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64), eps: f64) -> bool {
    let ((px, py), (ax, ay), (bx, by)) = (p, a, b);
    if (ax - bx).abs() < eps {
        return (px - ax).abs() < eps && ay.min(by) + eps < py && py < ay.max(by) - eps;
    }
    if (ay - by).abs() < eps {
        return (py - ay).abs() < eps && ax.min(bx) + eps < px && px < ax.max(bx) - eps;
    }
    let den = (bx - ax).powi(2) + (by - ay).powi(2);
    if den == 0.0 {
        return false;
    }
    let t = ((px - ax) * (bx - ax) + (py - ay) * (by - ay)) / den;
    if !(eps < t && t < 1.0 - eps) {
        return false;
    }
    let (cx, cy) = (ax + t * (bx - ax), ay + t * (by - ay));
    (cx - px).abs() < eps && (cy - py).abs() < eps
}

/// Compile a design into sheet text, returning the compiler so the checker can reuse its caches.
pub fn compile_design(des: Design) -> (String, Compiler) {
    let mut c = Compiler::new(des);
    let text = c.compile();
    (text, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_rounding() {
        // half-grid millimetres are exactly the ties Python's round() resolves downward
        assert_eq!(round_py(3.175, 2), 3.17);
        assert_eq!(round_py(5.715, 2), 5.71);
        assert_eq!(round_py(0.635, 2), 0.64);
    }
}
