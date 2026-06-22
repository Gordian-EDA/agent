//! Deterministic canonical YAML emission of a [`BoardDesign`].
//!
//! The output is the normalized form: stable key order, natural-sorted parts &
//! pads, flow-style leaf mappings. `parse_str(to_canonical_yaml(d))` reproduces
//! `d` exactly — that round-trip is the contract the tests pin.

use crate::model::*;
use indexmap::IndexMap;
use std::fmt::Write;

/// Quote a YAML scalar only when needed. `:` is safe because every mapping we
/// emit a bare scalar into is flow-style, where `key: a:b` keeps `a:b` as one
/// plain scalar (colon not followed by space). Footprint lib_ids rely on this.
fn q(s: &str) -> String {
    let safe = !s.is_empty()
        && !matches!(s, "null" | "Null" | "NULL" | "true" | "false")
        && s.chars().next().unwrap().is_ascii_alphabetic()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.+:~/-".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// Format a number compactly and losslessly for these decimals: `44.0` → `44`,
/// `0.8` → `0.8`. (Inputs are human-authored mm values, never long mantissas.)
fn n(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// Natural sort: alpha prefix, then numeric suffix (U2 < U10).
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn split(s: &str) -> (&str, u64) {
        let i = s.find(|c: char| c.is_ascii_digit()).unwrap_or(s.len());
        (&s[..i], s[i..].parse().unwrap_or(0))
    }
    split(a).cmp(&split(b))
}

fn sorted_keys<V>(m: &IndexMap<String, V>) -> Vec<&String> {
    let mut v: Vec<&String> = m.keys().collect();
    v.sort_by(|a, b| natural_cmp(a, b));
    v
}

pub fn to_canonical_yaml(d: &BoardDesign) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "version: 1");
    if let Some(name) = &d.name {
        let _ = writeln!(o, "name: {}", q(name));
    }

    // board
    let _ = writeln!(o, "board:");
    let _ = writeln!(o, "  layers: {}", d.board.layers);
    let _ = writeln!(o, "  outline: {}", outline_inline(&d.board.outline));
    let _ = writeln!(o, "  rules:");
    let r = &d.board.rules;
    let _ = writeln!(o, "    clearance: {}", n(r.clearance));
    let _ = writeln!(o, "    trace_width: {}", n(r.trace_width));
    let _ = writeln!(o, "    via: [{}, {}]", n(r.via_diameter), n(r.via_drill));
    if !r.net_widths.is_empty() {
        let parts: Vec<String> = sorted_keys(&r.net_widths)
            .into_iter()
            .map(|k| format!("{}: {}", q(k), n(r.net_widths[k])))
            .collect();
        let _ = writeln!(o, "    net_widths: {{{}}}", parts.join(", "));
    }
    if !r.pours.is_empty() {
        let mut pours = r.pours.clone();
        pours.sort_by(|a, b| natural_cmp(&a.net, &b.net).then(a.layer.cmp(&b.layer)));
        let parts: Vec<String> = pours
            .iter()
            .map(|p| format!("{{net: {}, layer: {}}}", q(&p.net), q(&p.layer)))
            .collect();
        let _ = writeln!(o, "    pours: [{}]", parts.join(", "));
    }

    // parts
    let _ = writeln!(o, "parts:");
    for refdes in sorted_keys(&d.parts) {
        let p = &d.parts[refdes];
        let _ = writeln!(o, "  {}: {}", q(refdes), part_inline(p));
    }

    // place (only if there are groups)
    if !d.groups.is_empty() {
        let _ = writeln!(o, "place:");
        let _ = writeln!(o, "  groups:");
        for name in sorted_keys(&d.groups) {
            let _ = writeln!(o, "    {}: {}", q(name), group_inline(&d.groups[name]));
        }
    }

    // keepouts (only if any)
    if !d.keepouts.is_empty() {
        let _ = writeln!(o, "keepouts:");
        for k in &d.keepouts {
            let layers: Vec<String> = k.layers.iter().map(|l| q(l)).collect();
            let _ = writeln!(
                o,
                "  - {{rect: [{}, {}, {}, {}], layers: [{}]}}",
                n(k.rect[0]),
                n(k.rect[1]),
                n(k.rect[2]),
                n(k.rect[3]),
                layers.join(", ")
            );
        }
    }

    o
}

fn outline_inline(outline: &Outline) -> String {
    match outline {
        Outline::Rect { w, h } => format!("{{rect: [{}, {}]}}", n(*w), n(*h)),
        Outline::Circle { r } => format!("{{circle: {}}}", n(*r)),
        Outline::Polygon(pts) => {
            let parts: Vec<String> = pts.iter().map(|(x, y)| format!("[{}, {}]", n(*x), n(*y))).collect();
            format!("{{polygon: [{}]}}", parts.join(", "))
        }
    }
}

fn part_inline(p: &Part) -> String {
    let mut fields = vec![format!("footprint: {}", q(&p.footprint))];
    if !p.pads.is_empty() {
        let mut pads: Vec<&String> = p.pads.keys().collect();
        pads.sort_by(|a, b| natural_cmp(a, b));
        let entries: Vec<String> = pads
            .into_iter()
            .map(|k| format!("{}: {}", q(k), q(&p.pads[k])))
            .collect();
        fields.push(format!("pads: {{{}}}", entries.join(", ")));
    }
    if p.edge {
        fields.push("edge: true".to_string());
    }
    if p.corner {
        fields.push("corner: true".to_string());
    }
    if let Some(l) = &p.lock {
        fields.push(format!("lock: {{at: [{}, {}], rot: {}}}", n(l.x), n(l.y), l.rot));
    }
    format!("{{{}}}", fields.join(", "))
}

fn group_inline(g: &Group) -> String {
    let mut fields = Vec::new();
    let members: Vec<String> = g.members.iter().map(|m| q(m)).collect();
    fields.push(format!("members: [{}]", members.join(", ")));
    if let Some(r) = &g.region {
        fields.push(format!("region: [{}, {}, {}, {}]", n(r[0]), n(r[1]), n(r[2]), n(r[3])));
    }
    if let Some(e) = &g.edge {
        fields.push(format!("edge: {}", q(e)));
    }
    if let Some(s) = &g.surround {
        fields.push(format!("surround: {}", q(s)));
    }
    if g.grid {
        fields.push("grid: true".to_string());
    }
    format!("{{{}}}", fields.join(", "))
}
