//! The frame furniture around the transcript and composer: the [`draw_status`]
//! footer, the [`draw_running`] in-flight indicator, the [`draw_scroll_indicator`]
//! badge, and the [`draw_help`] overlay.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};

use super::super::app::App;
use super::super::theme;
use super::{MARGIN, body, fmt_tokens};

/// The running-indicator spinner. Quadrant blocks (U+2596…U+259F) are far more
/// widely covered than braille, so they animate cleanly instead of rendering as
/// tofu boxes in fonts that lack the braille range.
const SPINNER: [&str; 4] = ["▘", "▝", "▗", "▖"];

/// Nominal context window used for the status-bar percentage (Claude-class
/// models on Bedrock).
const CONTEXT_WINDOW_TOKENS: u64 = 200_000;

/// A proportional scrollbar in the transcript's right-hand gutter, plus a
/// "jump to latest" hint once the view has left the tail.
///
/// The bar is drawn only when the document is taller than the viewport, so a
/// short conversation keeps a clean edge. Its thumb takes the accent while
/// scrolled back and recedes to [`theme::FAINT`] at the tail, which is the
/// whole signal: *you are not looking at the newest output*.
pub(super) fn draw_scrollbar(f: &mut Frame, area: Rect, app: &App) {
    let (viewport, max_top) = (app.viewport_h, app.scroll_max);
    if max_top == 0 || viewport == 0 {
        return;
    }
    let inner = body(area);
    let at_tail = app.scroll == 0;

    // The gutter column between the text and the terminal edge — the bar never
    // steals a column from the prose.
    let track = Rect {
        x: inner.x + inner.width + 1,
        y: area.y,
        width: 1,
        height: area.height,
    };
    let mut state = ScrollbarState::new(usize::from(max_top))
        .viewport_content_length(usize::from(viewport))
        .position(usize::from(max_top - app.scroll));
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            // No rail: at this end of the surface ramp a track reads as either
            // invisible or as noise, and the thumb alone carries the position.
            .track_symbol(None)
            .thumb_symbol("┃")
            .thumb_style(if at_tail {
                Style::default().fg(theme::FAINT)
            } else {
                Style::default().fg(theme::ACC)
            }),
        track,
        &mut state,
    );

    if !at_tail {
        draw_jump_hint(f, inner);
    }
}

/// The floating "you're behind" affordance, pinned to the bottom-right of the
/// transcript so it never displaces a row of content.
fn draw_jump_hint(f: &mut Frame, inner: Rect) {
    let spans = vec![
        Span::styled(" ↓ ", theme::BAND.patch(Style::default().fg(theme::ACC))),
        Span::styled("End", theme::BAND.patch(theme::POPUP_TITLE)),
        Span::styled(" jump to latest ", theme::BAND.patch(theme::META)),
    ];
    let w: u16 = spans
        .iter()
        .map(|s| s.content.chars().count() as u16)
        .sum();
    if w > inner.width || inner.height == 0 {
        return;
    }
    let pill = Rect {
        x: inner.x + inner.width - w,
        y: inner.y + inner.height - 1,
        width: w,
        height: 1,
    };
    f.render_widget(Clear, pill);
    f.render_widget(Paragraph::new(Line::from(spans)), pill);
}

/// The running indicator that replaces the old transcript-title spinner: an
/// animated frame, elapsed seconds, and the interrupt hint, plus a second detail
/// row naming the active tool or review phase. Drawn only while a turn is in
/// flight.
/// Queued prompts shown under the running indicator, capped so a long queue
/// can't swallow the transcript.
const MAX_QUEUED_ROWS: usize = 3;

/// Rows [`draw_running`] needs at the current queue depth — shared with the
/// layout in `ui::mod` so the reserved space and what actually renders can
/// never drift apart (the [`super::composer::approval_height`] pattern).
pub(super) fn running_rows(app: &App) -> u16 {
    let queued = if app.queued.is_empty() {
        0
    } else {
        let shown = app.queued.len().min(MAX_QUEUED_ROWS);
        let overflow = usize::from(app.queued.len() > MAX_QUEUED_ROWS);
        (shown + overflow) as u16
    };
    1 + u16::from(app.active_work.is_some()) + queued
}

