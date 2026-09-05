//! Turn a KiCAD export into a PNG the LLM can see.
//!
//! The schematic is exported as PDF and rasterized by poppler (`pdftoppm`), so
//! the drawing reaches the grader through the renderer KiCAD writes it for.
//! Rasterizing KiCAD's SVG means trusting a second reading of it, and both
//! readings available here are lossy: ImageMagick refuses seven of the
//! twenty-four corpus sheets outright (`vector graphics nested too deeply`),
//! and `resvg` had to have KiCAD's zero-width wire strokes patched before it
//! would draw them at all.
//!
//! `resvg` still draws SVGs this workspace writes itself: the coordinate
//! overlay, with the poppler raster embedded beneath it, and the PCB exports.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use kicad::KicadInstallation;

/// Poppler's PDF rasterizer: the renderer KiCAD's PDF export is written for.
const POPPLER: &str = "pdftoppm";

const DENSE_RENDER_ITEMS: usize = 40;
const REFERENCE_TEXT_HEIGHT_MM: f64 = 0.8;
const TARGET_REFERENCE_HEIGHT_PX: f64 = 10.0;

/// A rectangle of sheet millimetres: an SVG view box, or a crop of a PDF page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderBounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl RenderBounds {
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    pub fn width(self) -> f64 {
        self.max_x - self.min_x
    }

    pub fn height(self) -> f64 {
        self.max_y - self.min_y
    }
}

/// Colors used for an accessible coordinate overlay and its background.
#[derive(Clone, Copy, Debug)]
pub struct CoordinateOverlayStyle<'a> {
    pub background: &'a str,
    pub axis: &'a str,
    pub grid: &'a str,
    pub x_axis: &'a str,
    pub y_axis: &'a str,
}

/// Overview and optional detail resolution selected from content density and extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderPlan {
    pub overview_px: u32,
    pub detail_px: Option<u32>,
}

/// Select bounded overview/detail resolutions for physical drawing bounds.
pub fn render_plan(item_count: usize, bounds: RenderBounds, configured_max_px: u32) -> RenderPlan {
    let base = configured_max_px.max(1);
    let width = bounds.width().max(0.0);
    let height = bounds.height().max(0.0);
    let long = width.max(height);
    let needs_detail = item_count >= DENSE_RENDER_ITEMS
        || estimated_reference_pixels(long, base) < TARGET_REFERENCE_HEIGHT_PX;
    if !needs_detail {
        return RenderPlan {
            overview_px: base,
            detail_px: None,
        };
    }

    let margins = overlay_margins(long);
    let overview_long =
        (width + margins.left + margins.right).max(height + margins.top + margins.bottom);
    let required = ((overview_long / REFERENCE_TEXT_HEIGHT_MM) * TARGET_REFERENCE_HEIGHT_PX)
        .ceil()
        .max(base as f64) as u32;
    let detail_px = required.min(base.saturating_mul(2)).max(base);
    RenderPlan {
        overview_px: detail_px,
        detail_px: Some(detail_px),
    }
}

/// Add a background and coordinate rulers whose labels use `bounds` and `unit_label`.
pub fn add_coordinate_overlay(
    svg: &str,
    bounds: RenderBounds,
    unit_label: &str,
    style: CoordinateOverlayStyle<'_>,
) -> String {
    let Some((viewbox_start, viewbox_end, viewbox)) = find_viewbox(svg) else {
        return svg.to_owned();
    };
    if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return svg.to_owned();
    }
    let margins = overlay_margins(viewbox.w.max(viewbox.h));
    let expanded = ViewBox {
        x: viewbox.x - margins.left,
        y: viewbox.y - margins.top,
        w: viewbox.w + margins.left + margins.right,
        h: viewbox.h + margins.top + margins.bottom,
    };
    let background = format!(
        "\n  <rect id=\"gordian-render-background\" x=\"{:.4}\" y=\"{:.4}\" width=\"{:.4}\" height=\"{:.4}\" fill=\"{}\"/>\n",
        expanded.x, expanded.y, expanded.w, expanded.h, style.background
    );

    let mut out = String::with_capacity(svg.len() + 4096);
    out.push_str(&svg[..viewbox_start]);
    write!(
        out,
        "viewBox=\"{:.4} {:.4} {:.4} {:.4}\"",
        expanded.x, expanded.y, expanded.w, expanded.h
    )
    .unwrap();
    out.push_str(&svg[viewbox_end..]);
    let Some(mut out) = replace_root_dimension(&out, "width", expanded.w) else {
        return svg.to_owned();
    };
    let Some(updated) = replace_root_dimension(&out, "height", expanded.h) else {
        return svg.to_owned();
    };
    out = updated;
    let Some(svg_tag_start) = out.find("<svg") else {
        return svg.to_owned();
    };
    let Some(svg_tag_end) = out[svg_tag_start..]
        .find('>')
        .map(|end| end + svg_tag_start + 1)
    else {
        return svg.to_owned();
    };
    out.insert_str(svg_tag_end, &background);

    let mut overlay = String::new();
    push_coordinate_axes(&mut overlay, viewbox, bounds, unit_label, style);
    if let Some(insert) = out.rfind("</svg>") {
        out.insert_str(insert, &overlay);
    }
    out
}

