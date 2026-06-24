//! The input composer and the panes that cluster around it: the rounded input
//! box ([`draw_input`]), the apply-gate card just above it ([`draw_diff`]), and
//! the two popups that float over it — the `/command` completion list
//! ([`draw_completions`]) and the double-Esc unwind picker ([`draw_unwind`]).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use super::super::app::{App, PendingDiff};
use super::{MARGIN, body};

/// The `/command` completion popup, floated just above the input pane.
pub(super) fn draw_completions(f: &mut Frame, input_area: Rect, app: &App) {
    let Some((matches, selected)) = app.completion_view() else {
        return;
    };
    let name_w = matches.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .map(|(i, c)| {
            // Selected row: an accent caret + bold name. Others: a blank gutter,
            // plain name, dim description — the soft Codex selection, not an
            // inverted bar.
            let sel = selected == Some(i);
            let accent = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            let name_style = if sel { accent } else { Style::default().fg(Color::Cyan) };
            Line::from(vec![
                Span::styled(if sel { "› " } else { "  " }, accent),
                Span::styled(format!("{:<name_w$}  ", c.name), name_style),
                Span::styled(c.desc.to_string(), Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let content_w = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.chars().count()).sum::<usize>())
        .max()
        .unwrap_or(0) as u16;
    let maxw = input_area.width.saturating_sub(2 * MARGIN);
    let w = (content_w + 2).max(24).min(maxw);
    let h = (matches.len() as u16 + 2).min(input_area.y); // never above the screen top
    let popup = Rect {
        x: input_area.x + MARGIN, // align the popup's left edge with the composer
        y: input_area.y.saturating_sub(h),
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(
                    " commands · Tab ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}

/// Rows the apply-gate pane needs for this diff at this terminal width (summary
/// line wrapped + the hint line), capped so a huge diff can't squeeze out the
/// transcript.
pub(super) fn diff_height(d: &PendingDiff, width: u16) -> u16 {
    let inner_w = width.saturating_sub(MARGIN + 2).max(1) as usize; // minus the border
    let summary_w: usize = d
        .added
        .iter()
        .chain(&d.removed)
        .chain(&d.changed)
        .map(|r| r.chars().count() + 4) // "+ r   "
        .sum::<usize>()
        + 16; // "nets nn → nn"
    let summary_rows = summary_w.div_ceil(inner_w) as u16;
    // content = summary rows + a blank + the hint; +2 for the rounded border.
    (summary_rows + 2 + 2).clamp(5, 9)
}

pub(super) fn draw_diff(f: &mut Frame, area: Rect, app: &App) {
    let Some(d) = app.pending.as_ref() else {
        return;
    };

    // An actionable card: a caution-yellow rounded frame (matching the composer's
    // shape) titled "proposed changes", so a pending write reads as a deliberate
    // gate rather than another transcript line.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " proposed changes ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(body(area));
    f.render_widget(block, body(area));

    // Each refdes is a background-tinted chip: a soft green band for an add, red
    // for a delete, neutral slate for a modify — so the eye reads the operation
    // from the fill, not just a leading glyph. A plain gap separates chips so the
    // bands don't merge into one bar.
    let mut spans: Vec<Span> = Vec::new();
    for r in &d.added {
        spans.push(chip(format!("+ {r}"), Color::Green, Color::Rgb(22, 40, 26)));
        spans.push(chip_gap());
    }
    for r in &d.removed {
        spans.push(chip(format!("- {r}"), Color::Red, Color::Rgb(46, 24, 28)));
        spans.push(chip_gap());
    }
    for r in &d.changed {
        spans.push(chip(format!("~ {r}"), Color::Gray, Color::Rgb(38, 40, 51)));
        spans.push(chip_gap());
    }
    spans.push(Span::styled(
        format!("nets {} → {}", d.nets_before, d.nets_after),
        Style::default().fg(Color::DarkGray),
    ));

    // The key letters are sourced from the keybinding definitions, not hardcoded,
    // so the labels can never drift from what `event::map_key` actually accepts.
    let hint = Line::from(vec![
        Span::styled(
            format!("[{}] approve", super::super::event::APPROVE_KEY),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
        Span::styled(
            format!("[{}] reject", super::super::event::REJECT_KEY),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("    Esc rejects", Style::default().fg(Color::DarkGray)),
    ]);

    let para =
        Paragraph::new(vec![Line::from(spans), Line::from(""), hint]).wrap(Wrap { trim: true });
    f.render_widget(para, inner);
}

/// One background-tinted diff chip: bold `fg` label on a soft dark `bg` band, with
/// a one-cell pad on each side so the fill frames the refdes.
fn chip(label: String, fg: Color, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}

/// The un-tinted gap between two chips, so their background bands stay distinct.
fn chip_gap() -> Span<'static> {
    Span::raw("  ")
}

/// Rows the composer needs: one per draft line (split on `\n`), inside the
/// rounded border (+2), floored at 3 (one content row) and capped at a third of
/// the screen so a giant paste can't swallow the transcript.
pub(super) fn composer_height(app: &App, screen_h: u16) -> u16 {
    let draft_rows = app.input.split('\n').count().max(1) as u16;
    let cap = (screen_h / 3).max(3);
    (draft_rows + 2).clamp(3, cap)
}

pub(super) fn draw_input(f: &mut Frame, area: Rect, app: &App) {
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
    let caret = || Span::styled("› ", Style::default().fg(Color::Cyan));

    if app.pending.is_some() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "approve or reject the change above",
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    }
    if app.esc_armed {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Esc again: open the unwind picker (context only) — any key cancels",
                Style::default().fg(Color::Yellow),
            ))),
            inner,
        );
        return;
    }
    if app.input.is_empty() {
        let placeholder = if app.running && app.queued.is_some() {
            "agent is working… · 1 message queued (sends when the turn ends)"
        } else if app.running {
            "agent is working… · Tab queues your next message"
        } else {
            "type a prompt — /help for commands, ⏎ sends · ⇧⏎ newline"
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                caret(),
                Span::styled(
                    placeholder,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ])),
            inner,
        );
        if app.input_active() {
            f.set_cursor_position((inner.x + 2, inner.y));
        }
        return;
    }

    // A multi-line draft: each `\n`-separated logical line gets its own visible
    // row (the caret marks only the first), each windowed horizontally so its
    // own tail stays in view. Locate the cursor's line/column to place the
    // terminal cursor on the right row.
    let logical: Vec<&str> = app.input.split('\n').collect();
    let (cur_line, cur_col) = cursor_line_col(&app.input, app.cursor);
    let rows = inner.height as usize;
    let first = logical.len().saturating_sub(rows); // show the tail if it overflows
    let mut lines: Vec<Line> = Vec::new();
    for (i, text) in logical.iter().enumerate().skip(first) {
        let chars: Vec<char> = text.chars().collect();
        // Window this line so the cursor (on its own row) or its tail is visible.
        let focus = if i == cur_line { cur_col } else { chars.len() };
        let start = focus.saturating_sub(avail.saturating_sub(1));
        let visible: String = chars.iter().skip(start).take(avail).collect();
        let prefix = if i == 0 { caret() } else { Span::raw("  ") };
        lines.push(Line::from(vec![prefix, Span::raw(visible)]));
    }
    f.render_widget(Paragraph::new(lines), inner);

    // The terminal cursor on the cursor's row, windowed to match its line.
    if app.input_active() {
        let chars = logical.get(cur_line).map(|l| l.chars().count()).unwrap_or(0);
        let start = cur_col.min(chars).saturating_sub(avail.saturating_sub(1));
        let row = cur_line.saturating_sub(first) as u16;
        let x = inner.x + 2 + (cur_col - start) as u16;
        let y = (inner.y + row).min(inner.y + inner.height.saturating_sub(1));
        f.set_cursor_position((x.min(inner.x + inner.width.saturating_sub(1)), y));
    }
}

