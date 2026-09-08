//! Rendering a sheet: `kicad-cli sch export svg`, then rasterised by `resvg`,
//! optionally with the blue coordinate grid the model reads positions off.
//!
//! Port of `schagent/render.py` without its ImageMagick and Pillow dependencies:
//! the grid is injected into the SVG in millimetres (a grid unit is 1.27 mm)
//! instead of being drawn onto the raster, which is the same picture and one
//! process fewer.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};

/// Millimetres per schematic grid unit.
pub const GRID_MM: f64 = 1.27;

/// Dots per inch the reference renderer used.
const DPI: f64 = 110.0;

/// Long-edge ceiling in pixels. A vision model bills an image by fixed-size
/// patches and rejects one that needs too many, so a big sheet is scaled down
/// rather than sent at full size.
const MAX_PX: f64 = 2200.0;

/// Grid units between two labelled grid lines.
const GRID_STEP: u32 = 10;

/// A rendered sheet: the plain picture, the same picture with the coordinate
/// grid, and where both were written.
pub struct Sheet {
    /// PNG bytes without the overlay — what the critic grades.
    pub clean: Vec<u8>,
    /// PNG bytes with the blue grid — what the designer model inspects.
    pub grid: Vec<u8>,
    pub clean_path: PathBuf,
    pub grid_path: PathBuf,
}

/// Export `sch` to SVG with `kicad-cli`, rasterise it, and write
/// `<stem>.png` plus `<stem>_grid.png` beside it.
pub fn sheet(kicad_cli: &Path, sch: &Path, clean_path: &Path, grid_path: &Path) -> Result<Sheet> {
    let svg = export_svg(kicad_cli, sch)?;
    let clean = svg_to_png(&svg)?;
    let grid = svg_to_png(&with_grid(&svg))?;
    std::fs::write(clean_path, &clean)
        .with_context(|| format!("writing {}", clean_path.display()))?;
    std::fs::write(grid_path, &grid).with_context(|| format!("writing {}", grid_path.display()))?;
    Ok(Sheet {
        clean,
        grid,
        clean_path: clean_path.to_path_buf(),
        grid_path: grid_path.to_path_buf(),
    })
}

/// The single-page SVG `kicad-cli` plots for a schematic.
fn export_svg(kicad_cli: &Path, sch: &Path) -> Result<String> {
    let dir = tempdir()?;
    let out = std::process::Command::new(kicad_cli)
        .args(["sch", "export", "svg", "-o"])
        .arg(&dir)
        .arg("--no-background-color")
        .arg(sch)
        .output()
        .with_context(|| format!("running {}", kicad_cli.display()))?;
    let svgs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "svg"))
        .collect();
    let page = svgs
        .iter()
        .min_by_key(|p| p.as_os_str().len())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "kicad-cli exported no SVG: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })?;
    let text = std::fs::read_to_string(page)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(text)
}

fn tempdir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "gordian-render-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The page rectangle in millimetres, read from the root `viewBox`.
fn view_box(svg: &str) -> Option<(f64, f64)> {
    let start = svg.find("viewBox=\"")? + "viewBox=\"".len();
    let end = start + svg[start..].find('"')?;
    let mut numbers = svg[start..end]
        .split_whitespace()
        .filter_map(|n| n.parse::<f64>().ok());
    let (_, _, w, h) = (
        numbers.next()?,
        numbers.next()?,
        numbers.next()?,
        numbers.next()?,
    );
    Some((w, h))
}

