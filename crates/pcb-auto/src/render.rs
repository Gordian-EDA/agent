//! Fabrication-style board plots. KiCad's SVG viewBox is the board area in millimetres, so the
//! raster is an exact scaling of it; the Edge.Cuts strokes are darkened first because KiCad's
//! default near-white edge is invisible on a white plot.

use std::path::Path;

use kicad::KicadInstallation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Front,
    Back,
}

impl Side {
    fn layers(self) -> &'static str {
        match self {
            // the opposite copper stays in the picture as context
            Side::Front => "F.Cu,B.Cu,F.SilkS,Edge.Cuts",
            Side::Back => "B.Cu,F.Cu,B.SilkS,Edge.Cuts",
        }
    }
    fn mirror(self) -> bool {
        self == Side::Back
    }
}

const EDGE_CUT_COLORS: [&str; 2] = ["#D0D2CD", "#D5D7D2"];
const EDGE_CUT_INK: &str = "#1A1A1A";
const EDGE_CUT_WIDTH_MM: f64 = 0.3;
const MAX_PX: u32 = 1600;

/// Recolour the Edge.Cuts strokes in a KiCad SVG dark and thicken them. A board whose theme gives
/// Edge.Cuts some other colour matches nothing and is left exactly as KiCad wrote it.
pub fn darken_edge_cuts(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(i) = rest.find("style=\"") {
        let (head, tail) = rest.split_at(i + 7);
        out.push_str(head);
        let Some(j) = tail.find('"') else {
            out.push_str(tail);
            return out;
        };
        let style = &tail[..j];
        let is_edge = EDGE_CUT_COLORS
            .iter()
            .any(|c| style.to_ascii_uppercase().contains(&c.to_ascii_uppercase()));
        if is_edge {
            let mut fixed = String::new();
            for part in style.split(';') {
                let p = part.trim();
                if p.is_empty() {
                    continue;
                }
                if !fixed.is_empty() {
                    fixed.push_str("; ");
                }
                if p.starts_with("stroke-width") {
                    fixed.push_str(&format!("stroke-width:{EDGE_CUT_WIDTH_MM}"));
                } else if p.starts_with("stroke:") || p.starts_with("fill:") {
                    let key = p.split(':').next().unwrap_or("stroke");
                    let val = p.split(':').nth(1).unwrap_or("").trim();
                    let hit = EDGE_CUT_COLORS
                        .iter()
                        .any(|c| val.eq_ignore_ascii_case(c));
                    fixed.push_str(&format!(
                        "{key}:{}",
                        if hit { EDGE_CUT_INK } else { val }
                    ));
                } else {
                    fixed.push_str(p);
                }
            }
            out.push_str(&fixed);
        } else {
            out.push_str(style);
        }
        rest = &tail[j..];
    }
    out.push_str(rest);
    out
}

/// The SVG's viewBox, in millimetres.
fn svg_frame(svg: &str) -> Option<(f64, f64)> {
    let i = svg.find("viewBox=\"")? + 9;
    let rest = &svg[i..];
    let j = rest.find('"')?;
    let nums: Vec<f64> = rest[..j]
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    (nums.len() >= 4).then(|| (nums[2], nums[3]))
}

/// How much of the poured copper shows through under the tracks and pads.
const ZONE_FILL_ALPHA: f32 = 0.22;

fn rasterise(svg: &str, w_px: u32, h_px: u32) -> anyhow::Result<resvg::tiny_skia::Pixmap> {
    let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default())?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w_px, h_px)
        .ok_or_else(|| anyhow::anyhow!("cannot allocate a {w_px}x{h_px} raster"))?;
    let size = tree.size();
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(
            w_px as f32 / size.width(),
            h_px as f32 / size.height(),
        ),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap)
}

/// A copy of the board with its copper pours deleted, so the ink plot shows tracks and pads
/// instead of a wall of poured copper.
fn without_pours(pcb: &Path, into: &Path) -> anyhow::Result<bool> {
    let mut board = crate::model::Board::load(pcb)?;
    let before = board.tree.items.len();
    let items = std::mem::take(&mut board.tree.items);
    board.tree.items = items
        .into_iter()
        .filter(|c| {
            c.as_list()
                .map(|z| !(z.is("zone") && z.find("keepout").is_none()))
                .unwrap_or(true)
        })
        .collect();
    let removed = before - board.tree.items.len();
    board.save(Some(into))?;
    Ok(removed > 0)
}

/// Plot the board to PNG at fabrication colours, with any copper pour faded back under the ink.
pub fn render(
    kicad: &KicadInstallation,
    pcb: &Path,
    out_png: &Path,
    side: Side,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let plot = |src: &Path, name: &str| -> anyhow::Result<String> {
        let svg_path = dir.path().join(name);
        kicad.export_pcb_svg(src, &svg_path, side.layers(), side.mirror())?;
        Ok(darken_edge_cuts(&std::fs::read_to_string(&svg_path)?))
    };

    let bare = dir.path().join("bare.kicad_pcb");
    let poured = without_pours(pcb, &bare).unwrap_or(false);
    let ink = plot(if poured { &bare } else { pcb }, "ink.svg")?;
    let fill = if poured {
        plot(pcb, "fill.svg").ok()
    } else {
        None
    };

    let (w_mm, h_mm) = svg_frame(&ink).ok_or_else(|| anyhow::anyhow!("plot has no viewBox"))?;
    anyhow::ensure!(w_mm > 0.0 && h_mm > 0.0, "empty plot; does the board have an outline?");
    let scale = MAX_PX as f64 / w_mm.max(h_mm);
    let (w_px, h_px) = (
        (w_mm * scale).round().max(1.0) as u32,
        (h_mm * scale).round().max(1.0) as u32,
    );
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w_px, h_px)
        .ok_or_else(|| anyhow::anyhow!("cannot allocate a {w_px}x{h_px} raster"))?;
    pixmap.fill(resvg::tiny_skia::Color::WHITE);
    // The two plots share a viewBox — both frame on Edge.Cuts, which a fill cannot move — so they
    // composite exactly; a fill that somehow reframes is dropped rather than misregistered.
    if let Some(fill) = fill.filter(|f| svg_frame(f) == Some((w_mm, h_mm))) {
        let faded = rasterise(&fill, w_px, h_px)?;
        let paint = resvg::tiny_skia::PixmapPaint {
            opacity: ZONE_FILL_ALPHA,
            ..Default::default()
        };
        pixmap.draw_pixmap(
            0,
            0,
            faded.as_ref(),
            &paint,
            resvg::tiny_skia::Transform::identity(),
            None,
        );
    }
    let ink_map = rasterise(&ink, w_px, h_px)?;
    pixmap.draw_pixmap(
        0,
        0,
        ink_map.as_ref(),
        &resvg::tiny_skia::PixmapPaint::default(),
        resvg::tiny_skia::Transform::identity(),
        None,
    );
    if let Some(parent) = out_png.parent() {
        std::fs::create_dir_all(parent)?;
    }
    pixmap.save_png(out_png)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn edge_cut_styles_are_darkened_and_others_left_alone() {
        let svg = r#"<path style="fill:none; stroke:#D0D2CD; stroke-width:0.05"/><path style="stroke:#C83434; stroke-width:0.1"/>"#;
        let out = super::darken_edge_cuts(svg);
        assert!(out.contains("stroke:#1A1A1A"), "{out}");
        assert!(out.contains("stroke-width:0.3"), "{out}");
        assert!(out.contains("stroke:#C83434"), "other colours survive: {out}");
    }
}
