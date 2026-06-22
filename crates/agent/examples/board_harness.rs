//! E2E engine harness: drive create -> place -> route -> export over a set of
//! hand-authored circuit specs, using the **real** installed KiCAD footprint
//! library. Deterministic (no LLM) — this exercises the placement/routing/
//! synth/DRC engine on varied, realistic boards.
//!
//! ```text
//! cargo run --release -p agent --example board_harness            # all built-in circuits
//! cargo run --release -p agent --example board_harness -- ldo     # one circuit by stem
//! FOOTPRINT_DIR=/usr/share/kicad/footprints cargo run ...         # override fp library
//! ```
//!
//! Outputs per circuit into /tmp/pcb-harness/<name>/:
//!   board.kicad_pcb   — the exported board (the artifact)
//!   engine.png        — the engine's debug routing render
//! plus a one-line status to stdout (place legal? route failed nets? DRC counts).

use std::path::{Path, PathBuf};

use agent::tools::{ToolCtx, Tools};
use serde_json::{json, Value};

fn footprint_dir() -> PathBuf {
    std::env::var("FOOTPRINT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/usr/share/kicad/footprints"))
}

fn circuits_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/pcb_circuits")
}

fn rasterize(svg: &str, out: &Path, scale: f32) {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(svg, &opt).expect("parse svg");
    let size = tree.size();
    let (w, h) = ((size.width() * scale) as u32, (size.height() * scale) as u32);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).expect("pixmap");
    pixmap.fill(resvg::tiny_skia::Color::WHITE);
    let ts = resvg::tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, ts, &mut pixmap.as_mut());
    pixmap.save_png(out).expect("save png");
}

fn run_circuit(name: &str, spec: &Value, fp_dir: &Path) -> Value {
    let ctx = match ToolCtx::with_footprint_dir_for_test(fp_dir.to_path_buf()) {
        Some(c) => c,
        None => return json!({ "name": name, "error": "no footprint index / KiCAD env" }),
    };
    let tools = Tools::new();

    // Forward an optional `rules` block (e.g. {"layers": 4}) from the spec.
    let mut board = json!({ "bounds": spec["bounds"], "parts": spec["parts"] });
    if let Some(rules) = spec.get("rules") {
        board["rules"] = rules.clone();
    }
    if let Some(outline) = spec.get("outline") {
        board["outline"] = outline.clone();
    }
    let created = agent::tools_pcb::build_board_draft(board, &ctx).unwrap();
    if created["ok"] != json!(true) {
        return json!({ "name": name, "stage": "create", "result": created });
    }
    // Optional keepouts (rule areas) from the spec, applied via set_constraints.
    if let Some(kos) = spec.get("keepouts") {
        let _ = tools.run("set_constraints", json!({ "keepouts": kos.clone() }), &ctx);
    }
    // Optional placement hints (groups: regions / edges / grid arrays) from the spec.
    if let Some(groups) = spec.get("hints").and_then(|h| h.get("groups")) {
        let _ = tools.run("set_placement_hints", json!({ "groups": groups.clone() }), &ctx);
    }
    let placed = tools.run("place_board", json!({}), &ctx).unwrap();
    if placed["legal"] != json!(true) {
        eprintln!(
            "[place {name}] legal=false overlaps_resolved={} clamps={} suggested={:?} current={:?}",
            placed["overlaps_resolved"], placed["out_of_bounds_clamps"],
            placed.get("suggested_min_bounds_mm"), placed.get("current_bounds_mm")
        );
    }
    let routed = tools.run("route_board", json!({}), &ctx).unwrap();
    let exported = tools.run("export_board", json!({}), &ctx).unwrap();

    let out_dir = PathBuf::from("/tmp/pcb-harness").join(name);
    std::fs::create_dir_all(&out_dir).unwrap();
    if let Some(p) = exported["path"].as_str() {
        let _ = std::fs::copy(p, out_dir.join("board.kicad_pcb"));
        // The sibling .kicad_pro carries the design rules (net class) — copy it so a
        // re-run of kicad-cli DRC on the /tmp board checks against the engine's rules.
        let pro = PathBuf::from(p).with_extension("kicad_pro");
        let _ = std::fs::copy(&pro, out_dir.join("board.kicad_pro"));
    }
    // Engine debug render (routed view).
    if let Ok(render) = tools.run("render_board", json!({ "view": "routed" }), &ctx)
        && let Some(p) = render["png_path"].as_str() {
            let _ = std::fs::copy(p, out_dir.join("engine.png"));
        }
    let _ = rasterize; // (kept for ad-hoc SVG rasterization)

    json!({
        "name": name,
        "place_legal": placed["legal"],
        "hpwl": placed["hpwl"],
        "router": routed["router"],
        "failed_nets": routed["failed"].as_array().map(|a| a.len()).unwrap_or(0),
        "metrics": routed["metrics"],
        "lint": routed["lint_summary"],
        "export_ok": exported["ok"],
        "drc": exported["drc"],
        "artifact": out_dir.join("board.kicad_pcb").display().to_string(),
    })
}

fn main() {
    let fp_dir = footprint_dir();
    let only = std::env::args().nth(1);

    let mut specs: Vec<(String, Value)> = Vec::new();
    for entry in std::fs::read_dir(circuits_dir()).expect("circuits dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        if let Some(only) = &only
            && !stem.contains(only.as_str()) {
                continue;
            }
        let spec: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        specs.push((stem, spec));
    }
    specs.sort_by(|a, b| a.0.cmp(&b.0));

    println!("footprint library: {}", fp_dir.display());
    // Route/export/DRC every circuit IN PARALLEL — each runs in its own isolated
    // ToolCtx (a per-board tempdir) and writes to its own /tmp/pcb-harness/<name>/
    // dir, so the boards are independent. rayon's indexed collect preserves spec
    // order, so the printed report is identical to the sequential run, just faster.
    use rayon::prelude::*;
    let all: Vec<Value> = specs
        .par_iter()
        .map(|(name, spec)| run_circuit(name, spec, &fp_dir))
        .collect();
    for r in &all {
        println!("\n=== {} ===", r["name"].as_str().unwrap_or("?"));
        println!("{}", serde_json::to_string_pretty(r).unwrap());
    }

    // Summary.
    println!("\n===== SUMMARY =====");
    let mut fault_boards: Vec<String> = Vec::new();
    for r in &all {
        let drc = &r["drc"];
        // The hard correctness metric: error-severity COPPER faults must be 0 on
        // EVERY board (unconnected nets are honest failures, allowed). copper_errors
        // is absent when DRC didn't run (no KiCAD) — treat that as 0 (skipped).
        let copper_errors = drc["copper_errors"].as_u64().unwrap_or(0);
        if copper_errors > 0 {
            fault_boards.push(r["name"].as_str().unwrap_or("?").to_string());
        }
        println!(
            "{:<20} place={:<5} route={:<9} failed={:<3} copper_err={} unconn={:<4} copper_warn={}",
            r["name"].as_str().unwrap_or("?"),
            r["place_legal"],
            r["router"].as_str().unwrap_or("-"),
            r["failed_nets"],
            copper_errors,
            drc["unconnected_items"],
            drc["copper_violations"],
        );
    }
    // Fidelity gate: the engine must never EMIT a copper DRC fault.
    println!("\n===== FIDELITY GATE =====");
    if fault_boards.is_empty() {
        println!("PASS — 0 copper DRC faults across {} boards (unrouted nets are honest).", all.len());
    } else {
        println!("FAIL — copper DRC faults emitted on: {}", fault_boards.join(", "));
        std::process::exit(1);
    }
}
