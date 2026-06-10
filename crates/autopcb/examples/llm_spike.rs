//! Validation spike: exercise the production `kicad-bridge` provider and
//! search index against the REAL KiCAD symbol libraries.
//!
//! Usage:
//!   cargo run -p autopcb --example llm_spike -- --dump MCU_ST_STM32H7:STM32H743VITx
//!   cargo run -p autopcb --example llm_spike -- --search "usb-c receptacle"
//!   cargo run -p autopcb --example llm_spike -- design.circuit.yaml

use circuit_lang::{SymbolProvider, compile};
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;
use kicad_bridge::search::SymbolIndex;

fn main() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("error: no KiCAD installation found (set AUTO_PCB_SYMBOL_DIR?)");
        std::process::exit(1);
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag, lib_id] if flag == "--dump" => dump(env, lib_id),
        [flag, query] if flag == "--search" => search(&env, query),
        [flag, yaml_path, out_path] if flag == "--emit" => emit(env, yaml_path, out_path),
        [yaml_path] => compile_design(env, yaml_path),
        _ => {
            eprintln!(
                "usage: llm_spike --dump <Lib:Symbol>\n       \
                 llm_spike --search <query>\n       \
                 llm_spike --emit <design.circuit.yaml> <out.kicad_sch>\n       \
                 llm_spike <design.circuit.yaml>"
            );
            std::process::exit(2);
        }
    }
}

/// Compile a circuit YAML and emit a `.kicad_sch`, then run ERC on it.
fn emit(env: KicadEnv, yaml_path: &str, out_path: &str) {
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(yaml_path).expect("read yaml");
    let result = circuit_lang::compile(&src, &provider);
    for d in result
        .diagnostics
        .0
        .iter()
        .filter(|d| matches!(d.severity, circuit_lang::Severity::Error))
    {
        eprintln!("{d}");
    }
    let Some(design) = result.design else {
        eprintln!("COMPILE FAILED");
        std::process::exit(1);
    };
    let text = sch_engine::emit_design(&env, &design).expect("emit");
    std::fs::write(out_path, &text).expect("write .kicad_sch");
    let comps: usize = design.blocks.values().map(|b| b.components.len()).sum();
    let report = kicad_bridge::cli::KicadCli::new(&env)
        .erc(std::path::Path::new(out_path))
        .expect("erc");
    println!(
        "EMITTED {out_path}: {comps} components, ERC {} errors / {} warnings",
        report.error_count(),
        report.warning_count()
    );
}

/// Print a symbol's pins, or suggestions when it is not found.
fn dump(env: KicadEnv, lib_id: &str) {
    let provider = RealSymbolProvider::new(env);
    match provider.symbol(lib_id) {
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
    }
}

/// Fuzzy cross-library search; print ranked `Lib:Symbol` hits with pin counts.
fn search(env: &KicadEnv, query: &str) {
    let idx = match SymbolIndex::build(env) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("error: failed to build symbol index: {e}");
            std::process::exit(1);
        }
    };
    let hits = idx.search(query, 10);
    if hits.is_empty() {
        eprintln!("no matches for {query:?}");
        std::process::exit(1);
    }
    for h in hits {
        println!("  {:<48} {} pins", h.lib_id, h.pin_count);
    }
}

/// Compile a circuit YAML against the real libraries.
fn compile_design(env: KicadEnv, yaml_path: &str) {
    let provider = RealSymbolProvider::new(env);
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
