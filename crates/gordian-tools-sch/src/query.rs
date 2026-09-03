//! Human-readable schematic summaries and focused lookups.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write;

use anyhow::Result;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use geom::Rect;
use gordian_runtime::AgentRuntime;
use sch_doc::{
    Mirror, Net, NetSource, Netlist, PinRef, PlacedPin, SchDoc, SymbolInst, body_rect, placed_pins,
};
use serde_json::{Value, json};

use crate::refs;
use crate::session::Edit;

const FOOTPRINT_QUERY_CHUNK: usize = 4;

/// Search installed footprints by name, optionally ranking electrical compatibility.
pub fn search_footprints(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    if let Some(queries) = input.get("queries") {
        let Some(queries) = queries.as_array() else {
            return Ok(json!({ "error": "`queries` must be an array" }));
        };
        let mut results = Vec::with_capacity(queries.len());
        for chunk in queries.chunks(FOOTPRINT_QUERY_CHUNK) {
            results.extend(
                chunk
                    .iter()
                    .map(|item| search_footprints_one(item, ctx))
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        return Ok(json!({ "results": results }));
    }
    search_footprints_one(&input, ctx)
}

fn search_footprints_one(input: &Value, ctx: &AgentRuntime) -> Result<Value> {
    let symbol = input.get("symbol").and_then(Value::as_str);
    let query = input.get("query").and_then(Value::as_str);
    if symbol.is_none() && query.is_none() {
        return Ok(json!({ "error": "search_footprints needs `query` or `symbol`" }));
    }
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|limit| limit as usize)
        .unwrap_or(ctx.config().tools.default_search_limit);
    let hits = if let Some(symbol) = symbol {
        serde_json::to_value(
            gordian_runtime::footprint_compat::search_compatible_footprints(
                ctx, symbol, query, limit,
            )?,
        )?
    } else {
        serde_json::to_value(
            gordian_runtime::footprint_compat::search_footprints_by_name(
                ctx,
                query.expect("a query is required without a symbol"),
                limit,
            )?,
        )?
    };
    let mut result = json!({ "hits": hits });
    if let Some(symbol) = symbol {
        result["symbol"] = json!(symbol);
    }
    if let Some(query) = query {
        result["query"] = json!(query);
    }
    Ok(result)
}

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

fn orientation(symbol: &SymbolInst) -> String {
    let rot = ((symbol.at.rot % 360.0) + 360.0) % 360.0;
    let mut parts = Vec::new();
    if rot != 0.0 {
        parts.push(format!("r{rot:.0}"));
    }
    match symbol.mirror {
        Mirror::None => {}
        Mirror::X => parts.push("mx".to_string()),
        Mirror::Y => parts.push("my".to_string()),
    }
    parts.join(" ")
}

fn pose(symbol: &SymbolInst) -> String {
    let orientation = orientation(symbol);
    if orientation.is_empty() {
        format!("@{:.2},{:.2}", symbol.at.x, symbol.at.y)
    } else {
        format!("@{:.2},{:.2} {orientation}", symbol.at.x, symbol.at.y)
    }
}

fn refdes_parts(refdes: &str) -> (&str, u64, &str) {
    let digits = refdes
        .char_indices()
        .find(|(_, character)| character.is_ascii_digit())
        .map_or(refdes.len(), |(index, _)| index);
    let end = refdes[digits..]
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit())
        .map_or(refdes.len(), |(index, _)| digits + index);
    let number = refdes[digits..end].parse().unwrap_or(0);
    (&refdes[..digits], number, &refdes[end..])
}

fn compare_refdes(left: &str, right: &str) -> Ordering {
    refdes_parts(left).cmp(&refdes_parts(right))
}

/// A pin's number with a distinct function name appended.
fn pin_label(pin: &PlacedPin) -> String {
    if pin.name.is_empty() || pin.name == "~" || pin.name == pin.number {
        pin.number.clone()
    } else {
        format!("{}({})", pin.number, pin.name)
    }
}

fn net_of_pin<'a>(netlist: &'a Netlist, pin: &PlacedPin) -> Option<&'a str> {
    netlist
        .nets
        .iter()
        .find(|net| {
            net.pins.iter().any(|candidate| {
                candidate.refdes == pin.refdes
                    && candidate.unit == pin.unit
                    && candidate.pin == pin.number
            })
        })
        .map(|net| net.name.as_str())
}

