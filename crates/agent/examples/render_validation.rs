//! Render every docs/validation schematic to PNG for eyeball regression checks.
//!
//! Usage: cargo run -p agent --example render_validation

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;

fn main() -> anyhow::Result<()> {
    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let dir = std::path::Path::new("docs/validation");
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("kicad_sch") {
            continue;
        }
        let tmp = tempfile::tempdir()?;
        let svg_path = KicadCli::new(&env).export_svg(&path, tmp.path())?;
        let svg = std::fs::read_to_string(&svg_path)?;
        let png = agent::render::svg_to_png(&svg, 1600)?;
        let out = path.with_extension("png");
        std::fs::write(&out, png)?;
        println!("rendered {}", out.display());
    }
    Ok(())
}
