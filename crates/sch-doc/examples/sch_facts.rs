//! Deterministic schematic facts for the quality harness.
//!
//! ```text
//! sch_facts <project-dir-or-sch>            # symbols + netlist + warnings
//! sch_facts --diff <before> <after>         # net partition delta
//! ```
//!
//! Prints JSON on stdout. The quality runner is the only consumer, so the shape
//! is flat and stable rather than general: everything a check or a judge needs
//! about one schematic, and nothing else.

use std::path::{Path, PathBuf};

use sch_doc::{Netlist, SchDoc, SymbolInst, connect};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = match args.first().map(String::as_str) {
        Some("--diff") if args.len() == 3 => diff(Path::new(&args[1]), Path::new(&args[2])),
        Some(path) if args.len() == 1 && !path.starts_with("--") => facts(Path::new(path)),
        _ => {
            eprintln!("usage: sch_facts <project> | sch_facts --diff <before> <after>");
            std::process::exit(2);
        }
    };
    println!("{out}");
}

/// Every `.kicad_sch` under `root`, sorted; `root` itself if it is one.
/// Hidden directories are skipped: `.gordian/` holds undo snapshots and
/// renders, not sheets of the design.
fn sheet_paths(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let hidden = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'));
            if hidden {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "kicad_sch") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

struct Sheet {
    name: String,
    doc: SchDoc,
    netlist: Netlist,
}

fn load(root: &Path) -> (Vec<Sheet>, Vec<String>) {
    let mut sheets = Vec::new();
    let mut errors = Vec::new();
    for path in sheet_paths(root) {
        let name = path
            .strip_prefix(root)
            .ok()
            .filter(|rest| !rest.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(path.file_name().unwrap_or_default()))
            .to_string_lossy()
            .into_owned();
        match SchDoc::read(&path) {
            Ok(doc) => {
                let netlist = connect::extract(&doc);
                sheets.push(Sheet { name, doc, netlist });
            }
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    (sheets, errors)
}

fn facts(root: &Path) -> String {
    let (sheets, errors) = load(root);
    let mut symbols = Vec::new();
    let mut warnings = Vec::new();
    let mut partition: Vec<Vec<String>> = Vec::new();
    let mut unconnected = Vec::new();
    let mut no_connect = Vec::new();

    for sheet in &sheets {
        for symbol in sheet.doc.symbols() {
            let refdes = field(symbol, "Reference");
            symbols.push(Symbol {
                // Always unit-qualified: a multi-unit part must stay
                // distinguishable, and a part that gains or loses units must
                // not silently re-key the ones it kept.
                key: format!("{refdes}/{}", symbol.unit),
                refdes,
                uuid: symbol.uuid.clone(),
                sheet: sheet.name.clone(),
                lib_id: symbol.lib_id.clone(),
                unit: symbol.unit,
                x: symbol.at.x,
                y: symbol.at.y,
                rot: symbol.at.rot,
                mirror: format!("{:?}", symbol.mirror),
                dnp: symbol.dnp,
                fields: symbol
                    .fields
                    .iter()
                    .map(|(name, field)| (name.clone(), field.value.clone()))
                    .collect(),
            });
        }
        for warning in &sheet.netlist.warnings {
            warnings.push(format!("{}: {warning}", sheet.name));
        }
        partition.extend(sheet.netlist.partition());
        unconnected.extend(sheet.netlist.unconnected.iter().map(pin_name));
        no_connect.extend(sheet.netlist.no_connect.iter().map(pin_name));
    }
    symbols.sort_by(|a, b| (&a.sheet, &a.key).cmp(&(&b.sheet, &b.key)));
    partition.sort();
    unconnected.sort();
    no_connect.sort();

    let nets: Vec<String> = sheets
        .iter()
        .flat_map(|s| s.netlist.nets.iter().map(|n| n.name.clone()))
        .collect();
    let power_symbols = symbols.iter().filter(|s| s.refdes.starts_with('#')).count();

    let mut out = Object::new();
    out.add("sheets", array(sheets.iter().map(|s| quote(&s.name))));
    out.add("symbol_count", symbols.len().to_string());
    out.add("power_symbols", power_symbols.to_string());
    out.add("part_count", (symbols.len() - power_symbols).to_string());
    out.add("symbols", array(symbols.iter().map(Symbol::json)));
    out.add("nets", array(nets.iter().map(|n| quote(n))));
    out.add(
        "partition",
        array(partition.iter().map(|net| array(net.iter().map(|p| quote(p))))),
    );
    out.add("unconnected_pins", array(unconnected.iter().map(|p| quote(p))));
    out.add("no_connect_pins", array(no_connect.iter().map(|p| quote(p))));
    out.add("extractor_warnings", array(warnings.iter().map(|w| quote(w))));
    out.add("errors", array(errors.iter().map(|e| quote(e))));
    out.finish()
}

fn diff(before: &Path, after: &Path) -> String {
    let (before_sheets, _) = load(before);
    let (after_sheets, _) = load(after);
    let delta = Netlist::diff(&merge(&before_sheets), &merge(&after_sheets));

    let pair = |from: String, to: String| format!("{{\"from\":{from},\"to\":{to}}}");
    let mut out = Object::new();
    out.add("created", array(delta.created.iter().map(|n| quote(n))));
    out.add("removed", array(delta.removed.iter().map(|n| quote(n))));
    out.add(
        "merged",
        array(delta.merged.iter().map(|(from, to)| {
            pair(array(from.iter().map(|n| quote(n))), quote(to))
        })),
    );
    out.add(
        "split",
        array(delta.split.iter().map(|(from, to)| {
            pair(quote(from), array(to.iter().map(|n| quote(n))))
        })),
    );
    out.add(
        "renamed",
        array(delta.renamed.iter().map(|(from, to)| pair(quote(from), quote(to)))),
    );
    out.add(
        "pins_now_unconnected",
        array(delta.pins_now_unconnected.iter().map(|p| quote(&pin_name(p)))),
    );
    out.add(
        "pins_now_connected",
        array(delta.pins_now_connected.iter().map(|p| quote(&pin_name(p)))),
    );
    out.add("unchanged", delta.is_empty().to_string());
    out.finish()
}

/// One netlist over the whole project: the sheets' nets side by side. Cases are
/// single-sheet, where this is that sheet's own netlist; on a multi-sheet
/// project it is a flat union, which is all a partition delta needs.
fn merge(sheets: &[Sheet]) -> Netlist {
    let mut merged = Netlist::default();
    for sheet in sheets {
        merged.nets.extend(sheet.netlist.nets.iter().cloned());
        merged
            .unconnected
            .extend(sheet.netlist.unconnected.iter().cloned());
        merged
            .no_connect
            .extend(sheet.netlist.no_connect.iter().cloned());
    }
    merged.nets.sort_by(|a, b| a.name.cmp(&b.name));
    merged
}

struct Symbol {
    key: String,
    refdes: String,
    uuid: String,
    sheet: String,
    lib_id: String,
    unit: u32,
    x: f64,
    y: f64,
    rot: f64,
    mirror: String,
    dnp: bool,
    fields: Vec<(String, String)>,
}

impl Symbol {
    fn json(&self) -> String {
        let fields = self
            .fields
            .iter()
            .map(|(name, value)| format!("{}:{}", quote(name), quote(value)))
            .collect::<Vec<_>>()
            .join(",");
        let mut out = Object::new();
        out.add("key", quote(&self.key));
        out.add("ref", quote(&self.refdes));
        out.add("uuid", quote(&self.uuid));
        out.add("sheet", quote(&self.sheet));
        out.add("lib_id", quote(&self.lib_id));
        out.add("unit", self.unit.to_string());
        out.add("x", round(self.x));
        out.add("y", round(self.y));
        out.add("rot", round(self.rot));
        out.add("mirror", quote(&self.mirror));
        out.add("dnp", self.dnp.to_string());
        out.add("fields", format!("{{{fields}}}"));
        out.finish()
    }
}

fn field(symbol: &SymbolInst, name: &str) -> String {
    symbol.fields.get(name).map(|f| f.value.clone()).unwrap_or_default()
}

fn pin_name(pin: &sch_doc::PinRef) -> String {
    format!("{}.{}", pin.refdes, pin.pin)
}

/// A coordinate as JSON. A non-finite float has no JSON spelling, and
/// `{:.4}` would emit a bare `NaN` that breaks the whole document.
fn round(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.4}")
    } else {
        "null".to_string()
    }
}

/// A comma-joined JSON object built key by key.
#[derive(Default)]
struct Object {
    body: String,
}

impl Object {
    fn new() -> Object {
        Object::default()
    }

    fn add(&mut self, key: &str, value: String) {
        if !self.body.is_empty() {
            self.body.push(',');
        }
        self.body.push_str(&quote(key));
        self.body.push(':');
        self.body.push_str(&value);
    }

    fn finish(self) -> String {
        format!("{{{}}}", self.body)
    }
}

fn array(items: impl Iterator<Item = String>) -> String {
    format!("[{}]", items.collect::<Vec<_>>().join(","))
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