/// `1=VCC 2(G)=N_TR`, a symbol's pins and the nets they land on.
fn pin_map(pins: &[&PlacedPin], netlist: &Netlist) -> String {
    pins.iter()
        .map(|pin| {
            let label = pin_label(pin);
            match net_of_pin(netlist, pin) {
                Some(net) => format!("{label}={net}"),
                None => format!("{label}=-"),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn grouped_parts<'a>(symbols: impl Iterator<Item = &'a SymbolInst>) -> Vec<Vec<&'a SymbolInst>> {
    let mut symbols: Vec<&SymbolInst> = symbols.collect();
    symbols.sort_by(|left, right| {
        compare_refdes(left.refdes(), right.refdes()).then(left.unit.cmp(&right.unit))
    });
    let mut groups: Vec<Vec<&SymbolInst>> = Vec::new();
    for symbol in symbols {
        match groups.last_mut() {
            Some(group) if group[0].refdes() == symbol.refdes() => group.push(symbol),
            _ => groups.push(vec![symbol]),
        }
    }
    groups
}

fn write_full_part_suffix(out: &mut String, symbol: &SymbolInst) {
    let footprint = symbol
        .fields
        .get("Footprint")
        .map_or("", |field| &field.value);
    write!(out, " fp={footprint} uuid={}", symbol.uuid).expect("writing to a string cannot fail");
}

fn write_parts(
    out: &mut String,
    doc: &SchDoc,
    netlist: &Netlist,
    placed: &[PlacedPin],
    region: Option<Rect>,
    full: bool,
) {
    let all_parts: Vec<&SymbolInst> = doc
        .symbols()
        .filter(|symbol| !symbol.refdes().starts_with('#'))
        .collect();
    let ref_width = all_parts
        .iter()
        .map(|symbol| symbol.refdes().len())
        .max()
        .unwrap_or(3);
    let value_width = all_parts
        .iter()
        .map(|symbol| symbol.value().len())
        .max()
        .unwrap_or(5);
    let lib_width = all_parts
        .iter()
        .map(|symbol| symbol.lib_id.len())
        .max()
        .unwrap_or(6);
    let pose_width = all_parts
        .iter()
        .map(|symbol| pose(symbol).len())
        .max()
        .unwrap_or(1);
    let visible = all_parts
        .iter()
        .copied()
        .filter(|symbol| region.is_none_or(|bounds| bounds.contains(symbol.at.point())));

    for group in grouped_parts(visible) {
        let symbol = group[0];
        let unit_count = refs::units(doc, symbol.refdes()).len();
        write!(
            out,
            "{:<ref_width$}  {:<value_width$}  {:<lib_width$}  ",
            symbol.refdes(),
            symbol.value(),
            symbol.lib_id,
        )
        .expect("writing to a string cannot fail");
        if unit_count > 1 {
            write!(out, "{unit_count} units").expect("writing to a string cannot fail");
            if symbol.dnp {
                out.push_str(" DNP");
            }
            out.push('\n');
            for unit in group {
                let pins: Vec<&PlacedPin> =
                    placed.iter().filter(|pin| pin.owner == unit.uuid).collect();
                write!(
                    out,
                    "  unit {}  {:<pose_width$}  {}",
                    unit.unit,
                    pose(unit),
                    pin_map(&pins, netlist),
                )
                .expect("writing to a string cannot fail");
                if full {
                    write_full_part_suffix(out, unit);
                }
                out.push('\n');
            }
        } else {
            let pins: Vec<&PlacedPin> = placed
                .iter()
                .filter(|pin| pin.owner == symbol.uuid)
                .collect();
            write!(
                out,
                "{:<pose_width$}  {}",
                pose(symbol),
                pin_map(&pins, netlist),
            )
            .expect("writing to a string cannot fail");
            if symbol.dnp {
                out.push_str(" DNP");
            }
            if full {
                write_full_part_suffix(out, symbol);
            }
            out.push('\n');
        }
    }
}

fn write_power_symbols(out: &mut String, power: &[&SymbolInst]) {
    let mut counts = BTreeMap::new();
    for symbol in power {
        *counts.entry(symbol.value()).or_insert(0_usize) += 1;
    }
    out.push_str("\nPOWER SYMBOLS");
    for (value, count) in counts {
        write!(out, "  {value} ×{count}").expect("writing to a string cannot fail");
    }
    out.push('\n');
    let mut symbols = power.to_vec();
    symbols.sort_by(|left, right| compare_refdes(left.refdes(), right.refdes()));
    let ref_width = symbols
        .iter()
        .map(|symbol| symbol.refdes().len())
        .max()
        .unwrap_or(1);
    let value_width = symbols
        .iter()
        .map(|symbol| symbol.value().len())
        .max()
        .unwrap_or(1);
    for symbol in symbols {
        writeln!(
            out,
            "  {:<ref_width$}  {:<value_width$}  {}  uuid={}",
            symbol.refdes(),
            symbol.value(),
            pose(symbol),
            symbol.uuid,
        )
        .expect("writing to a string cannot fail");
    }
}

fn label_kind(kind: sch_doc::LabelKind) -> &'static str {
    match kind {
        sch_doc::LabelKind::Local => "local",
        sch_doc::LabelKind::Global => "global",
        sch_doc::LabelKind::Hier => "hierarchical",
    }
}

fn write_labels(out: &mut String, doc: &SchDoc) {
    let mut labels = doc.labels().collect::<Vec<_>>();
    labels.sort_by(|left, right| {
        sch_doc::unescape(&left.text)
            .cmp(&sch_doc::unescape(&right.text))
            .then_with(|| left.uuid.cmp(&right.uuid))
    });
    out.push_str("\nLABELS  (scope  name  @x,y  uuid)\n");
    for label in labels {
        writeln!(
            out,
            "  {}  {}  @{:.2},{:.2}  uuid={}",
            label_kind(label.kind),
            sch_doc::unescape(&label.text),
            label.at.x,
            label.at.y,
            label.uuid,
        )
        .expect("writing to a string cannot fail");
    }
}

fn write_wrapped_net(out: &mut String, net: &Net, name_width: usize) {
    let power_count = net
        .pins
        .iter()
        .filter(|pin| pin.refdes.starts_with('#'))
        .count();
    let mut items = Vec::new();
    if power_count > 0 {
        let noun = if power_count == 1 {
            "symbol"
        } else {
            "symbols"
        };
        items.push(format!("({power_count} power {noun})"));
    }
    items.extend(
        net.pins
            .iter()
            .filter(|pin| !pin.refdes.starts_with('#'))
            .map(refs::label),
    );
    let indent = " ".repeat(name_width + 2);
    let mut line = format!("{:<name_width$}  ", net.name);
    let mut has_item = false;
    for item in items {
        let separator = usize::from(has_item);
        if has_item && line.chars().count() + separator + item.chars().count() > 100 {
            writeln!(out, "{line}").expect("writing to a string cannot fail");
            line = format!("{indent}{item}");
        } else {
            if has_item {
                line.push(' ');
            }
            line.push_str(&item);
        }
        has_item = true;
    }
    writeln!(out, "{line}").expect("writing to a string cannot fail");
}

/// The whole sheet as sorted, grouped parts followed by connectivity.
pub fn read_schematic(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let (doc, netlist) = Edit::read(ctx)?;
    let full = input.get("detail").and_then(Value::as_str) == Some("full");
    let region = input
        .get("region")
        .and_then(Value::as_array)
        .and_then(|values| {
            let coordinates: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
            (coordinates.len() == 4).then(|| {
                Rect::new(
                    coordinates[0],
                    coordinates[1],
                    coordinates[2],
                    coordinates[3],
                )
            })
        });
    let placed = placed_pins(&doc);
    let power: Vec<&SymbolInst> = doc
        .symbols()
        .filter(|symbol| symbol.refdes().starts_with('#'))
        .collect();
    let part_count = doc.symbols().count() - power.len();
    let filename = ctx.sch_path().file_name().map_or_else(
        || ctx.sch_path().display().to_string(),
        |name| name.to_string_lossy().into(),
    );
    let mut out = format!(
        "{filename} — {part_count} parts, {} power symbols, {} nets\n\n\
         PARTS  (ref  value  symbol  @x,y rot  pins as number(name)=net; \"-\" = unconnected)\n",
        power.len(),
        netlist.nets.len(),
    );
    write_parts(&mut out, &doc, &netlist, &placed, region, full);
    write_power_symbols(&mut out, &power);
    write_labels(&mut out, &doc);

    out.push_str("\nNETS  (name: pins; power symbols counted, not listed)\n");
    let mut nets: Vec<&Net> = netlist.nets.iter().collect();
    nets.sort_by(|left, right| {
        let left_auto = left.source == NetSource::Auto;
        let right_auto = right.source == NetSource::Auto;
        left_auto
            .cmp(&right_auto)
            .then_with(|| left.name.cmp(&right.name))
    });
    let name_width = nets.iter().map(|net| net.name.len()).max().unwrap_or(1);
    for net in nets {
        write_wrapped_net(&mut out, net, name_width);
    }
    if !netlist.unconnected.is_empty() {
        write!(
            out,
            "\nUNCONNECTED  {}\n",
            netlist
                .unconnected
                .iter()
                .map(refs::label)
                .collect::<Vec<_>>()
                .join(" ")
        )
        .expect("writing to a string cannot fail");
    }
    if !netlist.no_connect.is_empty() {
        write!(
            out,
            "\nNO-CONNECT  {}\n",
            netlist
                .no_connect
                .iter()
                .map(refs::label)
                .collect::<Vec<_>>()
                .join(" ")
        )
        .expect("writing to a string cannot fail");
    }
    let bench = sch_floorplan::bench::benched(&doc);
    if !bench.is_empty() {
        write!(
            out,
            "\nBENCH  ({} placed and wired by name, not laid out — arrange them to finish)\n{}\n",
            bench.len(),
            bench.join(" ")
        )
        .expect("writing to a string cannot fail");
    }
    if !netlist.warnings.is_empty() {
        write!(out, "\nWARNINGS\n{}\n", netlist.warnings.join("\n"))
            .expect("writing to a string cannot fail");
    }
    Ok(Value::String(out))
}

fn body_size(doc: &SchDoc, symbol: &SymbolInst) -> String {
    body_rect(doc, symbol).map_or_else(
        || "-".to_string(),
        |body| {
            format!(
                "{:.2}×{:.2}mm",
                body.max_x - body.min_x,
                body.max_y - body.min_y
            )
        },
    )
}

fn user_fields(symbol: &SymbolInst) -> Vec<(&str, &str)> {
    let mut fields: Vec<(&str, &str)> = symbol
        .fields
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "Reference" | "Value" | "Footprint"))
        .map(|(name, field)| (name.as_str(), field.value.as_str()))
        .collect();
    fields.sort_unstable_by_key(|(name, _)| *name);
    fields
}

