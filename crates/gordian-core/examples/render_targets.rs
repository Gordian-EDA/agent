//! Focused iteration harness: render the four target validation fixtures
//! (compiled + emitted through the floorplan engine with their `*.layout.json`
//! IR sidecar) to PNGs in `/tmp/renders/` for side-by-side review against
//! `docs/validation/references/`.
//!
//! Usage: cargo run --release -p agent --example render_targets [name ...]
//! With no args, renders all four targets.

use circuit_lang::SymbolProvider;
use kicad_cli::cli::KicadCli;
use kicad_cli::env::KicadEnv;
use kicad_sexpr::provider::RealSymbolProvider;

const TARGETS: &[&str] =
    &["divider-filter", "mcp1703-power-entry", "555-blinker", "uart-level-translator"];

fn main() -> anyhow::Result<()> {
    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let dir = std::path::Path::new("docs/validation");
    let out_dir = std::path::Path::new("/tmp/renders");
    std::fs::create_dir_all(out_dir)?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let names: Vec<&str> =
        if args.is_empty() { TARGETS.to_vec() } else { args.iter().map(|s| s.as_str()).collect() };

    for name in names {
        let yaml = dir.join(format!("{name}.circuit.yaml"));
        let out = out_dir.join(format!("ours-{name}.png"));
        match render_fixture(&env, &yaml, &out) {
            Ok(()) => println!("rendered {}", out.display()),
            Err(e) => eprintln!("FAILED {name}: {e:#}"),
        }
    }
    Ok(())
}

fn render_fixture(env: &KicadEnv, yaml_path: &std::path::Path, out: &std::path::Path) -> anyhow::Result<()> {
    let src = std::fs::read_to_string(yaml_path)?;
    let provider = RealSymbolProvider::new(env.clone());
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.ok_or_else(|| {
        let errs: Vec<String> = result.diagnostics.0.iter().map(|d| d.message.clone()).collect();
        anyhow::anyhow!("compile produced no design: {}", errs.join("; "))
    })?;

    // INFER=1 → engine-inferred frame (no LLM/hand layout.json); else the sidecar.
    let ir = if std::env::var("INFER").is_ok() {
        sch_floorplan::floorplan::infer_ir(env, &design)
    } else {
        let ir_path = yaml_path.to_string_lossy().replace(".circuit.yaml", ".layout.json");
        match std::fs::read_to_string(&ir_path) {
            Ok(s) => sch_floorplan::floorplan::LayoutIr::from_json(&s)?,
            Err(_) => sch_floorplan::floorplan::baseline_ir(&design),
        }
    };
    let emit = sch_floorplan::floorplan::emit_strategy(env, &design, &ir, Box::new(greedy_place::Greedy))
        .map_err(|e| anyhow::anyhow!("emit failed: {e}"))?;

    let tmp = tempfile::tempdir()?;
    let sch_path = tmp.path().join("out.kicad_sch");
    std::fs::write(&sch_path, emit.sch.as_bytes())?;
    // Also drop the .kicad_sch next to the PNG for inspection.
    let sch_out = out.with_extension("kicad_sch");
    std::fs::write(&sch_out, emit.sch.as_bytes())?;

    let svg_dir = tempfile::tempdir()?;
    // Exclude the drawing sheet (page border + title block) so the render is
    // content-only, matching the zoomed-to-content reference screenshots.
    let svg_path = KicadCli::new(env).export_svg_opts(&sch_path, svg_dir.path(), true)?;
    let svg = std::fs::read_to_string(&svg_path)?;
    let png = gordian_core::render::svg_to_png(&svg, 1600)?;
    std::fs::write(out, png)?;
    for wmsg in &emit.layout_warnings {
        eprintln!("  WARN: {wmsg}");
    }
    eprintln!("  body_crossings={} ic_crossings={}", emit.crossings.body, emit.crossings.ic);
    for d in &emit.detected_idioms {
        eprintln!("  idiom {}: anchor={} parts={:?}", d.kind, d.anchor, d.parts);
    }
    Ok(())
}
