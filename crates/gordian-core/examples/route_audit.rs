//! Fast routing-completion audit: place + route every (or one) circuit spec and
//! report net/pin completion %, router used, and per-net failure reasons. Skips
//! export/DRC/render (the slow KiCAD round-trips), so it routes a hard board in
//! seconds instead of minutes — for measuring router QUALITY, not fidelity.

use std::path::{Path, PathBuf};

use gordian_core::tools::{PcbToolCtx, run_tool};
use serde_json::{json, Value};

fn footprint_dir() -> PathBuf {
    std::env::var("FOOTPRINT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/usr/share/kicad/footprints"))
}

fn circuits_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/pcb_circuits")
}

fn run_circuit(name: &str, spec: &Value, fp_dir: &Path) -> Value {
    let ctx = match PcbToolCtx::with_footprint_dir_for_test(fp_dir.to_path_buf()) {
        Some(c) => c,
        None => return json!({ "name": name, "error": "no footprint index / KiCAD env" }),
    };

    let mut board = json!({ "bounds": spec["bounds"], "parts": spec["parts"] });
    if let Some(rules) = spec.get("rules") {
        board["rules"] = rules.clone();
    }
    if let Some(outline) = spec.get("outline") {
        board["outline"] = outline.clone();
    }
    let created = gordian_core::tools_pcb::build_board_draft(board, &ctx).unwrap();
    if created["ok"] != json!(true) {
        return json!({ "name": name, "stage": "create", "result": created });
    }
    if spec.get("keepouts").is_some() || spec.get("hints").is_some() {
        let mut draft = gordian_core::tools_pcb::BoardDraft::load(&ctx).unwrap();
        gordian_core::tools_pcb::apply_spec_extras(&mut draft, spec);
        draft.save(&ctx).unwrap();
    }
    let t0 = std::time::Instant::now();
    let placed = run_tool("place_board", json!({}), &ctx).unwrap();
    let t_place = t0.elapsed().as_secs_f64();
    let t1 = std::time::Instant::now();
    let routed = run_tool("route_board", json!({}), &ctx).unwrap();
    let t_route = t1.elapsed().as_secs_f64();

    // Total nets the problem asked for: count distinct nets across all parts'
    // pad_nets (excluding obvious no-connects). We derive it from the failed list
    // plus the routed solution's trace/via connections to get total attempted.
    let failed = routed["failed"].as_array().cloned().unwrap_or_default();
    let n_failed = failed.len();

    // Count total connections (nets with >=2 pins) from the spec directly.
    let (total_nets, total_multipin) = count_nets(spec);

    let metrics = routed["metrics"].clone();

    // Group failure reasons by provenance tag.
    let mut reason_tags: std::collections::BTreeMap<String, usize> = Default::default();
    let mut failed_names: Vec<String> = Vec::new();
    for f in &failed {
        let r = f["reason"].as_str().unwrap_or("");
        let tag = r.split([':', ' ']).next().unwrap_or("?").to_string();
        *reason_tags.entry(tag).or_default() += 1;
        if let Some(c) = f["connection"].as_str() {
            failed_names.push(c.to_string());
        }
    }

    json!({
        "name": name,
        "place_legal": placed["legal"],
        "router": routed["router"],
        "total_nets": total_nets,
        "total_multipin_nets": total_multipin,
        "failed_nets": n_failed,
        "completion_pct": if total_multipin > 0 {
            100.0 * (total_multipin.saturating_sub(n_failed)) as f64 / total_multipin as f64
        } else { 100.0 },
        "metrics": metrics,
        "reason_tags": reason_tags,
        "failed_names": failed_names,
        "failed_full": failed.clone(),
        "t_place_s": (t_place * 10.0).round() / 10.0,
        "t_route_s": (t_route * 10.0).round() / 10.0,
        "engine_bug": routed.get("engine_bug").cloned().unwrap_or(json!(false)),
        "escape_bottleneck": routed.get("escape_bottleneck").cloned().unwrap_or(Value::Null),
    })
}

/// Count distinct nets and distinct nets touched by >=2 pins across the spec.
fn count_nets(spec: &Value) -> (usize, usize) {
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    if let Some(parts) = spec["parts"].as_array() {
        for p in parts {
            if let Some(pn) = p["pad_nets"].as_object() {
                for (_, net) in pn {
                    if let Some(n) = net.as_str() {
                        if n.is_empty() || n == "NC" {
                            continue;
                        }
                        *counts.entry(n.to_string()).or_default() += 1;
                    }
                }
            }
        }
    }
    let total = counts.len();
    let multipin = counts.values().filter(|&&c| c >= 2).count();
    (total, multipin)
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
        if let Some(only) = &only {
            // comma-separated substrings
            if !only.split(',').any(|s| stem.contains(s.trim())) {
                continue;
            }
        }
        let spec: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        specs.push((stem, spec));
    }
    specs.sort_by(|a, b| a.0.cmp(&b.0));

    eprintln!("footprint library: {}", fp_dir.display());
    use rayon::prelude::*;
    let all: Vec<Value> = specs
        .par_iter()
        .map(|(name, spec)| run_circuit(name, spec, &fp_dir))
        .collect();

    println!("{}", serde_json::to_string_pretty(&all).unwrap());

    // Compact summary table.
    eprintln!("\n{:<26} {:>8} {:>7} {:>6} {:>7} {:>6} {:>5} {:>5} {:>6}",
        "board", "router", "nets", "fail", "compl%", "traces", "vias", "tplc", "trte");
    for r in &all {
        eprintln!("{:<26} {:>8} {:>7} {:>6} {:>6.1} {:>7} {:>5} {:>5} {:>6}",
            r["name"].as_str().unwrap_or("?"),
            r["router"].as_str().unwrap_or("-"),
            r["total_multipin_nets"].as_u64().unwrap_or(0),
            r["failed_nets"].as_u64().unwrap_or(0),
            r["completion_pct"].as_f64().unwrap_or(0.0),
            r["metrics"]["traces"].as_u64().unwrap_or(0),
            r["metrics"]["vias"].as_u64().unwrap_or(0),
            r["t_place_s"].as_f64().unwrap_or(0.0),
            r["t_route_s"].as_f64().unwrap_or(0.0),
        );
    }
}
