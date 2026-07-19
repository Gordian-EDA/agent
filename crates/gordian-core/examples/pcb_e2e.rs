//! Schematic-to-PCB acceptance harness for boards whose schematic may carry
//! cosmetic authoring warnings. The schematic must be electrically sound (no
//! authoring or ERC errors); every PCB gate is strict: part floor, legal
//! placement, complete routing, clean DRC and silkscreen, a fabrication
//! bundle, and a wall-clock budget.
//!
//! Usage: `cargo run --release -p gordian-core --example pcb_e2e -- \
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
    let layer_count = args.next().map(|s| s.parse::<u32>()).transpose()?;
    let initial_bounds = args
        .next()
        .map(|s| -> anyhow::Result<(f64, f64)> {
            let (w, h) = s.split_once('x').context("bounds must be WxH")?;
            Ok((w.parse()?, h.parse()?))
        })
        .transpose()?;
    if args.next().is_some() {
        bail!(
            "usage: pcb_e2e <yaml> <fresh-output> [minimum-parts] [maximum-seconds] [layers] [WxH]"
        );
    }
    if output.exists() && output.read_dir()?.next().is_some() {
        bail!("output directory is not empty: {}", output.display());
    }

    let yaml = std::fs::read_to_string(&fixture)?;
    let env = KicadEnv::detect().context("no KiCad environment detected")?;
    let mut config = gordian_core::config::GordianConfig::default();
    if let Ok(router) = std::env::var("PCB_E2E_ROUTER") {
        config.engines.pcb_router = serde_json::from_value(json!(router))
            .map_err(|e| anyhow::anyhow!("bad PCB_E2E_ROUTER: {e}"))?;
    }
    let ctx = AgentRuntime::for_project_with_config(env, output.clone(), config)?;
    ctx.workspace().write_draft(&yaml, None)?;
    let started = Instant::now();

    let validation = step(&ctx, "validate_design", json!({}))?;
    if validation["errors"].as_u64().unwrap_or(1) != 0 {
        bail!("draft has authoring errors: {validation}");
    }
    let applied = step(&ctx, "apply_design", json!({"__commit": true}))?;
    if applied["erc"]["errors"].as_u64().unwrap_or(1) != 0 {
        bail!("schematic has ERC errors: {applied}");
    }
    let erc_warnings = applied["erc"]["warnings"].as_u64().unwrap_or(0);
    let layout_warnings = applied["layout_warnings"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);

    let pcb_started = Instant::now();
    let mut rule_map = serde_json::Map::new();
    if let Some(layers) = layer_count {
        rule_map.insert("layer_count".into(), json!(layers));
    }
    if let Ok(clearance) = std::env::var("PCB_E2E_CLEARANCE") {
        rule_map.insert("clearance".into(), json!(clearance.parse::<f64>().unwrap_or(0.2)));
    }
    if let Ok(width) = std::env::var("PCB_E2E_TRACE_WIDTH") {
        rule_map.insert(
            "min_trace_width".into(),
            json!(width.parse::<f64>().unwrap_or(0.25)),
        );
    }
    let rules = (!rule_map.is_empty()).then(|| Value::Object(rule_map));
    let with_rules = |mut input: Value| {
        if let (Some(rules), Value::Object(o)) = (&rules, &mut input) {
            o.insert("rules".into(), rules.clone());
        }
        input
    };
    let first_input = match initial_bounds {
        Some((w, h)) => json!({"bounds": [0.0, 0.0, w, h]}),
        None => json!({}),
    };
    let regenerated = step(&ctx, "regenerate_board", with_rules(first_input))?;
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
            with_rules(json!({"bounds": [0.0, 0.0, width, height]})),
        )?;
        placed = attempt(&ctx, "place_board", json!({}))?;
    }
    if placed["legal"] != Value::Bool(true) {
        bail!("placement is illegal: {placed}");
    }
    // Empirical shrink probe: the packing estimate over-reserves on THT-heavy
    // boards, so a legal placement often fits a tighter canvas. A canvas that
    // ballooned during illegal-placement retries jumps straight back to the
    // fresh packing estimate; otherwise one 0.85x attempt. Either way the
    // roomier legal canvas is restored if the tighter placement fails.
    if let (None, Some(width), Some(height)) = (
        initial_bounds,
        placed["current_bounds_mm"]["w"].as_f64(),
        placed["current_bounds_mm"]["h"].as_f64(),
    ) {
        let fit = (
            placed["fit_bounds_mm"]["w"].as_f64(),
            placed["fit_bounds_mm"]["h"].as_f64(),
        );
        let (tighter_w, tighter_h) = match fit {
            (Some(fw), Some(fh)) if fw * fh < width * height * 0.7 => (fw.ceil(), fh.ceil()),
            _ => ((width * 0.85).ceil(), (height * 0.85).ceil()),
        };
        step(
            &ctx,
            "regenerate_board",
            with_rules(json!({"bounds": [0.0, 0.0, tighter_w, tighter_h]})),
        )?;
        let tightened = attempt(&ctx, "place_board", json!({}))?;
        if tightened["legal"] == Value::Bool(true) {
            placed = tightened;
        } else {
            step(
                &ctx,
                "regenerate_board",
                with_rules(json!({"bounds": [0.0, 0.0, width, height]})),
            )?;
            placed = attempt(&ctx, "place_board", json!({}))?;
            if placed["legal"] != Value::Bool(true) {
                bail!("placement did not recover after tightening: {placed}");
            }
        }
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
        || checked["silk_warnings"].as_u64().unwrap_or(1) != 0
    {
        bail!("board is not clean: {checked}");
    }
    step(&ctx, "render_board", json!({}))?;
    let fab = step(&ctx, "export_fab", json!({}))?;
    if fab["file_count"].as_u64().unwrap_or(0) < 10 {
        bail!("fabrication bundle is incomplete: {fab}");
    }
    let pcb_elapsed = pcb_started.elapsed().as_secs_f64();
    ctx.close_kicad_session();

    let elapsed = started.elapsed().as_secs_f64();
    if elapsed > maximum_seconds {
        bail!("E2E took {elapsed:.3}s, over the {maximum_seconds:.3}s limit");
    }
    println!(
        "PASS fixture={} output={} parts={} erc_warnings={erc_warnings} \
         sch_layout_warnings={layout_warnings} pcb_seconds={pcb_elapsed:.3} total={elapsed:.3}s",
        fixture.display(),
        output.display(),
        regenerated["part_count"]
    );
    Ok(())
}
