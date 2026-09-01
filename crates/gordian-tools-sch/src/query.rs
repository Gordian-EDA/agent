//! Reading the sheet back: the compact listing the model works from, and the
//! three focused lookups it drills down with.

use std::collections::BTreeMap;

use anyhow::Result;
use geom::{Point2, Rect};
use gordian_runtime::AgentRuntime;
use sch_doc::{Netlist, PlacedPin, body_rect, placed_pins};
use serde_json::{Value, json};

use crate::place::{Occupancy, snap_point};
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

/// `1=VCC 2=N_TR`, a symbol's pins and the nets they land on.
fn pin_map(pins: &[&PlacedPin], netlist: &Netlist) -> String {
    pins.iter()
        .map(|p| match refs::net_of(netlist, &p.refdes, &p.number) {
            Some(net) => format!("{}={net}", p.number),
            None => format!("{}=-", p.number),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The whole sheet as one line per symbol, then the nets and the loose ends.
pub fn read_schematic(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let (doc, netlist) = Edit::read(ctx)?;
    let full = input.get("detail").and_then(Value::as_str) == Some("full");
    let region = input
        .get("region")
        .and_then(Value::as_array)
        .and_then(|v| {
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
        let pins: Vec<&PlacedPin> = placed
            .iter()
            .filter(|p| p.owner == symbol.uuid)
            .collect();
        out.push_str(&format!(
            "{} {} \"{}\" @({:.2},{:.2}){} [{}]",
            symbol.refdes(),
            symbol.lib_id,
            symbol.value(),
            symbol.at.x,
            symbol.at.y,
            rotation(symbol.at.rot),
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
    let pins: Vec<Value> = placed_pins(&doc)
        .iter()
        .filter(|p| p.owner == symbol.uuid)
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
    let fields: BTreeMap<&str, &str> = symbol
        .fields
        .iter()
        .map(|(name, field)| (name.as_str(), field.value.as_str()))
        .collect();
    let body = body_rect(&doc, symbol);
    Ok(json!({
        "ref": symbol.refdes(),
        "lib_id": symbol.lib_id,
        "at": [symbol.at.x, symbol.at.y],
        "rotation": symbol.at.rot,
        "unit": symbol.unit,
        "dnp": symbol.dnp,
        "in_bom": symbol.in_bom,
        "fields": fields,
        "body": body.map(|r| json!([r.min_x, r.min_y, r.max_x, r.max_y])),
        "pins": pins,
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
        return Ok(json!({
            "error": format!("no net `{name}`"),
            "nets": known,
        }));
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

/// Somewhere a `w`×`h` block fits without disturbing anything.
pub fn free_space(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let w = input.get("w").and_then(Value::as_f64).unwrap_or(10.0);
    let h = input.get("h").and_then(Value::as_f64).unwrap_or(10.0);
    let (doc, _) = Edit::read(ctx)?;
    let occupancy = Occupancy::of(&doc);
    let from = match input.get("near").and_then(Value::as_str) {
        Some(refdes) => match doc.symbol_by_ref(refdes) {
            Some(symbol) => symbol.at.point(),
            None => return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") })),
        },
        None => right_of(occupancy.content()),
    };
    match occupancy.nearest_free(snap_point(from), w, h) {
        Some(at) => Ok(json!({ "at": [at.x, at.y], "w": w, "h": h })),
        None => Ok(json!({ "error": "no free space that size on the sheet" })),
    }
}

/// Fresh ground to the right of everything drawn — where a block with no
/// anchor naturally belongs.
fn right_of(content: Rect) -> Point2 {
    Point2::new(content.max_x + 12.7, content.center().y)
}
