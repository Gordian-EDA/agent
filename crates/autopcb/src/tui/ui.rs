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
//! │ bedrock · opus · turns 2 · ctx 23.4k (12%) ····· /help /undo  │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! The transcript is wrapped by [`wrap_segments`] (not `Paragraph::wrap`) so
//! the scroll arithmetic — tail-following, clamping, the `↑n` indicator — is
//! exact in visual rows. Assistant prose is rendered through the markdown
//! module ([`super::md`]) first; wrapping styled segments rather than plain
//! strings is what lets bold/code spans survive line breaks.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use super::app::{App, Entry, NoticeLevel, PendingDiff, Speaker};
use super::md::{self, MdLine, WrapMode};

/// Braille spinner shown in the transcript title while a turn runs.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Nominal context window used for the status-bar percentage (Claude-class
/// models on Bedrock).
const CONTEXT_WINDOW_TOKENS: u64 = 200_000;

/// Compact token count: `950`, `23.4k`, `1.2M`.
fn fmt_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Left margin (in columns) before transcript/diff/input content, so prose
/// doesn't hug the terminal edge now that the border boxes are gone.
const MARGIN: u16 = 1;

/// Draw the whole cockpit.
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Size the diff pane to its content (0 when nothing is pending).
    let diff_h = app
        .pending
        .as_ref()
        .map(|d| diff_height(d, area.width))
        .unwrap_or(0);
    // The running indicator takes a row only while a turn is in flight.
    let running_h = u16::from(app.running);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),         // header
            Constraint::Min(3),            // transcript
            Constraint::Length(diff_h),    // proposed-changes pane
            Constraint::Length(running_h), // running indicator
            Constraint::Length(3),         // input composer (rounded box)
            Constraint::Length(1),         // status bar
        ])
        .split(area);

    draw_header(f, chunks[0], app);
    draw_transcript(f, chunks[1], app);
    if app.pending.is_some() {
        draw_diff(f, chunks[2], app);
    }
    if app.running {
        draw_running(f, chunks[3], app);
    }
    draw_input(f, chunks[4], app);
    draw_status(f, chunks[5], app);
    draw_completions(f, chunks[4], app);
    draw_unwind(f, chunks[4], app);

    if app.help {
        draw_help(f, area);
    }
}

/// Inset a pane by [`MARGIN`] columns on the left (full height).
fn body(area: Rect) -> Rect {
    Rect {
        x: area.x + MARGIN,
        y: area.y,
        width: area.width.saturating_sub(MARGIN),
        height: area.height,
    }
}

/// The `/command` completion popup, floated just above the input pane.
fn draw_completions(f: &mut Frame, input_area: Rect, app: &App) {
    let Some((matches, selected)) = app.completion_view() else {
        return;
    };
    let name_w = matches.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let highlight = selected == Some(i);
            let style = if highlight {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };
            Line::from(vec![
                Span::styled(format!(" {:<name_w$}  ", c.name), style),
                Span::styled(
                    c.desc.to_string(),
                    if highlight {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
            ])
        })
        .collect();

    let w = (lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0) as u16
        + 2)
    .min(input_area.width);
    let h = (matches.len() as u16 + 2).min(input_area.y); // never above the screen top
    let popup = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(h),
        width: w.max(20).min(input_area.width),
        height: h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Tab to complete "),
        ),
        popup,
    );
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let dot = if app.status.kicad_connected {
        "●"
    } else {
        "○"
    };
    let dim = Style::default().fg(Color::DarkGray);
    let sep = || Span::styled("  ·  ", dim);
    let spans = vec![
        // The brand carries the accent; everything else is metadata, so it dims.
        Span::styled(
            " auto-pcb",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(file_name(&app.status.sch_path), Style::default().fg(Color::Gray)),
        sep(),
        Span::styled(
            format!("KiCAD {dot}"),
            Style::default().fg(if app.status.kicad_connected {
                Color::Green
            } else {
                Color::Red
            }),
        ),
        sep(),
        Span::styled(
            if app.auto { "auto on" } else { "auto off" },
            if app.auto { Style::default().fg(Color::Yellow) } else { dim },
        ),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    // The `↑n` scrolled-back indicator used to live in the transcript border
    // title; with the border gone it sits at the header's right edge, overlaid
    // so it stays visible even when the title overflows a narrow terminal.
    if app.scroll > 0 {
        let ind = format!(" ↑{} ", app.scroll);
        let iw = (ind.chars().count() as u16).min(area.width);
        let ind_area = Rect {
            x: area.x + area.width - iw,
            y: area.y,
            width: iw,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                ind,
                Style::default().fg(Color::DarkGray),
            ))),
            ind_area,
        );
    }
}

