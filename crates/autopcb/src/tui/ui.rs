//! Rendering: draw an [`App`] onto a ratatui [`Frame`]. No I/O; the single
//! mutation is clamping `app.scroll` to the viewport (only the renderer knows
//! the wrapped line count).
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
//!
//! The transcript is wrapped by [`wrap_text`] (not `Paragraph::wrap`) so the
//! scroll arithmetic — tail-following, clamping, the `↑n` indicator — is exact
//! in visual rows.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::app::{App, Entry, PendingDiff, Speaker};

/// Braille spinner shown in the transcript title while a turn runs.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Width of the speaker gutter (`"you  "` / `"ai   "` / indent).
const GUTTER: usize = 5;

/// Draw the whole cockpit.
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Size the diff pane to its content (0 when nothing is pending).
    let diff_h = app
        .pending
        .as_ref()
        .map(|d| diff_height(d, area.width))
        .unwrap_or(0);

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

fn draw_transcript(f: &mut Frame, area: Rect, app: &mut App) {
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let lines: Vec<Line> = app
        .transcript
        .iter()
        .flat_map(|e| render_entry(e, inner_w))
        .collect();

    let inner_h = area.height.saturating_sub(2); // borders
    let total = lines.len() as u16;
    // scroll == 0 follows the tail; larger scrolls back into history. Clamp it
    // so over-scrolling never leaves the viewport stuck above the content.
    let max_top = total.saturating_sub(inner_h);
    app.scroll = app.scroll.min(max_top);
    let top = max_top - app.scroll;

    let mut title = String::from(" transcript ");
    if app.running {
        let frame = SPINNER[app.spinner % SPINNER.len()];
        let secs = app.turn_elapsed_secs().unwrap_or(0);
        title = format!(" transcript · {frame} agent working {secs}s ");
    }
    if app.scroll > 0 {
        title.push_str(&format!("· ↑{} ", app.scroll));
    }

    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((top, 0));
    f.render_widget(para, area);
}

/// Style one transcript entry into wrapped `Line`s: the first row carries the
/// speaker gutter, continuation rows are indented under it.
fn render_entry(e: &Entry, width: usize) -> Vec<Line<'static>> {
    let (gutter, gutter_style, body_style) = match e.speaker {
        Speaker::User => (
            "you  ",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            Style::default(),
        ),
        Speaker::Assistant => (
            "ai   ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Style::default(),
        ),
        Speaker::Tool => (
            "     ",
            Style::default(),
            Style::default().fg(Color::Magenta),
        ),
        Speaker::System => (
            "     ",
            Style::default(),
            Style::default().fg(Color::DarkGray),
        ),
    };
    let body_w = width.saturating_sub(GUTTER).max(1);
    wrap_text(&e.text, body_w)
        .into_iter()
        .enumerate()
        .map(|(i, piece)| {
            let lead = if i == 0 {
                Span::styled(gutter, gutter_style)
            } else {
                Span::raw(" ".repeat(GUTTER))
            };
            Line::from(vec![lead, Span::styled(piece, body_style)])
        })
        .collect()
}

/// Greedy word-wrap by char count. Respects embedded newlines; words longer
/// than `width` are hard-broken. Always returns at least one (possibly empty)
/// piece so every entry occupies a row.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        let mut current = String::new();
        let mut current_len = 0usize;
        for word in raw_line.split(' ') {
            let mut word = word;
            // Hard-break words that can never fit on one row.
            loop {
                let wlen = word.chars().count();
                let sep = if current_len == 0 { 0 } else { 1 };
                if current_len + sep + wlen <= width {
                    if sep == 1 {
                        current.push(' ');
                        current_len += 1;
                    }
                    current.push_str(word);
                    current_len += wlen;
                    break;
                }
                if wlen > width {
                    // Fill the rest of this row with the word's head.
                    let take = width - current_len - if current_len == 0 { 0 } else { 1 };
                    if take == 0 {
                        out.push(std::mem::take(&mut current));
                        current_len = 0;
                        continue;
                    }
                    if current_len > 0 {
                        current.push(' ');
                    }
                    let split = word
                        .char_indices()
                        .nth(take)
                        .map(|(i, _)| i)
                        .unwrap_or(word.len());
                    current.push_str(&word[..split]);
                    out.push(std::mem::take(&mut current));
                    current_len = 0;
                    word = &word[split..];
                    continue;
                }
                // The word fits on a fresh row.
                out.push(std::mem::take(&mut current));
                current_len = 0;
            }
        }
        out.push(current);
    }
    out
}

