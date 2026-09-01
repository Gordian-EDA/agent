//! Reading the sheet back: the compact listing the model works from, and the
//! three focused lookups it drills down with.

use std::collections::BTreeMap;

use anyhow::Result;
use geom::Rect;
use gordian_runtime::AgentRuntime;
use sch_doc::{Netlist, PlacedPin, body_rect, placed_pins};
use serde_json::{Value, json};

use crate::refs;
use crate::session::Edit;

/// Which edge of its symbol a pin leaves from.
fn side_of(pin: &PlacedPin) -> &'static str {
    if pin.out.x.abs() >= pin.out.y.abs() {
        if pin.out.x < 0.0 { "left" } else { "right" }
    } else if pin.out.y < 0.0 {
        "top"
    } else {
        "bottom"
    }
}

fn rotation(rot: f64) -> String {
    let rot = ((rot % 360.0) + 360.0) % 360.0;
    if rot == 0.0 {
        String::new()
    } else {
        format!(" r{rot:.0}")
    }
}

/// A pin's number, with its function name appended when the symbol names it
/// distinctly (a tube's `G`/`K`, a connector's `TX`) — never for an
/// unnamed `~` pin or a name that just repeats the number, which would only
/// echo noise. Without this, a same-shaped part with several unlabelled pins
/// (a triode's grid vs. cathode) is a guess from the number alone.
fn pin_label(p: &PlacedPin) -> String {
    if p.name.is_empty() || p.name == "~" || p.name == p.number {
        p.number.clone()
    } else {
        format!("{}({})", p.number, p.name)
    }
}

