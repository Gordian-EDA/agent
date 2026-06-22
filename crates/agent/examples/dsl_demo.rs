//! Demo: author a board ENTIRELY via the Board-DSL (`design_board`), then
//! place -> route -> export against the real installed KiCAD footprint library
//! + DRC. Proves the DSL authoring path end-to-end (no schematic, no LLM).
//!
//! ```text
//! cargo run --release -p agent --example dsl_demo
//! ```

use std::path::PathBuf;

use agent::tools::{ToolCtx, Tools};
use serde_json::{json, Value};

const BOARD_YAML: &str = r#"
version: 1
name: power-buck-dsl
board:
  layers: 4
  outline: {rect: [44, 32]}
  rules:
    clearance: 0.2
    trace_width: 0.2
    via: [0.6, 0.3]
    net_widths: {SW: 0.8, VOUT: 0.8}
    pours: [{net: GND, layer: bottom}]
parts:
  U1: {footprint: 'Package_SO:SOIC-8_3.9x4.9mm_P1.27mm', pads: {1: VIN, 2: SW, 3: GND, 4: FB, 5: VIN, 6: VOUT, 7: GND, 8: VIN}}
  L1: {footprint: 'Inductor_SMD:L_1210_3225Metric', pads: {1: SW, 2: VOUT}}
  C1: {footprint: 'Capacitor_SMD:C_1210_3225Metric', pads: {1: VIN, 2: GND}}
  C2: {footprint: 'Capacitor_SMD:C_1210_3225Metric', pads: {1: VOUT, 2: GND}}
  R1: {footprint: 'Resistor_SMD:R_0402_1005Metric', pads: {1: VOUT, 2: FB}}
  R2: {footprint: 'Resistor_SMD:R_0402_1005Metric', pads: {1: FB, 2: GND}}
  J1: {footprint: 'Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical', pads: {1: VIN, 2: GND}, edge: true}
  J2: {footprint: 'Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical', pads: {1: VOUT, 2: GND}, edge: true}
"#;

fn main() {
    let fp_dir = std::env::var("FOOTPRINT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/usr/share/kicad/footprints"));
    let ctx = match ToolCtx::with_footprint_dir_for_test(fp_dir) {
        Some(c) => c,
        None => {
            eprintln!("no KiCAD footprint index / env — skipping");
            return;
        }
    };
    let tools = Tools::new();

    let d = tools
        .run("design_board", json!({ "yaml": BOARD_YAML, "overwrite": true }), &ctx)
        .unwrap();
    println!("design_board -> {}", compact(&d));
    assert_eq!(d["ok"], json!(true), "DSL did not compile cleanly");
    assert_eq!(d["part_count"], json!(8));

    let p = tools.run("place_board", json!({}), &ctx).unwrap();
    println!("place_board  -> legal={} hpwl={}", p["legal"], p["hpwl"]);
    assert_eq!(p["legal"], json!(true), "placement illegal");

    let r = tools.run("route_board", json!({}), &ctx).unwrap();
    println!(
        "route_board  -> router={} failed_nets={}",
        r["router"],
        r["failed"].as_array().map_or(0, |a| a.len())
    );

    let out = "/tmp/dsl_demo.kicad_pcb";
    let e = tools.run("export_board", json!({ "path": out }), &ctx).unwrap();
    println!("export_board -> {}", compact(&e));

    // ── Round-trip: import the exported .kicad_pcb back into Board-DSL ──────────
    let imp = tools.run("import_board", json!({ "path": out }), &ctx).unwrap();
    let imported_yaml = imp["yaml"].as_str().unwrap_or("").to_string();
    println!(
        "\nimport_board -> part_count={}  ({} bytes of recovered YAML)",
        imp["part_count"],
        imported_yaml.len()
    );
    assert_eq!(imp["part_count"], json!(8), "round-trip lost parts");
    println!("--- recovered Board-DSL ---\n{imported_yaml}---");

    // Re-author from the imported YAML and re-export; layout is locked, so it must
    // reproduce a DRC-clean board.
    let d2 = tools
        .run("design_board", json!({ "yaml": imported_yaml, "overwrite": true }), &ctx)
        .unwrap();
    assert_eq!(d2["ok"], json!(true), "imported YAML did not recompile: {}", compact(&d2));
    tools.run("place_board", json!({}), &ctx).unwrap();
    tools.run("route_board", json!({}), &ctx).unwrap();
    let out2 = "/tmp/dsl_demo_roundtrip.kicad_pcb";
    let e2 = tools.run("export_board", json!({ "path": out2 }), &ctx).unwrap();
    println!("round-trip export -> {}", compact(&e2));

    println!(
        "\nAuthored from {} bytes of Board-DSL; round-tripped .kicad_pcb -> DSL -> .kicad_pcb.",
        BOARD_YAML.len()
    );
}

fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}
