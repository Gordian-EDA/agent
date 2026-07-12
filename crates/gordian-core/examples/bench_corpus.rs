//! Benchmark + render a corpus of circuit YAMLs through the PREMIUM (forced anneal)
//! Reports parts/pins/nets, layout warnings, body/ic crossings, and anneal seconds
//! (the 5s budget gate). PNGs land in the out dir (default /tmp/bench).
//!
//! Usage: cargo run --release -p gordian-core --example bench_corpus -- [--out DIR] FILE.circuit.yaml ...

use kicad_cli::KicadCli;
use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;
use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let provider = SymbolTable::from_env(&env);

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut out_dir = PathBuf::from("/tmp/bench");
    if let Some(p) = args.iter().position(|a| a == "--out") {
        out_dir = PathBuf::from(args[p + 1].clone());
        args.drain(p..=p + 1);
    }
    std::fs::create_dir_all(&out_dir)?;

    println!(
        "{:<22} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5} {:>7}",
        "circuit", "parts", "pins", "nets", "warn", "body", "ic", "xing", "anneal_s"
    );
    println!("{}", "-".repeat(70));
    let mut slowest = 0.0_f64;
    let mut total_warn = 0usize;
    let mut total_body = 0usize;
    for path in &args {
        let p = Path::new(path);
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("out")
            .to_string();
        match bench_one(&env, &provider, p, &out_dir, &stem) {
            Ok((parts, pins, nets, warn, body, ic, xing, secs)) => {
                slowest = slowest.max(secs);
                total_warn += warn;
                total_body += body;
                let flag = if secs > 5.0 { " !!>5s" } else { "" };
                println!(
                    "{stem:<22} {parts:>5} {pins:>5} {nets:>5} {warn:>5} {body:>5} {ic:>5} {xing:>5} {secs:>7.2}{flag}"
                );
            }
            Err(e) => println!("{stem:<22} ERR: {e}"),
        }
    }
    println!("{}", "-".repeat(70));
    println!("slowest anneal_s={slowest:.2}  total_warn={total_warn}  total_body={total_body}");
    Ok(())
}

#[allow(clippy::type_complexity)] // example harness: a flat metrics tuple is clearer than a one-off struct
fn bench_one(
    env: &KicadEnv,
    provider: &SymbolTable,
    yaml_path: &Path,
    out_dir: &Path,
    stem: &str,
) -> anyhow::Result<(usize, usize, usize, usize, usize, usize, usize, f64)> {
    let src = std::fs::read_to_string(yaml_path)?;
    let result = circuit_lang::compile(&src, provider);
    let design = result.design.ok_or_else(|| {
        let errs: Vec<String> = result
            .diagnostics
            .0
            .iter()
            .map(|d| d.message.clone())
            .collect();
        anyhow::anyhow!("compile produced no design: {}", errs.join("; "))
    })?;

    let ir = sch_floorplan::floorplan::infer_ir(env, &design);
    if std::env::var("SHOW_IR").is_ok() {
        eprintln!("  [{stem}] rails={:?}", ir.rails.keys().collect::<Vec<_>>());
        eprintln!("  [{stem}] frozen={:?}", ir.frozen);
        for d in &ir.idioms {
            eprintln!(
                "  [{stem}] idiom {} anchor={} parts={:?}",
                d.kind, d.anchor, d.parts
            );
        }
    }
    let t0 = std::time::Instant::now();
    let emit =
        sch_floorplan::floorplan::emit_strategy(env, &design, Box::new(anneal_place::Anneal), None)
            .map_err(|e| anyhow::anyhow!("emit failed: {e}"))?;
    let secs = t0.elapsed().as_secs_f64();

    let parts = design.blocks.values().map(|b| b.components.len()).sum();
    // Pin / net counts straight from the design.
    let (pin_count, net_count) = pin_net_counts(&design);

    let tmp = tempfile::tempdir()?;
    let sch_path = tmp.path().join("out.kicad_sch");
    std::fs::write(&sch_path, emit.sch.as_bytes())?;
    let sch_out = out_dir.join(format!("{stem}.kicad_sch"));
    std::fs::write(&sch_out, emit.sch.as_bytes())?;

    if std::env::var("SHOW_WARN").is_ok() {
        for wmsg in &emit.layout_warnings {
            eprintln!("  [{stem}] WARN: {wmsg}");
        }
    }
    let svg_dir = tempfile::tempdir()?;
    let svg_path = KicadCli::new(env).export_svg_opts(&sch_path, svg_dir.path(), true)?;
    let svg = std::fs::read_to_string(&svg_path)?;
    let png = gordian_core::render::svg_to_png(&svg, 1600)?;
    std::fs::write(out_dir.join(format!("{stem}.png")), png)?;

    Ok((
        parts,
        pin_count,
        net_count,
        emit.layout_warnings.len(),
        emit.crossings.body,
        emit.crossings.ic,
        emit.crossings.wire,
        secs,
    ))
}

fn pin_net_counts(design: &circuit_lang::Design) -> (usize, usize) {
    use circuit_lang::model::PinTarget;
    use std::collections::BTreeSet;
    let mut pins = 0usize;
    let mut nets: BTreeSet<String> = BTreeSet::new();
    for b in design.blocks.values() {
        for c in b.components.values() {
            for pt in c.pins.values() {
                pins += 1;
                if let PinTarget::Net(n) = pt {
                    nets.insert(n.to_string());
                }
            }
        }
    }
    (pins, nets.len())
}
