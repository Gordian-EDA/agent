//! Pure rendering: draw an [`App`] onto a ratatui [`Frame`]. No mutation, no I/O.
//!
//! Layout (spec §11):
//!
//! ```text
//! ┌─ auto-pcb ── design.kicad_sch ──────── [KiCAD ●] [auto OFF] ─┐
//! │ chat transcript (scrollable; tool cards collapsed)           │
//! ├──────────────────────────────────────────────────────────────┤
//! │ ◆ PROPOSED CHANGES  +U1 +R7  ~C2   nets 3→12   [a]pprove [r]…  │  (only when a diff is pending)
//! ├──────────────────────────────────────────────────────────────┤
//! │ > input…                                                      │
//! ├──────────────────────────────────────────────────────────────┤
//! │ bedrock · opus · turns 2 · applied 1 ········· :help :undo    │
//! └──────────────────────────────────────────────────────────────┘
//! ```

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::app::{App, Speaker};

/// Draw the whole cockpit.
pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    // Reserve a diff pane only when something is pending.
    let diff_h: u16 = if app.pending.is_some() { 4 } else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // header / title
            Constraint::Min(3),         // transcript
            Constraint::Length(diff_h), // proposed-changes pane
            Constraint::Length(3),      // input line
            Constraint::Length(1),      // status bar
        ])
        .split(area);

    draw_header(f, chunks[0], app);
    draw_transcript(f, chunks[1], app);
    if app.pending.is_some() {
        draw_diff(f, chunks[2], app);
    }
    draw_input(f, chunks[3], app);
    draw_status(f, chunks[4], app);

    if app.help {
        draw_help(f, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let dot = if app.status.kicad_connected {
        "●"
    } else {
        "○"
    };
    let auto = if app.auto { "auto ON" } else { "auto OFF" };
    let title = Line::from(vec![
        Span::styled(" auto-pcb ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("── "),
        Span::styled(
            file_name(&app.status.sch_path),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[KiCAD {dot}]"),
            Style::default().fg(if app.status.kicad_connected {
                Color::Green
            } else {
                Color::Red
            }),
        ),
        Span::raw(" "),
        Span::styled(
            format!("[{auto}]"),
            Style::default().fg(if app.auto {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ),
    ]);
    f.render_widget(Paragraph::new(title), area);
}

fn draw_transcript(f: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = app.transcript.iter().map(render_entry).collect();

    let inner_h = area.height.saturating_sub(2); // borders
    let total = lines.len() as u16;
    // scroll == 0 follows the tail; larger scrolls back into history.
    let max_top = total.saturating_sub(inner_h);
    let top = max_top.saturating_sub(app.scroll.min(max_top));

    let title = if app.running {
        " transcript · agent working… "
    } else {
        " transcript "
    };
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((top, 0));
    f.render_widget(para, area);
}

/// Style one transcript entry into a `Line`.
fn render_entry(e: &super::app::Entry) -> Line<'static> {
    match e.speaker {
        Speaker::User => Line::from(vec![
            Span::styled(
                "you  ",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(e.text.clone()),
        ]),
        Speaker::Assistant => Line::from(vec![
            Span::styled(
                "ai   ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(e.text.clone()),
        ]),
        Speaker::Tool => Line::from(Span::styled(
            format!("     {}", e.text),
            Style::default().fg(Color::Magenta),
        )),
        Speaker::System => Line::from(Span::styled(
            format!("     {}", e.text),
            Style::default().fg(Color::DarkGray),
        )),
    }
}

fn draw_diff(f: &mut Frame, area: Rect, app: &App) {
    let Some(d) = app.pending.as_ref() else {
        return;
    };

    let mut spans: Vec<Span> = vec![Span::styled(
        "◆ PROPOSED CHANGES  ",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )];
    for r in &d.added {
        spans.push(Span::styled(
            format!("+{r} "),
            Style::default().fg(Color::Green),
        ));
    }
    for r in &d.removed {
        spans.push(Span::styled(
            format!("-{r} "),
            Style::default().fg(Color::Red),
        ));
    }
    for r in &d.changed {
        spans.push(Span::styled(
            format!("~{r} "),
            Style::default().fg(Color::Cyan),
        ));
    }
    spans.push(Span::raw(format!(
        " nets {}→{} ",
        d.nets_before, d.nets_after
    )));

    let hint = Line::from(vec![
        Span::styled(
            "  [a]pprove",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            "[r]eject",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    ]);

    let para = Paragraph::new(vec![Line::from(spans), hint])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" apply-gate "),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(para, area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let (prompt, style) = if app.pending.is_some() {
        (
            "approve or reject the change above ([a]/[r])",
            Style::default().fg(Color::Yellow),
        )
    } else if app.running {
        (
            "agent is working — Esc to cancel",
            Style::default().fg(Color::DarkGray),
        )
    } else {
        ("", Style::default())
    };

    let line = if app.input_active() {
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Blue)),
            Span::raw(app.input.clone()),
            Span::styled("▏", Style::default().fg(Color::Blue)),
        ])
    } else {
        Line::from(Span::styled(prompt, style))
    };

    let para = Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(" input "));
    f.render_widget(para, area);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let s = &app.status;
    let left = format!(
        " {} · {} · turns {} · applied {}",
        s.provider,
        short_model(&s.model),
        s.turn_count,
        s.applied_count,
    );
    let right = ":help  :undo  :auto  :quit ";
    // Pad the middle so the right hint sits at the edge.
    let width = area.width as usize;
    let pad = width.saturating_sub(left.len() + right.len());
    let text = format!("{left}{}{right}", " ".repeat(pad));
    let para = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().bg(Color::DarkGray).fg(Color::White),
    )));
    f.render_widget(para, area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = 56u16.min(area.width.saturating_sub(4));
    let h = 12u16.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let lines = vec![
        Line::from(Span::styled(
            "auto-pcb copilot — help",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Type a prompt, Enter to send."),
        Line::from("a / r        approve / reject a proposed change"),
        Line::from(":auto        toggle auto-approve (yolo)"),
        Line::from(":undo        restore the previous schematic"),
        Line::from(":help        toggle this help"),
        Line::from(":quit / Esc  exit"),
        Line::from("PgUp/PgDn    scroll the transcript"),
        Line::from(""),
        Line::from(Span::styled(
            "Esc to close.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" help "))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

/// The trailing file name of a path string (for the header).
fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// A short model label for the status bar: the last dotted segment, trimmed.
fn short_model(model: &str) -> String {
    // e.g. "us.anthropic.claude-opus-4-5-20251101-v1:0" -> "claude-opus-4-5"
    let tail = model.rsplit('.').next().unwrap_or(model);
    let trimmed: String = tail.split('-').take(3).collect::<Vec<_>>().join("-");
    if trimmed.is_empty() {
        model.to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Msg, Status};
    use agent::AgentEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    /// Render an app to a TestBackend and return the buffer's text as one string.
    fn render_to_string(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        buffer_text(&buf)
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn app() -> App {
        App::new(Status::new(
            "bedrock",
            "us.anthropic.claude-opus-4-5-20251101-v1:0",
            "/tmp/proj/design.kicad_sch",
            true,
        ))
    }

    #[test]
    fn transcript_renders_user_message_and_tool_card() {
        let mut a = app();
        for c in "design a board".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::ToolStarted {
            name: "search_symbols".into(),
        }));
        a.update(Msg::Agent(AgentEvent::ToolFinished {
            name: "search_symbols".into(),
            summary: "\"STM32\" → 4 hits".into(),
        }));

        let text = render_to_string(&a, 80, 24);
        assert!(
            text.contains("design a board"),
            "user msg should render:\n{text}"
        );
        assert!(
            text.contains("search_symbols") && text.contains("4 hits"),
            "tool card should render:\n{text}"
        );
    }

    #[test]
    fn pending_diff_shows_approve_and_reject() {
        let mut a = app();
        a.update(Msg::PendingDiff(json!({
            "ok": true,
            "would_write": true,
            "diff": { "added": ["U1"], "removed": [], "changed": [], "nets_before": 0, "nets_after": 5 }
        })));
        let text = render_to_string(&a, 80, 24);
        assert!(text.contains("PROPOSED CHANGES"), "diff header:\n{text}");
        assert!(text.contains("+U1"), "added refdes:\n{text}");
        assert!(text.contains("approve"), "approve hint:\n{text}");
        assert!(text.contains("reject"), "reject hint:\n{text}");
    }

    #[test]
    fn status_bar_shows_the_model_name() {
        let a = app();
        let text = render_to_string(&a, 80, 24);
        assert!(text.contains("bedrock"), "provider in status bar:\n{text}");
        assert!(text.contains("claude-opus"), "model in status bar:\n{text}");
    }

    #[test]
    fn header_shows_schematic_and_kicad_state() {
        let a = app();
        let text = render_to_string(&a, 80, 24);
        assert!(text.contains("design.kicad_sch"), "sch path:\n{text}");
        assert!(text.contains("KiCAD"), "kicad indicator:\n{text}");
    }

    #[test]
    fn help_overlay_renders_when_toggled() {
        let mut a = app();
        a.help = true;
        let text = render_to_string(&a, 80, 24);
        assert!(text.contains("help"), "help overlay:\n{text}");
        assert!(text.contains(":undo"), "help lists commands:\n{text}");
    }
}