fn draw_transcript(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = body(area);
    let body_w = inner.width.max(1) as usize;
    let lines: Vec<Line> = app
        .transcript
        .iter()
        .flat_map(|e| render_entry(e, body_w))
        .collect();

    let total = lines.len() as u16;
    // scroll == 0 follows the tail; larger scrolls back into history. Clamp it
    // so over-scrolling never leaves the viewport stuck above the content.
    let max_top = total.saturating_sub(inner.height);
    app.scroll = app.scroll.min(max_top);
    let top = max_top - app.scroll;

    let para = Paragraph::new(lines).scroll((top, 0));
    f.render_widget(para, inner);
}

/// Style one transcript entry into wrapped `Line`s, in the Codex idiom: a blank
/// line opens each turn (user / assistant), the speaker's marker sits on the
/// first row with a hanging continuation indent under it, assistant prose is
/// markdown, and tool / system lines recede (dim, italic) so they read as
/// sub-steps of the turn above them.
///
/// `first` is the row-0 marker, `cont` the indent repeated on wrapped rows.
fn render_entry(e: &Entry, width: usize) -> Vec<Line<'static>> {
    // (first_marker, cont_marker, marker_style, body_style, markdown, gap_above)
    let (first, cont, marker_style, body_style, markdown, gap) = match e.speaker {
        // The user's turn: a cyan caret and bold text — the one thing the eye
        // should land on when scanning back through the transcript.
        Speaker::User => (
            "› ",
            "  ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            Style::default().add_modifier(Modifier::BOLD),
            false,
            true,
        ),
        // Assistant prose: a dim bullet, markdown body, hanging 2-col indent.
        Speaker::Assistant => (
            "• ",
            "  ",
            Style::default().fg(Color::Cyan),
            Style::default(),
            true,
            true,
        ),
        // Tool calls cluster under the assistant turn (no gap) and recede.
        Speaker::Tool => (
            "  ▸ ",
            "    ",
            Style::default().fg(Color::DarkGray),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            false,
            false,
        ),
        Speaker::System => {
            // Most system notes are dim; turn-end indicators carry a severity
            // tint (a finished turn green, a cap cutoff yellow, an error red).
            let color = match e.level {
                NoticeLevel::Plain => Color::DarkGray,
                NoticeLevel::Success => Color::Green,
                NoticeLevel::Warn => Color::Yellow,
                NoticeLevel::Error => Color::Red,
            };
            let body = match e.level {
                NoticeLevel::Plain => Style::default().fg(color).add_modifier(Modifier::ITALIC),
                _ => Style::default().fg(color),
            };
            ("  ", "  ", Style::default().fg(color), body, false, false)
        }
    };
    let body_w = width.saturating_sub(first.chars().count()).max(1);
    let logical: Vec<MdLine> = if markdown {
        md::render_markdown(&e.text, body_style)
    } else {
        e.text
            .split('\n')
            .map(|l| MdLine {
                segments: vec![(l.to_string(), body_style)],
                wrap: WrapMode::Word,
            })
            .collect()
    };

    // A blank opener separates turns; clustered sub-steps (tool/system) omit it.
    let mut lines = Vec::new();
    if gap {
        lines.push(Line::from(""));
    }
    let mut first_row = true;
    for ml in &logical {
        for row in wrap_segments(&ml.segments, body_w, ml.wrap == WrapMode::Preserve) {
            let marker = if first_row { first } else { cont };
            let mut spans = vec![Span::styled(marker, marker_style)];
            spans.extend(row);
            lines.push(Line::from(spans));
            first_row = false;
        }
    }
    if first_row {
        // e.g. an entry that was nothing but fence markers — still take a row
        // so the scroll math stays exact per entry.
        lines.push(Line::from(Span::styled(first, marker_style)));
    }
    lines
}

