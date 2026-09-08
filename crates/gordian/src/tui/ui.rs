//! Drawing an [`App`] onto a ratatui frame. No I/O; the only state written back
//! is layout-derived — the clamped scroll offset and the viewport height, which
//! only the renderer knows.
//!
//! ```text
//! │ transcript: prompts, tool rows, inline renders (scrollable)  │
//! │ ⠙ Working 42s · build                                        │
//! │ › add a boot header                                          │
//! │ openai · claude-opus-4-5 · ~/proj · edit · $0.04 · review 8.0 │
//! ```

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::app::{App, Entry, format_duration};
use super::theme;

/// Symmetric horizontal margin, so every pane shares one left edge.
const MARGIN: u16 = 3;

/// Rows the composer may grow to for a multi-line draft.
const COMPOSER_MAX: u16 = 8;

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Draw the whole cockpit.
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(Block::default().style(theme::PAGE), area);

    let working = u16::from(app.running);
    let composer = composer_height(app, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(working),
            Constraint::Length(composer),
            Constraint::Length(1),
        ])
        .split(area);

    draw_transcript(f, chunks[0], app);
    if app.running {
        draw_working(f, chunks[1], app);
    }
    draw_composer(f, chunks[2], app);
    draw_status(f, chunks[3], app);
    if app.help {
        draw_help(f, area);
    }
}

/// Inset a pane by [`MARGIN`] on both sides.
fn body(area: Rect) -> Rect {
    Rect {
        x: area.x + MARGIN,
        y: area.y,
        width: area.width.saturating_sub(2 * MARGIN),
        height: area.height,
    }
}

// ---- the transcript ----------------------------------------------------

fn draw_transcript(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = body(area);
    app.viewport_h = inner.height;
    if app.transcript.is_empty() {
        app.scroll_max = 0;
        draw_welcome(f, inner, app);
        return;
    }

    let lines = layout_lines(app, inner.width.max(1));
    let total = lines.len() as u16;
    let max_top = total.saturating_sub(inner.height);
    app.scroll_max = max_top;
    app.scroll = app.scroll.min(max_top);
    let top = max_top - app.scroll;
    f.render_widget(Paragraph::new(lines).scroll((top, 0)), inner);
}

/// The whole transcript as wrapped rows, so the scroll arithmetic is exact.
fn layout_lines(app: &mut App, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let w = width as usize;
    for entry in &app.transcript {
        match entry {
            Entry::User(text) => {
                lines.push(Line::from(""));
                lines.extend(wrap(text, theme::USER, w, "› ", "  "));
            }
            Entry::Assistant(text) => {
                lines.push(Line::from(""));
                lines.extend(wrap(text, theme::PROSE, w, "", ""));
            }
            Entry::Tool {
                name,
                args,
                result,
                seconds,
            } => {
                let mut head = vec![
                    Span::styled("  ⏵ ", theme::META),
                    Span::styled(name.clone(), theme::TOOL_NAME),
                ];
                if !args.is_empty() {
                    head.push(Span::styled(
                        format!(" {}", clip(args, w.saturating_sub(name.len() + 6))),
                        theme::META,
                    ));
                }
                lines.push(Line::from(head));
                if let Some(result) = result {
                    let detail = format!("{result}  ({seconds:.1}s)");
                    lines.extend(wrap(&detail, theme::SUBTLE, w, "    ↳ ", "      "));
                }
            }
            Entry::Render { label, path } => {
                lines.push(Line::from(Span::styled(
                    clip(&format!("  ▸ {label} — {}", path.display()), w),
                    theme::LINK,
                )));
                let rows = app.images.rows(path, width.saturating_sub(2).max(1));
                lines.extend(rows.iter().map(|row| {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(row.spans.iter().cloned());
                    Line::from(spans)
                }));
            }
            Entry::Review {
                score,
                mean,
                defects,
            } => lines.push(Line::from(vec![
                Span::styled("  ★ ", theme::META),
                Span::styled(
                    format!("{score:.0}/10"),
                    match *mean >= 8.0 {
                        true => theme::TOOL_NAME,
                        false => theme::WARNING,
                    },
                ),
                Span::styled(
                    format!(" mean {mean:.2} · {defects} defect(s)"),
                    theme::SUBTLE,
                ),
            ])),
            Entry::Note(text) => lines.extend(wrap(text, theme::SUBTLE, w, "  · ", "    ")),
            Entry::Error(text) => lines.extend(wrap(text, theme::DANGER, w, "  ! ", "    ")),
            Entry::Divider(text) => {
                lines.push(Line::from(""));
                lines.push(rule(text, w));
                lines.push(Line::from(""));
            }
        }
    }
    lines
}

/// `──── text ────────` at `width` columns.
fn rule(text: &str, width: usize) -> Line<'static> {
    let label = format!(" {text} ");
    let fill = width.saturating_sub(label.chars().count() + 4);
    Line::from(vec![
        Span::styled("────", theme::RULE),
        Span::styled(label, theme::META),
        Span::styled("─".repeat(fill), theme::RULE),
    ])
}

