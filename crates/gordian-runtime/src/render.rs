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
    let svg = thicken_schematic_wires(&svg);
    // `--exclude-drawing-sheet` removes the border but KiCad retains the full
    // page viewBox. At 1600 px its standard wire stroke is just under one pixel
    // and resvg drops many horizontal/vertical wires. 2400 px is the smallest
    // size at which the production OpenMyo fixture remains reliably legible.
    svg_to_png(&svg, max_px.max(2400))
}

/// KiCad exports default schematic wires as 0.1524 mm green strokes. Resvg can
/// drop those axis-aligned strokes when they land below one output pixel even
/// though other schematic geometry remains visible. Match the 0.254 mm symbol
/// stroke so wires survive rasterization and subsequent vision-image resizing.
fn thicken_schematic_wires(svg: &str) -> String {
    let thickened = svg.replace(
        "stroke:#009600; stroke-width:0.1524;",
        "stroke:#009600; stroke-width:0.2540;",
    );
    // KiCad 9.0.3 may emit the entire schematic-wire layer as `stroke:none`
    // even though its paths are the real committed wires. Older exports put
    // the green stroke on the group. In either form give each path an explicit
    // stroke: resvg then cannot lose it through absent/broken inheritance.
    let marker = [
        "<g style=\"fill:none; stroke:none;\">",
        "<g style=\"fill:none; \nstroke:#009600; stroke-width:0.2540;",
    ]
    .into_iter()
    .find_map(|marker| thickened.find(marker));
    let Some(group_start) = marker else {
        return thickened;
    };
    let Some(relative_end) = thickened[group_start..].find("</g>") else {
        return thickened;
    };
    let group_end = group_start + relative_end;
    let mut explicit = thickened[..group_start].to_owned();
    explicit.push_str(&thickened[group_start..group_end].replace(
        "<path d=",
        "<path style=\"fill:none;stroke:#009600;stroke-width:0.2540\" d=",
    ));
    explicit.push_str(&thickened[group_end..]);
    explicit
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

    #[test]
    fn thickens_kicad_default_wire_strokes_only() {
        let svg = "stroke:#009600; stroke-width:0.1524; stroke:#840000; stroke-width:0.1524;";
        let adjusted = thicken_schematic_wires(svg);
        assert!(adjusted.contains("stroke:#009600; stroke-width:0.2540;"));
        assert!(adjusted.contains("stroke:#840000; stroke-width:0.1524;"));
    }

    #[test]
    fn gives_kicad_wire_paths_explicit_strokes_for_resvg() {
        let svg = "before<g style=\"fill:none; \nstroke:#009600; stroke-width:0.1524; rest\"><path d=\"M0 0 L1 1\" /></g>after";
        let adjusted = thicken_schematic_wires(svg);
        assert!(adjusted.contains(
            "<path style=\"fill:none;stroke:#009600;stroke-width:0.2540\" d=\"M0 0 L1 1\" />"
        ));
    }

    #[test]
    fn restores_current_kicad_invisible_wire_group() {
        let svg = "before<g style=\"fill:none; stroke:none;\"><path d=\"M36.83 77.47 L49.53 77.47\" /></g>after";
        let adjusted = thicken_schematic_wires(svg);
        assert!(adjusted.contains(
            "<path style=\"fill:none;stroke:#009600;stroke-width:0.2540\" d=\"M36.83 77.47 L49.53 77.47\" />"
        ));
    }
}