/// Rows the apply-gate pane needs for this diff at this terminal width
/// (summary line wrapped + the hint line + borders), capped so a huge diff
/// can't squeeze out the transcript.
fn diff_height(d: &PendingDiff, width: u16) -> u16 {
    let inner_w = width.saturating_sub(2).max(1) as usize;
    let summary_w: usize = 20 // "◆ PROPOSED CHANGES  "
        + d.added.iter().chain(&d.removed).chain(&d.changed)
            .map(|r| r.chars().count() + 2)
            .sum::<usize>()
        + 16; // " nets nn→nn "
    let summary_rows = summary_w.div_ceil(inner_w) as u16;
    (summary_rows + 1 + 2).clamp(4, 8)
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
    let inner_w = area.width.saturating_sub(2).max(3) as usize;
    let avail = inner_w - 2; // minus the "> " prompt

    let title = if app.pending.is_some() {
        " input · waiting on the apply-gate "
    } else if app.running {
        " input · draft the next prompt (Esc cancels the turn) "
    } else {
        " input "
    };

    let line = if app.pending.is_some() {
        Line::from(Span::styled(
            "approve or reject the change above ([a]/[r])",
            Style::default().fg(Color::Yellow),
        ))
    } else if app.input.is_empty() {
        let placeholder = if app.running {
            "agent is working…"
        } else {
            "type a prompt — :help for commands"
        };
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Blue)),
            Span::styled(placeholder, Style::default().fg(Color::DarkGray)),
        ])
    } else {
        // Window the input horizontally so the cursor stays visible.
        let chars: Vec<char> = app.input.chars().collect();
        let start = app.cursor.saturating_sub(avail.saturating_sub(1));
        let visible: String = chars.iter().skip(start).take(avail).collect();
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Blue)),
            Span::raw(visible),
        ])
    };

    let para = Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(para, area);

    // A real terminal cursor at the edit point (only while typing is live).
    if app.input_active() {
        let start = app.cursor.saturating_sub(avail.saturating_sub(1));
        let x = area.x + 1 + 2 + (app.cursor - start) as u16;
        f.set_cursor_position((x.min(area.x + area.width.saturating_sub(2)), area.y + 1));
    }
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
    // Context-sensitive key hints.
    let right = if app.pending.is_some() {
        "a approve · r reject · Esc reject "
    } else if app.running {
        "Esc cancel turn · Ctrl-C quit "
    } else {
        ":help  :undo  :auto  :clear  :quit "
    };
    // Pad the middle so the right hint sits at the edge (count chars, not
    // bytes — the model label can contain multibyte punctuation).
    let width = area.width as usize;
    let pad = width.saturating_sub(left.chars().count() + right.chars().count());
    let text = format!("{left}{}{right}", " ".repeat(pad));
    let para = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().bg(Color::DarkGray).fg(Color::White),
    )));
    f.render_widget(para, area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = 60u16.min(area.width.saturating_sub(4));
    let h = 16u16.min(area.height.saturating_sub(2));
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
        Line::from("a / r            approve / reject a proposed change"),
        Line::from("Up / Down        recall prompt history"),
        Line::from("PgUp/PgDn/wheel  scroll the transcript"),
        Line::from("Ctrl-U/W/A/E     line editing (kill line/word, home/end)"),
        Line::from("Esc              close help / reject gate / clear input"),
        Line::from("                 / cancel the running turn / quit"),
        Line::from(":auto            toggle auto-approve (yolo)"),
        Line::from(":undo            restore the previous schematic"),
        Line::from(":clear           clear the transcript"),
        Line::from(":quit / Ctrl-C   exit"),
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
    fn render_to_string(app: &mut App, w: u16, h: u16) -> String {
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
    fn wrap_text_wraps_at_word_boundaries() {
        assert_eq!(wrap_text("one two three", 8), vec!["one two", "three"]);
        assert_eq!(wrap_text("short", 8), vec!["short"]);
        assert_eq!(wrap_text("", 8), vec![""]);
    }

    #[test]
    fn wrap_text_respects_embedded_newlines() {
        assert_eq!(wrap_text("a\nb", 10), vec!["a", "b"]);
    }

    #[test]
    fn wrap_text_hard_breaks_long_words() {
        assert_eq!(
            wrap_text("supercalifragilistic", 7),
            vec!["superca", "lifragi", "listic"]
        );
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

        let text = render_to_string(&mut a, 80, 24);
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
    fn multiline_assistant_text_renders_every_line() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantText(
            "first line\nsecond line".into(),
        )));
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("first line"), "line 1:\n{text}");
        assert!(text.contains("second line"), "line 2:\n{text}");
    }

    #[test]
    fn long_entries_wrap_and_the_tail_stays_visible() {
        let mut a = app();
        for i in 0..30 {
            a.update(Msg::Agent(AgentEvent::AssistantText(format!(
                "message number {i} with some extra words so that it wraps across rows"
            ))));
        }
        let text = render_to_string(&mut a, 40, 12);
        assert!(
            text.contains("number 29"),
            "newest entry visible at the tail:\n{text}"
        );
    }

    #[test]
    fn overscroll_is_clamped_and_indicated() {
        let mut a = app();
        for i in 0..20 {
            a.update(Msg::Agent(AgentEvent::AssistantText(format!("m{i}"))));
        }
        a.scroll = u16::MAX;
        let text = render_to_string(&mut a, 40, 12);
        assert!(a.scroll < u16::MAX, "scroll clamps to the content height");
        assert!(text.contains("↑"), "scrolled-back indicator:\n{text}");
        assert!(text.contains("m0"), "clamped view shows the top:\n{text}");
    }

    #[test]
    fn running_turn_shows_spinner_in_title() {
        let mut a = app();
        for c in "go".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Tick);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("agent working"), "spinner title:\n{text}");
    }

    #[test]
    fn pending_diff_shows_approve_and_reject() {
        let mut a = app();
        a.update(Msg::PendingDiff(json!({
            "ok": true,
            "would_write": true,
            "diff": { "added": ["U1"], "removed": [], "changed": [], "nets_before": 0, "nets_after": 5 }
        })));
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("PROPOSED CHANGES"), "diff header:\n{text}");
        assert!(text.contains("+U1"), "added refdes:\n{text}");
        assert!(text.contains("approve"), "approve hint:\n{text}");
        assert!(text.contains("reject"), "reject hint:\n{text}");
    }

    #[test]
    fn big_diffs_get_a_taller_pane_capped_at_eight() {
        let small = PendingDiff {
            added: vec!["U1".into()],
            ..Default::default()
        };
        assert_eq!(diff_height(&small, 80), 4);
        let big = PendingDiff {
            added: (0..60).map(|i| format!("U{i}")).collect(),
            ..Default::default()
        };
        assert_eq!(diff_height(&big, 80), 8);
    }

    #[test]
    fn placeholder_shows_when_input_is_empty() {
        let mut a = app();
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("type a prompt"), "placeholder:\n{text}");
    }

    #[test]
    fn status_bar_shows_the_model_name() {
        let mut a = app();
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("bedrock"), "provider in status bar:\n{text}");
        assert!(text.contains("claude-opus"), "model in status bar:\n{text}");
    }

    #[test]
    fn status_bar_hints_follow_the_mode() {
        let mut a = app();
        let idle = render_to_string(&mut a, 80, 24);
        assert!(idle.contains(":help"), "idle hints:\n{idle}");

        for c in "go".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        let running = render_to_string(&mut a, 80, 24);
        assert!(
            running.contains("Esc cancel turn"),
            "running hints:\n{running}"
        );

        a.update(Msg::PendingDiff(json!({
            "diff": { "added": ["U1"], "removed": [], "changed": [] }
        })));
        let gated = render_to_string(&mut a, 80, 24);
        assert!(gated.contains("a approve"), "gate hints:\n{gated}");
    }

    #[test]
    fn header_shows_schematic_and_kicad_state() {
        let mut a = app();
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("design.kicad_sch"), "sch path:\n{text}");
        assert!(text.contains("KiCAD"), "kicad indicator:\n{text}");
    }

    #[test]
    fn help_overlay_renders_when_toggled() {
        let mut a = app();
        a.help = true;
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("help"), "help overlay:\n{text}");
        assert!(text.contains(":undo"), "help lists commands:\n{text}");
    }
}
