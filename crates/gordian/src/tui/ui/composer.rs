//! The input composer and the panes that cluster around it: the flat input row
//! ([`draw_input`]) and the two popups that float over it — the `/command` completion list
//! ([`draw_completions`]) and the double-Esc unwind picker ([`draw_unwind`]).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::super::app::App;
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
    let label_w = matches
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    let rows: Vec<Line> = matches
        .iter()
        .enumerate()
        .map(|(i, c)| {
            menu_row(
                c.name,
                c.desc,
                label_w,
                input_area.width,
                selected == Some(i),
            )
        })
        .collect();
    draw_menu(f, input_area, rows);
}

/// One full-width menu row: the composer's own left indent, a padded label, then
/// the detail column. Every cell is painted out to the right edge so a selected
/// row reads as one unbroken bar instead of a highlight that stops at the text.
fn menu_row(
    label: &str,
    detail: &str,
    label_w: usize,
    width: u16,
    selected: bool,
) -> Line<'static> {
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
    let focused = app.input_active();
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
        let placeholder = if app.running {
            "agent is working… · Enter queues your next message"
        } else {
            "Ask Gordian anything..."
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(placeholder, theme::META))),
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