/// Wrap styled segments into rows of at most `width` chars, preserving each
/// char's style across breaks. Always returns at least one (possibly empty)
/// row so every logical line occupies a row.
///
/// `preserve` does a plain char-chunk wrap (code blocks: every space matters);
/// otherwise this is a greedy word wrap that collapses whitespace runs, keeps
/// the line's leading indent as a hanging indent, and hard-breaks words longer
/// than a row.
fn wrap_segments(
    segments: &[(String, Style)],
    width: usize,
    preserve: bool,
) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let flat: Vec<(char, Style)> = segments
        .iter()
        .flat_map(|(t, s)| t.chars().map(move |c| (c, *s)))
        .collect();

    if preserve {
        if flat.is_empty() {
            return vec![Vec::new()];
        }
        return flat.chunks(width).map(spans_of).collect();
    }

    // Leading whitespace becomes a hanging indent (so wrapped bullets stay
    // aligned under their marker's nesting level).
    let lead = flat
        .iter()
        .take_while(|(c, _)| c.is_whitespace())
        .count()
        .min(width.saturating_sub(1));
    let avail = width - lead;

    // Tokenize the rest into styled words; whitespace runs collapse.
    let mut words: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    for &(c, s) in &flat[lead..] {
        if c.is_whitespace() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push((c, s));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    for mut word in words {
        loop {
            let sep = usize::from(!row.is_empty());
            if row.len() + sep + word.len() <= avail {
                if sep == 1 {
                    // Style the separator like its neighbor so span runs merge.
                    let st = row.last().map(|&(_, s)| s).unwrap_or_default();
                    row.push((' ', st));
                }
                row.extend(word);
                break;
            }
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                continue;
            }
            // A word longer than a whole row: hard-break it.
            rows.push(word[..avail].to_vec());
            word = word[avail..].to_vec();
        }
    }
    rows.push(row); // also preserves intentionally blank lines

    rows.into_iter()
        .map(|r| {
            let mut spans = Vec::new();
            if lead > 0 {
                spans.push(Span::raw(" ".repeat(lead)));
            }
            spans.extend(spans_of(&r));
            spans
        })
        .collect()
}

/// Merge a styled char run back into `Span`s (consecutive equal styles join).
fn spans_of(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut out: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut style: Option<Style> = None;
    for &(c, s) in chars {
        match style {
            Some(cur) if cur == s => buf.push(c),
            Some(cur) => {
                out.push(Span::styled(std::mem::take(&mut buf), cur));
                buf.push(c);
                style = Some(s);
            }
            None => {
                buf.push(c);
                style = Some(s);
            }
        }
    }
    if let Some(cur) = style {
        out.push(Span::styled(buf, cur));
    }
    out
}