fn write_symbol_pin_table(
    out: &mut String,
    pins: &[&PlacedPin],
    netlist: &Netlist,
    widths: (usize, usize, usize, usize, usize),
) {
    let (pin_width, name_width, type_width, side_width, at_width) = widths;
    writeln!(
        out,
        "  {:<pin_width$}  {:<name_width$}  {:<type_width$}  {:<side_width$}  {:<at_width$}  net",
        "pin", "name", "type", "side", "at"
    )
    .expect("writing to a string cannot fail");
    for pin in pins {
        let at = format!("{:.2},{:.2}", pin.at.x, pin.at.y);
        let net = net_of_pin(netlist, pin).unwrap_or("-");
        writeln!(
            out,
            "  {:<pin_width$}  {:<name_width$}  {:<type_width$}  {:<side_width$}  {:<at_width$}  {net}",
            pin.number,
            pin.name,
            pin.etype,
            side_of(pin),
            at,
        )
        .expect("writing to a string cannot fail");
    }
}

/// One symbol in full as aligned plain text.
pub fn get_symbol(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(refdes) = input.get("ref").and_then(Value::as_str) else {
        return Ok(json!({ "error": "get_symbol needs `ref`" }));
    };
    let (doc, netlist) = Edit::read(ctx)?;
    let qualified = resolve_unit_reference(&doc, refdes);
    let lookup = qualified
        .as_ref()
        .map_or(refdes, |(reference, _)| reference.as_str());
    let Some(symbol) = qualified
        .as_ref()
        .and_then(|(_, unit)| {
            refs::units(&doc, lookup)
                .into_iter()
                .find(|(candidate, _)| candidate == unit)
                .and_then(|(_, uuid)| doc.symbol(&uuid))
        })
        .or_else(|| doc.symbol_by_ref(lookup))
    else {
        return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
    };
    let placed = placed_pins(&doc);
    let units: Vec<(u32, &SymbolInst)> = refs::units(&doc, lookup)
        .iter()
        .filter(|(unit, _)| qualified.as_ref().is_none_or(|(_, wanted)| unit == wanted))
        .filter_map(|(unit, uuid)| Some((*unit, doc.symbol(uuid)?)))
        .collect();
    let all_pins: Vec<&PlacedPin> = units
        .iter()
        .flat_map(|(_, unit)| placed.iter().filter(|pin| pin.owner == unit.uuid))
        .collect();
    let widths = (
        all_pins
            .iter()
            .map(|pin| pin.number.len())
            .max()
            .unwrap_or(0)
            .max("pin".len()),
        all_pins
            .iter()
            .map(|pin| pin.name.len())
            .max()
            .unwrap_or(0)
            .max("name".len()),
        all_pins
            .iter()
            .map(|pin| pin.etype.len())
            .max()
            .unwrap_or(0)
            .max("type".len()),
        all_pins
            .iter()
            .map(|pin| side_of(pin).len())
            .max()
            .unwrap_or(0)
            .max("side".len()),
        all_pins
            .iter()
            .map(|pin| format!("{:.2},{:.2}", pin.at.x, pin.at.y).len())
            .max()
            .unwrap_or(0)
            .max("at".len()),
    );
    let footprint = symbol
        .fields
        .get("Footprint")
        .map_or("", |field| &field.value);
    let unit_note = if let Some((_, unit)) = qualified {
        format!("  requested as {refdes} → unit {unit}")
    } else if units.len() > 1 {
        format!("  {} units", units.len())
    } else {
        String::new()
    };
    let mut out = format!(
        "{}  {}  {}{unit_note}  fp={footprint}  DNP={}  in_bom={}\n",
        symbol.refdes(),
        symbol.value(),
        symbol.lib_id,
        if symbol.dnp { "yes" } else { "no" },
        if symbol.in_bom { "yes" } else { "no" },
    );
    let fields = user_fields(symbol);
    out.push_str("fields");
    if fields.is_empty() {
        out.push_str("  -");
    } else {
        for (name, value) in fields {
            write!(out, "  {name}={value}").expect("writing to a string cannot fail");
        }
    }
    out.push_str("\n\n");
    for (index, (unit_number, unit)) in units.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if units.len() > 1 {
            write!(out, "unit {unit_number}  ").expect("writing to a string cannot fail");
        }
        writeln!(
            out,
            "{}  body {}  uuid={}",
            pose(unit),
            body_size(&doc, unit),
            unit.uuid,
        )
        .expect("writing to a string cannot fail");
        let pins: Vec<&PlacedPin> = placed.iter().filter(|pin| pin.owner == unit.uuid).collect();
        write_symbol_pin_table(&mut out, &pins, &netlist, widths);
    }
    Ok(Value::String(out))
}

