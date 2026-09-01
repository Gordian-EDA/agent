//! Print a schematic's placement and connectivity as JSON, for diffing a file
//! before and after an edit.
//!
//! Usage: `cargo run -p sch-doc --example snapshot -- <file.kicad_sch>`

use std::collections::BTreeMap;

use sch_doc::{SchDoc, connect};

fn quote(text: &str) -> String {
    let escaped: String = text
        .chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' => vec!['\\', 'n'],
            other => vec![other],
        })
        .collect();
    format!("\"{escaped}\"")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: snapshot <file>")?;
    let doc = SchDoc::read(&path)?;

    let mut symbols: BTreeMap<String, String> = BTreeMap::new();
    for symbol in doc.symbols() {
        let key = format!("{}#{}", symbol.refdes(), symbol.uuid);
        symbols.insert(
            key,
            format!(
                "[{:.4},{:.4},{:.1},{},{}]",
                symbol.at.x,
                symbol.at.y,
                symbol.at.rot,
                quote(&symbol.lib_id),
                quote(symbol.value())
            ),
        );
    }

    let netlist = connect::extract(&doc);
    let mut nets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for net in &netlist.nets {
        let mut pins: Vec<String> = net
            .pins
            .iter()
            .map(|p| format!("{}.{}", p.refdes, p.pin))
            .collect();
        pins.sort();
        pins.dedup();
        nets.insert(net.name.clone(), pins);
    }

    let render = |map: BTreeMap<String, String>| {
        map.into_iter()
            .map(|(k, v)| format!("{}:{v}", quote(&k)))
            .collect::<Vec<_>>()
            .join(",")
    };
    let net_body = nets
        .into_iter()
        .map(|(name, pins)| {
            let list = pins.iter().map(|p| quote(p)).collect::<Vec<_>>().join(",");
            format!("{}:[{list}]", quote(&name))
        })
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "{{\"symbols\":{{{}}},\"nets\":{{{net_body}}}}}",
        render(symbols)
    );
    Ok(())
}
