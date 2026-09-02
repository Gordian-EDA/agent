//! Stand-alone DRC bench: run the standard rule suite over the golden
//! problem/solution pairs in `fixtures/` and print wall time plus the finding
//! counts the oracle reports.
//!
//! `cargo run -p pcb-drc --example bench` — no agent, no KiCAD, no network.

use std::path::Path;
use std::time::Instant;

use pcb_drc::StandardDrc;
use pcb_model::{Drc, Finding, RouteSolution, RoutingView};

fn read<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let json = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

fn main() {
    println!(
        "{:<12} {:>7} {:>6} {:>9} {:>9} {:>9} {:>9}",
        "board", "traces", "vias", "ms", "findings", "geometry", "connect"
    );
    for board in ["quad", "led-r", "congested"] {
        let view: RoutingView = read(&format!("{board}.problem.json"));
        let solution: RouteSolution = read(&format!("{board}.solution.json"));

        let started = Instant::now();
        let findings = StandardDrc.check(&view, &solution);
        let ms = started.elapsed().as_secs_f64() * 1e3;

        let geometry = findings.iter().filter(|f| f.is_geometry()).count();
        println!(
            "{:<12} {:>7} {:>6} {:>9.2} {:>9} {:>9} {:>9}",
            board,
            solution.traces.len(),
            solution.vias.len(),
            ms,
            findings.len(),
            geometry,
            findings.len() - geometry
        );
    }
    let names = pcb_drc::DrcSuite::standard().rule_names();
    println!("\nrules ({}): {}", names.len(), names.join(", "));
    let _ = std::mem::size_of::<Finding>();
}
