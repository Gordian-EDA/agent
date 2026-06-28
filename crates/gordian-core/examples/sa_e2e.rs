//! End-to-end placement scoreboard across validation fixtures (anneal engine).
//!
//! Usage: cargo run --release -p gordian-core --example sa_e2e [name ...]

use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan::{self, LayoutIr};

const FIXTURES: &[&str] = &[
    "divider-filter",
    "mcp1703-power-entry",
    "555-blinker",
    "uart-level-translator",
    "grid-demo",
    "rf-lna-frontend",
    "mixed-signal-adc-frontend",
    "bedrock-oneshot-bluepill",
    "bedrock-selfrepair-bluepill",
    "bga-fpga-ice40",
];

fn render(env: &KicadEnv, provider: &SymbolTable, name: &str) -> anyhow::Result<usize> {
    let dir = std::path::Path::new("docs/validation");
    let src = std::fs::read_to_string(dir.join(format!("{name}.circuit.yaml")))?;
    let result = circuit_lang::compile(&src, provider);
    let design = result
        .design
        .ok_or_else(|| anyhow::anyhow!("compile failed: {name}"))?;
    let ir = match std::fs::read_to_string(dir.join(format!("{name}.layout.json"))) {
        Ok(s) => LayoutIr::from_json(&s)?,
        Err(_) => floorplan::infer_ir(env, &design),
    };
    let out = floorplan::emit_strategy(env, &design, Box::new(anneal_place::Anneal), Some(ir))?;
    Ok(out.layout_warnings.len())
}

fn main() -> anyhow::Result<()> {
    let env = KicadEnv::detect().expect("no KiCAD environment");
    let provider = SymbolTable::from_env(&env);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let names: Vec<&str> = if args.is_empty() {
        FIXTURES.to_vec()
    } else {
        args.iter().map(String::as_str).collect()
    };

    println!("{:<30} {:>8} {:>10}", "fixture", "warn", "anneal_s");
    println!("{}", "-".repeat(52));
    let mut perfect = 0;
    let mut slowest = 0.0_f64;
    for name in &names {
        let t0 = std::time::Instant::now();
        let w = render(&env, &provider, name)
            .map(|n| n.to_string())
            .unwrap_or_else(|e| format!("ERR:{e}"));
        let secs = t0.elapsed().as_secs_f64();
        slowest = slowest.max(secs);
        if w == "0" {
            perfect += 1;
        }
        let flag = if secs > 5.0 { " !!>5s" } else { "" };
        println!("{name:<30} {w:>8} {secs:>10.2}{flag}");
    }
    println!("{}", "-".repeat(52));
    println!(
        "{:<30} {:>8} {:>10.2}",
        "0-warn / slowest",
        format!("{perfect}/{}", names.len()),
        slowest
    );
    Ok(())
}
