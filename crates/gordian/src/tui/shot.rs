//! Screenshots: render an [`App`] through ratatui's `TestBackend` and serialise
//! the resulting cell buffer to an SVG — one `<rect>` per coloured background,
//! one centred `<text>` per glyph.
//!
//! It is how the cockpit's cosmetics are reviewed without a terminal (`/shot`
//! writes one into the project), and it doubles as the smoke test that a frame
//! draws at all.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use super::app::App;
use super::theme;
use super::ui;

/// Cell geometry in SVG user units. `CH` is a generous line height so the
/// capture reads as airily as a comfortable terminal does.
const CW: f64 = 9.0;
const CH: f64 = 20.0;
const FS: f64 = 14.0;
/// Margin around the terminal content, so the capture reads as a window.
const PAD: f64 = 18.0;

/// Draw `app` at `w`×`h` cells and write the SVG into `dir`, returning its path.
pub fn capture(app: &mut App, dir: &Path, w: u16, h: u16) -> Result<PathBuf> {
    let mut terminal = Terminal::new(TestBackend::new(w, h))?;
    terminal.draw(|f| ui::draw(f, app))?;
    let svg = to_svg(terminal.backend().buffer());
    let path = dir.join("tui.svg");
    std::fs::write(&path, svg).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// A colour as CSS hex. The cockpit paints from [`theme`], which is truecolour
/// end to end, so only `Rgb` and `Reset` can appear.
fn hex(c: Color, is_fg: bool) -> Option<String> {
    match c {
        Color::Reset if is_fg => hex(theme::FG, true),
        Color::Reset => None,
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        other => unreachable!("the cockpit paints only theme truecolours, got {other:?}"),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Serialise a rendered cell buffer to an SVG document.
fn to_svg(buf: &Buffer) -> String {
    let (w, h) = (buf.area.width, buf.area.height);
    let (cw, ch) = (CW * w as f64, CH * h as f64);
    let (pw, ph) = (cw + 2.0 * PAD, ch + 2.0 * PAD);
    let panel = hex(theme::BG0, false).expect("theme colours are truecolour");
    let backdrop = hex(Color::Rgb(0x08, 0x07, 0x06), false).expect("truecolour");

    let mut s = String::new();
    let _ = writeln!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{pw:.0}\" height=\"{ph:.0}\" \
         viewBox=\"0 0 {pw:.0} {ph:.0}\">\n\
         <rect width=\"{pw:.0}\" height=\"{ph:.0}\" fill=\"{backdrop}\"/>\n\
         <rect x=\"{:.0}\" y=\"{:.0}\" width=\"{:.0}\" height=\"{:.0}\" rx=\"10\" fill=\"{panel}\"/>\n\
         <g transform=\"translate({PAD:.0} {PAD:.0})\">",
        PAD - 8.0,
        PAD - 8.0,
        cw + 16.0,
        ch + 16.0,
    );
    // Backgrounds first, under the glyphs. The overlap keeps inline renders —
    // which are solid half-block cells — from showing seams.
    for y in 0..h {
        for x in 0..w {
            let Some(bg) = buf.cell((x, y)).and_then(|c| hex(c.bg, false)) else {
                continue;
            };
            let _ = writeln!(
                s,
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"{bg}\"/>",
                x as f64 * CW,
                y as f64 * CH,
                CW + 0.6,
                CH + 0.6,
            );
        }
    }
    for y in 0..h {
        for x in 0..w {
            let Some(cell) = buf.cell((x, y)) else {
                continue;
            };
            let symbol = cell.symbol();
            if symbol.trim().is_empty() {
                continue;
            }
            let fg = hex(cell.fg, true).unwrap_or_else(|| hex(theme::FG, true).unwrap());
            let bold = match cell.modifier.contains(Modifier::BOLD) {
                true => " font-weight=\"bold\"",
                false => "",
            };
            let tx = x as f64 * CW + CW / 2.0;
            let ty = y as f64 * CH + (CH + FS * 0.7) / 2.0;
            let _ = writeln!(
                s,
                "<text x=\"{tx:.1}\" y=\"{ty:.1}\" font-family=\"DejaVu Sans Mono, monospace\" \
                 font-size=\"{FS}\" fill=\"{fg}\" text-anchor=\"middle\"{bold}>{}</text>",
                xml_escape(symbol),
            );
        }
    }
    s.push_str("</g></svg>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Msg, Status, TurnEnd};
    use gordian_core::AgentEvent;
    use std::path::PathBuf;

    /// The end-to-end smoke test: a scripted session draws a frame and the
    /// capture lands on disk as a well-formed SVG carrying the transcript.
    #[test]
    fn a_scripted_session_captures_to_an_svg() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Status::new(
            "openai",
            "claude-opus-4-5",
            PathBuf::from("/tmp/proj"),
        ));
        for c in "design a 555 blinker".chars() {
            app.update(Msg::Char(c));
        }
        app.update(Msg::Submit);
        app.update(Msg::Agent(AgentEvent::ToolCall {
            name: "build".into(),
            args: "{\"bytes\":1204}".into(),
        }));
        app.update(Msg::Agent(AgentEvent::ToolResult {
            name: "build".into(),
            seconds: 3.4,
            summary: "v1: 12 parts, no issues".into(),
        }));
        app.update(Msg::Agent(AgentEvent::Render {
            label: "build 1".into(),
            path: render_png(dir.path()),
        }));
        app.update(Msg::TurnEnded(TurnEnd::Completed("erc clean".into())));

        let path = capture(&mut app, dir.path(), 100, 30).unwrap();

        let svg = std::fs::read_to_string(&path).unwrap();
        assert!(svg.starts_with("<svg"), "{svg:.80}");
        assert!(svg.ends_with("</svg>\n"));
        assert!(svg.contains(">b</text>"), "the transcript glyphs are drawn");
        assert!(
            svg.contains("fill=\"#4080c0\""),
            "the inline render's own colours reach the capture"
        );
    }

    /// A PNG for the inline render row: a flat colour the capture can be
    /// searched for.
    fn render_png(dir: &Path) -> PathBuf {
        let path = dir.join("v1_grid.png");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 64, 32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[0x40, 0x80, 0xc0].repeat(64 * 32))
            .unwrap();
        path
    }
}
