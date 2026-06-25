//! Dev-only TUI screenshot harness.
//!
//! The TUI is a terminal app, so to review its *cosmetics* visually we render
//! representative [`App`] states through ratatui's [`TestBackend`] into a cell
//! buffer, then serialise that buffer to an SVG (one `<rect>` per coloured cell
//! background, one centred `<text>` per glyph). `tools/tui_shot.sh` runs this
//! test and rasterises the SVGs to PNGs with cairosvg.
//!
//! This is the visual-feedback loop: edit `ui.rs` → `tools/tui_shot.sh` → look at
//! the PNGs → refine. It is also a smoke test that every state renders without
//! panicking.

use std::fmt::Write as _;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use gordian_core::AgentEvent;

use super::app::{App, Entry, Msg, NoticeLevel, PendingDiff, Speaker, Status};
use super::ui;

/// Where the SVGs are written; `tools/tui_shot.sh` reads from here.
const OUT_DIR: &str = "/tmp/tui_shots";

// Cell geometry, in SVG user units. Monospace glyphs are centred per cell, so the
// advance width never drifts off the grid. CH is a generous ~1.5× line-height
// (24/15) so the captures read airy, the way a comfortable terminal looks — real
// terminal leading is the emulator's to set, but the screenshot is ours.
const CW: f64 = 9.0;
const CH: f64 = 24.0;
const FS: f64 = 15.0;

// Default terminal fg/bg used for `Color::Reset`.
const BG: &str = "#1a1b26";
const FG: &str = "#c0caf5";

