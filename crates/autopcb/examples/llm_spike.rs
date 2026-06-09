//! Validation spike: compile circuit YAML against the REAL KiCAD symbol
//! libraries. Not production code — Plan 2's kicad-bridge replaces this.
//!
//! Usage:
//!   cargo run -p autopcb --example llm_spike -- --dump MCU_ST_STM32H7:STM32H743VITx
//!   cargo run -p autopcb --example llm_spike -- design.circuit.yaml

use circuit_lang::{PinMeta, PinType, SymbolMeta, SymbolProvider, compile};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

const SYMBOL_DIR: &str = "/usr/share/kicad/symbols";

/// Minimal depth-tracking S-expression scanner for .kicad_sym files.
/// Extracts (symbol "NAME" ...) blocks at library-root depth and their pins.
struct LibScanner;

impl LibScanner {
    /// Split source into top-level `(symbol "NAME" ...)` blocks (depth 1).
    fn top_level_symbols(src: &str) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let bytes = src.as_bytes();
        let mut depth = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    // skip string
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                }
                b'(' => {
                    depth += 1;
                    if depth == 2 {
                        let rest = &src[i..];
                        if let Some(name) = rest
                            .strip_prefix("(symbol \"")
                            .and_then(|r| r.split('"').next())
                        {
                            // capture the whole balanced block
                            let start = i;
                            let mut d = 0usize;
                            let mut j = i;
                            while j < bytes.len() {
                                match bytes[j] {
                                    b'"' => {
                                        j += 1;
                                        while j < bytes.len() && bytes[j] != b'"' {
                                            if bytes[j] == b'\\' {
                                                j += 1;
                                            }
                                            j += 1;
                                        }
                                    }
                                    b'(' => d += 1,
                                    b')' => {
                                        d -= 1;
                                        if d == 0 {
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                                j += 1;
                            }
                            out.insert(
                                name.to_string(),
                                src[start..=j.min(src.len() - 1)].to_string(),
                            );
                            depth -= 1; // we consumed the block; rewind depth
                            i = j;
                        }
                    }
                }
                b')' => depth = depth.saturating_sub(1),
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// Extract pins from one symbol block: (pin <etype> ... (name "N") (number "M"))
    fn pins(block: &str) -> Vec<PinMeta> {
        let mut pins = Vec::new();
        let mut rest = block;
        while let Some(p) = rest.find("(pin ") {
            rest = &rest[p + 5..];
            let etype_tok: String = rest
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != ')')
                .collect();
            let name = Self::quoted_after(rest, "(name \"");
            let number = Self::quoted_after(rest, "(number \"");
            if let (Some(name), Some(number)) = (name, number) {
                let etype = match etype_tok.as_str() {
                    "power_in" => PinType::PowerInput,
                    "power_out" => PinType::PowerOutput,
                    "passive" => PinType::Passive,
                    _ => PinType::Other,
                };
                pins.push(PinMeta {
                    number,
                    name,
                    etype,
                    unit: 1,
                });
            }
        }
        pins
    }

    fn quoted_after(s: &str, marker: &str) -> Option<String> {
        let i = s.find(marker)?;
        s[i + marker.len()..].split('"').next().map(str::to_string)
    }

    fn extends_target(block: &str) -> Option<String> {
        Self::quoted_after(block, "(extends \"")
    }
}

/// SymbolProvider over the real installed KiCAD libraries, with `extends`
/// resolution. Lazily loads one .kicad_sym per referenced library.
struct RealLibProvider {
    cache: RefCell<HashMap<String, SymbolMeta>>,
    libs: RefCell<HashMap<String, HashMap<String, String>>>,
}

impl RealLibProvider {
    fn new() -> Self {
        Self {
            cache: RefCell::new(HashMap::new()),
            libs: RefCell::new(HashMap::new()),
        }
    }

    fn lib_blocks(&self, lib: &str) -> Option<HashMap<String, String>> {
        if let Some(b) = self.libs.borrow().get(lib) {
            return Some(b.clone());
        }
        let path = PathBuf::from(SYMBOL_DIR).join(format!("{lib}.kicad_sym"));
        let src = std::fs::read_to_string(path).ok()?;
        let blocks = LibScanner::top_level_symbols(&src);
        self.libs
            .borrow_mut()
            .insert(lib.to_string(), blocks.clone());
        Some(blocks)
    }

    fn resolve(&self, lib: &str, sym: &str, hops: u8) -> Option<SymbolMeta> {
        if hops > 4 {
            return None; // extends cycle guard
        }
        let blocks = self.lib_blocks(lib)?;
        let block = blocks.get(sym)?;
        let mut pins = LibScanner::pins(block);
        if pins.is_empty()
            && let Some(parent) = LibScanner::extends_target(block)
            && let Some(meta) = self.resolve(lib, &parent, hops + 1)
        {
            pins = meta.pins;
        }
        Some(SymbolMeta { pins })
    }
}

impl SymbolProvider for RealLibProvider {
    fn symbol(&self, lib_id: &str) -> Option<&SymbolMeta> {
        let (lib, sym) = lib_id.split_once(':')?;
        if !self.cache.borrow().contains_key(lib_id) {
            let meta = self.resolve(lib, sym, 0)?;
            self.cache.borrow_mut().insert(lib_id.to_string(), meta);
        }
        // SAFETY-free leak: spike-only — cache entries live for program duration.
        let cache = self.cache.borrow();
        let meta = cache.get(lib_id)?;
        Some(Box::leak(Box::new(meta.clone())))
    }

    fn suggest(&self, lib_id: &str) -> Vec<String> {
        let Some((lib, sym)) = lib_id.split_once(':') else {
            return vec![];
        };
        let Some(blocks) = self.lib_blocks(lib) else {
            return vec![];
        };
        let mut hits: Vec<(usize, String)> = blocks
            .keys()
            .filter(|k| !k.contains("_0_") && !k.contains("_1_"))
            .map(|k| {
                (
                    strdist(&sym.to_lowercase(), &k.to_lowercase()),
                    format!("{lib}:{k}"),
                )
            })
            .filter(|(d, _)| *d <= 6)
            .collect();
        hits.sort();
        hits.into_iter().take(3).map(|(_, k)| k).collect()
    }
}

/// Tiny Levenshtein (avoid pulling strsim into autopcb for a spike).
fn strdist(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let provider = RealLibProvider::new();

    match args.as_slice() {
        [flag, lib_id] if flag == "--dump" => match provider.symbol(lib_id) {
            Some(meta) => {
                println!("{lib_id}: {} pins", meta.pins.len());
                for p in &meta.pins {
                    println!("  {:>4}  {:<12} {:?}", p.number, p.name, p.etype);
                }
            }
            None => {
                eprintln!("symbol not found: {lib_id}");
                for s in provider.suggest(lib_id) {
                    eprintln!("  did you mean: {s}");
                }
                std::process::exit(1);
            }
        },
        [yaml_path] => {
            let src = std::fs::read_to_string(yaml_path).expect("read yaml");
            let result = compile(&src, &provider);
            for d in &result.diagnostics.0 {
                println!("{d}");
            }
            match result.design {
                Some(design) => {
                    let comps: usize = design.blocks.values().map(|b| b.components.len()).sum();
                    let nets: std::collections::BTreeSet<_> = design
                        .blocks
                        .values()
                        .flat_map(|b| b.components.values())
                        .flat_map(|c| {
                            c.pins
                                .values()
                                .chain(c.units.values().flatten().map(|(_, t)| t))
                        })
                        .filter_map(|t| match t {
                            circuit_lang::model::PinTarget::Net(n) => Some(n.clone()),
                            _ => None,
                        })
                        .collect();
                    println!("\nCOMPILE OK: {} components, {} nets", comps, nets.len());
                }
                None => {
                    println!("\nCOMPILE FAILED (errors above)");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("usage: llm_spike --dump <Lib:Symbol> | llm_spike <design.circuit.yaml>");
            std::process::exit(2);
        }
    }
}