fn estimated_reference_pixels(long_mm: f64, long_edge_px: u32) -> f64 {
    if long_mm <= 0.0 {
        return f64::INFINITY;
    }
    REFERENCE_TEXT_HEIGHT_MM * long_edge_px as f64 / long_mm
}

#[derive(Clone, Copy, Debug)]
struct ViewBox {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn find_viewbox(svg: &str) -> Option<(usize, usize, ViewBox)> {
    let attr_start = svg.find("viewBox=\"")?;
    let value_start = attr_start + "viewBox=\"".len();
    let value_end = svg[value_start..].find('"')? + value_start;
    let mut vals = svg[value_start..value_end]
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(str::parse::<f64>);
    let viewbox = ViewBox {
        x: vals.next()?.ok()?,
        y: vals.next()?.ok()?,
        w: vals.next()?.ok()?,
        h: vals.next()?.ok()?,
    };
    Some((attr_start, value_end + 1, viewbox))
}

fn replace_root_dimension(svg: &str, attr: &str, value_mm: f64) -> Option<String> {
    let svg_tag_start = svg.find("<svg")?;
    let svg_tag_end = svg[svg_tag_start..].find('>')? + svg_tag_start + 1;
    let tag = &svg[svg_tag_start..svg_tag_end];
    let needle = format!("{attr}=\"");
    let mut out = String::with_capacity(svg.len() + 16);
    if let Some(relative_attr) = tag.find(&needle) {
        let value_start = svg_tag_start + relative_attr + needle.len();
        let value_end = svg[value_start..].find('"')? + value_start;
        out.push_str(&svg[..value_start]);
        write!(out, "{value_mm:.4}mm").unwrap();
        out.push_str(&svg[value_end..]);
    } else {
        let insert = svg_tag_end - 1;
        out.push_str(&svg[..insert]);
        write!(out, " {attr}=\"{value_mm:.4}mm\"").unwrap();
        out.push_str(&svg[insert..]);
    }
    Some(out)
}

fn push_coordinate_axes(
    out: &mut String,
    viewbox: ViewBox,
    bounds: RenderBounds,
    unit_label: &str,
    style: CoordinateOverlayStyle<'_>,
) {
    let long = viewbox.w.max(viewbox.h);
    let margins = overlay_margins(long);
    let axis_gap = (margins.left * 0.36).clamp(2.8, 6.2);
    let tick = (long * 0.012).clamp(0.45, 1.5);
    let font = (long * 0.018).clamp(1.0, 2.2);
    let arrow = (tick * 2.8).clamp(1.8, 3.8);
    let step = nice_tick_step(bounds.width().max(bounds.height()));
    let x_axis_y = viewbox.y + viewbox.h + axis_gap;
    let y_axis_x = viewbox.x - axis_gap;
    let x_axis_end = viewbox.x + viewbox.w + arrow;
    let y_axis_end = viewbox.y + viewbox.h + arrow;
    let map_x = |x: f64| viewbox.x + (x - bounds.min_x) * viewbox.w / bounds.width();
    let map_y = |y: f64| viewbox.y + (y - bounds.min_y) * viewbox.h / bounds.height();

    out.push_str("\n<g id=\"gordian-coordinate-rulers\" fill=\"none\" stroke-linecap=\"round\" font-family=\"ui-monospace, SFMono-Regular, Menlo, Consolas, monospace\">\n");
    let mut x = first_tick(bounds.min_x, step);
    while x <= bounds.max_x + 1e-6 {
        let lx = map_x(x);
        if lx > viewbox.x + 1e-6 && lx < viewbox.x + viewbox.w - 1e-6 {
            writeln!(
                out,
                "    <line x1=\"{lx:.4}\" y1=\"{:.4}\" x2=\"{lx:.4}\" y2=\"{:.4}\" stroke=\"{}\" stroke-width=\"0.0800\" stroke-opacity=\"0.22\"/>",
                viewbox.y,
                viewbox.y + viewbox.h,
                style.grid,
            )
            .unwrap();
        }
        writeln!(
            out,
            "    <line x1=\"{lx:.4}\" y1=\"{:.4}\" x2=\"{lx:.4}\" y2=\"{:.4}\" stroke=\"{}\" stroke-width=\"0.1800\" stroke-opacity=\"0.95\"/>",
            x_axis_y - tick,
            x_axis_y + tick,
            style.axis,
        )
        .unwrap();
        writeln!(
            out,
            "    <text x=\"{lx:.4}\" y=\"{:.4}\" fill=\"{}\" stroke=\"none\" font-size=\"{font:.4}\" text-anchor=\"middle\">{}</text>",
            x_axis_y + tick + font,
            style.axis,
            fmt_axis_label(x),
        )
        .unwrap();
        x += step;
    }

    let mut y = first_tick(bounds.min_y, step);
    while y <= bounds.max_y + 1e-6 {
        let ly = map_y(y);
        if ly > viewbox.y + 1e-6 && ly < viewbox.y + viewbox.h - 1e-6 {
            writeln!(
                out,
                "    <line x1=\"{:.4}\" y1=\"{ly:.4}\" x2=\"{:.4}\" y2=\"{ly:.4}\" stroke=\"{}\" stroke-width=\"0.0800\" stroke-opacity=\"0.22\"/>",
                viewbox.x,
                viewbox.x + viewbox.w,
                style.grid,
            )
            .unwrap();
        }
        writeln!(
            out,
            "    <line x1=\"{:.4}\" y1=\"{ly:.4}\" x2=\"{:.4}\" y2=\"{ly:.4}\" stroke=\"{}\" stroke-width=\"0.1800\" stroke-opacity=\"0.95\"/>",
            y_axis_x - tick,
            y_axis_x + tick,
            style.axis,
        )
        .unwrap();
        writeln!(
            out,
            "    <text x=\"{:.4}\" y=\"{:.4}\" fill=\"{}\" stroke=\"none\" font-size=\"{font:.4}\" text-anchor=\"end\">{}</text>",
            y_axis_x - tick * 1.2,
            ly + font * 0.35,
            style.axis,
            fmt_axis_label(y),
        )
        .unwrap();
        y += step;
    }

    writeln!(
        out,
        "    <line x1=\"{:.4}\" y1=\"{x_axis_y:.4}\" x2=\"{x_axis_end:.4}\" y2=\"{x_axis_y:.4}\" stroke=\"{}\" stroke-width=\"0.2800\" stroke-opacity=\"0.98\"/>",
        viewbox.x, style.x_axis,
    )
    .unwrap();
    writeln!(
        out,
        "    <polygon points=\"{:.4},{:.4} {:.4},{:.4} {:.4},{:.4}\" fill=\"{}\" stroke=\"none\" fill-opacity=\"0.98\"/>",
        x_axis_end,
        x_axis_y,
        x_axis_end - arrow,
        x_axis_y - arrow * 0.45,
        x_axis_end - arrow,
        x_axis_y + arrow * 0.45,
        style.x_axis,
    )
    .unwrap();
    writeln!(
        out,
        "    <line x1=\"{y_axis_x:.4}\" y1=\"{:.4}\" x2=\"{y_axis_x:.4}\" y2=\"{y_axis_end:.4}\" stroke=\"{}\" stroke-width=\"0.2800\" stroke-opacity=\"0.98\"/>",
        viewbox.y, style.y_axis,
    )
    .unwrap();
    writeln!(
        out,
        "    <polygon points=\"{:.4},{:.4} {:.4},{:.4} {:.4},{:.4}\" fill=\"{}\" stroke=\"none\" fill-opacity=\"0.98\"/>",
        y_axis_x,
        y_axis_end,
        y_axis_x - arrow * 0.45,
        y_axis_end - arrow,
        y_axis_x + arrow * 0.45,
        y_axis_end - arrow,
        style.y_axis,
    )
    .unwrap();
    writeln!(
        out,
        "    <text x=\"{:.4}\" y=\"{:.4}\" fill=\"{}\" stroke=\"none\" font-size=\"{:.4}\" font-weight=\"700\" text-anchor=\"start\">X {unit_label}</text>",
        x_axis_end + font * 0.45,
        x_axis_y + font * 0.35,
        style.x_axis,
        font * 1.08,
    )
    .unwrap();
    writeln!(
        out,
        "    <text x=\"{:.4}\" y=\"{:.4}\" fill=\"{}\" stroke=\"none\" font-size=\"{:.4}\" font-weight=\"700\" text-anchor=\"middle\">Y {unit_label}</text>",
        y_axis_x,
        y_axis_end + font * 1.2,
        style.y_axis,
        font * 1.08,
    )
    .unwrap();
    out.push_str("  </g>\n");
}

#[derive(Clone, Copy, Debug)]
struct OverlayMargins {
    top: f64,
    right: f64,
    bottom: f64,
    left: f64,
}

fn overlay_margins(long: f64) -> OverlayMargins {
    let main = (long * 0.12).clamp(8.0, 18.0);
    OverlayMargins {
        top: (long * 0.018).clamp(1.2, 3.0),
        right: main * 1.25,
        bottom: main,
        left: main,
    }
}

fn nice_tick_step(span: f64) -> f64 {
    let raw = (span / 6.0).max(1.0);
    let exp = raw.log10().floor();
    let base = 10f64.powf(exp);
    for factor in [1.0, 2.0, 5.0, 10.0] {
        let step = factor * base;
        if step >= raw {
            return step;
        }
    }
    10.0 * base
}

fn first_tick(min: f64, step: f64) -> f64 {
    (min / step).ceil() * step
}

fn fmt_axis_label(value: f64) -> String {
    let value = if value.abs() < 0.0005 { 0.0 } else { value };
    if (value - value.round()).abs() < 0.0005 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// One schematic exported to PDF, rasterizable at any crop and resolution.
pub struct SchematicSheet {
    _dir: tempfile::TempDir,
    pdf: std::path::PathBuf,
}

/// A rasterized crop and the physical rectangle its pixels actually cover.
///
/// `bounds` is the requested rectangle snapped to whole pixels, so overlay
/// coordinates drawn over the image land where the drawing puts them.
pub struct Raster {
    pub png: Vec<u8>,
    pub bounds: RenderBounds,
    pub width_px: u32,
    pub height_px: u32,
}

/// Export `sch` to a PDF that can be rasterized repeatedly.
pub fn schematic_sheet(env: &KicadInstallation, sch: &Path) -> Result<SchematicSheet> {
    let dir = tempfile::tempdir().context("temp dir for schematic PDF export")?;
    let pdf = dir.path().join("sheet.pdf");
    env.export_sch_pdf(sch, &pdf)
        .context("exporting schematic PDF")?;
    Ok(SchematicSheet { _dir: dir, pdf })
}

impl SchematicSheet {
    /// Rasterize `bounds` so its long edge is `long_edge_px` pixels.
    pub fn raster(&self, bounds: RenderBounds, long_edge_px: u32) -> Result<Raster> {
        let Some(Crop { dpi, x, y, w, h }) = crop(bounds, long_edge_px) else {
            anyhow::bail!("empty render bounds {bounds:?}");
        };

        let out = tempfile::tempdir().context("temp dir for schematic raster")?;
        let root = out.path().join("page");
        let status = std::process::Command::new(POPPLER)
            .args(["-r", &format!("{dpi:.6}"), "-png", "-singlefile"])
            .args(["-x", &x.to_string(), "-y", &y.to_string()])
            .args(["-W", &w.to_string(), "-H", &h.to_string()])
            .arg(&self.pdf)
            .arg(&root)
            .output()
            .with_context(|| format!("running {POPPLER}"))?;
        anyhow::ensure!(
            status.status.success(),
            "{POPPLER} failed: {}",
            String::from_utf8_lossy(&status.stderr).trim(),
        );
        let png = std::fs::read(root.with_extension("png"))
            .with_context(|| format!("{POPPLER} produced no page for {bounds:?}"))?;

        // The page can clip the request; report the rectangle the pixels cover.
        let pixmap = resvg::tiny_skia::Pixmap::decode_png(&png).context("decoding page PNG")?;
        let mm = |value: u32| value as f64 * 25.4 / dpi;
        Ok(Raster {
            bounds: RenderBounds::new(
                mm(x),
                mm(y),
                mm(x + pixmap.width()),
                mm(y + pixmap.height()),
            ),
            width_px: pixmap.width(),
            height_px: pixmap.height(),
            png,
        })
    }
}

/// Where a millimetre rectangle lands on a PDF page rasterized at `dpi`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Crop {
    dpi: f64,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// The page crop whose long edge is `long_edge_px` pixels, or `None` if
/// `bounds` encloses nothing.
fn crop(bounds: RenderBounds, long_edge_px: u32) -> Option<Crop> {
    let long_mm = bounds.width().max(bounds.height());
    if long_mm <= 0.0 {
        return None;
    }
    let dpi = long_edge_px.max(1) as f64 / long_mm * 25.4;
    let px = |mm: f64| (mm * dpi / 25.4).round().max(0.0) as u32;
    let (x, y) = (px(bounds.min_x), px(bounds.min_y));
    Some(Crop {
        dpi,
        x,
        y,
        w: px(bounds.max_x) - x,
        h: px(bounds.max_y) - y,
    })
}

/// Wrap `raster` as an SVG in its own physical coordinates, so the coordinate
/// overlay draws over the rendered sheet instead of over KiCAD's SVG export.
pub fn raster_svg(raster: &Raster) -> String {
    use base64::Engine;
    let b = raster.bounds;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.4}mm\" height=\"{h:.4}mm\" \
         viewBox=\"{x:.4} {y:.4} {w:.4} {h:.4}\">\n\
         <image x=\"{x:.4}\" y=\"{y:.4}\" width=\"{w:.4}\" height=\"{h:.4}\" \
         href=\"data:image/png;base64,{data}\"/>\n</svg>\n",
        x = b.min_x,
        y = b.min_y,
        w = b.width(),
        h = b.height(),
        data = base64::engine::general_purpose::STANDARD.encode(&raster.png),
    )
}

