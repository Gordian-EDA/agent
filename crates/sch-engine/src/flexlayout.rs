//! Flexbox-style schematic layout: the model describes each block as nested rows/columns of parts.
//!
//! ```text
//! design["layout"] = [
//!   {"title": "POWER", "note": "...", "tree":
//!      {"row": [ {"part": "J1"}, {"part": "F1", "rot": 90},
//!                {"col": [ {"part": "C1"}, {"part": "C2"} ], "gap": 4 },
//!                {"part": "U2"} ],
//!       "gap": 8, "align": "center"} }
//! ]
//! ```
//! Leaf: `{"part": id, "rot": 0|90|180|270, "mirror": ""|"y"}` (default: series passives horizontal, shunts vertical).
//! Container: `{"row": [...]}` or `{"col": [...]}` with `"gap"` (grid units, default 8) and `"align"`:
//! `"center"` (default: anchors/pin lines aligned), `"start"`, `"end"`, `"stretch-space"` (spread evenly).
//! Parts of the netlist that appear in no tree are auto-laid out in extra blocks.
//!
//! Port of `schagent/flexlayout.py`; algorithms, constants and iteration order follow the Python one-to-one.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::{json, Value};

use crate::engine::{is_power_net, strip_power_parts, GroupLayout, PartInst};
use crate::geo::{self, is_gnd, pack_blocks, snap_even, Geo};

pub const DEFAULT_GAP: f64 = 8.0;

// ---------------------------------------------------------------- JSON helpers

/// Python `str(v)` for the scalar shapes a layout tree can hold.
fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// Python `float(x)` over a JSON scalar, falling back to `dflt` when the key is absent or unusable.
fn fnum(v: Option<&Value>, dflt: f64) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(dflt),
        Some(Value::String(s)) => s.parse().unwrap_or(dflt),
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        _ => dflt,
    }
}

fn inum(v: Option<&Value>, dflt: i32) -> i32 {
    fnum(v, dflt as f64) as i32
}