/// Word-wrap `text` to `width`, with `first` on the opening row and `rest` on
/// every continuation.
fn wrap(
    text: &str,
    style: Style,
    width: usize,
    first: &str,
    rest: &str,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        let mut prefix = if out.is_empty() { first } else { rest };
        let room = |p: &str| width.saturating_sub(p.chars().count()).max(8);
        for word in paragraph.split_whitespace() {
            let candidate = match current.is_empty() {
                true => word.chars().count(),
                false => current.chars().count() + 1 + word.chars().count(),
            };
            if candidate > room(prefix) && !current.is_empty() {
                out.push(row(prefix, &current, style));
                prefix = rest;
                current.clear();
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        out.push(row(prefix, &current, style));
    }
    out
}

fn row(prefix: &str, text: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(prefix.to_string(), theme::META),
        Span::styled(text.to_string(), style),
    ])
}

fn clip(text: &str, width: usize) -> String {
    match text.chars().count() > width {
        true => text.chars().take(width.saturating_sub(1)).collect::<String>() + "…",
        false => text.to_string(),
    }
}

/// The splash that fills the pane until the first prompt.
fn draw_welcome(f: &mut Frame, area: Rect, app: &App) {
    let mode = match app.status.edit_mode {
        true => "editing the sheet already in this project",
        false => "no sheet yet — the first prompt designs one",
    };
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled("GORDIAN", theme::LOGO)),
        Line::from(Span::styled(
            "schematics and boards, designed by conversation",
            theme::SUBTLE,
        )),
        Line::from(""),
        Line::from(Span::styled(
            clip(
                &format!("{}  ({mode})", app.status.project),
                area.width as usize,
            ),
            theme::META,
        )),
        Line::from(""),
        Line::from(Span::styled("Try:", theme::HEADING)),
        Line::from(Span::styled(
            "  design an STM32 Blue Pill with USB-C, a 3V3 LDO and boot headers",
            theme::PROSE,
        )),
        Line::from(Span::styled(
            "  a 555 blinker on a 5V rail, schematic only",
            theme::PROSE,
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Enter runs · ⇧Enter newline · Esc stops · Ctrl-C quits · /help",
            theme::META,
        )),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

// ---- the working row, composer and status bar --------------------------

fn draw_working(f: &mut Frame, area: Rect, app: &App) {
    let elapsed = format_duration(app.elapsed().unwrap_or(0));
    let mut spans = vec![
        Span::styled(
            format!("{} ", SPINNER[app.spinner % SPINNER.len()]),
            theme::SPINNER,
        ),
        Span::styled(format!("Working {elapsed}"), theme::PROSE),
        Span::styled(
            format!(" · {} tool call(s)", app.turn_tools),
            theme::SUBTLE,
        ),
    ];
    if let Some(work) = &app.active_work {
        spans.push(Span::styled(
            format!(" · {}", clip(work, area.width as usize / 2)),
            theme::META,
        ));
    }
    if !app.queued.is_empty() {
        spans.push(Span::styled(
            format!(" · {} queued", app.queued.len()),
            theme::WARNING,
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), body(area));
}

/// Rows the composer needs: its rule, plus the wrapped draft, capped.
fn composer_height(app: &App, area: Rect) -> u16 {
    let width = body(area).width.max(1) as usize;
    let rows = app
        .input
        .split('\n')
        .map(|line| (line.chars().count() + 2).div_ceil(width).max(1) as u16)
        .sum::<u16>();
    1 + rows.clamp(1, COMPOSER_MAX)
}

fn draw_composer(f: &mut Frame, area: Rect, app: &App) {
    f.render_widget(Block::default().style(theme::BAND), area);
    let inner = body(area);
    let rule_style = match app.running {
        true => theme::RULE_FOCUS,
        false => theme::RULE,
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            rule_style,
        ))),
        Rect {
            height: 1,
            ..inner
        },
    );
    let field = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if app.input.is_empty() {
        let hint = match app.running {
            true => "type the next instruction — it runs when this one lands",
            false => "describe the circuit, or the change you want",
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("› ", theme::CARET),
                Span::styled(hint, theme::META),
            ])),
            field,
        );
        f.set_cursor_position(Position::new(field.x + 2, field.y));
        return;
    }
    let lines: Vec<Line> = app
        .input
        .split('\n')
        .enumerate()
        .map(|(i, line)| {
            Line::from(vec![
                Span::styled(if i == 0 { "› " } else { "  " }, theme::CARET),
                Span::styled(line.to_string(), theme::PROSE),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), field);
    let (row, col) = caret(&app.input, app.cursor);
    f.set_cursor_position(Position::new(
        field.x + 2 + col.min(field.width.saturating_sub(3)),
        field.y + row.min(field.height.saturating_sub(1)),
    ));
}

/// The caret's (row, column) within the composer for a char offset.
fn caret(input: &str, cursor: usize) -> (u16, u16) {
    let before: String = input.chars().take(cursor).collect();
    let row = before.matches('\n').count() as u16;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() as u16;
    (row, col)
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let s = &app.status;
    let ledger = &s.ledger;
    let cost = ledger
        .cost(&s.model)
        .map(|c| format!("${c:.2}"))
        .unwrap_or_else(|| "—".to_string());
    let mut parts = vec![
        s.provider.clone(),
        s.model.clone(),
        s.project.clone(),
        match s.edit_mode {
            true => "edit".to_string(),
            false => "new".to_string(),
        },
        format!("turns {}", s.turns),
        format!(
            "{} req · {} in / {} out · {cost}",
            ledger.requests,
            tokens(ledger.input_tokens()),
            tokens(ledger.output)
        ),
    ];
    if let Some(review) = s.review {
        parts.push(format!("review {review:.1}"));
    }
    if !s.kicad {
        parts.push("KiCad missing".to_string());
    }
    let text = clip(&parts.join(" · "), body(area).width as usize);
    let style = match s.kicad {
        true => theme::META,
        false => theme::WARNING,
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(text, style))),
        body(area),
    );
}

