//! Parse an existing `.kicad_sch` into a [`Design`], losslessly enough for round-trip editing.
//!
//! Uuids, custom `lib_symbols` and every node the model never edits (sheets, buses, images,
//! polylines) are preserved, so recompiling an untouched design reproduces the source sheet.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::json;

use crate::model::{Design, Label, Part, Power, Rect, Text, rot_point};
use crate::sexp::{self, Sexp};
use crate::symlib::{SymbolInfo, parse_symbol_node};

fn round4(v: f64) -> f64 {
    format!("{v:.4}").parse().unwrap_or(v)
}

fn xy(n: &Sexp) -> [f64; 2] {
    let l = n.as_list().unwrap_or(&[]);
    [
        l.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
        l.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0),
    ]
}

fn at_of(n: Option<&Sexp>) -> [f64; 3] {
    let Some(l) = n.and_then(|n| n.as_list()) else {
        return [0.0, 0.0, 0.0];
    };
    [
        l.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
        l.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0),
        l.get(3).and_then(|v| v.as_f64()).unwrap_or(0.0).trunc(),
    ]
}

fn justify_of(effects: Option<&Sexp>) -> String {
    let Some(j) = effects.and_then(|e| e.child("justify")) else {
        return String::new();
    };
    j.as_list().unwrap()[1..]
        .iter()
        .map(|a| a.text())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `([x, y, rot, justify], hidden)` of a named property.
fn prop_pos(node: &Sexp, name: &str) -> (Option<Vec<serde_json::Value>>, bool) {
    let Some(p) = node.prop(name) else {
        return (None, false);
    };
    let eff = p.child("effects");
    let hidden = eff
        .and_then(|e| e.child("hide"))
        .map(|h| h.as_list().unwrap()[1].text() == "yes")
        .unwrap_or(false);
    let v = p.child("at").map(|a| {
        let a = at_of(Some(a));
        vec![
            json!(a[0]),
            json!(a[1]),
            json!(a[2] as i64),
            json!(justify_of(eff)),
        ]
    });
    (v, hidden)
}

/// The node's first atom as text (a symbol's name, a label's string, a `(uuid "...")` value).
fn text_of(n: Option<&Sexp>) -> String {
    n.and_then(|n| n.as_list())
        .and_then(|l| l.get(1))
        .map(|v| v.text())
        .unwrap_or_default()
}

pub fn load(path: &std::path::Path) -> Result<Design> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text)
}