fn sstr<'a>(node: &'a Value, key: &str, dflt: &'a str) -> &'a str {
    node.get(key).and_then(|v| v.as_str()).unwrap_or(dflt)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `repr()` of a plain string.
fn repr(s: &str) -> String {
    format!("'{s}'")
}

/// Python `round()`: half away from zero is *not* what CPython does — it rounds half to even.
fn py_round(v: f64) -> i64 {
    let r = v.round();
    if (v - v.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        (r - v.signum()) as i64
    } else {
        r as i64
    }
}

fn max_or0<I: Iterator<Item = f64>>(it: I) -> f64 {
    it.fold(None::<f64>, |a, b| Some(a.map_or(b, |a| a.max(b))))
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------- leaf geometry

/// `(w, h, ax, ay)`: box size and the anchor offset inside the box (box top-left at 0,0).
fn leaf_box(p: &PartInst, rot: i32, mirror: &str) -> (f64, f64, f64, f64) {
    let e = p.extent_of([0.0, 0.0], rot, mirror);
    (e[2] - e[0], e[3] - e[1], -e[0], -e[1])
}

/// Is `net` a rail of this group (present, but not a signal net)?
fn is_rail(gl: &GroupLayout, net: &str) -> bool {
    gl.nets.contains_key(net) && !gl.signal_nets.contains_key(net)
}

fn default_orient(p: &PartInst, gl: &GroupLayout, axis: &str) -> (i32, String) {
    if p.is_connector || p.pins.len() > 4 {
        return (0, String::new());
    }
    if !p.two_pin {
        // transistors etc.: upright, choosing rot 0/180 and mirror so a supply pin points up and a ground pin down
        let mut best = (0, String::new());
        let mut best_s: Option<f64> = None;
        for (rot, mirror) in [(0, ""), (0, "y"), (180, ""), (180, "y")] {
            let mut sc = 0.0f64;
            for pin in &p.pins {
                let Some(net) = p.pinmap.get(&pin.number) else { continue };
                if is_rail(gl, net) {
                    let d = p.pin_dir_at(pin, rot, mirror);
                    let want = if is_gnd(net) { (0, 1) } else { (0, -1) };
                    sc += if d == want {
                        0.0
                    } else if d.1 == 0 {
                        6.0
                    } else {
                        20.0
                    };
                }
            }
            sc += if rot == 0 { 0.0 } else { 1.0 };
            sc += if mirror.is_empty() { 0.0 } else { 0.5 };
            if best_s.is_none_or(|b| sc < b) {
                best = (rot, mirror.to_string());
                best_s = Some(sc);
            }
        }
        return best;
    }
    let nets: Vec<Option<&String>> = p.pins.iter().map(|pin| p.pinmap.get(&pin.number)).collect();
    let rails: Vec<&String> = nets
        .iter()
        .filter_map(|n| *n)
        .filter(|n| is_rail(gl, n))
        .collect();
    // shunt to ground, or between two rails
    let on_rail = rails.iter().any(|n| is_gnd(n)) || rails.len() == 2;
    if on_rail || axis == "col" {
        // vertical: ground pin down / supply pin up
        let vrots = rots_with_axis(p, "v");
        for rot in &vrots {
            for pin in &p.pins {
                let Some(net) = p.pinmap.get(&pin.number) else { continue };
                let d = p.pin_dir_at(pin, *rot, "");
                if is_rail(gl, net)
                    && ((is_gnd(net) && d == (0, 1)) || (!is_gnd(net) && d == (0, -1)))
                {
                    return (*rot, String::new());
                }
            }
        }
        return (vrots.first().copied().unwrap_or(0), String::new());
    }
    // horizontal series part (in a row)
    let hrots = rots_with_axis(p, "h");
    (hrots.first().copied().unwrap_or(0), String::new())
}

/// Rotations (preferring 0/90) that put a 2-pin part's pins on a vertical (`v`) or horizontal (`h`) axis.
fn rots_with_axis(p: &PartInst, axis: &str) -> Vec<i32> {
    let mut out = Vec::new();
    if p.pins.len() < 2 {
        return out;
    }
    for rot in [0, 90, 180, 270] {
        let a = p.pin_pos_at(&p.pins[0], [0.0, 0.0], rot, "");
        let b = p.pin_pos_at(&p.pins[1], [0.0, 0.0], rot, "");
        let horiz = (a[1] - b[1]).abs() < 0.01;
        if (axis == "h") == horiz {
            out.push(rot);
        }
    }
    out
}

/// The model's `rot` for 2-pin parts follows the R/C convention (0 = standing, 90 = lying); map it onto parts
/// whose library drawing is horizontal (crystals, diodes, LEDs) so 90 still means "lying along the row".
fn explicit_two_pin_rot(p: &PartInst, rot: i32) -> i32 {
    let want = if rot == 90 || rot == 270 { "h" } else { "v" };
    let rots = rots_with_axis(p, want);
    if rots.is_empty() || rots.contains(&rot) {
        return rot;
    }
    let base = rots[0];
    if rot == 180 || rot == 270 {
        (base + 180).rem_euclid(360)
    } else {
        base
    }
}

// ---------------------------------------------------------------- measured tree

/// A measured layout node (the Python dict returned by `_measure`).
#[derive(Clone, Debug)]
struct MNode {
    /// `"leaf"`, `"row"` or `"col"`.
    kind: &'static str,
    w: f64,
    h: f64,
    /// Alignment line (may differ from the anchor for 2-pin parts standing across the axis).
    ax: f64,
    ay: f64,
    /// Anchor offset inside the box.
    aax: f64,
    aay: f64,
    /// Index into `GroupLayout::parts`.
    part: Option<usize>,
    rot: i32,
    mirror: String,
    children: Vec<MNode>,
    gap: f64,
    align: String,
    /// Main-axis child positions imposed by [`align_col_to_pins`].
    offsets: Option<Vec<f64>>,
}

impl MNode {
    fn err_leaf() -> MNode {
        MNode {
            kind: "leaf",
            w: 4.0,
            h: 4.0,
            ax: 2.0,
            ay: 2.0,
            aax: 2.0,
            aay: 2.0,
            part: None,
            rot: 0,
            mirror: String::new(),
            children: Vec::new(),
            gap: 0.0,
            align: "center".to_string(),
            offsets: None,
        }
    }

    fn set_box(&mut self, b: (f64, f64, f64, f64)) {
        self.w = b.0;
        self.h = b.1;
        self.ax = b.2;
        self.ay = b.3;
        self.aax = b.2;
        self.aay = b.3;
    }
}

/// Recursively compute sizes of a layout tree node.
fn measure(
    node: &mut Value,
    gl: &GroupLayout,
    by_id: &HashMap<String, usize>,
    errors: &mut Vec<String>,
    path: &str,
    axis: &str,
) -> MNode {
    if node.get("part").is_some() {
        return measure_leaf(node, gl, by_id, errors, path, axis);
    }
    let kind: &'static str = if node.get("row").is_some() {
        "row"
    } else if node.get("col").is_some() {
        "col"
    } else {
        errors.push(format!("{path}: node must have 'part', 'row' or 'col'"));
        return MNode::err_leaf();
    };
    let n = node[kind].as_array().map_or(0, |a| a.len());
    let mut kids = Vec::with_capacity(n);
    for i in 0..n {
        let sub = format!("{path}.{kind}[{i}]");
        kids.push(measure(&mut node[kind][i], gl, by_id, errors, &sub, kind));
    }
    {
        let empty = Vec::new();
        let raws = node[kind].as_array().unwrap_or(&empty);
        face_neighbours(&mut kids, kind, raws, gl);
    }
    if kind == "row" {
        // grid-like alignment: a column next to a multi-pin part lines its children up with the pins they connect to
        for i in 0..kids.len() {
            if kids[i].kind != "col" {
                continue;
            }
            for (j, side) in [(i as i64 - 1, -1i32), (i as i64 + 1, 1)] {
                if j < 0 || j as usize >= kids.len() {
                    continue;
                }
                let j = j as usize;
                let ok = kids[j].kind == "leaf"
                    && kids[j].part.is_some_and(|pi| {
                        gl.parts[pi].pins.len() >= 3 && !gl.parts[pi].is_connector
                    });
                if ok {
                    let ic = kids[j].clone();
                    if align_col_to_pins(&mut kids[i], &ic, side, gl) {
                        break;
                    }
                }
            }
        }
    }
    // flex-wrap: a row wider than a sheet column (~150 units) wraps into several rows
    let maxw = fnum(node.get("wrap"), 150.0);
    let gap0 = fnum(node.get("gap"), DEFAULT_GAP);
    if kind == "row"
        && !truthy(node.get("_wrapped"))
        && kids.len() >= 2
        && kids.iter().map(|k| k.w).sum::<f64>() + gap0 * (kids.len() - 1) as f64 > maxw
    {
        if let Some(mut wrapped) = wrap_row(node, &kids, gl, maxw, gap0, path, errors) {
            return measure(&mut wrapped, gl, by_id, errors, path, axis);
        }
    }
    let mut gap = gap0;
    let big = kids.iter().any(|k| {
        k.kind == "leaf" && k.part.is_some_and(|pi| gl.parts[pi].pins.len() > 8)
    });
    if big {
        gap = gap.max(10.0);
    }
    let align = sstr(node, "align", "center").to_string();
    let (w, h, ax, ay);
    if kind == "row" {
        w = kids.iter().map(|k| k.w).sum::<f64>() + gap * kids.len().saturating_sub(1) as f64;
        // cross axis: align anchors -> compute above/below extents relative to the anchor line
        let above = max_or0(kids.iter().map(|k| k.ay));
        let below = max_or0(kids.iter().map(|k| k.h - k.ay));
        if align == "center" {
            h = above + below;
            ay = above;
        } else {
            h = max_or0(kids.iter().map(|k| k.h));
            ay = h / 2.0;
        }
        ax = kids.first().map_or(w / 2.0, |k| k.ax);
    } else {
        h = kids.iter().map(|k| k.h).sum::<f64>() + gap * kids.len().saturating_sub(1) as f64;
        let left = max_or0(kids.iter().map(|k| k.ax));
        let right = max_or0(kids.iter().map(|k| k.w - k.ax));
        if align == "center" {
            w = left + right;
            ax = left;
        } else {
            w = max_or0(kids.iter().map(|k| k.w));
            ax = w / 2.0;
        }
        ay = kids.first().map_or(h / 2.0, |k| k.ay);
    }
    MNode {
        kind,
        w,
        h,
        ax,
        ay,
        aax: ax,
        aay: ay,
        part: None,
        rot: 0,
        mirror: String::new(),
        children: kids,
        gap,
        align,
        offsets: None,
    }
}

fn measure_leaf(
    node: &mut Value,
    gl: &GroupLayout,
    by_id: &HashMap<String, usize>,
    errors: &mut Vec<String>,
    path: &str,
    axis: &str,
) -> MNode {
    let pid = value_str(&node["part"]);
    let Some(&pi) = by_id.get(&pid) else {
        errors.push(format!(
            "{path}: unknown part {} (parts of this block only)",
            repr(&pid)
        ));
        return MNode::err_leaf();
    };
    let p = &gl.parts[pi];
    let has_rot = node.get("rot").is_some();
    let (mut rot, mut mirror) = if has_rot {
        (inum(node.get("rot"), 0), sstr(node, "mirror", "").to_string())
    } else {
        default_orient(p, gl, axis)
    };
    if has_rot && p.two_pin && !p.is_connector {
        rot = explicit_two_pin_rot(p, rot);
        // an explicit orientation never points a ground pin up or a supply pin down
        for pin in &p.pins {
            let Some(net) = p.pinmap.get(&pin.number) else { continue };
            if is_rail(gl, net) {
                let dd = p.pin_dir_at(pin, rot, &mirror);
                if (is_gnd(net) && dd == (0, -1)) || (!is_gnd(net) && dd == (0, 1)) {
                    rot = (rot + 180).rem_euclid(360);
                    let dir = if is_gnd(net) { "down" } else { "up" };
                    errors.push(format!("note: {pid} was flipped so its {net} pin points {dir}"));
                    break;
                }
            }
        }
    }
    if node.get("mirror").is_some() && !has_rot {
        mirror = sstr(node, "mirror", "").to_string();
    }
    if has_rot && p.two_pin && !p.is_connector && p.pins.len() >= 2 {
        // a shunt to ground laid flat along a row is (almost) always wrong: stand it up
        let grounded = p
            .pins
            .iter()
            .filter_map(|pin| p.pinmap.get(&pin.number))
            .any(|n| !n.is_empty() && is_gnd(n));
        if grounded {
            let a = p.pin_pos_at(&p.pins[0], [0.0, 0.0], rot, &mirror);
            let b = p.pin_pos_at(&p.pins[1], [0.0, 0.0], rot, &mirror);
            if (a[1] - b[1]).abs() < 0.01 {
                (rot, mirror) = default_orient(p, gl, axis);
                errors.push(format!(
                    "note: {pid} is a shunt to ground; its explicit rot was ignored (shunts stand vertical)"
                ));
                if let Some(o) = node.as_object_mut() {
                    o.remove("rot");
                }
            }
        }
    }
    let mut leaf = MNode {
        kind: "leaf",
        part: Some(pi),
        rot,
        mirror,
        ..MNode::err_leaf()
    };
    leaf.set_box(leaf_box(p, leaf.rot, &leaf.mirror));
    align_line(&mut leaf, axis, gl);
    leaf
}

/// Build the wrapped replacement tree for an over-wide row (`None` when no wrap is needed).
fn wrap_row(
    node: &Value,
    kids: &[MNode],
    gl: &GroupLayout,
    maxw: f64,
    gap0: f64,
    path: &str,
    errors: &mut Vec<String>,
) -> Option<Value> {
    fn is_ic(k: &MNode, gl: &GroupLayout) -> bool {
        if k.kind == "leaf" {
            k.part
                .is_some_and(|pi| gl.parts[pi].pins.len() > 8 && !gl.parts[pi].is_connector)
        } else {
            k.children.iter().any(|c| is_ic(c, gl))
        }
    }
    let kind = if node.get("row").is_some() { "row" } else { "col" };
    let empty = Vec::new();
    let raws = node[kind].as_array().unwrap_or(&empty);
    let align = sstr(node, "align", "center").to_string();
    let tall: Vec<usize> = (0..kids.len()).filter(|&i| is_ic(&kids[i], gl)).collect();
    let rest: Vec<usize> = (0..kids.len()).filter(|&i| !is_ic(&kids[i], gl)).collect();
    let pack = |idxs: &[usize], limit: f64| -> Vec<Vec<usize>> {
        let mut rows: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        let mut curw = 0.0f64;
        for &i in idxs {
            if !cur.is_empty() && curw + gap0 + kids[i].w > limit {
                rows.push(std::mem::take(&mut cur));
                curw = 0.0;
            }
            cur.push(i);
            curw += (if cur.len() > 1 { gap0 } else { 0.0 }) + kids[i].w;
        }
        rows.push(cur);
        rows
    };
    let row_json = |r: &[usize]| {
        json!({"row": r.iter().map(|&i| raws[i].clone()).collect::<Vec<_>>(),
               "gap": gap0, "align": align, "_wrapped": true})
    };
    if !tall.is_empty() && !rest.is_empty() {
        // keep the tall item(s) (an MCU) and stack the small items in wrapped rows beside it
        let avail = maxw - tall.iter().map(|&i| kids[i].w).sum::<f64>() - gap0 * tall.len() as f64;
        let rows = pack(&rest, avail.max(70.0));
        let side = json!({"col": rows.iter().map(|r| row_json(r)).collect::<Vec<_>>(),
                          "gap": 10, "align": "start"});
        let mut items: Vec<Value> = tall.iter().map(|&i| raws[i].clone()).collect();
        items.push(side);
        errors.push(format!(
            "note: {path} row was wider than {maxw:.0} units; small items were wrapped into {} rows beside the tall part",
            rows.len()
        ));
        return Some(json!({"row": items, "gap": gap0, "align": "start", "_wrapped": true}));
    }
    let rows = pack(&(0..kids.len()).collect::<Vec<_>>(), maxw);
    if rows.len() > 1 {
        errors.push(format!(
            "note: {path} row was wider than {maxw:.0} units and was wrapped into {} rows",
            rows.len()
        ));
        return Some(json!({"col": rows.iter().map(|r| row_json(r)).collect::<Vec<_>>(),
                           "gap": 10, "align": "start"}));
    }
    None
}

/// Signal nets of a measured node's subtree.
fn node_nets(k: &MNode, gl: &GroupLayout) -> HashSet<String> {
    if k.kind == "leaf" {
        return match k.part {
            Some(pi) => gl.parts[pi]
                .pinmap
                .values()
                .filter(|n| gl.signal_nets.contains_key(n))
                .cloned()
                .collect(),
            None => HashSet::new(),
        };
    }
    let mut out = HashSet::new();
    for c in &k.children {
        out.extend(node_nets(c, gl));
    }
    out
}

/// `col`: measured column beside the multi-pin leaf `ic` (side -1: ic is left of the column, +1: right). Children
/// connected to the ic's facing pins are moved onto those pins' lines (order kept, gaps stretched as needed).
/// Sets `col.offsets` (main-axis child positions) and the column's alignment line. Returns true when applied.
fn align_col_to_pins(col: &mut MNode, ic: &MNode, side: i32, gl: &GroupLayout) -> bool {
    let p = &gl.parts[ic.part.expect("ic leaf has a part")];
    let want = if side == -1 { (1, 0) } else { (-1, 0) }; // ic pins pointing toward the column
    let line = ic.ay - ic.aay; // ic alignment line, relative to its anchor
    let mut pin_t: HashMap<String, f64> = HashMap::new();
    for pin in &p.pins {
        let Some(net) = p.pinmap.get(&pin.number) else { continue };
        if gl.signal_nets.contains_key(net)
            && p.pin_dir_at(pin, ic.rot, &ic.mirror) == want
        {
            let y = p.pin_pos_at(pin, [0.0, 0.0], ic.rot, &ic.mirror)[1] - line;
            pin_t.entry(net.clone()).or_insert(y);
        }
    }
    // pins on the ic's top/bottom edge: a child hanging off them sits just above/below that edge, beside the ic,
    // so its wire is a short hook instead of a loop over the whole symbol
    for pin in &p.pins {
        let Some(net) = p.pinmap.get(&pin.number) else { continue };
        let dd = p.pin_dir_at(pin, ic.rot, &ic.mirror);
        if gl.signal_nets.contains_key(net)
            && !pin_t.contains_key(net)
            && (dd == (0, -1) || dd == (0, 1))
        {
            let y = p.pin_pos_at(pin, [0.0, 0.0], ic.rot, &ic.mirror)[1] - line
                + dd.1 as f64 * 4.0;
            pin_t.insert(net.clone(), y);
        }
    }
    if pin_t.is_empty() {
        return false;
    }
    let mut targets: Vec<Option<f64>> = Vec::new();
    for k in &col.children {
        let mut ts: Vec<f64> = node_nets(k, gl)
            .iter()
            .filter_map(|n| pin_t.get(n).copied())
            .collect();
        ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
        targets.push(ts.first().copied());
    }
    if targets.iter().all(|t| t.is_none()) {
        return false;
    }
    let gap = col.gap;
    let (mut cursor, mut l, mut offs) = (0.0f64, None::<f64>, Vec::new());
    for (k, t) in col.children.iter().zip(targets.iter()) {
        let mut y = cursor;
        if let Some(t) = t {
            match l {
                None => l = Some(y + k.ay - t),
                Some(lv) => y = cursor.max(lv + t - k.ay),
            }
        }
        offs.push(y);
        cursor = y + k.h + gap;
    }
    col.offsets = Some(offs);
    col.h = cursor - gap;
    col.ay = l.expect("at least one target");
    true
}

/// Alignment line of a leaf: for a 2-pin part standing across the container axis (a shunt in a row, a series part
/// in a column) align its NEAR pin on the line so the through-wire passes its pin tip; otherwise the anchor.
fn align_line(leaf: &mut MNode, axis: &str, gl: &GroupLayout) {
    let two_pin = leaf
        .part
        .is_some_and(|pi| gl.parts[pi].two_pin && gl.parts[pi].pins.len() >= 2);
    if !two_pin {
        leaf.ax = leaf.aax;
        leaf.ay = leaf.aay;
        return;
    }
    let p = &gl.parts[leaf.part.unwrap()];
    let pos: Vec<[f64; 2]> = p
        .pins
        .iter()
        .map(|pin| p.pin_pos_at(pin, [0.0, 0.0], leaf.rot, &leaf.mirror))
        .collect();
    let vertical = (pos[0][0] - pos[1][0]).abs() < 0.01;
    if axis == "row" && vertical {
        leaf.ay = leaf.aay + pos[0][1].min(pos[1][1]);
    } else if axis == "col" && !vertical {
        leaf.ax = leaf.aax + pos[0][0].min(pos[1][0]);
    }
}

// ---------------------------------------------------------------- lint / fold

fn tree_ids(node: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = node.get("part") {
        out.push(value_str(p));
    }
    for k in ["row", "col"] {
        if let Some(a) = node.get(k).and_then(|v| v.as_array()) {
            for c in a {
                out.extend(tree_ids(c));
            }
        }
    }
    out
}

fn tree_of(blk: &Value) -> &Value {
    const EMPTY: Value = Value::Null;
    blk.get("tree").unwrap_or(&EMPTY)
}

fn id_set(blk: &Value) -> BTreeSet<String> {
    tree_ids(tree_of(blk)).into_iter().collect()
}

fn title_of(blk: &Value) -> String {
    sstr(blk, "title", "").to_string()
}

/// Remove and return the leaf `{"part": key}` from `node`'s subtree (None if absent).
fn take_leaf(node: &mut Value, key: &str) -> Option<Value> {
    for kind in ["row", "col"] {
        if node.get(kind).is_none() {
            continue;
        }
        let len = node[kind].as_array().map_or(0, |a| a.len());
        for i in 0..len {
            if node[kind][i].get("part").and_then(|v| v.as_str()) == Some(key) {
                return Some(node[kind].as_array_mut().unwrap().remove(i));
            }
            if let Some(got) = take_leaf(&mut node[kind][i], key) {
                return Some(got);
            }
        }
    }
    None
}

fn replace_leaf(node: &mut Value, key: &str, new: &Value) -> bool {
    for kind in ["row", "col"] {
        if node.get(kind).is_none() {
            continue;
        }
        let len = node[kind].as_array().map_or(0, |a| a.len());
        for i in 0..len {
            if node[kind][i].get("part").and_then(|v| v.as_str()) == Some(key) {
                node[kind][i] = new.clone();
                return true;
            }
            if replace_leaf(&mut node[kind][i], key, new) {
                return true;
            }
        }
    }
    false
}

/// Drop empty containers left behind by [`take_leaf`].
fn prune(node: &mut Value) -> bool {
    for kind in ["row", "col"] {
        if node.get(kind).is_none() {
            continue;
        }
        let taken = node[kind]
            .as_array_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut kept = Vec::new();
        for mut c in taken {
            if prune(&mut c) {
                kept.push(c);
            }
        }
        let has = !kept.is_empty();
        node[kind] = Value::Array(kept);
        return has;
    }
    true
}

/// Auto-fix fragmented / badly proportioned trees. Returns `(layout, notes)`.
fn lint_layout(
    layout: &[Value],
    parts: &[PartInst],
    by_id: &HashMap<String, usize>,
    power: &HashSet<String>,
) -> (Vec<Value>, Vec<String>) {
    let mut notes: Vec<String> = Vec::new();
    let mut layout: Vec<Value> = layout.to_vec();
    if layout.is_empty() {
        return (layout, notes);
    }
    let nets_of_tree = |tree: &Value| -> HashSet<String> {
        let mut out = HashSet::new();
        for i in tree_ids(tree) {
            if let Some(&pi) = by_id.get(&i) {
                for n in parts[pi].pinmap.values() {
                    if n != "nc" && n != "float" && !is_power_net(n, power) {
                        out.insert(n.clone());
                    }
                }
            }
        }
        out
    };

    // 0. unused units of a multi-unit IC (all pins nc/float) sit in a row beside that IC's power unit
    let all_keys: BTreeSet<String> = layout.iter().flat_map(|b| tree_ids(tree_of(b))).collect();
    let pin_nets = |pi: usize| -> Vec<&str> {
        parts[pi]
            .pins
            .iter()
            .map(|pin| {
                parts[pi]
                    .pinmap
                    .get(&pin.number)
                    .map_or("nc", |s| s.as_str())
            })
            .collect()
    };
    for key in &all_keys {
        let Some(&pi) = by_id.get(key) else { continue };
        if !key.contains('#') {
            continue;
        }
        if !pin_nets(pi)
            .iter()
            .all(|n| matches!(*n, "nc" | "float" | ""))
        {
            continue;
        }
        let base = key.split('#').next().unwrap();
        // NOTE: Python iterates an unordered set here and takes pw[0]; the sorted BTreeSet makes this deterministic.
        let pw: Vec<&String> = all_keys
            .iter()
            .filter(|k| k.split('#').next().unwrap() == base && *k != key)
            .filter(|k| by_id.contains_key(*k))
            .filter(|k| {
                let qi = by_id[*k];
                let q = &parts[qi];
                q.pins.iter().all(|pin| {
                    let v = q.pinmap.get(&pin.number);
                    is_power_net(v.map_or("nc", |s: &String| s.as_str()), power)
                        || matches!(v.map(|s| s.as_str()), Some("nc") | Some("float") | None)
                }) && q.pins.iter().any(|pin| {
                    is_power_net(q.pinmap.get(&pin.number).map_or("", |s: &String| s.as_str()), power)
                })
            })
            .collect();
        let Some(pw0) = pw.first().map(|s| (*s).clone()) else { continue };
        let mut leaf: Option<Value> = None;
        for blk in layout.iter_mut() {
            if tree_of(blk).get("part").and_then(|v| v.as_str()) == Some(key.as_str()) {
                continue;
            }
            let Some(t) = blk.get_mut("tree") else { continue };
            if let Some(got) = take_leaf(t, key) {
                prune(t);
                leaf = Some(got);
                break;
            }
        }
        let Some(leaf) = leaf else { continue };
        for blk in layout.iter_mut() {
            if tree_of(blk).get("part").and_then(|v| v.as_str()) == Some(pw0.as_str()) {
                let t = blk["tree"].clone();
                blk["tree"] = json!({"row": [t, leaf], "gap": 4, "align": "center"});
                break;
            }
            let new = json!({"row": [{"part": pw0}, leaf], "gap": 4, "align": "center"});
            if let Some(t) = blk.get_mut("tree") {
                if replace_leaf(t, &pw0, &new) {
                    break;
                }
            }
        }
        notes.push(format!(
            "note: unused unit {key} was moved next to the power unit {pw0}"
        ));
    }
    layout.retain(|blk| !tree_ids(tree_of(blk)).is_empty());

    // 1. fold blocks with < 3 small parts into the block sharing the most signal nets (bounded target size)
    let big_part = |ids: &BTreeSet<String>| {
        ids.iter()
            .any(|i| by_id.get(i).is_some_and(|&pi| parts[pi].pins.len() >= 8))
    };
    let mut changed = true;
    while changed && layout.len() > 1 {
        changed = false;
        for i in 0..layout.len() {
            let ids = id_set(&layout[i]);
            if ids.is_empty() || ids.len() >= 3 || big_part(&ids) {
                continue;
            }
            let mine = nets_of_tree(&layout[i]["tree"]);
            let (mut best, mut best_w) = (None, 0usize);
            for j in 0..layout.len() {
                let oids = id_set(&layout[j]);
                if j == i || oids.len() < 3 || oids.len() >= 12 {
                    continue;
                }
                let w = nets_of_tree(&layout[j]["tree"])
                    .intersection(&mine)
                    .count();
                if w > best_w {
                    best = Some(j);
                    best_w = w;
                }
            }
            let Some(bj) = best else { continue };
            let blk_tree = layout[i]["tree"].clone();
            let blk_title = title_of(&layout[i]);
            let tgt_title = title_of(&layout[bj]);
            // keep the target balanced: a wide row gets the small tree stacked below, a column gets it beside
            let t = layout[bj]["tree"].clone();
            let row_len = t.get("row").and_then(|v| v.as_array()).map(|a| a.len());
            match row_len {
                Some(n) if n < 3 => {
                    layout[bj]["tree"]["row"]
                        .as_array_mut()
                        .unwrap()
                        .push(blk_tree);
                }
                Some(_) => {
                    layout[bj]["tree"] = json!({"col": [t, blk_tree], "gap": 8, "align": "start"});
                }
                None if t.get("col").is_some() => {
                    layout[bj]["tree"] = json!({"row": [t, blk_tree], "gap": 10});
                }
                None => {
                    layout[bj]["tree"] = json!({"row": [t, blk_tree], "gap": 8});
                }
            }
            notes.push(format!(
                "note: block '{blk_title}' ({} parts) was folded into '{tgt_title}'; \
                 define it inside that block's tree yourself next time",
                ids.len()
            ));
            layout.remove(i);
            changed = true;
            break;
        }
    }

    // 1b. remaining tiny blocks (no good target): gather them into one block
    let tiny: Vec<usize> = (0..layout.len())
        .filter(|&i| {
            let ids = id_set(&layout[i]);
            !ids.is_empty() && ids.len() < 3 && !big_part(&ids)
        })
        .collect();
    if tiny.len() >= 2 {
        let trees: Vec<Value> = tiny.iter().map(|&i| layout[i]["tree"].clone()).collect();
        let titles: Vec<String> = tiny.iter().map(|&i| title_of(&layout[i])).collect();
        for &i in tiny.iter().rev() {
            layout.remove(i);
        }
        let rows: Vec<Value> = trees
            .chunks(4)
            .map(|c| json!({"row": c.to_vec(), "gap": 10, "align": "start"}))
            .collect();
        layout.push(json!({"title": "MISC", "tree": {"col": rows, "gap": 10, "align": "start"}}));
        let listed: Vec<String> = titles.iter().take(6).map(|t| format!("'{t}'")).collect();
        notes.push(format!(
            "note: small blocks {} were gathered into one MISC block; \
             compose them inside the blocks they belong to next time",
            listed.join(", ")
        ));
    }

    // 2. very tall top-level columns: split their children into two side-by-side columns
    for blk in layout.iter_mut() {
        let t = tree_of(blk).clone();
        let Some(kids) = t.get("col").and_then(|v| v.as_array()) else { continue };
        if kids.len() < 4 {
            continue;
        }
        let half = kids.len().div_ceil(2);
        let g = fnum(t.get("gap"), DEFAULT_GAP);
        let title = title_of(blk);
        let n = kids.len();
        blk["tree"] = json!({"row": [{"col": kids[..half].to_vec(), "gap": g},
                                     {"col": kids[half..].to_vec(), "gap": g}], "gap": 12});
        notes.push(format!(
            "note: block '{title}' was a column of {n} items; it was split into two \
             side-by-side columns for a balanced block (compose it that way yourself next time)"
        ));
    }
    (layout, notes)
}

// ---------------------------------------------------------------- neighbours

fn leaf_nets(k: &MNode, gl: &GroupLayout) -> HashSet<String> {
    match k.part {
        Some(pi) => gl.parts[pi]
            .pins
            .iter()
            .filter_map(|pin| gl.parts[pi].pinmap.get(&pin.number))
            .filter(|n| *n != "nc" && *n != "float")
            .cloned()
            .collect(),
        None => HashSet::new(),
    }
}

/// Series 2-pin parts whose orientation was not given explicitly: put the pin that shares a net with the previous
/// sibling on the near side (left in a row, top in a column).
fn face_neighbours(kids: &mut [MNode], kind: &str, raw_nodes: &[Value], gl: &GroupLayout) {
    let rails: HashSet<String> = gl
        .nets
        .keys()
        .filter(|n| !gl.signal_nets.contains_key(n))
        .cloned()
        .collect();
    for i in 0..kids.len() {
        if kids[i].kind != "leaf" {
            continue;
        }
        let Some(pi) = kids[i].part else { continue };
        if raw_nodes.get(i).is_some_and(|n| n.get("rot").is_some()) {
            continue;
        }
        let p = &gl.parts[pi];
        let prev_is = i > 0;
        let next_is = i + 1 < kids.len();
        let pn = if prev_is && kids[i - 1].kind == "leaf" {
            leaf_nets(&kids[i - 1], gl)
        } else {
            HashSet::new()
        };
        let nn = if next_is && kids[i + 1].kind == "leaf" {
            leaf_nets(&kids[i + 1], gl)
        } else {
            HashSet::new()
        };
        if p.is_connector {
            // a connector at the start/end of a row (column) faces the circuit: mirror it when all its pins point
            // away from its only neighbour (humans mirror the symbol rather than draw wires around it)
            if raw_nodes.get(i).is_some_and(|n| n.get("mirror").is_some()) || prev_is == next_is {
                continue;
            }
            let dirs: HashSet<(i32, i32)> = p
                .pins
                .iter()
                .map(|pin| p.pin_dir_at(pin, kids[i].rot, &kids[i].mirror))
                .collect();
            if dirs.len() != 1 {
                continue;
            }
            let d = *dirs.iter().next().unwrap();
            let want = if kind == "row" {
                ((next_is && d == (-1, 0)) || (prev_is && d == (1, 0))).then_some("y")
            } else {
                ((next_is && d == (0, -1)) || (prev_is && d == (0, 1))).then_some("x")
            };
            if let Some(w) = want {
                kids[i].mirror = w.to_string();
                let b = leaf_box(p, kids[i].rot, &kids[i].mirror);
                kids[i].set_box(b);
                align_line(&mut kids[i], kind, gl);
            }
            continue;
        }
        if !p.two_pin {
            // multi-pin part: align on the pin that connects to the previous sibling (else the next one), so a
            // series passive feeds straight into e.g. an op-amp input instead of the symbol's centre line
            let want_dir = if kind == "row" { (-1, 0) } else { (0, -1) };
            for (nets_, dd) in [(&pn, want_dir), (&nn, (-want_dir.0, -want_dir.1))] {
                let hit = p.pins.iter().find(|pin| {
                    p.pinmap
                        .get(&pin.number)
                        .is_some_and(|n| nets_.contains(n))
                        && p.pin_dir_at(pin, kids[i].rot, &kids[i].mirror) == dd
                });
                if let Some(pin) = hit {
                    let pp = p.pin_pos_at(pin, [0.0, 0.0], kids[i].rot, &kids[i].mirror);
                    if kind == "row" {
                        kids[i].ay = kids[i].aay + pp[1];
                    } else {
                        kids[i].ax = kids[i].aax + pp[0];
                    }
                    break;
                }
            }
            continue;
        }
        if p.pins.len() < 2 {
            continue;
        }
        let pin_positions = |k: &MNode| -> Vec<[f64; 2]> {
            p.pins
                .iter()
                .map(|pin| p.pin_pos_at(pin, [0.0, 0.0], k.rot, &k.mirror))
                .collect()
        };
        let mut pos = pin_positions(&kids[i]);
        let vertical = (pos[0][0] - pos[1][0]).abs() < 0.01;
        let mut across = (kind == "row" && vertical) || (kind == "col" && !vertical);
        if across && kind == "row" && prev_is && kids[i - 1].kind == "leaf" {
            // a rail-terminated series element (R -> LED -> GND at the end of a row) lies along the row like its
            // neighbours; only a true shunt (both neighbours on its top net) stands across
            let nets2: Vec<Option<&str>> = p
                .pins
                .iter()
                .map(|pin| p.pinmap.get(&pin.number).map(|s| s.as_str()))
                .collect();
            let rail: Vec<&str> = nets2
                .iter()
                .filter_map(|n| *n)
                .filter(|n| is_rail(gl, n))
                .collect();
            let sig: Vec<Option<&str>> = nets2
                .iter()
                .copied()
                .filter(|n| match n {
                    Some(s) => !rail.contains(s),
                    None => true,
                })
                .collect();
            let matches_prev = rail.len() == 1
                && sig.len() == 1
                && sig[0].is_some_and(|s| pn.contains(s))
                && !sig[0].is_some_and(|s| nn.contains(s));
            if matches_prev {
                if let Some(&hr) = rots_with_axis(p, "h").first() {
                    kids[i].rot = hr;
                    kids[i].mirror = String::new();
                    let b = leaf_box(p, kids[i].rot, "");
                    kids[i].set_box(b);
                    align_line(&mut kids[i], kind, gl);
                    pos = pin_positions(&kids[i]);
                    across = false;
                }
            }
        }
        // part standing across the axis (shunt): the pin that touches the neighbours' net must be on the
        // container's line (top pin in a row, left pin in a column)
        let first_is_near = if across {
            if kind == "row" {
                pos[0][1] < pos[1][1]
            } else {
                pos[0][0] < pos[1][0]
            }
        } else if kind == "row" {
            pos[0][0] < pos[1][0]
        } else {
            pos[0][1] < pos[1][1]
        };
        let near = usize::from(!first_is_near);
        let far = 1 - near;
        let near_net = p.pinmap.get(&p.pins[near].number);
        let far_net = p.pinmap.get(&p.pins[far].number);
        // only signal nets say which way a part faces (a shared GND/supply rail is not the through-line)
        let prev_nets: HashSet<&String> = pn.iter().filter(|n| !rails.contains(*n)).collect();
        let next_nets: HashSet<&String> = nn.iter().filter(|n| !rails.contains(*n)).collect();
        let has = |s: &HashSet<&String>, n: Option<&String>| n.is_some_and(|n| s.contains(n));
        let mut flip = if across {
            let nb: HashSet<&String> = prev_nets.union(&next_nets).copied().collect();
            has(&nb, far_net) && !has(&nb, near_net)
        } else if !prev_nets.is_empty() && has(&prev_nets, far_net) && !has(&prev_nets, near_net) {
            true
        } else {
            !next_nets.is_empty() && has(&next_nets, near_net) && !has(&next_nets, far_net)
        };
        if flip {
            // never end up with a ground pin up or a supply pin down
            let nr = (kids[i].rot + 180).rem_euclid(360);
            for pin in &p.pins {
                let Some(net) = p.pinmap.get(&pin.number) else { continue };
                if rails.contains(net) {
                    let dd = p.pin_dir_at(pin, nr, &kids[i].mirror);
                    if (is_gnd(net) && dd == (0, -1)) || (!is_gnd(net) && dd == (0, 1)) {
                        flip = false;
                    }
                }
            }
        }
        if flip {
            kids[i].rot = (kids[i].rot + 180).rem_euclid(360);
            let b = leaf_box(p, kids[i].rot, &kids[i].mirror);
            kids[i].set_box(b);
            align_line(&mut kids[i], kind, gl);
        }
    }
}

// ---------------------------------------------------------------- placement

type Placement = ([i64; 2], i32, String);

/// Assign absolute positions; `(x, y)` is the node's box top-left.
fn place(node: &MNode, x: f64, y: f64, gl: &GroupLayout, out: &mut HashMap<String, Placement>) {
    if node.kind == "leaf" {
        if let Some(pi) = node.part {
            // snap the alignment line (pin line) to the grid, then derive the anchor from it
            let lx = snap_even(x + node.ax);
            let ly = snap_even(y + node.ay);
            let ax_ = lx - (node.ax - node.aax);
            let ay_ = ly - (node.ay - node.aay);
            out.insert(
                gl.parts[pi].key.clone(),
                ([py_round(ax_), py_round(ay_)], node.rot, node.mirror.clone()),
            );
        }
        return;
    }
    let (kids, gap, align) = (&node.children, node.gap, node.align.as_str());
    if node.kind == "row" {
        let mut cx = x;
        for k in kids {
            let cy = match align {
                "center" => y + node.ay - k.ay,
                "end" => y + node.h - k.h,
                _ => y,
            };
            place(k, cx, cy, gl, out);
            cx += k.w + gap;
        }
    } else {
        let mut cy = y;
        for (i, k) in kids.iter().enumerate() {
            let cx = match align {
                "center" => x + node.ax - k.ax,
                "end" => x + node.w - k.w,
                _ => x,
            };
            if let Some(offs) = &node.offsets {
                cy = y + offs[i];
            }
            place(k, cx, cy, gl, out);
            cy += k.h + gap;
        }
    }
}

// ---------------------------------------------------------------- entry points

/// Netlist design with a `"layout"` list -> raw design JSON.
pub fn build_flex_design(d: &Value) -> (Value, Vec<String>) {
    let mut d = d.clone();
    let (geos, mut errors) = circuit_geos_mut(&mut d);
    if errors.iter().any(|e| !e.starts_with("note:")) {
        return (json!({}), errors);
    }
    let want_paper = sstr(&d, "paper", "A4").to_string();
    let mut tries = vec![want_paper.clone()];
    tries.extend(["A4", "A3", "A2"].iter().filter(|p| **p != want_paper).map(|p| p.to_string()));
    if want_paper == "A4" {
        // a small design fills an A5 sheet instead of floating in the upper half of an A4
        let area: f64 = geos
            .iter()
            .map(|(_, g)| {
                let b = g.bbox();
                (b[2] - b[0]) * (b[3] - b[1])
            })
            .sum();
        if let Some([ax, ay, _, _]) = geo::paper("A5") {
            if area < 0.55 * (ax - 14.0) * (ay - 14.0) {
                tries.insert(0, "A5".to_string());
            }
        }
    }
    let mut raw: Option<Value> = None;
    for pt in &tries {
        let Some([xmax, ymax, tbx, tby]) = geo::paper(pt) else { continue };
        let sheet = pack_blocks(&geos, 14.0, 14.0, xmax, ymax, (tbx, tby), &[], *pt == want_paper);
        if let Some(sheet) = sheet {
            let mut r = sheet.to_json();
            r["paper"] = json!(pt);
            if *pt != want_paper {
                errors.push(if pt != "A5" {
                    format!("note: content did not fit on {want_paper}; paper changed to {pt}")
                } else {
                    "note: small design: paper set to A5".to_string()
                });
            }
            raw = Some(r);
            break;
        }
    }
    let Some(mut raw) = raw else {
        return (json!({}), vec!["design does not fit even on A2".to_string()]);
    };
    for k in ["title", "rev", "date", "company", "comments"] {
        if let Some(v) = d.get(k) {
            raw[k] = v.clone();
        }
    }
    (raw, errors)
}

/// Netlist design with `"layout"` -> list of `(title, Geo)` blocks (local coordinates), errors.
pub fn circuit_geos(d: &Value) -> (Vec<(String, Geo)>, Vec<String>) {
    let mut d = d.clone();
    circuit_geos_mut(&mut d)
}

/// One block of the layout: title, member part indices, index into `layout`, whether it carries a tree.
struct Block {
    title: String,
    members: Vec<usize>,
    /// The block's JSON object. Python keeps a live reference into `layout`; the trees are mutated by
    /// [`measure`] (an ignored `rot` is dropped) across the two routing passes, so the copy is owned here.
    blk: Value,
    has_tree: bool,
}

/// Canonicalise `{"part": id, "unit": n}` leaves to the `"U1#2"` key form and collect the keys in tree order.
fn collect(node: &mut Value, acc: &mut Vec<String>) {
    if node.get("part").is_some() {
        let mut key = value_str(&node["part"]);
        let unit = inum(node.get("unit"), 1);
        if unit != 1 && !key.contains('#') {
            key = format!("{key}#{unit}");
        }
        node["part"] = json!(key);
        acc.push(key);
    }
    for k in ["row", "col"] {
        let n = node.get(k).and_then(|v| v.as_array()).map_or(0, |a| a.len());
        for i in 0..n {
            collect(&mut node[k][i], acc);
        }
    }
}

fn circuit_geos_mut(d: &mut Value) -> (Vec<(String, Geo)>, Vec<String>) {
    let mut errors: Vec<String> = strip_power_parts(d);
    let mut parts: Vec<PartInst> = Vec::new();
    let part_json = d
        .get("parts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for pj in &part_json {
        match PartInst::new(pj) {
            Ok(p) => parts.push(p),
            Err(e) => return (Vec::new(), vec![e.to_string()]),
        }
    }
    if parts.is_empty() {
        return (Vec::new(), vec!["no parts".to_string()]);
    }
    let str_set = |k: &str| -> HashSet<String> {
        d.get(k)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(value_str).collect())
            .unwrap_or_default()
    };
    let power = str_set("power");
    let flags = str_set("flags");
    // multi-unit symbols: leaf {"part": "U1", "unit": 2}; internally keyed as "U1#2" (unit 1 = plain "U1")
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for (i, p) in parts.iter().enumerate() {
        by_id.insert(format!("{}#{}", p.id, p.unit), i);
        if p.unit == 1 {
            by_id.insert(p.id.clone(), i);
        }
    }
    for p in parts.iter_mut() {
        p.key = if p.unit == 1 {
            p.id.clone()
        } else {
            format!("{}#{}", p.id, p.unit)
        };
    }
    let mut layout: Vec<Value> = d
        .get("layout")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut used: HashSet<String> = HashSet::new();
    let mut blocks: Vec<Block> = Vec::new();
    for blk in layout.iter_mut() {
        let mut ids = Vec::new();
        if blk.get("tree").is_some() {
            collect(&mut blk["tree"], &mut ids);
        }
        let title = title_of(blk);
        let mut members = Vec::new();
        for i in &ids {
            if !by_id.contains_key(i) {
                errors.push(format!(
                    "layout block '{title}': unknown part {}",
                    repr(i)
                ));
            } else if used.contains(i) {
                errors.push(format!(
                    "layout block '{title}': part {} appears twice",
                    repr(i)
                ));
            } else {
                members.push(by_id[i]);
                used.insert(i.clone());
            }
        }
        if !members.is_empty() {
            let has_tree = blk.get("tree").is_some();
            blocks.push(Block { title, members, blk: blk.clone(), has_tree });
        }
    }
    let (new_layout, lint_notes) = lint_layout(&layout, &parts, &by_id, &power);
    layout = new_layout;
    errors.extend(lint_notes.iter().cloned());
    if !lint_notes.is_empty() {
        // re-collect membership after the lints changed the trees
        used.clear();
        blocks.clear();
        for blk in layout.iter_mut() {
            let mut ids = Vec::new();
            if blk.get("tree").is_some() {
                collect(&mut blk["tree"], &mut ids);
            }
            let members: Vec<usize> = ids
                .iter()
                .filter(|i| by_id.contains_key(*i) && !used.contains(*i))
                .map(|i| by_id[i])
                .collect();
            used.extend(ids.iter().filter(|i| by_id.contains_key(*i)).cloned());
            if !members.is_empty() {
                let has_tree = blk.get("tree").is_some();
                blocks.push(Block {
                    title: title_of(blk),
                    members,
                    blk: blk.clone(),
                    has_tree,
                });
            }
        }
    }
    let rest: Vec<usize> = (0..parts.len())
        .filter(|&i| !used.contains(&parts[i].key))
        .collect();
    if !rest.is_empty() {
        // parts the model left out of every tree: a MISC block laid out as a grid of rows of four
        let rows: Vec<Value> = rest
            .chunks(4)
            .map(|c| {
                json!({"row": c.iter().map(|&i| json!({"part": parts[i].key})).collect::<Vec<_>>(),
                       "gap": 8})
            })
            .collect();
        let tree = if rows.len() > 1 {
            json!({"col": rows, "gap": 8})
        } else {
            rows[0].clone()
        };
        let misc = json!({"title": "MISC", "tree": tree});
        blocks.push(Block {
            title: "MISC".to_string(),
            members: rest.clone(),
            blk: misc,
            has_tree: true,
        });
        errors.push(format!(
            "note: parts not mentioned in any layout tree were put in a MISC block: {}",
            rest.iter()
                .map(|&i| parts[i].id.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if errors.iter().any(|e| !e.starts_with("note:")) {
        return (Vec::new(), errors);
    }
    // flags: one group per flagged net (prefer power-ish groups with connectors)
    let mut flag_group: HashMap<String, usize> = HashMap::new();
    for net in &flags {
        let mut cands: Vec<(i32, i32, i64, usize)> = Vec::new();
        for (bi, b) in blocks.iter().enumerate() {
            let npins: i64 = b
                .members
                .iter()
                .map(|&mi| parts[mi].pinmap.values().filter(|v| *v == net).count() as i64)
                .sum();
            let has_j = b.members.iter().any(|&mi| {
                parts[mi].info.ref_prefix == "J" && parts[mi].pinmap.values().any(|v| v == net)
            });
            if npins > 0 {
                let up = b.title.to_uppercase();
                cands.push((
                    i32::from(!(up.contains("POWER") || up.contains("SUPPLY"))),
                    i32::from(!has_j),
                    -npins,
                    bi,
                ));
            }
        }
        if let Some(m) = cands.iter().min() {
            flag_group.insert(net.clone(), m.3);
        }
    }
    let mut all_nets: HashMap<String, usize> = HashMap::new();
    for b in &blocks {
        for &mi in &b.members {
            for pin in &parts[mi].pins {
                if let Some(net) = parts[mi].pinmap.get(&pin.number) {
                    if !net.is_empty() && net != "nc" && net != "float" {
                        *all_nets.entry(net.clone()).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    let mut geos: Vec<(String, Geo)> = Vec::new();
    for bi in 0..blocks.len() {
        let (title, has_tree) = (blocks[bi].title.clone(), blocks[bi].has_tree);
        // Python passes an unordered set of flagged nets; sorting keeps the Rust engine deterministic.
        let mut here: Vec<String> = flag_group
            .iter()
            .filter(|(_, g)| **g == bi)
            .map(|(n, _)| n.clone())
            .collect();
        here.sort();
        let members: Vec<PartInst> = blocks[bi].members.iter().map(|&mi| parts[mi].clone()).collect();
        let mut label_pins: HashSet<(String, String)> = HashSet::new();
        let mut done: Option<GroupLayout> = None;
        for _ in 0..2 {
            let mut gl = GroupLayout::new(
                members.clone(),
                power.clone(),
                here.clone(),
                all_nets.clone(),
                label_pins.clone(),
            );
            gl.max_wire = 44.0; // the model chose adjacency: wire generously
            let by_key: HashMap<String, usize> = gl
                .parts
                .iter()
                .enumerate()
                .map(|(i, p)| (p.key.clone(), i))
                .collect();
            let path = format!("block '{title}'");
            let node = measure(
                &mut blocks[bi].blk["tree"],
                &gl,
                &by_key,
                &mut errors,
                &path,
                "row",
            );
            if errors.iter().any(|e| !e.starts_with("note:")) {
                return (Vec::new(), errors);
            }
            let mut pos: HashMap<String, Placement> = HashMap::new();
            place(&node, 0.0, 0.0, &gl, &mut pos);
            for i in 0..gl.parts.len() {
                let Some((at, rot, mir)) = pos.get(&gl.parts[i].key).cloned() else {
                    return (
                        Vec::new(),
                        vec![format!(
                            "block '{title}': {} was not placed by its layout tree",
                            gl.parts[i].key
                        )],
                    );
                };
                gl.parts[i].at = [at[0] as f64, at[1] as f64];
                gl.parts[i].rot = rot;
                gl.parts[i].mirror = mir;
            }
            gl.route();
            let fresh = gl.labelled_pins.difference(&label_pins).next().is_some();
            label_pins.extend(gl.labelled_pins.iter().cloned());
            done = Some(gl);
            if !fresh {
                break;
            }
        }
        let mut gl = done.expect("two passes ran");
        let mut g = match gl.emit() {
            Ok(g) => g,
            Err(e) => return (Vec::new(), vec![format!("block '{title}': {e}")]),
        };
        let bb = g.bbox();
        let (bw, bh) = (bb[2] - bb[0], bb[3] - bb[1]);
        if has_tree && (bh > 2.2 * bw + 10.0 || bw > 4.0 * bh + 20.0) {
            let shape = if bh > bw { "tall" } else { "wide" };
            errors.push(format!(
                "note: block '{title}' is {bw:.0} x {bh:.0} units - very {shape}; \
                 rebalance its tree (put sub-circuits side by side in a row / stack them in a col)"
            ));
        }
        if !title.is_empty() {
            g.add_text(&title, [bb[0], bb[1] - 3.0], 2.5, true);
        }
        if truthy(blocks[bi].blk.get("note")) {
            let note = value_str(&blocks[bi].blk["note"]);
            let bb2 = g.bbox();
            g.add_text(&note, [bb[0], bb2[3] + 4.0], 1.27, false);
        }
        geos.push((title, g));
    }
    let notes: Vec<String> = d
        .get("notes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|v| truthy(Some(v)))
                .map(value_str)
                .collect()
        })
        .unwrap_or_default();
    if !notes.is_empty() {
        let mut g = Geo::new();
        g.add_text("NOTES", [0.0, -3.0], 2.5, true);
        let mut y = 2.0;
        for n in &notes {
            g.add_text(n, [0.0, y], 1.27, false);
            let lines = n.matches('\n').count() + 1 + n.chars().count() / 60;
            y += 1.5 * lines as f64 + 2.0;
        }
        geos.push(("notes".to_string(), g));
    }
    (geos, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(v: Value) -> Vec<String> {
        tree_ids(&v)
    }

    #[test]
    fn tree_ids_walks_rows_and_cols_in_order() {
        assert_eq!(
            t(json!({"row": [{"part": "J1"}, {"col": [{"part": "C1"}, {"part": "C2"}]}, {"part": "U2"}]})),
            ["J1", "C1", "C2", "U2"]
        );
        assert_eq!(t(json!({"part": 5})), ["5"]);
        assert_eq!(t(json!({})), Vec::<String>::new());
        // a container that also carries a part yields the part first
        assert_eq!(t(json!({"part": "R1", "row": [{"part": "R2"}]})), ["R1", "R2"]);
    }

    #[test]
    fn take_and_prune_remove_the_leaf_and_empty_containers() {
        let mut tree = json!({"row": [{"part": "U1"}, {"col": [{"part": "U1#2"}]}]});
        let got = take_leaf(&mut tree, "U1#2").unwrap();
        assert_eq!(got, json!({"part": "U1#2"}));
        prune(&mut tree);
        assert_eq!(tree, json!({"row": [{"part": "U1"}]}));
        assert!(take_leaf(&mut tree, "nope").is_none());
    }

    #[test]
    fn replace_leaf_swaps_in_place() {
        let mut tree = json!({"col": [{"row": [{"part": "A"}, {"part": "B"}]}]});
        assert!(replace_leaf(&mut tree, "B", &json!({"row": [{"part": "B"}, {"part": "X"}]})));
        assert_eq!(
            tree,
            json!({"col": [{"row": [{"part": "A"}, {"row": [{"part": "B"}, {"part": "X"}]}]}]})
        );
        assert!(!replace_leaf(&mut tree, "Z", &json!({"part": "Z"})));
    }

    #[test]
    fn collect_canonicalises_unit_keys() {
        let mut tree = json!({"row": [{"part": "U1", "unit": 2}, {"part": "U1"}, {"part": "U2", "unit": 1}]});
        let mut ids = Vec::new();
        collect(&mut tree, &mut ids);
        assert_eq!(ids, ["U1#2", "U1", "U2"]);
        assert_eq!(tree["row"][0]["part"], json!("U1#2"));
    }

    #[test]
    fn py_round_is_half_to_even() {
        assert_eq!(py_round(2.5), 2);
        assert_eq!(py_round(3.5), 4);
        assert_eq!(py_round(-2.5), -2);
        assert_eq!(py_round(2.4), 2);
        assert_eq!(py_round(2.6), 3);
    }
}
