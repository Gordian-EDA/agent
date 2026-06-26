//! The input composer and the panes that cluster around it: the flat input row
//! ([`draw_input`]), the apply-gate card just above it ([`draw_diff`]), and the
//! two popups that float over it — the `/command` completion list
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
            let accent = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            let name_style = if sel {
                accent
            } else {
                Style::default().fg(Color::Cyan)
            };
            Line::from(vec![
                Span::styled(if sel { "› " } else { "  " }, accent),
                Span::styled(format!("{:<name_w$}  ", c.name), name_style),
                Span::styled(c.desc.to_string(), Style::default().fg(Color::DarkGray)),
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
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}

/// Rows the apply-gate pane needs for this diff at this terminal width: action
/// row, summary row, and a wrapped preview, capped so a huge diff can't squeeze
/// out the transcript.
pub(super) fn diff_height(d: &PendingDiff, width: u16) -> u16 {
    let inner_w = width.saturating_sub(2 * MARGIN).max(1) as usize;
    let preview_rows = diff_preview_text(d, 8).chars().count().div_ceil(inner_w) as u16;
    (2 + preview_rows).clamp(3, 5)
}

pub(super) fn draw_diff(f: &mut Frame, area: Rect, app: &App) {
    let Some(d) = app.pending.as_ref() else {
        return;
    };

    // The key letters are sourced from the keybinding definitions, not hardcoded,
    // so the labels can never drift from what `event::map_key` actually accepts.
    let accent = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let action = Line::from(vec![
        Span::styled("change pending", accent),
        Span::styled("   ", dim),
        Span::styled(
            format!("[{}] approve", super::super::event::APPROVE_KEY),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   ", dim),
        Span::styled(
            format!("[{}] reject", super::super::event::REJECT_KEY),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("   Esc cancel", dim),
    ]);
    let summary = Line::from(vec![
        Span::styled(
            format!("+{} added", d.added.len()),
            Style::default().fg(Color::Green),
        ),
        Span::styled("   ", dim),
        Span::styled(
            format!("-{} removed", d.removed.len()),
            Style::default().fg(Color::Red),
        ),
        Span::styled("   ", dim),
        Span::styled(
            format!("~{} changed", d.changed.len()),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled("   ", dim),
        Span::styled(
            format!("nets {} -> {}", d.nets_before, d.nets_after),
            Style::default().fg(Color::Gray),
        ),
    ]);
    let preview = Line::from(diff_preview_spans(d, 8));

    let para = Paragraph::new(vec![action, summary, preview]).wrap(Wrap { trim: true });
    f.render_widget(para, body(area));
}

fn diff_preview_text(d: &PendingDiff, limit: usize) -> String {
    let mut parts = Vec::new();
    let mut total = 0usize;
    for (prefix, refs) in [("+", &d.added), ("-", &d.removed), ("~", &d.changed)] {
        for r in refs {
            total += 1;
            if parts.len() < limit {
                parts.push(format!("{prefix}{r}"));
            }
        }
    }
    if total > parts.len() {
        parts.push(format!("+{} more", total - parts.len()));
    }
    if parts.is_empty() {
        "no component changes".into()
    } else {
        parts.join("  ")
    }
}

fn diff_preview_spans(d: &PendingDiff, limit: usize) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("refs  ", Style::default().fg(Color::DarkGray))];
    let mut shown = 0usize;
    let mut total = 0usize;
    for (prefix, refs, color) in [
        ("+", &d.added, Color::Green),
        ("-", &d.removed, Color::Red),
        ("~", &d.changed, Color::Yellow),
    ] {
        for r in refs {
            total += 1;
            if shown < limit {
                if shown > 0 {
                    spans.push(Span::styled("  ", Style::default().fg(Color::DarkGray)));
                }
                spans.push(Span::styled(
                    format!("{prefix}{r}"),
                    Style::default().fg(color),
                ));
                shown += 1;
            }
        }
    }
    if total == 0 {
        spans.push(Span::styled(
            "no component changes",
            Style::default().fg(Color::DarkGray),
        ));
    } else if total > shown {
        spans.push(Span::styled("  ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("+{} more", total - shown),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
}

/// Rows the composer needs: top/bottom separators plus one row per draft line
/// (split on `\n`), capped so a giant paste can't swallow the transcript.
pub(super) fn composer_height(app: &App, screen_h: u16, screen_w: u16) -> u16 {
    let input_area = Rect {
        x: 0,
        y: 0,
        width: screen_w,
        height: screen_h,
    };
    let content_w = body(input_area).width.saturating_sub(2).max(1) as usize;
    let draft_rows = wrapped_input_rows(&app.input, content_w).len().max(1) as u16;
    let cap = (screen_h / 3).max(3);
    (draft_rows + 2).clamp(3, cap)
}

pub(super) fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    // Claude Code style: full-width border lines with footer-aligned input text.
    // No box; the border touches the terminal edge while typing follows the
    // chrome margin.
    let focused = app.input_active() && app.pending.is_none();
    let rule = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let outer = area;
    let rule_text = "─".repeat(outer.width as usize);
    let rule_line = || {
        Paragraph::new(Line::from(Span::styled(
            rule_text.clone(),
            Style::default().fg(rule),
        )))
    };
    f.render_widget(rule_line(), Rect { height: 1, ..outer });
    if outer.height > 1 {
        f.render_widget(
            rule_line(),
            Rect {
                y: outer.y + outer.height - 1,
                height: 1,
                ..outer
            },
        );
    }
    let inner = body(Rect {
        y: outer.y + 1,
        height: outer.height.saturating_sub(2),
        ..outer
    });
    let marker = Rect {
        x: outer.x,
        y: inner.y,
        width: inner.x.saturating_sub(outer.x).max(1),
        height: inner.height,
    };

    let avail = inner.width.max(1) as usize;
    let caret = || Span::styled("›", Style::default().fg(Color::Cyan));
    f.render_widget(Paragraph::new(Line::from(caret())), marker);

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
            Paragraph::new(Line::from(Span::styled(
                placeholder,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ))),
            inner,
        );
        if app.input_active() {
            f.set_cursor_position((inner.x, inner.y));
        }
        return;
    }

    // A multi-line draft wraps visually instead of scrolling horizontally.
    let (cur_line, cur_col) = cursor_line_col(&app.input, app.cursor);
    let wrapped = wrapped_input_rows(&app.input, avail);
    let cursor_row = cursor_visual_row(&wrapped, cur_line, cur_col, avail);
    let rows = inner.height as usize;
    let first = cursor_row.saturating_add(1).saturating_sub(rows);
    let mut lines: Vec<Line> = Vec::new();
    for row in wrapped.iter().skip(first).take(rows) {
        lines.push(Line::from(Span::raw(row.text.clone())));
    }
    f.render_widget(Paragraph::new(lines), inner);

    // The terminal cursor on the wrapped visual row.
    if app.input_active() {
        let visible_row = cursor_row.saturating_sub(first) as u16;
        let row_start = wrapped.get(cursor_row).map(|r| r.start).unwrap_or(0);
        let x = inner.x + cur_col.saturating_sub(row_start) as u16;
        let y = (inner.y + visible_row).min(inner.y + inner.height.saturating_sub(1));
        f.set_cursor_position((x.min(inner.x + inner.width.saturating_sub(1)), y));
    }
}

#[derive(Clone, Debug)]
struct DraftRow {
    line: usize,
    start: usize,
    text: String,
}

fn wrapped_input_rows(input: &str, width: usize) -> Vec<DraftRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for (line, logical) in input.split('\n').enumerate() {
        let chars: Vec<char> = logical.chars().collect();
        if chars.is_empty() {
            rows.push(DraftRow {
                line,
                start: 0,
                text: String::new(),
            });
            continue;
        }
        let mut start = 0;
        while start < chars.len() {
            let end = (start + width).min(chars.len());
            rows.push(DraftRow {
                line,
                start,
                text: chars[start..end].iter().collect(),
            });
            start = end;
        }
    }
    rows
}

fn cursor_visual_row(rows: &[DraftRow], line: usize, col: usize, width: usize) -> usize {
    let width = width.max(1);
    let target_start = col.saturating_sub(1) / width * width;
    rows.iter()
        .position(|r| r.line == line && r.start == target_start)
        .or_else(|| rows.iter().rposition(|r| r.line == line))
        .unwrap_or_else(|| rows.len().saturating_sub(1))
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
            let accent = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            let text_style = if sel {
                accent
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(vec![
                Span::styled(if sel { "› " } else { "  " }, accent),
                Span::styled(
                    format!("↶{:<idx_w$}  ", i + 1),
                    Style::default().fg(Color::DarkGray),
                ),
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
    let w = (content_w + 2)
        .max(24)
        .min(input_area.width.saturating_sub(2 * MARGIN));
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
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
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
        assert_eq!(diff_height(&small, 80), 3);
        let big = PendingDiff {
            added: (0..60).map(|i| format!("LONG_REF_{i}")).collect(),
            ..Default::default()
        };
        assert_eq!(
            diff_height(&big, 24),
            5,
            "capped so it can't eat the transcript"
        );
    }
}
