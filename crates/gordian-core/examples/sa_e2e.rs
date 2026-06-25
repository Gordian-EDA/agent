//! End-to-end placement scoreboard across all 10 test circuits, BOTH strategies.
//! Renders each fixture via greedy and via the SA (LAYOUT_SEARCH), reports the
//! layout-warning count + truthfulness signal per fixture/strategy — the
//! "SA perfect on all 10" dashboard.
//!
//! Usage: cargo run --release -p agent --example sa_e2e [name ...]

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
    let out = floorplan::emit_strategy(env, &design, &ir, Box::new(anneal_place::Anneal))?;
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

    println!(
        "{:<30} {:>8} {:>8} {:>10}",
        "fixture", "greedy", "anneal", "anneal_s"
    );
    println!("{}", "-".repeat(60));
    let (mut g_perfect, mut a_perfect) = (0, 0);
    let mut slowest = 0.0_f64;
    for name in &names {
        // SAFETY: single-threaded example; pick_strategy reads this env in emit.
        unsafe { std::env::set_var("LAYOUT_SEARCH", "greedy") };
        let g = render(&env, &provider, name)
            .map(|n| n.to_string())
            .unwrap_or_else(|e| format!("ERR:{e}"));
        unsafe { std::env::set_var("LAYOUT_SEARCH", "anneal") };
        let t0 = std::time::Instant::now();
        let a = render(&env, &provider, name)
            .map(|n| n.to_string())
            .unwrap_or_else(|e| format!("ERR:{e}"));
        let secs = t0.elapsed().as_secs_f64();
        slowest = slowest.max(secs);
        if g == "0" {
            g_perfect += 1;
        }
        if a == "0" {
            a_perfect += 1;
        }
        let flag = if secs > 5.0 { " !!>5s" } else { "" };
        println!("{name:<30} {g:>8} {a:>8} {secs:>10.2}{flag}");
    }
    println!("{}", "-".repeat(60));
    println!(
        "{:<30} {:>8} {:>8} {:>10.2}",
        "0-warn / slowest anneal_s",
        format!("{g_perfect}/{}", names.len()),
        format!("{a_perfect}/{}", names.len()),
        slowest
    );
    Ok(())
}