/// `1=VCC 2(G)=N_TR`, a symbol's pins and the nets they land on.
fn pin_map(pins: &[&PlacedPin], netlist: &Netlist) -> String {
    pins.iter()
        .map(|p| {
            let label = pin_label(p);
            match refs::net_of(netlist, &p.refdes, &p.number) {
                Some(net) => format!("{label}={net}"),
                None => format!("{label}=-"),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The whole sheet as one line per symbol, then the nets and the loose ends.
pub fn read_schematic(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let (doc, netlist) = Edit::read(ctx)?;
    let full = input.get("detail").and_then(Value::as_str) == Some("full");
    let region = input.get("region").and_then(Value::as_array).and_then(|v| {
        let n: Vec<f64> = v.iter().filter_map(Value::as_f64).collect();
        (n.len() == 4).then(|| Rect::new(n[0], n[1], n[2], n[3]))
    });

    let placed = placed_pins(&doc);
    let mut out = String::new();
    out.push_str(&format!(
        "{} — {} symbols, {} nets\n\nSYMBOLS\n",
        ctx.sch_path().display(),
        doc.symbols().count(),
        netlist.nets.len()
    ));
    for symbol in doc.symbols() {
        if region.is_some_and(|r| !r.contains(symbol.at.point())) {
            continue;
        }
        let pins: Vec<&PlacedPin> = placed.iter().filter(|p| p.owner == symbol.uuid).collect();
        // The halves of a dual part share one reference; say so on every line
        // so `U1` twice reads as one two-unit part rather than a duplicate.
        let units = refs::units(&doc, symbol.refdes()).len();
        let unit = match units {
            0 | 1 => String::new(),
            n => format!(" unit {}/{n}", symbol.unit),
        };
        out.push_str(&format!(
            "{} {} \"{}\" @({:.2},{:.2}){}{} [{}]",
            symbol.refdes(),
            symbol.lib_id,
            symbol.value(),
            symbol.at.x,
            symbol.at.y,
            rotation(symbol.at.rot),
            unit,
            pin_map(&pins, &netlist),
        ));
        if symbol.dnp {
            out.push_str(" DNP");
        }
        if full {
            let footprint = symbol.fields.get("Footprint").map_or("", |f| &f.value);
            out.push_str(&format!(" fp={footprint} uuid={}", symbol.uuid));
        }
        out.push('\n');
    }

    out.push_str("\nNETS\n");
    for net in &netlist.nets {
        out.push_str(&format!(
            "{}: {}\n",
            net.name,
            net.pins
                .iter()
                .map(refs::label)
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    if !netlist.unconnected.is_empty() {
        out.push_str(&format!(
            "\nUNCONNECTED PINS\n{}\n",
            netlist
                .unconnected
                .iter()
                .map(refs::label)
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    if !netlist.no_connect.is_empty() {
        out.push_str(&format!(
            "\nNO-CONNECT PINS\n{}\n",
            netlist
                .no_connect
                .iter()
                .map(refs::label)
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    if !netlist.warnings.is_empty() {
        out.push_str(&format!("\nWARNINGS\n{}\n", netlist.warnings.join("\n")));
    }
    Ok(Value::String(out))
}

/// One symbol in full: where it sits, what it is, and where each pin is.
pub fn get_symbol(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(refdes) = input.get("ref").and_then(Value::as_str) else {
        return Ok(json!({ "error": "get_symbol needs `ref`" }));
    };
    let (doc, netlist) = Edit::read(ctx)?;
    let Some(symbol) = doc.symbol_by_ref(refdes) else {
        return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
    };
    let placed = placed_pins(&doc);
    let units: Vec<Value> = refs::units(&doc, refdes)
        .iter()
        .filter_map(|(unit, uuid)| {
            let instance = doc.symbol(uuid)?;
            let pins: Vec<Value> = placed
                .iter()
                .filter(|p| p.owner == *uuid)
                .map(|p| {
                    json!({
                        "number": p.number,
                        "name": p.name,
                        "type": p.etype,
                        "side": side_of(p),
                        "at": [p.at.x, p.at.y],
                        "net": refs::net_of(&netlist, &p.refdes, &p.number),
                    })
                })
                .collect();
            let body = body_rect(&doc, instance);
            Some(json!({
                "unit": unit,
                "uuid": uuid,
                "at": [instance.at.x, instance.at.y],
                "rotation": instance.at.rot,
                "body": body.map(|r| json!([r.min_x, r.min_y, r.max_x, r.max_y])),
                "pins": pins,
            }))
        })
        .collect();
    let fields: BTreeMap<&str, &str> = symbol
        .fields
        .iter()
        .map(|(name, field)| (name.as_str(), field.value.as_str()))
        .collect();
    Ok(json!({
        "ref": symbol.refdes(),
        "lib_id": symbol.lib_id,
        "dnp": symbol.dnp,
        "in_bom": symbol.in_bom,
        "fields": fields,
        // One entry per unit. A single-unit part has exactly one; the halves of
        // a dual part are one part with one value and one footprint, and every
        // mutator but `move_symbols` addresses them together through `ref`.
        "units": units,
    }))
}

/// One net: its pins and how it got its name.
pub fn get_net(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(name) = input.get("name").and_then(Value::as_str) else {
        return Ok(json!({ "error": "get_net needs `name`" }));
    };
    let (doc, netlist) = Edit::read(ctx)?;
    let Some(net) = netlist.nets.iter().find(|n| n.name == name) else {
        let mut known: Vec<&str> = netlist.nets.iter().map(|n| n.name.as_str()).collect();
        known.sort_unstable();
        let closest = known
            .iter()
            .max_by(|a, b| strsim::jaro_winkler(a, name).total_cmp(&strsim::jaro_winkler(b, name)))
            .filter(|candidate| strsim::jaro_winkler(candidate, name) > 0.8);
        let error = match closest {
            Some(candidate) => format!("no net `{name}` — did you mean `{candidate}`?"),
            None => format!("no net `{name}`"),
        };
        return Ok(json!({ "error": error, "nets": known }));
    };
    let labels: Vec<Value> = doc
        .labels()
        .filter(|l| sch_doc::unescape(&l.text) == name)
        .map(|l| json!({ "at": [l.at.x, l.at.y], "kind": format!("{:?}", l.kind) }))
        .collect();
    Ok(json!({
        "name": net.name,
        "named_by": format!("{:?}", net.source),
        "pins": net.pins.iter().map(refs::label).collect::<Vec<_>>(),
        "labels": labels,
    }))
}