/// The running indicator that replaces the old transcript-title spinner: an
/// animated frame, elapsed seconds, the output tokens streamed this turn, and
/// the interrupt hint. Drawn only while a turn is in flight.
fn draw_running(f: &mut Frame, area: Rect, app: &App) {
    let frame = SPINNER[app.spinner % SPINNER.len()];
    let secs = app.turn_elapsed_secs().unwrap_or(0);
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = vec![
        Span::styled(
            format!("{frame} "),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("working", Style::default().fg(Color::Yellow)),
        Span::styled(format!(" · {secs}s"), dim),
    ];
    let toks = app.turn_output_tokens();
    if toks > 0 {
        spans.push(Span::styled(format!(" · ↓{} tok", fmt_tokens(toks)), dim));
    }
    spans.push(Span::styled(" · esc to interrupt", dim));
    f.render_widget(Paragraph::new(Line::from(spans)), body(area));
}

/// Rows the apply-gate pane needs for this diff at this terminal width (summary
/// line wrapped + the hint line), capped so a huge diff can't squeeze out the
/// transcript.
fn diff_height(d: &PendingDiff, width: u16) -> u16 {
    let inner_w = width.saturating_sub(MARGIN).max(1) as usize;
    let summary_w: usize = 20 // "◆ PROPOSED CHANGES  "
        + d.added.iter().chain(&d.removed).chain(&d.changed)
            .map(|r| r.chars().count() + 2)
            .sum::<usize>()
        + 16; // " nets nn→nn "
    let summary_rows = summary_w.div_ceil(inner_w) as u16;
    (summary_rows + 1).clamp(2, 6)
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

    let para = Paragraph::new(vec![Line::from(spans), hint]).wrap(Wrap { trim: true });
    f.render_widget(para, body(area));
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    // A rounded composer box (Codex idiom). The border brightens to the accent
    // while typing is live and dims otherwise, so the eye knows where focus is.
    let focused = app.input_active() && app.pending.is_none();
    let border = if focused { Color::Cyan } else { Color::DarkGray };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border));
    let inner = block.inner(body(area));
    f.render_widget(block, body(area));

    let avail = (inner.width as usize).saturating_sub(2).max(1); // minus the "› " prompt

    let line = if app.pending.is_some() {
        Line::from(Span::styled(
            "approve or reject the change above ([a]/[r])",
            Style::default().fg(Color::Yellow),
        ))
    } else if app.esc_armed {
        Line::from(Span::styled(
            "Esc again: open the unwind picker (context only) — any key cancels",
            Style::default().fg(Color::Yellow),
        ))
    } else if app.input.is_empty() {
        let placeholder = if app.running {
            "agent is working…"
        } else {
            "type a prompt — /help for commands, Tab completes"
        };
        Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Cyan)),
            Span::styled(
                placeholder,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ])
    } else {
        // Window the input horizontally so the cursor stays visible.
        let chars: Vec<char> = app.input.chars().collect();
        let start = app.cursor.saturating_sub(avail.saturating_sub(1));
        let visible: String = chars.iter().skip(start).take(avail).collect();
        Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Cyan)),
            Span::raw(visible),
        ])
    };

    f.render_widget(Paragraph::new(line), inner);

    // A real terminal cursor at the edit point (only while typing is live).
    if app.input_active() {
        let start = app.cursor.saturating_sub(avail.saturating_sub(1));
        let x = inner.x + 2 + (app.cursor - start) as u16; // "› " is 2 cols
        f.set_cursor_position((x.min(inner.x + inner.width.saturating_sub(1)), inner.y));
    }
}

