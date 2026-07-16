//! Deterministic, no-LLM schematic-to-fabrication acceptance harness.
//!
//! Usage:
//! `cargo run --release -p gordian-core --example deterministic_e2e -- \
//!    fixtures/openmyo-emg.circuit.yaml /tmp/gordian-openmyo-e2e`

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, anyhow, bail};
use gordian_core::AgentRuntime;
use gordian_core::tools::run_tool;
use kicad_env::KicadEnv;
use serde_json::{Value, json};

fn run_step(ctx: &AgentRuntime, name: &str, input: Value) -> anyhow::Result<Value> {
    let started = Instant::now();
    let result = run_tool(name, input, ctx).with_context(|| format!("running {name}"))?;
    println!("{name}: {:.3}s {result}", started.elapsed().as_secs_f64());
    if let Some(error) = result.get("error") {
        bail!("{name} failed: {error}");
    }
    if result.get("ok") == Some(&Value::Bool(false)) {
        bail!("{name} was not successful: {result}");
    }
    Ok(result)
}

fn has_front_back_detail_paths(value: &Value) -> bool {
    value.as_object().is_some_and(|paths| {
        ["front", "back"]
            .iter()
            .all(|side| paths.get(*side).is_some_and(Value::is_string))
    })
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let fixture = PathBuf::from(args.next().ok_or_else(|| {
        anyhow!("usage: deterministic_e2e <fixture.circuit.yaml> <output-directory>")
    })?);
    let output = PathBuf::from(args.next().ok_or_else(|| {
        anyhow!("usage: deterministic_e2e <fixture.circuit.yaml> <output-directory>")
    })?);
    if args.next().is_some() {
        bail!("usage: deterministic_e2e <fixture.circuit.yaml> <output-directory>");
    }
    if output.exists()
        && output
            .read_dir()
            .with_context(|| format!("reading {}", output.display()))?
            .next()
            .is_some()
    {
        bail!(
            "output directory {} is not empty; use a fresh directory for an authoritative run",
            output.display()
        );
    }

    let yaml = std::fs::read_to_string(&fixture)
        .with_context(|| format!("reading {}", fixture.display()))?;
    let env = KicadEnv::detect().ok_or_else(|| anyhow!("no KiCad environment detected"))?;
    let ctx = AgentRuntime::for_project(env, output.clone())?;
    ctx.workspace().write_draft(&yaml, None)?;

    let started = Instant::now();
    let validation = run_step(&ctx, "validate_design", json!({ "yaml": yaml }))?;
    if validation["errors"].as_u64().unwrap_or(0) != 0 {
        bail!("fixture has authoring errors: {validation}");
    }
    let applied = run_step(&ctx, "apply_design", json!({ "__commit": true }))?;
    if applied["erc"]["errors"].as_u64().unwrap_or(1) != 0
        || applied["erc"]["warnings"].as_u64().unwrap_or(1) != 0
        || applied["layout_warnings"]
            .as_array()
            .is_none_or(|warnings| !warnings.is_empty())
    {
        bail!("schematic has ERC or layout findings: {applied}");
    }
    run_step(&ctx, "render_schematic", json!({}))?;
    let regenerated = run_step(
        &ctx,
        "regenerate_board",
        json!({
            "bounds": { "width": 86.0, "height": 63.0 },
            "rules": {
                "layer_count": 4,
                "clearance": 0.2,
                "min_trace_width": 0.2,
                "via_diameter": 0.6,
                "via_drill": 0.3,
                "net_widths": {
                    "+9V": 0.5,
                    "-9V": 0.5,
                    "+3V3": 0.4,
                    "GND": 0.5
                }
            }
        }),
    )?;
    if regenerated["part_count"].as_u64().unwrap_or(0) < 40 {
        bail!("benchmark must produce at least 40 physical parts: {regenerated}");
    }
    let placement = run_step(
        &ctx,
        "place_board",
        json!({
            "groups": [
                {
                    "name": "mount_nw",
                    "members": ["H1"],
                    "region": {"min_x": 3.6, "min_y": 3.6, "max_x": 3.8, "max_y": 3.8},
                    "grid": true
                },
                {
                    "name": "mount_ne",
                    "members": ["H2"],
                    "region": {"min_x": 82.2, "min_y": 3.6, "max_x": 82.4, "max_y": 3.8},
                    "grid": true
                },
                {
                    "name": "power_entry",
                    "members": ["J1"],
                    "region": {"min_x": 35.0, "min_y": 5.0, "max_x": 41.0, "max_y": 8.2},
                    "grid": true
                },
                {
                    "name": "power",
                    "members": ["C1", "C2", "C6", "C7"],
                    "region": {"min_x": 48.0, "min_y": 4.0, "max_x": 72.0, "max_y": 15.0},
                    "grid": true
                },
                {
                    "name": "input",
                    "members": ["J2"],
                    "region": {"min_x": 1.6, "min_y": 28.0, "max_x": 1.9, "max_y": 35.0},
                    "grid": true
                },
                {
                    "name": "output",
                    "members": ["J3"],
                    "region": {"min_x": 84.1, "min_y": 26.0, "max_x": 84.4, "max_y": 37.0},
                    "grid": true
                },
                {
                    "name": "preamp",
                    "members": ["U2", "R1", "R2", "R3", "C3", "C4", "C5", "C8", "C9", "C10", "C11"],
                    "region": {"min_x": 8.0, "min_y": 18.0, "max_x": 36.0, "max_y": 53.0},
                    "grid": true
                },
                {
                    "name": "filter",
                    "members": ["U1", "C12", "C13", "R4", "R5", "R6", "R7", "R8", "R11", "C15", "C16", "R12", "R13"],
                    "region": {"min_x": 44.0, "min_y": 18.0, "max_x": 68.0, "max_y": 49.0},
                    "grid": true
                },
                {
                    "name": "offset",
                    "members": ["C14", "R9", "R10", "RV1"],
                    "region": {"min_x": 70.0, "min_y": 47.0, "max_x": 82.0, "max_y": 59.0},
                    "grid": true
                },
                {
                    "name": "postamp",
                    "members": ["R14", "RV2", "R15"],
                    "region": {"min_x": 42.0, "min_y": 54.0, "max_x": 65.0, "max_y": 60.0},
                    "grid": true
                }
            ],
            "edge_seek": [],
            "corner_seek": ["H1", "H2"]
        }),
    )?;
    if placement["legal"] != Value::Bool(true) {
        bail!("placement is not legal: {placement}");
    }
    let routing = run_step(&ctx, "route_board", json!({}))?;
    if routing["failed"]
        .as_array()
        .is_some_and(|failed| !failed.is_empty())
    {
        bail!("routing left failed nets: {routing}");
    }
    let checked = run_step(&ctx, "check_board", json!({}))?;
    if checked["drc_clean"] != Value::Bool(true)
        || checked["unconnected_items"].as_u64().unwrap_or(1) != 0
        || checked["silk_warnings"].as_u64().unwrap_or(1) != 0
    {
        bail!("PCB is not DRC clean: {checked}");
    }
    let rendered = run_step(&ctx, "render_board", json!({}))?;
    if rendered["source"] != "saved_board_file"
        || !has_front_back_detail_paths(&rendered["detail_paths"])
    {
        bail!("dense board render did not produce saved-file front/back details: {rendered}");
    }
    let fab = run_step(&ctx, "export_fab", json!({}))?;
    if fab["file_count"].as_u64().unwrap_or(0) < 10 {
        bail!("fabrication export is incomplete: {fab}");
    }
    ctx.close_kicad_session();

    let elapsed = started.elapsed();
    if elapsed.as_secs_f64() >= 180.0 {
        bail!("E2E exceeded the 180 second budget: {elapsed:.3?}");
    }

    println!(
        "PASS fixture={} output={} total={:.3}s",
        fixture.display(),
        output.display(),
        elapsed.as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_back_detail_gate_matches_render_board_object_shape() {
        assert!(has_front_back_detail_paths(&json!({
            "front": "/tmp/front.png",
            "back": "/tmp/back.png"
        })));
        assert!(!has_front_back_detail_paths(&json!([
            "/tmp/front.png",
            "/tmp/back.png"
        ])));
        assert!(!has_front_back_detail_paths(&json!({
            "front": "/tmp/front.png"
        })));
    }
}