/// Render `svg` to PNG bytes, scaling so the long edge is `max_px` pixels.
pub fn svg_to_png(svg: &str, max_px: u32) -> Result<Vec<u8>> {
    let opt = resvg::usvg::Options {
        fontdb: render_fontdb().clone(),
        ..Default::default()
    };
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

fn render_fontdb() -> &'static Arc<resvg::usvg::fontdb::Database> {
    static FONT_DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    FONT_DB.get_or_init(|| {
        let mut database = resvg::usvg::fontdb::Database::new();
        database.load_system_fonts();
        Arc::new(database)
    })
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
    fn coordinate_overlay_places_ticks_at_expected_millimetres() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 10">
<rect x="0" y="0" width="20" height="10"/>
</svg>"#;
        let overlaid = add_coordinate_overlay(
            svg,
            RenderBounds::new(10.0, 20.0, 30.0, 30.0),
            "mm",
            CoordinateOverlayStyle {
                background: "#ffffff",
                axis: "#111827",
                grid: "#64748b",
                x_axis: "#be123c",
                y_axis: "#1d4ed8",
            },
        );

        assert!(overlaid.contains("id=\"gordian-coordinate-rulers\""));
        assert!(overlaid.contains("x1=\"10.0000\" y1=\"0.0000\""));
        assert!(overlaid.contains("x1=\"0.0000\" y1=\"5.0000\""));
        assert!(overlaid.contains(">20</text>"));
        assert!(overlaid.contains(">25</text>"));
        assert!(overlaid.contains(">X mm</text>"));
        assert!(overlaid.contains(">Y mm</text>"));
    }

    #[test]
    fn a_crop_puts_the_requested_long_edge_on_the_page() {
        let page = crop(RenderBounds::new(10.0, 20.0, 110.0, 70.0), 2000).expect("crop");
        assert_eq!((page.w, page.h), (2000, 1000));
        assert_eq!((page.x, page.y), (200, 400));
        assert!((page.dpi - 2000.0 / 100.0 * 25.4).abs() < 1e-9);
        assert_eq!(crop(RenderBounds::new(0.0, 0.0, 0.0, 0.0), 100), None);
    }

    #[test]
    fn a_raster_carries_its_own_page_rectangle_into_the_overlay_svg() {
        let raster = Raster {
            png: vec![1, 2, 3],
            bounds: RenderBounds::new(10.0, 20.0, 110.0, 70.0),
            width_px: 2000,
            height_px: 1000,
        };
        let svg = raster_svg(&raster);
        assert!(svg.contains("viewBox=\"10.0000 20.0000 100.0000 50.0000\""));
        assert!(svg.contains("href=\"data:image/png;base64,AQID\""));
    }

    #[test]
    fn large_or_dense_render_plans_request_details() {
        let small = render_plan(12, RenderBounds::new(0.0, 0.0, 80.0, 70.0), 1600);
        assert_eq!(small.detail_px, None);

        let dense = render_plan(40, RenderBounds::new(0.0, 0.0, 80.0, 70.0), 1600);
        assert_eq!(dense.detail_px, Some(1600));

        let large = render_plan(12, RenderBounds::new(0.0, 0.0, 200.0, 120.0), 1600);
        assert!(large.detail_px.is_some_and(|pixels| pixels > 1600));
    }
}