/// The same SVG with a blue coordinate grid in grid units drawn over it, and the
/// grid-unit number written at both ends of every line — the reference render's
/// overlay, in vector form.
fn with_grid(svg: &str) -> String {
    let Some((w, h)) = view_box(svg) else {
        return svg.to_string();
    };
    let close = match svg.rfind("</svg>") {
        Some(at) => at,
        None => return svg.to_string(),
    };
    let font = 11.0 / (DPI / 25.4);
    let mut overlay = String::from("<g id=\"gordian-grid\">");
    let mut unit = 0u32;
    loop {
        let mm = f64::from(unit) * GRID_MM;
        if mm > w && mm > h {
            break;
        }
        let opacity = if unit.is_multiple_of(50) { 0.55 } else { 0.27 };
        if mm <= w {
            overlay.push_str(&format!(
                "<line x1=\"{mm:.3}\" y1=\"0\" x2=\"{mm:.3}\" y2=\"{h:.3}\" stroke=\"#005AC8\" stroke-width=\"0.12\" stroke-opacity=\"{opacity}\"/>\
                 <text x=\"{:.3}\" y=\"{:.3}\" font-size=\"{font:.3}\" fill=\"#003CB4\">{unit}</text>\
                 <text x=\"{:.3}\" y=\"{:.3}\" font-size=\"{font:.3}\" fill=\"#003CB4\">{unit}</text>",
                mm + 0.5,
                font,
                mm + 0.5,
                h - 0.6,
            ));
        }
        if mm <= h {
            overlay.push_str(&format!(
                "<line x1=\"0\" y1=\"{mm:.3}\" x2=\"{w:.3}\" y2=\"{mm:.3}\" stroke=\"#005AC8\" stroke-width=\"0.12\" stroke-opacity=\"{opacity}\"/>\
                 <text x=\"0.5\" y=\"{:.3}\" font-size=\"{font:.3}\" fill=\"#003CB4\">{unit}</text>\
                 <text x=\"{:.3}\" y=\"{:.3}\" font-size=\"{font:.3}\" fill=\"#003CB4\">{unit}</text>",
                mm + font,
                w - 6.0,
                mm + font,
            ));
        }
        unit += GRID_STEP;
    }
    overlay.push_str("</g>");
    let mut out = String::with_capacity(svg.len() + overlay.len());
    out.push_str(&svg[..close]);
    out.push_str(&overlay);
    out.push_str(&svg[close..]);
    out
}

/// Rasterise an SVG at the reference renderer's 110 dpi onto a white page,
/// scaled down when the page would exceed [`MAX_PX`] on its long edge.
pub fn svg_to_png(svg: &str) -> Result<Vec<u8>> {
    let options = resvg::usvg::Options {
        fontdb: fonts().clone(),
        dpi: DPI as f32,
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_str(svg, &options).context("parsing the sheet SVG")?;
    let size = tree.size();
    let long = size.width().max(size.height()) as f64;
    let scale = (MAX_PX / long).min(1.0) as f32;
    let (w, h) = (
        (size.width() * scale).ceil() as u32,
        (size.height() * scale).ceil() as u32,
    );
    if w == 0 || h == 0 {
        bail!("the sheet SVG has an empty page");
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).context("allocating the page raster")?;
    pixmap.fill(resvg::tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().context("encoding the page PNG")
}

fn fonts() -> &'static Arc<resvg::usvg::fontdb::Database> {
    static DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"25.4mm\" height=\"12.7mm\" \
                       viewBox=\"0.0000 0.0000 25.4000 12.7000\"></svg>";

    #[test]
    fn the_page_rectangle_comes_from_the_view_box() {
        assert_eq!(view_box(SVG), Some((25.4, 12.7)));
    }

    #[test]
    fn grid_lines_land_on_multiples_of_ten_grid_units() {
        let gridded = with_grid(SVG);
        assert!(gridded.contains("x1=\"0.000\""));
        assert!(gridded.contains("x1=\"12.700\""), "{gridded}");
        assert!(gridded.contains("x1=\"25.400\""));
        assert!(!gridded.contains("x1=\"38.100\""));
        assert!(gridded.ends_with("</svg>"));
    }

    #[test]
    fn a_gridded_page_rasterises() {
        let png = svg_to_png(&with_grid(SVG)).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}