fn resolve_unit_reference(doc: &SchDoc, requested: &str) -> Option<(String, u32)> {
    if doc.symbol_by_ref(requested).is_some() {
        return None;
    }
    let suffix = requested.chars().last()?;
    if !suffix.is_ascii_uppercase() {
        return None;
    }
    let base = requested.strip_suffix(suffix)?;
    if !base.ends_with(|character: char| character.is_ascii_digit()) {
        return None;
    }
    let unit = u32::from(suffix as u8 - b'A') + 1;
    refs::units(doc, base)
        .iter()
        .any(|(candidate, _)| *candidate == unit)
        .then(|| (base.to_string(), unit))
}

fn net_source(source: NetSource) -> &'static str {
    match source {
        NetSource::Auto => "auto",
        NetSource::SheetPin => "sheet pin",
        NetSource::Hier => "hierarchical label",
        NetSource::Local => "local label",
        NetSource::Power => "power symbol",
        NetSource::Global => "global label",
    }
}

fn power_symbol_net(symbol: &SymbolInst) -> Option<&str> {
    if !symbol.refdes().starts_with("#PWR") {
        return None;
    }
    match symbol.value() {
        "" => symbol.lib_id.strip_prefix("power:"),
        value => Some(value),
    }
}

fn placed_net_pin<'a>(placed: &'a [PlacedPin], pin: &PinRef) -> Option<&'a PlacedPin> {
    placed.iter().find(|candidate| {
        candidate.refdes == pin.refdes && candidate.unit == pin.unit && candidate.number == pin.pin
    })
}