/// Map a ratatui [`Color`] to a CSS hex string. Named ANSI colours use a
/// Tokyo-Night-ish palette; `Reset` as a background returns `None` (the terminal
/// background shows through). RGB/indexed are mapped faithfully.
fn hex(c: Color, is_fg: bool) -> Option<String> {
    let named = match c {
        Color::Reset => return if is_fg { Some(FG.into()) } else { None },
        Color::Black => "#15161e",
        Color::Red => "#f7768e",
        Color::Green => "#9ece6a",
        Color::Yellow => "#e0af68",
        Color::Blue => "#7aa2f7",
        Color::Magenta => "#bb9af7",
        Color::Cyan => "#7dcfff",
        Color::Gray => "#a9b1d6",
        Color::DarkGray => "#565f89",
        Color::LightRed => "#ff7a93",
        Color::LightGreen => "#b9f27c",
        Color::LightYellow => "#ff9e64",
        Color::LightBlue => "#7da6ff",
        Color::LightMagenta => "#c8a2ff",
        Color::LightCyan => "#b4f9f8",
        Color::White => "#c0caf5",
        Color::Rgb(r, g, b) => return Some(format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Indexed(i) => return Some(indexed(i)),
    };
    Some(named.into())
}

/// The standard xterm 256-colour palette (16 ANSI + 6×6×6 cube + grayscale ramp).
fn indexed(i: u8) -> String {
    const ANSI: [&str; 16] = [
        "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
        "#565f89", "#ff7a93", "#b9f27c", "#ff9e64", "#7da6ff", "#c8a2ff", "#b4f9f8", "#c0caf5",
    ];
    match i {
        0..=15 => ANSI[i as usize].into(),
        16..=231 => {
            let i = i - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let lvl = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            format!("#{:02x}{:02x}{:02x}", lvl(r), lvl(g), lvl(b))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            format!("#{v:02x}{v:02x}{v:02x}")
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// SVG path data for a box-drawing glyph occupying the cell at `(x0, y0)`. Strokes
/// reach the cell edges so neighbours connect seamlessly; corners use a quadratic
/// turn of radius `R` for a smooth round. Returns `None` for non-box glyphs.
fn box_path(ch: &str, x0: f64, y0: f64) -> Option<String> {
    const R: f64 = 4.0; // corner radius
    let (x1, y1) = (x0 + CW, y0 + CH);
    let (cx, cy) = (x0 + CW / 2.0, y0 + CH / 2.0);
    let d = match ch {
        "─" | "━" => format!("M{x0:.1} {cy:.1}L{x1:.1} {cy:.1}"),
        "│" | "┃" => format!("M{cx:.1} {y0:.1}L{cx:.1} {y1:.1}"),
        // Rounded corners: a stub to one edge, a quadratic turn, a stub to the other.
        "╭" => format!(
            "M{x1:.1} {cy:.1}L{:.1} {cy:.1}Q{cx:.1} {cy:.1} {cx:.1} {:.1}L{cx:.1} {y1:.1}",
            cx + R,
            cy + R
        ),
        "╮" => format!(
            "M{x0:.1} {cy:.1}L{:.1} {cy:.1}Q{cx:.1} {cy:.1} {cx:.1} {:.1}L{cx:.1} {y1:.1}",
            cx - R,
            cy + R
        ),
        "╰" => format!(
            "M{x1:.1} {cy:.1}L{:.1} {cy:.1}Q{cx:.1} {cy:.1} {cx:.1} {:.1}L{cx:.1} {y0:.1}",
            cx + R,
            cy - R
        ),
        "╯" => format!(
            "M{x0:.1} {cy:.1}L{:.1} {cy:.1}Q{cx:.1} {cy:.1} {cx:.1} {:.1}L{cx:.1} {y0:.1}",
            cx - R,
            cy - R
        ),
        // Square corners.
        "┌" => format!("M{x1:.1} {cy:.1}L{cx:.1} {cy:.1}L{cx:.1} {y1:.1}"),
        "┐" => format!("M{x0:.1} {cy:.1}L{cx:.1} {cy:.1}L{cx:.1} {y1:.1}"),
        "└" => format!("M{x1:.1} {cy:.1}L{cx:.1} {cy:.1}L{cx:.1} {y0:.1}"),
        "┘" => format!("M{x0:.1} {cy:.1}L{cx:.1} {cy:.1}L{cx:.1} {y0:.1}"),
        // Tees and cross.
        "├" => format!("M{cx:.1} {y0:.1}L{cx:.1} {y1:.1}M{cx:.1} {cy:.1}L{x1:.1} {cy:.1}"),
        "┤" => format!("M{cx:.1} {y0:.1}L{cx:.1} {y1:.1}M{cx:.1} {cy:.1}L{x0:.1} {cy:.1}"),
        "┬" => format!("M{x0:.1} {cy:.1}L{x1:.1} {cy:.1}M{cx:.1} {cy:.1}L{cx:.1} {y1:.1}"),
        "┴" => format!("M{x0:.1} {cy:.1}L{x1:.1} {cy:.1}M{cx:.1} {cy:.1}L{cx:.1} {y0:.1}"),
        "┼" => format!("M{x0:.1} {cy:.1}L{x1:.1} {cy:.1}M{cx:.1} {y0:.1}L{cx:.1} {y1:.1}"),
        _ => return None,
    };
    Some(d)
}

/// Serialise a rendered cell buffer to an SVG string.
fn buffer_to_svg(buf: &Buffer) -> String {
    let (w, h) = (buf.area.width, buf.area.height);
    let (cwpx, chpx) = (CW * w as f64, CH * h as f64);
    // Pad the terminal content and float it as a rounded panel on a backdrop, so
    // the PNG reads like a windowed capture rather than edge-to-edge text.
    const PAD: f64 = 20.0;
    let (pw, ph) = (cwpx + 2.0 * PAD, chpx + 2.0 * PAD);
    let mut s = String::new();
    let _ = writeln!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{pw:.0}\" height=\"{ph:.0}\" \
         viewBox=\"0 0 {pw:.0} {ph:.0}\">\n\
         <rect width=\"{pw:.0}\" height=\"{ph:.0}\" fill=\"#12131c\"/>\n\
         <rect x=\"{:.0}\" y=\"{:.0}\" width=\"{:.0}\" height=\"{:.0}\" rx=\"10\" fill=\"{BG}\"/>\n\
         <g transform=\"translate({PAD:.0} {PAD:.0})\">",
        PAD - 8.0,
        PAD - 8.0,
        cwpx + 16.0,
        chpx + 16.0,
    );
    // Background-rect layer (drawn first, under the glyphs).
    for y in 0..h {
        for x in 0..w {
            let Some(cell) = buf.cell((x, y)) else {
                continue;
            };
            if let Some(bg) = hex(cell.bg, false) {
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
    }
    // Glyph layer.
    for y in 0..h {
        for x in 0..w {
            let Some(cell) = buf.cell((x, y)) else {
                continue;
            };
            let sym = cell.symbol();
            if sym == " " || sym.is_empty() {
                continue;
            }
            let fg = hex(cell.fg, true).unwrap_or_else(|| FG.into());
            let m = cell.modifier;
            // Box-drawing glyphs render as stroked paths spanning the full cell, so
            // adjacent border cells join into smooth continuous lines with rounded
            // corners — centred text glyphs would leave gaps at every cell seam.
            if let Some(d) = box_path(sym, x as f64 * CW, y as f64 * CH) {
                let opacity = if m.contains(Modifier::DIM) { 0.55 } else { 1.0 };
                let _ = writeln!(
                    s,
                    "<path d=\"{d}\" fill=\"none\" stroke=\"{fg}\" stroke-width=\"1.4\" \
                     stroke-linecap=\"round\" stroke-linejoin=\"round\" opacity=\"{opacity}\"/>",
                );
                continue;
            }
            let bold = if m.contains(Modifier::BOLD) {
                " font-weight=\"bold\""
            } else {
                ""
            };
            let italic = if m.contains(Modifier::ITALIC) {
                " font-style=\"italic\""
            } else {
                ""
            };
            let dim = if m.contains(Modifier::DIM) {
                " opacity=\"0.55\""
            } else {
                ""
            };
            let tx = x as f64 * CW + CW / 2.0;
            // Baseline centred in the (now taller) cell: vertical middle + half the
            // cap height, so glyphs sit mid-row rather than hugging the top.
            let ty = y as f64 * CH + (CH + FS * 0.7) / 2.0;
            let _ = writeln!(
                s,
                "<text x=\"{tx:.1}\" y=\"{ty:.1}\" font-family=\"DejaVu Sans Mono, monospace\" \
                 font-size=\"{FS}\" fill=\"{fg}\" text-anchor=\"middle\"{bold}{italic}{dim}>{}</text>",
                xml_escape(sym),
            );
        }
    }
    s.push_str("</g></svg>\n");
    s
}

/// Render `app` at `w`×`h` cells and write `OUT_DIR/<name>.svg`.
fn shoot(name: &str, w: u16, h: u16, app: &mut App) {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("test backend");
    term.draw(|f| ui::draw(f, app)).expect("draw");
    let svg = buffer_to_svg(term.backend().buffer());
    std::fs::create_dir_all(OUT_DIR).expect("mkdir");
    std::fs::write(format!("{OUT_DIR}/{name}.svg"), svg).expect("write svg");
}

fn status() -> Status {
    Status::new(
        "openai",
        "claude-opus-4-8",
        "/home/you/projects/buck/design.kicad_sch",
        true,
    )
}

fn push(app: &mut App, speaker: Speaker, text: &str, level: NoticeLevel) {
    app.transcript.push(Entry {
        speaker,
        text: text.into(),
        level,
    });
}

/// Drive a finished tool card through the real agent-event path (start → finish)
/// so the screenshot exercises the same renderer code as production — the card's
/// single marker comes from the renderer, never the event text.
fn tool(app: &mut App, name: &str, summary: &str) {
    app.update(Msg::Agent(AgentEvent::ToolStarted { name: name.into() }));
    app.update(Msg::Agent(AgentEvent::ToolFinished {
        name: name.into(),
        summary: summary.into(),
        image_path: None,
    }));
}

/// A representative conversation, shared by several states. Built through the
/// same `update`/agent-event path the running app uses, so the captures can't
/// drift from real rendering (e.g. mask a double marker).
fn seed_conversation(app: &mut App) {
    push(
        app,
        Speaker::User,
        "design a 5V 3A buck converter from 12V in",
        NoticeLevel::Plain,
    );
    app.update(Msg::Agent(AgentEvent::AssistantText(
        "I'll build a synchronous buck around a TPS54331. Let me search for parts and lay out \
         the power stage with input/output bulk caps and a feedback divider."
            .into(),
    )));
    tool(app, "search_symbols", "\"buck converter\" → 12 hits");
    tool(app, "create_design", "8 parts → ok");
    tool(app, "run_erc", "0 errors, 2 warnings");
    // Drive real usage so the footer's TOKEN / COST / CONTEXT / cached HUD shows:
    // a cold first call (cache write) then a warm one (cache read) — the cached
    // prefix is exactly the prompt-caching win the HUD is meant to surface.
    app.update(Msg::Agent(AgentEvent::Usage {
        input_tokens: 8_200,
        output_tokens: 900,
        cache_write_tokens: 6_400,
        cache_read_tokens: 0,
    }));
    app.update(Msg::Agent(AgentEvent::Usage {
        input_tokens: 9_100,
        output_tokens: 1_400,
        cache_write_tokens: 0,
        cache_read_tokens: 6_400,
    }));
    app.update(Msg::Agent(AgentEvent::AssistantText(
        "Done — the buck converter schematic compiles cleanly with **0 ERC errors**. The power \
         stage, feedback network, and decoupling are all in place. Want me to lay out the PCB next?"
            .into(),
    )));
    push(
        app,
        Speaker::System,
        "turn finished · 6 tool calls · 12.4k tokens",
        NoticeLevel::Success,
    );
}

#[test]
fn tui_screenshots() {
    // 1. Idle chat with a completed turn.
    let mut app = App::new(status());
    seed_conversation(&mut app);
    app.input = "now route the board".into();
    app.cursor = app.input.chars().count();
    shoot("01_chat", 96, 32, &mut app);

    // 2. The apply-gate: a change awaiting approval.
    let mut app = App::new(status());
    seed_conversation(&mut app);
    app.pending = Some(PendingDiff {
        added: vec!["C7 (100nF)".into(), "R5 (10k)".into()],
        removed: vec![],
        changed: vec!["U1 footprint".into()],
        nets_before: 14,
        nets_after: 15,
    });
    shoot("02_apply_gate", 96, 32, &mut app);

    // 3. A turn in flight (running indicator + spinner) with the live tool-detail
    //    row naming the call now executing.
    let mut app = App::new(status());
    seed_conversation(&mut app);
    app.running = true;
    app.spinner = 3;
    app.turn_tool_calls = 4;
    app.active_tool = Some("route_board".into());
    shoot("03_running", 96, 32, &mut app);

    // 4. Empty / first-launch state.
    let mut app = App::new(status());
    shoot("04_empty", 96, 32, &mut app);

    // 5. The `/`-command completion popup, floated above the composer.
    let mut app = App::new(status());
    seed_conversation(&mut app);
    for c in "/c".chars() {
        app.update(Msg::Char(c));
    }
    shoot("05_completions", 96, 32, &mut app);

    // 6. The unwind picker (double-Esc), listing recent prompts.
    let mut app = App::new(status());
    seed_conversation(&mut app);
    app.open_unwind(vec![
        "design a 5V 3A buck converter from 12V in".into(),
        "use a TPS54331 and add a soft-start cap".into(),
        "bump the output cap to 47uF".into(),
    ]);
    shoot("06_unwind", 96, 32, &mut app);

    // 7. The :help overlay.
    let mut app = App::new(status());
    seed_conversation(&mut app);
    app.help = true;
    shoot("07_help", 96, 32, &mut app);

    // 8. Markdown + notice styling: accented headings, a fenced code block (slate
    //    band, gutter rule, language label), inline code, and the three notice
    //    levels as callouts.
    let mut app = App::new(status());
    push(
        &mut app,
        Speaker::User,
        "show me the feedback divider values",
        NoticeLevel::Plain,
    );
    app.update(Msg::Agent(AgentEvent::AssistantText(
        "## Feedback network\n\
         The divider sets the output to **5.0 V** with `Vref = 0.8 V`:\n\n\
         ```python\n\
         r2 = r1 * (vout / vref - 1)\n\
         # 10k * (5.0/0.8 - 1) = 52.5k\n\
         ```\n\n\
         I picked the nearest E96 value, `52.3k`."
            .into(),
    )));
    tool(&mut app, "run_erc", "0 errors, 1 warning");
    push(
        &mut app,
        Speaker::System,
        "applied — ERC 0 errors, 1 warning",
        NoticeLevel::Success,
    );
    push(
        &mut app,
        Speaker::System,
        "Hit the per-turn step limit after 41s · 8 tool calls — send \"continue\" to resume",
        NoticeLevel::Warn,
    );
    push(
        &mut app,
        Speaker::System,
        "Stopped after 12s — provider throttled the request (429)",
        NoticeLevel::Error,
    );
    shoot("08_markdown", 96, 32, &mut app);

    // 9. A turn mid-stream: assistant prose still arriving token-by-token (a
    //    live entry with its trailing cursor), the running indicator below.
    let mut app = App::new(status());
    push(
        &mut app,
        Speaker::User,
        "design a 3.3V LDO with input and output caps",
        NoticeLevel::Plain,
    );
    tool(&mut app, "search_symbols", "\"LDO 3.3V\" → 9 hits");
    for chunk in [
        "I'll use an **AMS1117-3.3** with a 10µF input cap and a 22µF output ",
        "cap for stability. Adding a power-on LED on the output and",
    ] {
        app.update(Msg::Agent(AgentEvent::AssistantDelta(chunk.into())));
    }
    app.running = true;
    app.spinner = 2;
    app.turn_tool_calls = 1;
    shoot("09_streaming", 96, 32, &mut app);

    // 10. An inline board render preview (the lead's ask). Under the screenshot
    //     harness there is no image picker, so the cell renders its stable
    //     text-label placeholder; the live terminal shows real graphics here.
    let mut app = App::new(status());
    push(
        &mut app,
        Speaker::User,
        "route the board and show me the result",
        NoticeLevel::Plain,
    );
    app.update(Msg::Agent(AgentEvent::ToolStarted {
        name: "route_board".into(),
    }));
    app.update(Msg::Agent(AgentEvent::ToolFinished {
        name: "route_board".into(),
        summary: "2-layer · 0 failed nets".into(),
        image_path: None,
    }));
    app.update(Msg::Agent(AgentEvent::ToolStarted {
        name: "render_board".into(),
    }));
    app.update(Msg::Agent(AgentEvent::ToolFinished {
        name: "render_board".into(),
        summary: "routed view → ok".into(),
        image_path: Some("/home/you/projects/buck/.gordian/renders/003.png".into()),
    }));
    app.update(Msg::Agent(AgentEvent::AssistantText(
        "Routed cleanly on two layers — preview above.".into(),
    )));
    shoot("10_preview", 96, 32, &mut app);
}