pub fn parse(text: &str) -> Result<Design> {
    let root = sexp::loads(text).context("not a valid s-expression")?;
    let mut des = Design {
        uuid: text_of(root.child("uuid")),
        ..Default::default()
    };
    if let Some(p) = root.child("paper") {
        des.paper = text_of(Some(p));
    }
    if let Some(tb) = root.child("title_block") {
        for k in ["title", "rev", "company", "date"] {
            if let Some(c) = tb.child(k) {
                let v = text_of(Some(c));
                match k {
                    "title" => des.title = v,
                    "rev" => des.rev = v,
                    "company" => des.company = v,
                    _ => des.date = v,
                }
            }
        }
        for c in tb.children("comment") {
            des.comments.push(
                c.as_list()
                    .and_then(|l| l.get(2))
                    .map(|v| v.text())
                    .unwrap_or_default(),
            );
        }
    }
    if let Some(libs) = root.child("lib_symbols") {
        for s in libs.children("symbol") {
            des.lib_symbols.insert(text_of(Some(s)), s.clone());
        }
    }
    let infos: HashMap<String, SymbolInfo> = des
        .lib_symbols
        .iter()
        .map(|(k, v)| (k.clone(), parse_symbol_node(v, k)))
        .collect();

    for node in &root.as_list().unwrap()[1..] {
        if !node.is_list() {
            continue;
        }
        match node.tag() {
            "symbol" => {
                let lib_id = text_of(node.child("lib_id"));
                let at = at_of(node.child("at"));
                let rot = at[2] as i32;
                let mirror = node
                    .child("mirror")
                    .map(|m| text_of(Some(m)))
                    .unwrap_or_default();
                let unit = node
                    .child("unit")
                    .and_then(|u| u.as_list()?.get(1)?.as_f64())
                    .unwrap_or(1.0) as i32;
                let uuid = node
                    .child("uuid")
                    .map(|u| text_of(Some(u)))
                    .unwrap_or_default();
                let mut reference = node.prop_value("Reference").unwrap_or_else(|| "?".into());
                // some files store the reference in instances
                if let Some(inst) = node.child("instances") {
                    for proj in inst.children("project") {
                        for pth in proj.children("path") {
                            if let Some(r) = pth.child("reference") {
                                reference = text_of(Some(r));
                            }
                            if des.sheet_path_uuid.is_empty() {
                                des.sheet_path_uuid = text_of(Some(pth));
                                des.project = text_of(Some(proj));
                            }
                        }
                    }
                }
                let lib_name = node
                    .child("lib_name")
                    .map(|n| text_of(Some(n)))
                    .unwrap_or_default();
                let info = infos.get(&lib_name).or_else(|| infos.get(&lib_id));
                let is_power = info
                    .map(|i| i.power)
                    .unwrap_or_else(|| lib_id.starts_with("power:"))
                    || reference.starts_with("#PWR");
                let value = node.prop_value("Value").unwrap_or_default();
                if is_power {
                    // connection point = the (single) pin position
                    let (px, py) = match info.and_then(|i| i.pins.first()) {
                        Some(pin) => rot_point(pin.x, pin.y, rot, &mirror),
                        None => (0.0, 0.0),
                    };
                    let (val_at, hide_value) = prop_pos(node, "Value");
                    let net = if value.is_empty() {
                        lib_id
                            .split_once(':')
                            .map(|(_, n)| n.to_string())
                            .unwrap_or_else(|| lib_id.clone())
                    } else {
                        value.clone()
                    };
                    des.power.push(Power {
                        net,
                        at: [round4(at[0] + px), round4(at[1] + py)],
                        rot,
                        lib: lib_id,
                        reference,
                        uuid,
                        value,
                        val_at,
                        hide_value,
                        mirror,
                    });
                    continue;
                }
                let (ref_at, _) = prop_pos(node, "Reference");
                let (val_at, hide_value) = prop_pos(node, "Value");
                let mut part = Part {
                    id: reference,
                    lib: lib_id,
                    at: [at[0], at[1]],
                    rot,
                    mirror,
                    value,
                    unit,
                    footprint: node.prop_value("Footprint").unwrap_or_default(),
                    dnp: node
                        .child("dnp")
                        .map(|d| text_of(Some(d)) == "yes")
                        .unwrap_or(false),
                    uuid,
                    lib_name,
                    ref_at,
                    val_at,
                    hide_value,
                    ..Default::default()
                };
                for pr in node.children("property") {
                    let name = text_of(Some(pr));
                    if matches!(name.as_str(), "Reference" | "Value" | "Footprint")
                        || name.starts_with("ki_")
                    {
                        continue;
                    }
                    let v = pr
                        .as_list()
                        .and_then(|l| l.get(2))
                        .map(|v| v.text())
                        .unwrap_or_default();
                    if !v.is_empty() && v != "~" {
                        part.fields.insert(name, json!(v));
                    }
                }
                des.parts.push(part);
            }
            "wire" => {
                des.wires.push(
                    node.child("pts")
                        .map(|p| p.children("xy").into_iter().map(xy).collect())
                        .unwrap_or_default(),
                );
                des.wire_uuids.push(
                    node.child("uuid")
                        .map(|u| text_of(Some(u)))
                        .unwrap_or_default(),
                );
            }
            tag @ ("label" | "global_label" | "hierarchical_label") => {
                let at = at_of(node.child("at"));
                des.labels.push(Label {
                    text: text_of(Some(node)),
                    at: [at[0], at[1]],
                    rot: at[2] as i32,
                    kind: match tag {
                        "global_label" => "global",
                        "hierarchical_label" => "hier",
                        _ => "local",
                    }
                    .into(),
                    shape: node
                        .child("shape")
                        .map(|s| text_of(Some(s)))
                        .unwrap_or_else(|| "input".into()),
                    uuid: node
                        .child("uuid")
                        .map(|u| text_of(Some(u)))
                        .unwrap_or_default(),
                    justify: justify_of(node.child("effects")),
                });
            }
            "no_connect" => {
                if let Some(at) = node.child("at") {
                    des.nc.push(xy(at));
                }
            }
            "junction" => {
                if let Some(at) = node.child("at") {
                    des.junctions.push(xy(at));
                }
            }
            "text" => {
                let at = at_of(node.child("at"));
                let eff = node.child("effects");
                let mut size = 1.27;
                let mut bold = false;
                if let Some(f) = eff.and_then(|e| e.child("font")) {
                    if let Some(sz) = f.child("size") {
                        size = sz
                            .as_list()
                            .and_then(|l| l.get(1)?.as_f64())
                            .unwrap_or(1.27);
                    }
                    bold = f
                        .child("bold")
                        .map(|b| text_of(Some(b)) == "yes")
                        .unwrap_or(false);
                }
                let j = justify_of(eff);
                des.texts.push(Text {
                    text: text_of(Some(node)),
                    at: [at[0], at[1]],
                    rot: at[2] as i32,
                    size,
                    bold,
                    justify: if j.is_empty() {
                        "left bottom".into()
                    } else {
                        j
                    },
                    uuid: node
                        .child("uuid")
                        .map(|u| text_of(Some(u)))
                        .unwrap_or_default(),
                });
            }
            "rectangle" => des.rects.push(Rect {
                start: node.child("start").map(xy).unwrap_or_default(),
                end: node.child("end").map(xy).unwrap_or_default(),
                uuid: node
                    .child("uuid")
                    .map(|u| text_of(Some(u)))
                    .unwrap_or_default(),
            }),
            "version" | "generator" | "generator_version" | "uuid" | "paper" | "title_block"
            | "lib_symbols" | "sheet_instances" | "symbol_instances" | "embedded_fonts" => {}
            _ => des.extra_nodes.push(node.clone()),
        }
    }
    Ok(des)
}

#[cfg(test)]
mod tests {
    /// Custom properties must keep the file's order, not an alphabetical one, so an untouched
    /// symbol recompiles byte-for-byte. (`serde_json`'s `preserve_order` feature carries this.)
    #[test]
    fn custom_properties_keep_their_order() {
        let sheet = "(kicad_sch (symbol (lib_id \"X:Y\") (at 0 0 0) (unit 1) \
                     (property \"Reference\" \"U1\" (at 0 0 0)) \
                     (property \"MPN\" \"a\" (at 0 0 0)) \
                     (property \"Manufacturer\" \"b\" (at 0 0 0)) \
                     (property \"Availability\" \"c\" (at 0 0 0))))";
        let des = super::parse(sheet).unwrap();
        let names: Vec<&String> = des.parts[0].fields.keys().collect();
        assert_eq!(names, ["MPN", "Manufacturer", "Availability"]);
    }
}