/// The (line, column) of a char-offset cursor in a `\n`-split string — the
/// number of newlines before it, and its offset within that line.
fn cursor_line_col(input: &str, cursor: usize) -> (usize, usize) {
    let mut line = 0;
    let mut col = 0;
    for (i, c) in input.chars().enumerate() {
        if i == cursor {
            return (line, col);
        }
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// The double-Esc unwind picker, floated just above the input pane. Lists the
/// agent's recent prompts newest-first; the selected row (and everything below
/// it in time) is what an Enter would unwind.
pub(super) fn draw_unwind(f: &mut Frame, input_area: Rect, app: &App) {
    let Some(p) = app.unwind.as_ref() else {
        return;
    };
    let idx_w = p.prompts.len().to_string().len();
    let lines: Vec<Line> = p
        .prompts
        .iter()
        .enumerate()
        .map(|(i, prompt)| {
            // Soft selection: accent caret + bold on the chosen row; others dim.
            let sel = i == p.selected;
            let accent = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            let text_style = if sel {
                accent
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(vec![
                Span::styled(if sel { "› " } else { "  " }, accent),
                Span::styled(format!("↶{:<idx_w$}  ", i + 1), Style::default().fg(Color::DarkGray)),
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
    let w = (content_w + 2).max(24).min(input_area.width.saturating_sub(2 * MARGIN));
    let h = (p.prompts.len() as u16 + 2).min(input_area.y); // never above the screen top
    let popup = Rect {
        x: input_area.x + MARGIN, // align with the composer
        y: input_area.y.saturating_sub(h),
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(
                    " unwind to… · ↑↓ Enter · Esc ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_diffs_get_a_taller_pane_capped() {
        let small = PendingDiff {
            added: vec!["U1".into()],
            ..Default::default()
        };
        // One summary row + a blank + the hint row, inside a rounded border (+2):
        // floored at 5 so the card always frames cleanly.
        assert_eq!(diff_height(&small, 80), 5);
        let big = PendingDiff {
            added: (0..60).map(|i| format!("U{i}")).collect(),
            ..Default::default()
        };
        assert_eq!(diff_height(&big, 80), 9, "capped so it can't eat the transcript");
    }
}
