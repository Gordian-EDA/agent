//! The input composer and the panes that cluster around it: the flat input row
//! ([`draw_input`]), the approval card just above it ([`draw_approval`]), and the
//! two popups that float over it — the `/command` completion list
//! ([`draw_completions`]) and the double-Esc unwind picker ([`draw_unwind`]).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::super::app::{App, PendingApproval};
use super::super::theme;
use super::{MARGIN, body};

/// The `/command` completion list, seated directly on top of the composer.
///
/// No border, no title, no caret: the rows *are* the widget, so the list reads
/// as the composer growing upward rather than as a dialog opening over it.
pub(super) fn draw_completions(f: &mut Frame, input_area: Rect, app: &App) {
    let Some((matches, selected)) = app.completion_view() else {
        return;
    };
    let label_w = matches.iter().map(|c| c.name.chars().count()).max().unwrap_or(0);
    let rows: Vec<Line> = matches
        .iter()
        .enumerate()
        .map(|(i, c)| menu_row(c.name, c.desc, label_w, input_area.width, selected == Some(i)))
        .collect();
    draw_menu(f, input_area, rows);
}

/// One full-width menu row: the composer's own left indent, a padded label, then
/// the detail column. Every cell is painted out to the right edge so a selected
/// row reads as one unbroken bar instead of a highlight that stops at the text.
fn menu_row(label: &str, detail: &str, label_w: usize, width: u16, selected: bool) -> Line<'static> {
    let (label_style, detail_style) = if selected {
        (theme::MENU_SEL_LABEL, theme::MENU_SEL)
    } else {
        (theme::MENU_LABEL, theme::MENU_DETAIL)
    };
    let indent = MARGIN as usize;
    let label = format!("{label:<label_w$}");
    // Trim the detail (never the label) when the terminal is too narrow, then pad
    // the row out to the full width so the bar spans it.
    let room = (width as usize).saturating_sub(indent + label.chars().count() + 2);
    let detail: String = detail.chars().take(room).collect();
    let pad = room - detail.chars().count();
    Line::from(vec![
        Span::styled(" ".repeat(indent), detail_style),
        Span::styled(label, label_style),
        Span::styled(format!("  {detail}{}", " ".repeat(pad)), detail_style),
    ])
}

/// Float a borderless menu on top of the composer, growing upward from it.
fn draw_menu(f: &mut Frame, input_area: Rect, rows: Vec<Line<'static>>) {
    let h = (rows.len() as u16).min(input_area.y);
    if h == 0 {
        return;
    }
    let popup = Rect {
        x: input_area.x,
        y: input_area.y - h,
        width: input_area.width,
        height: h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(rows).style(theme::MENU), popup);
}

/// Rows the approval pane needs at this terminal width: action row, summary row,
/// and a wrapped diff/argument preview, capped so a large proposal cannot squeeze
/// out the transcript.
pub(super) fn approval_height(pending: &PendingApproval, width: u16) -> u16 {
    let inner_w = width.saturating_sub(2 * MARGIN).max(1) as usize;
    let preview_rows = approval_preview_text(pending)
        .chars()
        .count()
        .div_ceil(inner_w) as u16;
    (2 + preview_rows).clamp(3, 5)
}

pub(super) fn draw_approval(f: &mut Frame, area: Rect, app: &App) {
    let Some(pending) = app.pending.as_ref() else {
        return;
    };

    // The key letters are sourced from the keybinding definitions, not hardcoded,
    // so the labels can never drift from what `event::map_key` actually accepts.
    let accent = Style::default()
        .fg(theme::ACC)
        .add_modifier(Modifier::BOLD);
    let dim = theme::META;
    let pending_label = match pending {
        PendingApproval::Schematic { .. } => "schematic change pending",
        PendingApproval::Operation { .. } => "operation pending",
    };
    let action = Line::from(vec![
        Span::styled(pending_label, accent),
        Span::styled("   ", dim),
        Span::styled(
            format!("[{}] approve", super::super::event::APPROVE_KEY),
            theme::SUCCESS.add_modifier(Modifier::BOLD),
        ),
        Span::styled("   ", dim),
        Span::styled(
            format!("[{}] reject", super::super::event::REJECT_KEY),
            theme::DANGER.add_modifier(Modifier::BOLD),
        ),
        Span::styled("   Esc cancel", dim),
    ]);
    let (summary, preview) = match pending {
        PendingApproval::Schematic {
            added,
            removed,
            changed,
            nets_before,
            nets_after,
        } => (
            Line::from(vec![
                Span::styled(
                    format!("+{} added", added.len()),
                    theme::ADDED,
                ),
                Span::styled("   ", dim),
                Span::styled(
                    format!("-{} removed", removed.len()),
                    theme::REMOVED,
                ),
                Span::styled("   ", dim),
                Span::styled(
                    format!("~{} changed", changed.len()),
                    theme::CHANGED,
                ),
                Span::styled("   ", dim),
                Span::styled(
                    format!("nets {nets_before} -> {nets_after}"),
                    theme::SUBTLE,
                ),
            ]),
            Line::from(diff_preview_spans(added, removed, changed, 8)),
        ),
        PendingApproval::Operation {
            operation,
            arguments,
        } => (
            Line::from(vec![
                Span::styled("run  ", dim),
                Span::styled(
                    operation.clone(),
                    theme::WARNING.add_modifier(Modifier::BOLD),
                ),
                Span::styled("  (mutates project/board)", dim),
            ]),
            Line::from(vec![
                Span::styled("args  ", dim),
                Span::styled(
                    operation_args_text(arguments),
                    theme::SUBTLE,
                ),
            ]),
        ),
    };

    let para = Paragraph::new(vec![action, summary, preview]).wrap(Wrap { trim: true });
    f.render_widget(para, body(area));
}

