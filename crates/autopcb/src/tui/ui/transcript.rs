//! The scrollable chat pane: [`draw_transcript`] lays the entries out and
//! [`render_entry`] styles one entry into wrapped [`Line`]s (markdown for
//! assistant prose, recessed cards for tool/system lines). [`wrap_segments`] is
//! the styled word-wrap that keeps the scroll arithmetic exact in visual rows.
//! The first-launch [`draw_welcome`] splash lives here too, since it fills this
//! pane until the first turn.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Entry, NoticeLevel, Speaker};
use super::super::md::{self, LineKind, MdLine, WrapMode};
use super::body;

pub(super) fn draw_transcript(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = body(area);
    app.viewport_h = inner.height;
    // Before the first real exchange, fill the pane with a welcome splash rather
    // than leaving it blank (the Codex first-launch idiom).
    let started = app
        .transcript
        .iter()
        .any(|e| matches!(e.speaker, Speaker::User | Speaker::Assistant));
    if !started {
        draw_welcome(f, inner);
        return;
    }

    let body_w = inner.width.max(1) as usize;
    // Thread the previous speaker so a turn gets one blank row of rhythm at its
    // boundaries — before a new user/assistant turn, the first tool card of a
    // run, and the turn-summary line — while consecutive same-class lines stay
    // tight.
    let mut prev: Option<Speaker> = None;
    let mut lines: Vec<Line> = Vec::new();
    for (i, e) in app.transcript.iter().enumerate() {
        if gap_above(prev, e.speaker) {
            lines.push(Line::from(""));
        }
        // The entry still being streamed gets a trailing cursor so live prose
        // reads as in-flight; it word-wraps with the text tail and vanishes the
        // moment the turn finalizes the entry.
        if app.live_assistant == Some(i) {
            let mut live = e.clone();
            live.text.push('▌');
            lines.extend(render_entry(&live, body_w));
        } else {
            lines.extend(render_entry(e, body_w));
        }
        prev = Some(e.speaker);
    }

    let total = lines.len() as u16;
    // scroll == 0 follows the tail; larger scrolls back into history. Clamp it
    // so over-scrolling never leaves the viewport stuck above the content.
    let max_top = total.saturating_sub(inner.height);
    app.scroll = app.scroll.min(max_top);
    let top = max_top - app.scroll;

    let para = Paragraph::new(lines).scroll((top, 0));
    f.render_widget(para, inner);
}

/// The first-launch splash, shown in the transcript pane until the first turn:
/// the brand, a tagline, a few example prompts, and the key hints — vertically
/// centred so an empty cockpit feels intentional rather than blank.
fn draw_welcome(f: &mut Frame, area: Rect) {
    let accent = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let caret = Style::default().fg(Color::Cyan);
    let example = |s: &'static str| {
        Line::from(vec![Span::styled("    › ", caret), Span::styled(s, Style::default())])
    };
    let lines = vec![
        Line::from(Span::styled("auto-pcb", accent)),
        Line::from(Span::styled(
            "the schematic & PCB design copilot",
            dim,
        )),
        Line::from(""),
        Line::from(Span::styled("  Try:", dim)),
        example("design a 3.3V LDO regulator with input and output caps"),
        example("add a USB-C connector with CC pull-down resistors"),
        example("lay out and route the PCB for this schematic"),
        Line::from(""),
        Line::from(Span::styled(
            "  /help for commands  ·  Tab completes  ·  /auto toggles auto-apply",
            dim,
        )),
    ];

    // Centre vertically; the splash shares the header's left edge (its area is
    // already inset by the body margin — no extra indent).
    let h = lines.len() as u16;
    let top = area.y + area.height.saturating_sub(h) / 2;
    let block = Rect {
        x: area.x,
        y: top,
        width: area.width,
        height: h.min(area.height),
    };
    f.render_widget(Paragraph::new(lines), block);
}

/// Style one transcript entry into wrapped `Line`s, in the Codex idiom: a blank
/// line opens each turn (user / assistant), the speaker's marker sits on the
/// first row with a hanging continuation indent under it, assistant prose is
/// markdown, and tool / system lines recede (dim, italic) so they read as
/// sub-steps of the turn above them.
///
/// Whether a blank rhythm row belongs *before* an entry of class `cur` that
/// follows one of class `prev` (`None` = top of the transcript): a new
/// user/assistant turn, the first tool card of a run, and the turn-summary line
/// all open with a gap; consecutive same-class lines stay tight.
fn gap_above(prev: Option<Speaker>, cur: Speaker) -> bool {
    let Some(prev) = prev else { return false };
    match cur {
        Speaker::User => true,
        Speaker::Assistant => prev != Speaker::Assistant,
        Speaker::Tool => prev != Speaker::Tool,
        Speaker::System => matches!(prev, Speaker::Assistant | Speaker::Tool),
    }
}

