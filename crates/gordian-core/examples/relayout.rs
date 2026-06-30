//! Re-lay-out a netlist through a chosen placement ENGINE and render it — the A/B
//! harness for comparing engines (anneal vs cluster) on the same circuit.
//!
//! Input is either a human `.kicad_sch` (lifted to a netlist, discarding the human's
//! placement) or an agent `circuit.yaml`. The chosen engine re-places it from scratch;
//! we render the result and print the objective metrics (warnings, crossings).
//!
//! Usage:
//!   cargo run --release -p gordian-core --example relayout -- \
//!       <file.kicad_sch|file.yaml> [--engine anneal|cluster] [--out DIR] [--tag NAME]

use std::path::PathBuf;

use kicad_cli::KicadCli;
use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;

fn engine_of(name: &str) -> Box<dyn sch_floorplan::contract::PlacementEngine> {
    match name {
        "anneal" | "sa" => Box::new(anneal_place::Anneal),
        "cluster" | "cluster-place" => Box::new(cluster_place::ClusterPlace),
        other => panic!("unknown engine `{other}` (anneal|cluster)"),
    }
}

fn main() -> anyhow::Result<()> {
    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let provider = SymbolTable::from_env(&env);

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut out_dir = PathBuf::from("/tmp/relayout");
    let mut engine_name = "anneal".to_string();
    let take = |flag: &str, args: &mut Vec<String>| -> Option<String> {
        args.iter().position(|a| a == flag).map(|p| {
            let v = args[p + 1].clone();
            args.drain(p..=p + 1);
            v
        })
    };
    if let Some(v) = take("--out", &mut args) {
        out_dir = PathBuf::from(v);
    }
    if let Some(v) = take("--engine", &mut args) {
        engine_name = v;
    }
    let tag = take("--tag", &mut args);
    std::fs::create_dir_all(&out_dir)?;

    let path = PathBuf::from(args.first().expect("need an input file"));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("out")
        .to_string();
    let label = tag.unwrap_or_else(|| format!("{stem}-{engine_name}"));

    // Get the circuit YAML: lift a .kicad_sch, or read a .yaml directly.
    let yaml = if path.extension().and_then(|e| e.to_str()) == Some("kicad_sch") {
        sch_io::read::lift(&env, &path)?
    } else {
        std::fs::read_to_string(&path)?
    };
    let result = circuit_lang::compile(&yaml, &provider);
    let design = result.design.ok_or_else(|| {
        let errs: Vec<String> = result
            .diagnostics
            .0
            .iter()
            .map(|d| d.message.clone())
            .collect();
        anyhow::anyhow!("compile produced no design: {}", errs.join("; "))
    })?;

    let t0 = std::time::Instant::now();
    let emit = sch_floorplan::floorplan::emit_strategy(&env, &design, engine_of(&engine_name), None)
        .map_err(|e| anyhow::anyhow!("emit failed: {e}"))?;
    let secs = t0.elapsed().as_secs_f64();

    let parts: usize = design.blocks.values().map(|b| b.components.len()).sum();
    let sch_out = out_dir.join(format!("{label}.kicad_sch"));
    std::fs::write(&sch_out, emit.sch.as_bytes())?;

    let svg_dir = tempfile::tempdir()?;
    let svg_path = KicadCli::new(&env).export_svg_opts(&sch_out, svg_dir.path(), true)?;
    let svg = std::fs::read_to_string(&svg_path)?;
    let png = gordian_core::render::svg_to_png(&svg, 1600)?;
    let png_out = out_dir.join(format!("{label}.png"));
    std::fs::write(&png_out, png)?;

    println!(
        "{label:<28} parts={parts:>3} warn={:>3} body={:>3} ic={:>3} xing={:>3} t={secs:>5.2}s -> {}",
        emit.layout_warnings.len(),
        emit.crossings.body,
        emit.crossings.ic,
        emit.crossings.wire,
        png_out.display()
    );
    Ok(())
}