/// One net as an aligned pin listing with its naming source.
pub fn get_net(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(name) = input.get("name").and_then(Value::as_str) else {
        return Ok(json!({ "error": "get_net needs `name`" }));
    };
    let (doc, netlist) = Edit::read(ctx)?;
    let power_only = || {
        let pins = placed_pins(&doc)
            .into_iter()
            .filter(|pin| pin.refdes.starts_with("#PWR"))
            .filter(|pin| {
                doc.symbol(&pin.owner)
                    .and_then(power_symbol_net)
                    .is_some_and(|net| net == name)
            })
            .map(|pin| PinRef {
                refdes: pin.refdes,
                unit: pin.unit,
                pin: pin.number,
                dnp: pin.dnp,
            })
            .collect::<Vec<_>>();
        let exists = doc
            .symbols()
            .any(|symbol| power_symbol_net(symbol) == Some(name));
        exists.then(|| Net {
            name: name.to_string(),
            source: NetSource::Power,
            pins,
        })
    };
    let synthetic;
    let net = if let Some(net) = netlist.nets.iter().find(|net| net.name == name) {
        net
    } else if let Some(net) = power_only() {
        synthetic = net;
        &synthetic
    } else {
        let mut known = netlist
            .nets
            .iter()
            .map(|net| net.name.clone())
            .chain(
                doc.symbols()
                    .filter_map(power_symbol_net)
                    .map(str::to_string),
            )
            .collect::<Vec<_>>();
        known.sort();
        known.dedup();
        if let Some(resolved) = unique_close_net(name, &known) {
            let mut result = get_net(json!({"name": resolved}), ctx)?;
            return match result.as_object_mut() {
                Some(object) => {
                    object.insert("resolved_from".to_string(), json!(name));
                    Ok(result)
                }
                None => Ok(json!({
                    "resolved_from": name,
                    "name": resolved,
                    "report": result,
                })),
            };
        }
        return Ok(json!({ "error": format!("no net `{name}`"), "nets": known }));
    };
    let placed = placed_pins(&doc);
    let pins: Vec<&PlacedPin> = net
        .pins
        .iter()
        .filter(|pin| !pin.refdes.starts_with('#'))
        .filter_map(|pin| placed_net_pin(&placed, pin))
        .collect();
    let ref_width = pins
        .iter()
        .map(|pin| format!("{}.{}", pin.refdes, pin.number).len())
        .max()
        .unwrap_or(1);
    let name_width = pins.iter().map(|pin| pin.name.len()).max().unwrap_or(1);
    let type_width = pins.iter().map(|pin| pin.etype.len()).max().unwrap_or(1);
    let mut out = format!(
        "NET {}  {} pins  named by: {}\n",
        net.name,
        pins.len(),
        net_source(net.source),
    );
    for pin in pins {
        let reference = format!("{}.{}", pin.refdes, pin.number);
        writeln!(
            out,
            "{reference:<ref_width$}  {:<name_width$}  {:<type_width$}  @{:.2},{:.2}",
            pin.name, pin.etype, pin.at.x, pin.at.y,
        )
        .expect("writing to a string cannot fail");
    }
    let scene = sch_doc::connect::scene(&doc);
    let labels = doc
        .labels()
        .filter(|label| {
            sch_doc::unescape(&label.text) == net.name
                || scene.points.iter().any(|(point, point_net)| {
                    point_net == &net.name && point.near_eq(label.at.point(), geom::EPS)
                })
        })
        .collect::<Vec<_>>();
    if !labels.is_empty() {
        out.push_str("LABELS\n");
        for label in labels {
            writeln!(
                out,
                "{}  @{:.2},{:.2}  uuid={}",
                label_kind(label.kind),
                label.at.x,
                label.at.y,
                label.uuid,
            )
            .expect("writing to a string cannot fail");
        }
    }
    Ok(Value::String(out))
}

fn unique_close_net<'a>(query: &str, candidates: &'a [String]) -> Option<&'a str> {
    let matcher = SkimMatcherV2::default().ignore_case();
    let mut matches = candidates
        .iter()
        .filter(|candidate| candidate.as_str() != query)
        .filter_map(|candidate| {
            let score = matcher
                .fuzzy_match(candidate, query)
                .into_iter()
                .chain(matcher.fuzzy_match(query, candidate))
                .max()?;
            Some((score, candidate.as_str()))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));
    let (best_score, best) = matches.first().copied()?;
    (matches.get(1).is_none_or(|(score, _)| *score < best_score)).then_some(best)
}
