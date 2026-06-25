//! The frame furniture around the transcript and composer: the [`draw_header`]
//! brand/status row, the [`draw_status`] footer, the [`draw_running`] in-flight
//! indicator, and the [`draw_help`] overlay.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};

use super::super::app::App;
use super::{MARGIN, body, fmt_tokens};

/// The running-indicator spinner. Quadrant blocks (U+2596…U+259F) are far more
/// widely covered than braille, so they animate cleanly instead of rendering as
/// tofu boxes in fonts that lack the braille range.
const SPINNER: [&str; 4] = ["▘", "▝", "▗", "▖"];

/// Nominal context window used for the status-bar percentage (Claude-class
/// models on Bedrock).
const CONTEXT_WINDOW_TOKENS: u64 = 200_000;

pub(super) fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let dot = if app.status.kicad_connected {
        "●"
    } else {
        "○"
    };
    let area = body(area);
    let dim = Style::default().fg(Color::DarkGray);
    let sep = || Span::styled("  ·  ", dim);
    let mut spans = vec![
        // The brand carries the accent; everything else is metadata, so it dims.
        Span::styled(
            "Gordian",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(
            file_name(&app.status.sch_path),
            Style::default().fg(Color::Gray),
        ),
        sep(),
        Span::styled(
            format!("KiCAD {dot}"),
            Style::default().fg(if app.status.kicad_connected {
                Color::Green
            } else {
                Color::Red
            }),
        ),
    ];
    // `auto` only appears when it's ON — the default (off) is silent, not chrome.
    if app.auto {
        spans.push(sep());
        spans.push(Span::styled("auto on", Style::default().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    // The `↑n` scrolled-back indicator sits at the header's right edge, overlaid
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

/// The running indicator that replaces the old transcript-title spinner: an
/// animated frame, elapsed seconds, the output tokens streamed this turn, and
/// the interrupt hint, plus a second detail row naming the tool now executing.
/// Drawn only while a turn is in flight.
pub(super) fn draw_running(f: &mut Frame, area: Rect, app: &App) {
    let frame = SPINNER[app.spinner % SPINNER.len()];
    let secs = app.turn_elapsed_secs().unwrap_or(0);
    let dim = Style::default().fg(Color::DarkGray);
    // While an approval gate holds the turn the verb says so, and the elapsed
    // clock is already frozen (see `App::turn_elapsed_secs`); else it's "working".
    let gated = app.pending.is_some();
    let verb = if gated {
        "waiting for approval"
    } else {
        "working"
    };
    let mut spans = vec![
        Span::styled(
            format!("{frame} "),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(verb, Style::default().fg(Color::Yellow)),
        Span::styled(format!(" · {secs}s"), dim),
    ];
    let toks = app.turn_output_tokens();
    if toks > 0 {
        spans.push(Span::styled(format!(" · ↓{} tok", fmt_tokens(toks)), dim));
    }
    if !gated {
        spans.push(Span::styled(" · esc to interrupt", dim));
    }

    let mut lines = vec![Line::from(spans)];
    // The detail row names the tool now executing, so a long call reads as
    // progress rather than a stall.
    if let Some(tool) = &app.active_tool {
        lines.push(Line::from(vec![
            Span::styled("  ↳ ", dim),
            Span::styled(
                format!("{tool}…"),
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(lines), body(area));
}

pub(super) fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let area = body(area);
    // The right side only carries a hint that isn't already on screen. A pending
    // change shows its actions on the card, so the footer stays quiet there.
    let right = if app.ctrl_c_armed {
        "Ctrl-C again to quit"
    } else if app.running {
        "Ctrl-C Ctrl-C quit"
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
    let text = if width <= right_len {
        right.chars().take(width).collect::<String>()
    } else {
        let avail = width - right_len; // columns to the left of the hint
        let left = status_left(app, avail);
        let pad = avail.saturating_sub(left.chars().count());
        format!("{left}{}{right}", " ".repeat(pad))
    };
    // A recessed footer (dim, no bar) rather than a heavy inverted band — it
    // carries the cost/context HUD and hints without competing with the transcript.
    let para = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(para, area);
}

/// Build the footer's left run — model identity plus the live TOKEN / COST /
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
    if l.total_tokens() > 0 {
        fields.push(format!(
            "{} tok ({}/{})",
            fmt_tokens(l.total_tokens()),
            fmt_tokens(l.input + l.cache_read + l.cache_write),
            fmt_tokens(l.output),
        ));
    }
    if let Some(cost) = l.cost(&s.model) {
        fields.push(format!("${cost:.2}"));
    } else if l.total_tokens() > 0 {
        // Priced model unknown — show a placeholder, never a wrong number.
        fields.push("—".into());
    }
    // A small "cached" badge when the cache is doing non-trivial work (showcases
    // the prompt-caching win). Threshold avoids noise on tiny warmups.
    if l.cache_read >= 1_000 {
        fields.push(format!("⚡{} cached", fmt_tokens(l.cache_read)));
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

pub(super) fn draw_help(f: &mut Frame, area: Rect) {
    let accent = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    // A key/description row: the key in accent, the description in soft gray.
    let kv = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!("{k:<15} "), Style::default().fg(Color::Cyan)),
            Span::styled(d.to_string(), Style::default().fg(Color::Gray)),
        ])
    };
    let section = |t: &str| {
        Line::from(Span::styled(
            t.to_string(),
            dim.add_modifier(Modifier::BOLD),
        ))
    };

    let mut lines = vec![
        section("KEYS"),
        kv("Enter", "send the prompt"),
        kv("Shift/Alt-Enter", "newline (multi-line prompt)"),
        kv("Tab", "complete a /command"),
        kv("a / r", "approve / reject a proposed change"),
        kv("Up / Down", "recall prompt history"),
        kv("PgUp / PgDn", "jump the transcript by a screenful"),
        kv("Ctrl-U/W/A/E", "line editing (kill line/word, home/end)"),
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
    lines.push(Line::from(Span::styled("Esc to close", dim)));

    // Wide enough that key/description rows never wrap (longest desc + key col +
    // border + horizontal padding), so the height stays exact. Tight vertical
    // padding keeps every row on screen even on a short (24-row) terminal.
    let w = 68u16.min(area.width.saturating_sub(2 * MARGIN));
    let h = (lines.len() as u16 + 2).min(area.height); // +border; use the full height if needed
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(dim)
                    .padding(Padding::horizontal(2))
                    .title(Span::styled(" help · keys & commands ", accent)),
            )
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