/// Compact token count: `950`, `23.4k`, `1.2M`.
fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rows: Vec<(&str, &str)> = vec![
        ("Enter", "run the prompt (queued while a run works)"),
        ("⇧/⌥ Enter", "newline"),
        ("Esc", "stop the run in flight"),
        ("Ctrl-C", "quit (twice while running)"),
        ("↑ / ↓", "recall earlier prompts"),
        ("⇧/Ctrl ↑↓, PgUp/PgDn, wheel", "scroll the transcript"),
        ("Ctrl-End", "jump to the live tail"),
        ("/clear", "empty the transcript"),
        ("/shot", "write an SVG of this frame into the project"),
        ("/quit", "leave"),
    ];
    let width = 62.min(area.width.saturating_sub(4));
    let height = (rows.len() as u16 + 4).min(area.height);
    let panel = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Block::default().style(theme::OVERLAY), panel);
    let mut lines = vec![
        Line::from(Span::styled(" Keys", theme::HEADING)),
        Line::from(""),
    ];
    lines.extend(rows.into_iter().map(|(key, what)| {
        Line::from(vec![
            Span::styled(format!(" {key:<28}"), theme::TOOL_NAME),
            Span::styled(what.to_string(), theme::PROSE),
        ])
    }));
    lines.push(Line::from(Span::styled(" Esc closes", theme::META)));
    f.render_widget(Paragraph::new(lines), panel);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Msg, Status, TurnEnd};
    use gordian_core::AgentEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn app() -> App {
        App::new(Status::new(
            "openai",
            "claude-opus-4-5",
            PathBuf::from("/tmp/proj"),
        ))
    }

    fn text_of(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_empty_cockpit_shows_the_splash_and_the_project() {
        let text = text_of(&mut app(), 90, 24);
        assert!(text.contains("GORDIAN"), "{text}");
        assert!(text.contains("/tmp/proj"), "{text}");
    }

    /// A whole scripted run draws: the prompt, the tool rows with their results,
    /// the review, and the closing rule.
    #[test]
    fn a_scripted_run_renders_every_kind_of_row() {
        let mut a = app();
        for c in "design a 555 blinker".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::Assistant(
            "Searching for the timer symbol.".into(),
        )));
        a.update(Msg::Agent(AgentEvent::ToolCall {
            name: "search_symbols".into(),
            args: "{\"query\":\"NE555\"}".into(),
        }));
        a.update(Msg::Agent(AgentEvent::ToolResult {
            name: "search_symbols".into(),
            seconds: 0.2,
            summary: "Timer:NE555".into(),
        }));
        a.update(Msg::Agent(AgentEvent::Review {
            score: 8.0,
            mean: 7.6,
            samples: vec![8.0],
            defects: 2,
        }));
        a.update(Msg::TurnEnded(TurnEnd::Completed("erc clean".into())));

        let text = text_of(&mut a, 90, 30);
        assert!(text.contains("design a 555 blinker"), "{text}");
        assert!(text.contains("search_symbols"), "{text}");
        assert!(text.contains("Timer:NE555"), "{text}");
        assert!(text.contains("8/10"), "{text}");
        assert!(text.contains("Worked for"), "{text}");
        assert!(text.contains("erc clean"), "{text}");
    }

    #[test]
    fn the_working_row_shows_while_a_run_is_in_flight() {
        let mut a = app();
        for c in "go".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::Note("board: routing".into())));
        let text = text_of(&mut a, 90, 20);
        assert!(text.contains("Working"), "{text}");
        assert!(text.contains("board: routing"), "{text}");
    }

    #[test]
    fn help_lists_the_keys_over_the_transcript() {
        let mut a = app();
        a.help = true;
        let text = text_of(&mut a, 90, 24);
        assert!(text.contains("Keys"), "{text}");
        assert!(text.contains("recall earlier prompts"), "{text}");
    }

    /// Long prose wraps into the pane instead of running off it.
    #[test]
    fn prose_wraps_to_the_pane_width() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Assistant("word ".repeat(60))));
        let text = text_of(&mut a, 40, 24);
        assert!(text.lines().all(|l| l.chars().count() == 40));
        assert!(text.matches("word").count() > 20, "{text}");
    }
}
