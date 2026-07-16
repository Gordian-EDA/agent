//! Rasterize a `kicad-cli`-exported SVG into a PNG the LLM can see.
//!
//! Pure-Rust via `resvg` — no system rasterizer needed. KiCAD plots text as
//! stroked polylines, so an empty fontdb renders correctly. The long edge is
//! normally capped at `max_px`. Schematics use a 2400 px readability floor:
//! KiCad's 0.254 mm wire strokes otherwise rasterize below one pixel on a large
//! custom sheet and disappear while their junction dots remain visible.

use std::path::Path;

use anyhow::{Context, Result};
use kicad_cli::KicadCli;
use kicad_env::KicadEnv;

/// Render a committed `.kicad_sch` to PNG bytes — the image
/// the in-loop vision LAYOUT critic looks at. Exports the schematic to an SVG in a
/// throwaway temp dir, then rasterizes it. Errors propagate so the caller can
/// degrade to a netlist-only review (the layout pass is best-effort).
pub fn schematic_png(env: &KicadEnv, sch: &Path, max_px: u32) -> Result<Vec<u8>> {
    let tmp = tempfile::tempdir().context("temp dir for schematic SVG export")?;
    let svg_path = KicadCli::new(env)
        // The in-loop critic needs circuit detail, not the drawing sheet.
        .export_svg_opts(sch, tmp.path(), true)
        .context("exporting schematic SVG")?;
    let svg = std::fs::read_to_string(&svg_path).context("reading exported SVG")?;
    // `--exclude-drawing-sheet` removes the border but KiCad retains the full
    // page viewBox. At 1600 px its standard wire stroke is just under one pixel
    // and resvg drops many horizontal/vertical wires. 2400 px is the smallest
    // size at which the production OpenMyo fixture remains reliably legible.
    svg_to_png(&svg, max_px.max(2400))
}

/// Render `svg` to PNG bytes, scaling so the long edge is `max_px` pixels.
pub fn svg_to_png(svg: &str, max_px: u32) -> Result<Vec<u8>> {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(svg, &opt).context("parsing SVG")?;
    let size = tree.size();
    let scale = max_px as f32 / size.width().max(size.height());
    let w = ((size.width() * scale).ceil() as u32).max(1);
    let h = ((size.height() * scale).ceil() as u32).max(1);

    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).context("allocating pixmap")?;
    // KiCAD SVGs assume a paper-white background; resvg default is transparent.
    pixmap.fill(resvg::tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().context("encoding PNG")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 100x50 red rectangle. Rasterized at max_px=200 the long edge must be
    /// 200 px and the pixel data non-trivial.
    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50">
        <rect x="10" y="10" width="80" height="30" fill="red"/></svg>"##;

    #[test]
    fn rasterizes_svg_to_scaled_png() {
        let png = svg_to_png(SVG, 200).expect("render");
        // PNG magic bytes.
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert!(png.len() > 100, "PNG too small: {} bytes", png.len());
    }

    #[test]
    fn rejects_malformed_svg() {
        assert!(svg_to_png("not svg at all", 200).is_err());
    }
}
