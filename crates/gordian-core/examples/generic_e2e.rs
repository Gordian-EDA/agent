//! Generic no-LLM schematic-to-fabrication acceptance harness.
//!
//! Usage: `cargo run --release -p gordian-core --example generic_e2e -- \
//!   <design.circuit.yaml> <fresh-output-directory> [minimum-parts] [maximum-seconds]`

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, bail};
use gordian_core::AgentRuntime;
use gordian_core::tools::run_tool;
use kicad_env::KicadEnv;
use serde_json::{Value, json};

fn attempt(ctx: &AgentRuntime, name: &str, input: Value) -> anyhow::Result<Value> {
    let started = Instant::now();
    let value = run_tool(name, input, ctx).with_context(|| format!("running {name}"))?;
    println!("{name}: {:.3}s", started.elapsed().as_secs_f64());
    Ok(value)
}

fn step(ctx: &AgentRuntime, name: &str, input: Value) -> anyhow::Result<Value> {
    let value = attempt(ctx, name, input)?;
    if value.get("error").is_some() || value.get("ok") == Some(&Value::Bool(false)) {
        bail!("{name} failed: {value}");
    }
    Ok(value)
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let fixture = PathBuf::from(args.next().context("missing circuit YAML")?);
    let output = PathBuf::from(args.next().context("missing output directory")?);
    let minimum_parts = args
        .next()
        .unwrap_or_else(|| "40".to_owned())
        .parse::<u64>()?;
    let maximum_seconds = args
        .next()
        .unwrap_or_else(|| "180".to_owned())
        .parse::<f64>()?;
    if args.next().is_some() {
        bail!("usage: generic_e2e <yaml> <fresh-output> [minimum-parts] [maximum-seconds]");
    }
    if output.exists() && output.read_dir()?.next().is_some() {
        bail!("output directory is not empty: {}", output.display());
    }

    let yaml = std::fs::read_to_string(&fixture)?;
    let env = KicadEnv::detect().context("no KiCad environment detected")?;
    let ctx = AgentRuntime::for_project(env, output.clone())?;
    ctx.workspace().write_draft(&yaml, None)?;
    let started = Instant::now();

    let validation = step(&ctx, "validate_design", json!({}))?;
    if validation["errors"].as_u64().unwrap_or(1) != 0
        || validation["warnings"].as_u64().unwrap_or(1) != 0
    {
        bail!("draft is not authoring-clean: {validation}");
    }
    let applied = step(&ctx, "apply_design", json!({"__commit": true}))?;
    if applied["erc"]["errors"].as_u64().unwrap_or(1) != 0
        || applied["erc"]["warnings"].as_u64().unwrap_or(1) != 0
        || applied["layout_warnings"]
            .as_array()
            .is_none_or(|warnings| !warnings.is_empty())
    {
        bail!("schematic is not clean: {applied}");
    }
    step(&ctx, "render_schematic", json!({}))?;
    let regenerated = step(&ctx, "regenerate_board", json!({}))?;
    if regenerated["part_count"].as_u64().unwrap_or(0) < minimum_parts {
        bail!("board has too few parts: {regenerated}");
    }
    let mut placed = attempt(&ctx, "place_board", json!({}))?;
    for _ in 0..3 {
        if placed["legal"] == Value::Bool(true) {
            break;
        }
        let (Some(width), Some(height)) = (
            placed["suggested_min_bounds_mm"]["w"].as_f64(),
            placed["suggested_min_bounds_mm"]["h"].as_f64(),
        ) else {
            break;
        };
        step(
            &ctx,
            "regenerate_board",
            json!({"bounds": [0.0, 0.0, width, height]}),
        )?;
        placed = attempt(&ctx, "place_board", json!({}))?;
    }
    if placed["legal"] != Value::Bool(true) {
        bail!("placement is illegal: {placed}");
    }
    let routed = step(&ctx, "route_board", json!({}))?;
    if routed["failed"]
        .as_array()
        .is_none_or(|failed| !failed.is_empty())
    {
        bail!("routing incomplete: {routed}");
    }
    let checked = step(&ctx, "check_board", json!({}))?;
    if checked["drc_clean"] != Value::Bool(true)
        || checked["unconnected_items"].as_u64().unwrap_or(1) != 0
    {
        bail!("board is not clean: {checked}");
    }
    step(&ctx, "render_board", json!({}))?;
    let fab = step(&ctx, "export_fab", json!({}))?;
    if fab["file_count"].as_u64().unwrap_or(0) < 10 {
        bail!("fabrication bundle is incomplete: {fab}");
    }
    ctx.close_kicad_session();

    let elapsed = started.elapsed().as_secs_f64();
    if elapsed > maximum_seconds {
        bail!("E2E took {elapsed:.3}s, over the {maximum_seconds:.3}s limit");
    }
    println!(
        "PASS fixture={} output={} parts={} total={elapsed:.3}s",
        fixture.display(),
        output.display(),
        regenerated["part_count"]
    );
    Ok(())
}