pub(super) fn draw_running(f: &mut Frame, area: Rect, app: &App) {
    let frame = SPINNER[app.spinner % SPINNER.len()];
    let secs = app.turn_elapsed_secs().unwrap_or(0);
    let dim = theme::META;
    // While an approval gate holds the turn the verb says so, and the elapsed
    // clock is already frozen (see `App::turn_elapsed_secs`); else it's "working".
    let gated = app.pending.is_some();
    let verb = if gated {
        "waiting for approval"
    } else {
        "working"
    };
    let mut spans = vec![
        Span::styled(format!("{frame} "), theme::SPINNER),
        Span::styled(verb, Style::default().fg(theme::ACC)),
        Span::styled(format!(" · {secs}s"), dim),
    ];
    if !gated {
        spans.push(Span::styled(" · esc to interrupt", dim));
    }

    let mut lines = vec![Line::from(spans)];
    // The detail row names the current unit of work, so a long call or async
    // review pass reads as progress rather than a stall.
    if let Some(work) = &app.active_work {
        lines.push(Line::from(vec![
            Span::styled("  ↳ ", dim),
            Span::styled(
                format!("{work}…"),
                theme::SUBTLE.add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    // Every Enter pressed mid-turn queues rather than vanishing — list what's
    // waiting, oldest (next to run) first, so a queue is never invisible.
    if !app.queued.is_empty() {
        let inner_w = body(area).width as usize;
        let shown = app.queued.len().min(MAX_QUEUED_ROWS);
        for (i, prompt) in app.queued.iter().take(shown).enumerate() {
            let prefix = format!("  {} ", i + 1);
            let room = inner_w.saturating_sub(prefix.chars().count());
            let text: String = prompt.chars().take(room).collect();
            lines.push(Line::from(vec![
                Span::styled(prefix, dim),
                Span::styled(text, theme::SUBTLE),
            ]));
        }
        if app.queued.len() > MAX_QUEUED_ROWS {
            lines.push(Line::from(Span::styled(
                format!("  +{} more queued", app.queued.len() - MAX_QUEUED_ROWS),
                dim,
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), body(area));
}

pub(super) fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let area = body(area);
    let dim = theme::META;
    let armed_quit = theme::WARNING.add_modifier(Modifier::BOLD);
    // The right side only carries a hint that isn't already on screen. A pending
    // change shows its actions on the card, so the footer stays quiet there.
    let right = if app.ctrl_c_armed {
        "Ctrl-C again to quit"
    } else if app.esc_armed {
        "Esc again to unwind"
    } else {
        ""
    };

    // Compose a bar exactly `width` columns wide: pin `right` to the edge, give
    // `left` the rest, and progressively drop HUD fields (then ellipsize) rather
    // than let the two collide. (Char counts, not bytes — multibyte punctuation.)
    let width = area.width as usize;
    let right_len = right.chars().count();
    let right_style = if app.ctrl_c_armed { armed_quit } else { dim };
    let line = if width <= right_len {
        Line::from(Span::styled(
            right.chars().take(width).collect::<String>(),
            right_style,
        ))
    } else {
        let avail = width - right_len; // columns to the left of the hint
        let left = status_left(app, avail);
        let pad = avail.saturating_sub(left.chars().count());
        Line::from(vec![
            Span::styled(left, dim),
            Span::styled(" ".repeat(pad), dim),
            Span::styled(right, right_style),
        ])
    };
    // A recessed footer (dim, no bar) rather than a heavy inverted band — it
    // carries the cost/context HUD and hints without competing with the transcript.
    let para = Paragraph::new(line);
    f.render_widget(para, area);
}

/// Build the footer's left run — model identity plus the live TOKENS / COST /
/// CONTEXT / elapsed HUD — to fit within `avail` columns. Fields are joined by
/// ` · ` and dropped from the tail (least essential first) until the run fits;
/// the model anchor is ellipsized only as a last resort, matching the rest of
/// the chrome's ellipsis discipline.
fn status_left(app: &App, avail: usize) -> String {
    let s = &app.status;
    let l = &s.ledger;

    // The anchor is always present (ellipsized last); the rest are HUD fields in
    // priority order — earlier fields survive longer as the bar narrows.
    let anchor = format!("{} · {}", s.provider, short_model(&s.model));
    let mut fields: Vec<String> = Vec::new();
    if s.applied_count > 0 {
        fields.push(format!("{} applied", s.applied_count));
    }
    if l.provider_requests > 0 {
        fields.push(format!("{} provider req", l.provider_requests));
    }
    if l.input_tokens() > 0 || l.output > 0 {
        fields.push(format!(
            "in {} / out {}",
            fmt_tokens(l.input_tokens()),
            fmt_tokens(l.output)
        ));
    }
    if let Some(cost) = l.cost(&s.model) {
        fields.push(format!("${cost:.2}"));
    }
    if s.ctx_tokens > 0 {
        let used = s.ctx_tokens.min(CONTEXT_WINDOW_TOKENS);
        let left_pct =
            ((CONTEXT_WINDOW_TOKENS - used) as f64 / CONTEXT_WINDOW_TOKENS as f64 * 100.0).round();
        fields.push(format!("{left_pct:.0}% ctx left"));
    }
    if let Some(secs) = app.turn_elapsed_secs() {
        fields.push(format!("{secs}s"));
    }

    // Drop fields from the tail until the anchor + remaining fields fit, then
    // ellipsize the anchor if even it overflows.
    loop {
        let joined = if fields.is_empty() {
            anchor.clone()
        } else {
            format!("{anchor} · {}", fields.join(" · "))
        };
        if joined.chars().count() <= avail {
            return joined;
        }
        if fields.pop().is_none() {
            // Only the anchor is left and it still overflows — ellipsize it.
            let kept: String = anchor.chars().take(avail.saturating_sub(1)).collect();
            return format!("{kept}…");
        }
    }
}

/// The help overlay, in the same borderless idiom as the completion and
/// unwind menus: a floating panel with no box or title bar, just an indented
/// reference card seated on [`theme::BAND`].
pub(super) fn draw_help(f: &mut Frame, area: Rect) {
    let accent = theme::POPUP_TITLE;
    let dim = theme::META;
    // A key/description row: the key in accent, the description in soft gray,
    // both indented under the heading so the card reads as a list, not a box.
    let indent = " ".repeat(MARGIN as usize);
    let kv = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!("{indent}{k:<15} "), Style::default().fg(theme::INFO)),
            Span::styled(d.to_string(), theme::SUBTLE),
        ])
    };
    let section = |t: &str| {
        Line::from(Span::styled(
            format!("{indent}{t}"),
            dim.add_modifier(Modifier::BOLD),
        ))
    };

    let mut lines = vec![
        Line::from(Span::styled(format!("{indent}help · keys & commands"), accent)),
        Line::from(""),
        section("KEYS"),
        kv("Enter", "send the prompt"),
        kv("Shift/Alt-Enter", "newline (multi-line prompt)"),
        kv("Tab", "complete a /command"),
        kv("a / r", "approve / reject a proposed change"),
        kv("Up / Down", "recall prompt history"),
        kv("Shift-Up/Down", "scroll the transcript one line"),
        kv("Mouse wheel", "scroll the transcript"),
        kv("PgUp / PgDn", "jump the transcript by a screenful"),
        kv("End", "jump to the latest output"),
        kv("Ctrl-U/W/A/E", "line editing (kill line/word, home/end)"),
        kv("Ctrl-Left/Right", "move by word"),
        kv("Esc", "close help / reject gate / clear input"),
        kv("Esc Esc", "unwind the last turn (context only)"),
        kv("Ctrl-C Ctrl-C", "exit"),
        Line::from(""),
        section("COMMANDS"),
    ];
    for c in crate::tui::app::COMMANDS {
        lines.push(kv(c.name, c.desc));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(format!("{indent}Esc to close"), dim)));

    // Full width, like the completion and unwind menus — a narrower centred
    // card left the surrounding transcript visible down both sides with
    // nothing to separate the two, which read as corruption rather than a
    // deliberate margin once the border that used to mark the edge was gone.
    let h = (lines.len() as u16).min(area.height);
    let popup = Rect {
        x: area.x,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: area.width,
        height: h,
    };
    f.render_widget(Clear, popup);
    // `trim: false` — `trim: true` strips each line's LEADING whitespace before
    // wrapping, which would eat the indent along with it.
    f.render_widget(
        Paragraph::new(lines).style(theme::BAND).wrap(Wrap { trim: false }),
        popup,
    );
}

/// A model label for the status bar: the routing prefix stripped, the model's
/// own name left whole. Only the namespace ("us.anthropic.") is noise here —
/// the id itself is the useful part, so it isn't cut down to a fixed word
/// count (that clipped names shorter than the vendor prefix it was meant to
/// remove, e.g. "6-luna" instead of "gpt-6-luna").
fn short_model(model: &str) -> String {
    // e.g. "us.anthropic.claude-opus-4-5-20251101-v1:0" -> "claude-opus-4-5-20251101-v1:0"
    let tail = model.rsplit('.').next().unwrap_or(model);
    if tail.is_empty() {
        model.to_string()
    } else {
        tail.to_string()
    }
}
