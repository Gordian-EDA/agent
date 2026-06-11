//! Render every docs/validation schematic to PNG for eyeball regression checks.
//!
//! Renders both raw `*.kicad_sch` files and `*.circuit.yaml` fixtures (compiled
//! + emitted through the schematic engine) to `<name>.png` alongside the source.
//!
//! Usage: cargo run -p agent --example render_validation

use circuit_lang::SymbolProvider;
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn render_sch_to_png(env: &KicadEnv, sch_path: &std::path::Path, out: &std::path::Path) -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let svg_path = KicadCli::new(env).export_svg(sch_path, tmp.path())?;
    let svg = std::fs::read_to_string(&svg_path)?;
    let png = agent::render::svg_to_png(&svg, 1600)?;
    std::fs::write(out, png)?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let dir = std::path::Path::new("docs/validation");

    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();

        if name.ends_with(".circuit.yaml") {
            // Compile + emit the fixture, then render the resulting .kicad_sch.
            let stem = name.trim_end_matches(".circuit.yaml");
            let out = path.with_file_name(format!("{stem}.png"));
            match render_fixture(&env, &path, &out) {
                Ok(()) => println!("rendered {}", out.display()),
                Err(e) => eprintln!("WARNING: failed to render fixture {}: {e:#}", path.display()),
            }
            continue;
        }

        if path.extension().and_then(|e| e.to_str()) == Some("kicad_sch") {
            let out = path.with_extension("png");
            render_sch_to_png(&env, &path, &out)?;
            println!("rendered {}", out.display());
        }
    }
    Ok(())
}

fn render_fixture(env: &KicadEnv, yaml_path: &std::path::Path, out: &std::path::Path) -> anyhow::Result<()> {
    let src = std::fs::read_to_string(yaml_path)?;
    let provider = RealSymbolProvider::new(env.clone());
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.ok_or_else(|| {
        let errs: Vec<String> = result
            .diagnostics
            .0
            .iter()
            .map(|d| d.message.clone())
            .collect();
        anyhow::anyhow!("compile produced no design: {}", errs.join("; "))
    })?;

    let emit = sch_engine::emit_design_reconciled(env, &design, None, &Default::default())
        .map_err(|e| anyhow::anyhow!("emit failed: {e}"))?;

    let tmp = tempfile::tempdir()?;
    let sch_path = tmp.path().join("out.kicad_sch");
    std::fs::write(&sch_path, emit.sch.as_bytes())?;

    render_sch_to_png(env, &sch_path, out)
}