fn approval_preview_text(pending: &PendingApproval) -> String {
    match pending {
        PendingApproval::Schematic {
            added,
            removed,
            changed,
            ..
        } => diff_preview_text(added, removed, changed, 8),
        PendingApproval::Operation {
            operation,
            arguments,
        } => format!("run {operation}  args {}", operation_args_text(arguments)),
    }
}

fn diff_preview_text(
    added: &[String],
    removed: &[String],
    changed: &[String],
    limit: usize,
) -> String {
    let mut parts = Vec::new();
    let mut total = 0usize;
    for (prefix, refs) in [("+", added), ("-", removed), ("~", changed)] {
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

fn diff_preview_spans(
    added: &[String],
    removed: &[String],
    changed: &[String],
    limit: usize,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("refs  ", theme::META)];
    let mut shown = 0usize;
    let mut total = 0usize;
    for (prefix, refs, style) in [
        ("+", added, theme::ADDED),
        ("-", removed, theme::REMOVED),
        ("~", changed, theme::CHANGED),
    ] {
        for r in refs {
            total += 1;
            if shown < limit {
                if shown > 0 {
                    spans.push(Span::styled("  ", theme::META));
                }
                spans.push(Span::styled(format!("{prefix}{r}"), style));
                shown += 1;
            }
        }
    }
    if total == 0 {
        spans.push(Span::styled("no component changes", theme::META));
    } else if total > shown {
        spans.push(Span::styled("  ", theme::META));
        spans.push(Span::styled(format!("+{} more", total - shown), theme::META));
    }
    spans
}

fn operation_args_text(arguments: &serde_json::Value) -> String {
    const MAX_CHARS: usize = 240;
    let raw = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into());
    if raw.chars().count() <= MAX_CHARS {
        return raw;
    }
    let mut shortened: String = raw.chars().take(MAX_CHARS - 1).collect();
    shortened.push('…');
    shortened
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
        theme::RULE_FOCUS
    } else {
        theme::RULE
    };
    let outer = area;
    // The composer is seated one surface step above the page, so the input band
    // reads as a distinct place to type rather than a gap in the transcript.
    f.render_widget(Block::default().style(theme::BAND), outer);
    let rule_text = "─".repeat(outer.width as usize);
    let rule_line = || Paragraph::new(Line::from(Span::styled(rule_text.clone(), rule)));
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
    let caret = || Span::styled("❯", Style::default().fg(theme::ACC));
    f.render_widget(Paragraph::new(Line::from(caret())), marker);

    if app.pending.is_some() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "approve or reject the change above",
                theme::META,
            ))),
            inner,
        );
        return;
    }
    if app.esc_armed {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Esc again: open the unwind picker (context only) — any key cancels",
                theme::WARNING,
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
                theme::META,
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
        lines.push(Line::from(Span::styled(row.text.clone(), theme::PROSE)));
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

/// The double-Esc unwind picker, in the same borderless menu idiom as the
/// completion list. Prompts are newest-first; the selected row (and everything
/// below it in time) is what an Enter would unwind.
pub(super) fn draw_unwind(f: &mut Frame, input_area: Rect, app: &App) {
    let Some(p) = app.unwind.as_ref() else {
        return;
    };
    let idx_w = p.prompts.len().to_string().len() + 1; // the ↶ rides the index
    let rows: Vec<Line> = p
        .prompts
        .iter()
        .enumerate()
        .map(|(i, prompt)| {
            menu_row(
                &format!("↶{}", i + 1),
                prompt,
                idx_w,
                input_area.width,
                i == p.selected,
            )
        })
        .collect();
    draw_menu(f, input_area, rows);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_diffs_get_a_taller_pane_capped() {
        let small = PendingApproval::Schematic {
            added: vec!["U1".into()],
            removed: vec![],
            changed: vec![],
            nets_before: 0,
            nets_after: 1,
        };
        assert_eq!(approval_height(&small, 80), 3);
        let big = PendingApproval::Schematic {
            added: (0..60).map(|i| format!("LONG_REF_{i}")).collect(),
            removed: vec![],
            changed: vec![],
            nets_before: 0,
            nets_after: 60,
        };
        assert_eq!(
            approval_height(&big, 24),
            5,
            "capped so it can't eat the transcript"
        );
    }

    #[test]
    fn operation_approval_names_the_tool_and_arguments() {
        let pending = PendingApproval::Operation {
            operation: "move_parts".into(),
            arguments: serde_json::json!({
                "moves": [{"reference": "U1", "by": [1, 2]}]
            }),
        };

        let preview = approval_preview_text(&pending);

        assert!(preview.contains("move_parts"), "{preview}");
        assert!(preview.contains("U1"), "{preview}");
        assert!(!preview.contains("no component changes"), "{preview}");
    }
}
