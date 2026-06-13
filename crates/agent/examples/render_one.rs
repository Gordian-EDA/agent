//! Render ONE circuit fixture to a content-cropped PNG for fast layout iteration.
//!
//! Compiles a `*.circuit.yaml`, emits via the schematic engine, exports a
//! FRAMELESS SVG (no page border / title block), rasterizes, and trims the
//! white margin so the drawing fills the image — directly comparable to the
//! content-cropped references in docs/validation/references/.
//!
//! Usage: cargo run -p agent --example render_one -- <in.circuit.yaml> <out.png>

use circuit_lang::SymbolProvider;
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let yaml = args.next().expect("usage: render_one <in.circuit.yaml> <out.png>");
    let out = args.next().expect("usage: render_one <in.circuit.yaml> <out.png>");

    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let src = std::fs::read_to_string(&yaml)?;
    let provider = RealSymbolProvider::new(env.clone());
    let result = circuit_lang::compile(&src, &provider as &dyn SymbolProvider);
    let design = result.design.ok_or_else(|| {
        let errs: Vec<String> = result.diagnostics.0.iter().map(|d| d.message.clone()).collect();
        anyhow::anyhow!("compile produced no design: {}", errs.join("; "))
    })?;

    // Layout IR: a `<stem>.layout.json` sidecar (the floorplan the subagent
    // would emit) if present, else the deterministic baseline.
    let ir_path = std::path::Path::new(&yaml)
        .to_string_lossy()
        .replace(".circuit.yaml", ".layout.json");
    let ir = match std::fs::read_to_string(&ir_path) {
        Ok(s) => {
            eprintln!("using layout IR {ir_path}");
            sch_engine::floorplan::LayoutIr::from_json(&s)?
        }
        Err(_) => sch_engine::floorplan::baseline_ir(&design),
    };
    let emit = sch_engine::floorplan::emit(&env, &design, &ir)
        .map_err(|e| anyhow::anyhow!("emit failed: {e}"))?;
    for w in &emit.layout_warnings {
        eprintln!("layout-warning: {w}");
    }

    let tmp = tempfile::tempdir()?;
    let sch_path = tmp.path().join("out.kicad_sch");
    std::fs::write(&sch_path, emit.sch.as_bytes())?;

    match KicadCli::new(&env).erc(&sch_path) {
        Ok(r) => {
            eprintln!("ERC: {} errors, {} warnings", r.error_count(), r.warning_count());
            for v in r.violations.iter().take(12) {
                eprintln!("  [{}] {} — {}", v.severity, v.kind, v.description);
            }
        }
        Err(e) => eprintln!("ERC failed to run: {e}"),
    }

    let svg_path = KicadCli::new(&env).export_svg_opts(&sch_path, tmp.path(), true)?;
    let svg = std::fs::read_to_string(&svg_path)?;
    let png = render_cropped(&svg, 2600, 24)?;
    std::fs::write(&out, png)?;
    println!("rendered {out}");
    Ok(())
}

/// Rasterize `svg` (long edge ~`max_px`), then crop to non-white content with
/// `pad` px of margin and encode PNG.
fn render_cropped(svg: &str, max_px: u32, pad: u32) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(svg, &opt).context("parsing SVG")?;
    let size = tree.size();
    let scale = max_px as f32 / size.width().max(size.height());
    let w = ((size.width() * scale).ceil() as u32).max(1);
    let h = ((size.height() * scale).ceil() as u32).max(1);

    let mut pm = resvg::tiny_skia::Pixmap::new(w, h).context("allocating pixmap")?;
    pm.fill(resvg::tiny_skia::Color::WHITE);
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pm.as_mut());

    // Content bbox: any pixel that isn't near-white.
    let data = pm.data();
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (w, h, 0u32, 0u32);
    let bg = |r: u8, g: u8, b: u8| r > 248 && g > 248 && b > 248;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            if !bg(data[i], data[i + 1], data[i + 2]) {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if min_x > max_x {
        // Empty (shouldn't happen) — return full pixmap.
        return pm.encode_png().context("encoding PNG");
    }
    let cx0 = min_x.saturating_sub(pad);
    let cy0 = min_y.saturating_sub(pad);
    let cx1 = (max_x + pad).min(w - 1);
    let cy1 = (max_y + pad).min(h - 1);
    let (cw, ch) = (cx1 - cx0 + 1, cy1 - cy0 + 1);

    let mut cropped = resvg::tiny_skia::Pixmap::new(cw, ch).context("allocating crop")?;
    {
        let dst = cropped.data_mut();
        for y in 0..ch {
            let src_row = (((cy0 + y) * w + cx0) * 4) as usize;
            let dst_row = ((y * cw) * 4) as usize;
            dst[dst_row..dst_row + (cw * 4) as usize]
                .copy_from_slice(&data[src_row..src_row + (cw * 4) as usize]);
        }
    }
    cropped.encode_png().context("encoding cropped PNG")
}