/// `first` is the row-0 marker, `cont` the indent repeated on wrapped rows.
fn render_entry(e: &Entry, width: usize) -> Vec<Line<'static>> {
    // (first_marker, cont_marker, marker_style, body_style, markdown) — the
    // inter-entry blank is owned by `draw_transcript` (see `gap_above`).
    let (first, cont, marker_style, body_style, markdown) = match e.speaker {
        // The user's turn: a cyan caret and bold text — the one thing the eye
        // should land on when scanning back through the transcript.
        Speaker::User => (
            "› ",
            "  ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            Style::default().add_modifier(Modifier::BOLD),
            false,
        ),
        // Assistant prose: plain markdown at a blank 2-col gutter, aligned under
        // the user's text. No bullet — the user's caret alone marks the turns, so
        // the transcript stays lean (the Codex idiom).
        Speaker::Assistant => ("  ", "  ", Style::default(), Style::default(), true),
        // Tool calls cluster under the assistant turn and recede.
        Speaker::Tool => (
            "  ▸ ",
            "    ",
            Style::default().fg(Color::DarkGray),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            false,
        ),
        Speaker::System => {
            // A notice reads as a callout: a level glyph leads it, the text takes
            // the level colour, and a warn/error gets a colored left rule on every
            // row so failures stand out from the recessed metadata around them.
            let (glyph, color) = match e.level {
                NoticeLevel::Plain => ("• ", Color::DarkGray),
                NoticeLevel::Success => ("✓ ", Color::Green),
                NoticeLevel::Warn => ("⚠ ", Color::Yellow),
                NoticeLevel::Error => ("✗ ", Color::Red),
            };
            let body = match e.level {
                NoticeLevel::Plain | NoticeLevel::Success => {
                    Style::default().fg(color).add_modifier(Modifier::DIM)
                }
                _ => Style::default().fg(color),
            };
            // Continuation rows of a loud notice keep a colored rule; quiet ones
            // just indent under the glyph.
            let loud = matches!(e.level, NoticeLevel::Warn | NoticeLevel::Error);
            let cont = if loud { "▌ " } else { "  " };
            (glyph, cont, Style::default().fg(color), body, false)
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
                kind: LineKind::Prose,
            })
            .collect()
    };

    // Warn/error notices get a one-cell background tint so the whole line reads
    // as a callout band, not just a colored glyph.
    let notice_tint = match (e.speaker, e.level) {
        (Speaker::System, NoticeLevel::Error) => Some(Color::Rgb(58, 30, 36)),
        (Speaker::System, NoticeLevel::Warn) => Some(Color::Rgb(54, 46, 28)),
        _ => None,
    };

    let mut lines = Vec::new();
    let mut first_row = true;
    for ml in &logical {
        let code = matches!(ml.kind, LineKind::Code { .. });
        let mut logical_first = true;
        for row in wrap_segments(&ml.segments, body_w, ml.wrap == WrapMode::Preserve) {
            let mut spans = if code {
                // The language label rides the code block's opening row only,
                // which is the only logical line carrying `lang`.
                code_row_spans(ml, row, logical_first)
            } else {
                let marker = if first_row { first } else { cont };
                let mut s = vec![Span::styled(marker, marker_style)];
                s.extend(row);
                s
            };
            logical_first = false;
            if let Some(bg) = notice_tint {
                spans = spans
                    .into_iter()
                    .map(|s| {
                        let st = s.style.bg(bg);
                        Span::styled(s.content, st)
                    })
                    .collect();
            }
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

/// Frame one wrapped row of a fenced code block: a slate background across the
/// row, a DarkGray left rule in place of the speaker gutter, and — on the
/// opening row — a dim language label so the block reads as code without a
/// boxed container.
fn code_row_spans(ml: &MdLine, row: Vec<Span<'static>>, opening: bool) -> Vec<Span<'static>> {
    const SLATE: Color = Color::Rgb(33, 36, 51);
    let bg = |st: Style| st.bg(SLATE);
    let mut spans = vec![Span::styled("▎ ", bg(Style::default().fg(Color::DarkGray)))];
    for s in row {
        spans.push(Span::styled(s.content, bg(s.style)));
    }
    if opening {
        if let LineKind::Code { lang: Some(lang) } = &ml.kind {
            spans.push(Span::styled(
                format!("  {lang}"),
                bg(Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)),
            ));
        }
    }
    spans
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