/// The double-Esc unwind picker, floated just above the input pane. Lists the
/// agent's recent prompts newest-first; the selected row (and everything below
/// it in time) is what an Enter would unwind.
fn draw_unwind(f: &mut Frame, input_area: Rect, app: &App) {
    let Some(p) = app.unwind.as_ref() else {
        return;
    };
    let idx_w = p.prompts.len().to_string().len();
    let lines: Vec<Line> = p
        .prompts
        .iter()
        .enumerate()
        .map(|(i, prompt)| {
            let sel = i == p.selected;
            let (lead, lead_style, text_style) = if sel {
                let hl = Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD);
                ("▶ ", hl, Style::default().fg(Color::Black).bg(Color::Cyan))
            } else {
                (
                    "  ",
                    Style::default().fg(Color::Cyan),
                    Style::default().fg(Color::Gray),
                )
            };
            Line::from(vec![
                Span::styled(format!("{lead}↶{:<idx_w$} ", i + 1), lead_style),
                Span::styled(prompt.clone(), text_style),
            ])
        })
        .collect();

    let content_w = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0) as u16;
    // `.max().min()` not `clamp()`: a terminal narrower than the floor would
    // make clamp(lo, hi) panic with lo > hi.
    let w = (content_w + 2).max(24).min(input_area.width);
    let h = (p.prompts.len() as u16 + 2).min(input_area.y); // never above the screen top
    let popup = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(h),
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" unwind to… · ↑↓ Enter · Esc cancels "),
        ),
        popup,
    );
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let s = &app.status;
    let mut left = format!(
        " {} · {} · turns {} · applied {}",
        s.provider,
        short_model(&s.model),
        s.turn_count,
        s.applied_count,
    );
    if s.ctx_tokens > 0 {
        let pct = (s.ctx_tokens as f64 / CONTEXT_WINDOW_TOKENS as f64 * 100.0).round() as u64;
        left.push_str(&format!(" · ctx {} ({pct}%)", fmt_tokens(s.ctx_tokens)));
    }
    // Context-sensitive key hints. While running, the interrupt hint already
    // lives on the running line, so the bar only needs the hard-quit reminder.
    let right = if app.pending.is_some() {
        "a approve · r reject · Esc reject "
    } else if app.running {
        "Ctrl-C quit "
    } else if app.esc_armed {
        "Esc unwind last turn "
    } else {
        "/help  /undo  /auto  /clear  /quit "
    };

    // Compose a bar exactly `width` columns wide: pin `right` to the edge, give
    // `left` the rest, and ellipsize `left` rather than let the two collide.
    // (Char counts, not bytes — the labels carry multibyte punctuation.)
    let width = area.width as usize;
    let right_len = right.chars().count();
    let text = if width <= right_len {
        right.chars().take(width).collect::<String>()
    } else {
        let avail = width - right_len; // columns to the left of the hint
        let left = if left.chars().count() > avail {
            let kept: String = left.chars().take(avail.saturating_sub(1)).collect();
            format!("{kept}…")
        } else {
            left
        };
        let pad = avail.saturating_sub(left.chars().count());
        format!("{left}{}{right}", " ".repeat(pad))
    };
    // A recessed footer (dim, no bar) rather than a heavy inverted band — it
    // carries context and hints without competing with the transcript.
    let para = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(para, area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = 64u16.min(area.width.saturating_sub(4));
    let h = 23u16.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let mut lines = vec![
        Line::from(Span::styled(
            "auto-pcb copilot — help",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Type a prompt, Enter to send. Tab completes /commands."),
        Line::from("a / r            approve / reject a proposed change"),
        Line::from("Up / Down        recall prompt history"),
        Line::from("PgUp/PgDn/wheel  scroll the transcript"),
        Line::from("Ctrl-U/W/A/E     line editing (kill line/word, home/end)"),
        Line::from("Esc              close help / reject gate / clear input"),
        Line::from("                 / cancel the running turn"),
        Line::from("Esc Esc          unwind the last turn (context only)"),
        Line::from("Ctrl-C           exit"),
        Line::from(""),
    ];
    for c in crate::tui::app::COMMANDS {
        lines.push(Line::from(format!("{:<16} {}", c.name, c.desc)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Esc to close.",
        Style::default().fg(Color::DarkGray),
    )));
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

    /// Plain text of one wrapped row.
    fn row_text(row: &[Span]) -> String {
        row.iter().map(|s| s.content.as_ref()).collect()
    }

    fn plain(s: &str) -> Vec<(String, Style)> {
        vec![(s.to_string(), Style::default())]
    }

    #[test]
    fn wrap_segments_wraps_at_word_boundaries() {
        let rows = wrap_segments(&plain("one two three"), 8, false);
        let texts: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
        assert_eq!(texts, vec!["one two", "three"]);
        assert_eq!(wrap_segments(&plain(""), 8, false).len(), 1);
    }

    #[test]
    fn wrap_segments_hard_breaks_long_words() {
        let rows = wrap_segments(&plain("supercalifragilistic"), 7, false);
        let texts: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
        assert_eq!(texts, vec!["superca", "lifragi", "listic"]);
    }

    #[test]
    fn wrap_segments_keeps_styles_across_breaks() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let segs = vec![
            ("plain words then ".to_string(), Style::default()),
            ("boldly styled tail".to_string(), bold),
        ];
        let rows = wrap_segments(&segs, 22, false);
        assert!(rows.len() > 1, "must wrap to test style survival");
        // Every char from the bold segment keeps its style, wherever it lands.
        let styled: Vec<(String, Style)> = rows
            .iter()
            .flatten()
            .map(|s| (s.content.to_string(), s.style))
            .collect();
        assert!(
            styled
                .iter()
                .any(|(t, st)| t.contains("tail") && *st == bold),
            "bold survives the wrap: {styled:?}"
        );
    }

    #[test]
    fn wrap_segments_preserve_mode_keeps_spaces() {
        let rows = wrap_segments(&plain("  indented: code"), 8, true);
        let texts: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
        assert_eq!(texts, vec!["  indent", "ed: code"]);
    }

    #[test]
    fn wrapped_bullets_hang_under_their_indent() {
        let segs = vec![(
            "  • a nested bullet that wraps".to_string(),
            Style::default(),
        )];
        let rows = wrap_segments(&segs, 14, false);
        assert!(rows.len() > 1);
        for r in &rows {
            assert!(
                row_text(r).starts_with("  "),
                "hanging indent: {:?}",
                row_text(r)
            );
        }
    }

    #[test]
    fn assistant_markdown_renders_styled_not_literal() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantText(
            "I added **R1** with `10k`:\n- pull-up\n```yaml\nnets:\n```".into(),
        )));
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("R1"), "bold text shows:\n{text}");
        assert!(!text.contains("**"), "bold markers stripped:\n{text}");
        assert!(!text.contains('`'), "code markers stripped:\n{text}");
        assert!(text.contains("• pull-up"), "bullet normalized:\n{text}");
        assert!(text.contains("nets:"), "fence content shows:\n{text}");
        assert!(!text.contains("yaml"), "fence marker line hidden:\n{text}");
    }

    #[test]
    fn user_text_is_never_markdown_rendered() {
        let mut a = app();
        for c in "literally **stars**".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        let text = render_to_string(&mut a, 80, 24);
        assert!(
            text.contains("**stars**"),
            "user input stays literal:\n{text}"
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
    fn running_turn_shows_the_status_line() {
        let mut a = app();
        for c in "go".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Tick);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("working"), "running verb:\n{text}");
        assert!(text.contains("esc to interrupt"), "interrupt hint:\n{text}");
    }

    #[test]
    fn running_line_shows_streamed_output_tokens() {
        let mut a = app();
        for c in "go".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 100,
            output_tokens: 1200,
        }));
        a.update(Msg::Tick);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("↓1.2k tok"), "per-turn output tokens:\n{text}");
    }

    #[test]
    fn idle_app_has_no_running_line() {
        let mut a = app();
        let text = render_to_string(&mut a, 80, 24);
        assert!(!text.contains("esc to interrupt"), "no running line idle:\n{text}");
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
    fn big_diffs_get_a_taller_pane_capped() {
        let small = PendingDiff {
            added: vec!["U1".into()],
            ..Default::default()
        };
        // One summary row + the hint row, borderless.
        assert_eq!(diff_height(&small, 80), 2);
        let big = PendingDiff {
            added: (0..60).map(|i| format!("U{i}")).collect(),
            ..Default::default()
        };
        assert_eq!(diff_height(&big, 80), 6, "capped so it can't eat the transcript");
    }

    #[test]
    fn placeholder_shows_when_input_is_empty() {
        let mut a = app();
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("type a prompt"), "placeholder:\n{text}");
    }

    #[test]
    fn completion_popup_lists_matches_and_highlights_selection() {
        let mut a = app();
        for c in "/c".chars() {
            a.update(Msg::Char(c));
        }
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("/clear"), "popup lists /clear:\n{text}");
        assert!(text.contains("/compact"), "popup lists /compact:\n{text}");
        assert!(text.contains("Tab to complete"), "popup title:\n{text}");

        a.update(Msg::Complete);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("/clear"), "first match filled:\n{text}");
    }

    #[test]
    fn no_completion_popup_for_plain_prompts() {
        let mut a = app();
        for c in "hello".chars() {
            a.update(Msg::Char(c));
        }
        let text = render_to_string(&mut a, 80, 24);
        assert!(!text.contains("Tab to complete"), "no popup:\n{text}");
    }

    #[test]
    fn status_bar_shows_context_tokens_after_usage() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 23_000,
            output_tokens: 400,
        }));
        let text = render_to_string(&mut a, 100, 24);
        assert!(text.contains("ctx 23.4k"), "token display:\n{text}");
        assert!(text.contains("(12%)"), "window percentage:\n{text}");
    }

    #[test]
    fn esc_armed_shows_the_unwind_hint() {
        let mut a = app();
        a.update(Msg::Cancel);
        assert!(a.esc_armed);
        let text = render_to_string(&mut a, 80, 24);
        assert!(
            text.contains("open the unwind picker"),
            "unwind hint:\n{text}"
        );
    }

    #[test]
    fn user_message_has_an_accent_caret_and_no_speaker_labels() {
        let mut a = app();
        for c in "hello there".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("›"), "user accent caret:\n{text}");
        assert!(text.contains("hello there"), "user text:\n{text}");
        assert!(!text.contains("you  "), "no `you` gutter label:\n{text}");
        assert!(!text.contains("ai   "), "no `ai` gutter label:\n{text}");
    }

    #[test]
    fn unwind_picker_overlay_lists_prompts_and_hint() {
        let mut a = app();
        a.open_unwind(vec!["swap the regulator".into(), "add usb-c".into()]);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("unwind to"), "picker title:\n{text}");
        assert!(text.contains("swap the regulator"), "newest prompt:\n{text}");
        assert!(text.contains("add usb-c"), "older prompt:\n{text}");
    }

    #[test]
    fn fmt_tokens_scales() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(23_400), "23.4k");
        assert_eq!(fmt_tokens(1_200_000), "1.2M");
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
        assert!(idle.contains("/help"), "idle hints:\n{idle}");

        for c in "go".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        let running = render_to_string(&mut a, 80, 24);
        // The interrupt hint now lives on the running line; the bar keeps the
        // hard-quit reminder.
        assert!(
            running.contains("esc to interrupt"),
            "running line hint:\n{running}"
        );
        assert!(running.contains("Ctrl-C quit"), "running bar hint:\n{running}");

        a.update(Msg::PendingDiff(json!({
            "diff": { "added": ["U1"], "removed": [], "changed": [] }
        })));
        let gated = render_to_string(&mut a, 80, 24);
        assert!(gated.contains("a approve"), "gate hints:\n{gated}");
    }

    #[test]
    fn status_bar_ellipsizes_instead_of_colliding_with_hints() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 19_000,
            output_tokens: 200,
        }));
        // A terminal too narrow for the full status + the full key hints.
        let text = render_to_string(&mut a, 56, 24);
        let bar = text.lines().last().expect("status row");
        // Exactly one row's width — the dark bar fills the whole line, with no
        // doubled/overlapping content where the two halves used to collide.
        assert_eq!(bar.chars().count(), 56, "bar fills the width exactly:\n{bar}");
        // The right-edge hints survive intact; the left status is ellipsized.
        assert!(bar.contains("/quit"), "right hint pinned to the edge:\n{bar}");
        assert!(bar.contains('…'), "left status is truncated, not overlapped:\n{bar}");
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
        assert!(text.contains("/undo"), "help lists commands:\n{text}");
        assert!(text.contains("Esc Esc"), "help covers unwind:\n{text}");
    }
}
