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
use ratatui::widgets::{Paragraph, StatefulWidget};
use ratatui_image::{Resize, StatefulImage};

use super::super::app::{App, Entry, ImageState, NoticeLevel, Speaker};
use super::super::md::{self, LineKind, MdLine, WrapMode};
use super::{RenderCtx, body};

/// The most rows an inline image preview may occupy, so a render can never eat the
/// viewport (mirrors the apply-gate's `diff_height` cap).
const MAX_IMAGE_ROWS: u16 = 20;

/// One vertical band of the transcript document: a run of styled text rows, or an
/// inline image (indexed into [`App::images`]) with the row height it occupies.
enum Block {
    Text(Vec<Line<'static>>),
    Image { idx: usize, rows: u16 },
}

impl Block {
    fn height(&self) -> u16 {
        match self {
            Block::Text(lines) => lines.len() as u16,
            Block::Image { rows, .. } => *rows,
        }
    }
}

pub(super) fn draw_transcript(f: &mut Frame, area: Rect, app: &mut App, ctx: &mut RenderCtx) {
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
    let blocks = layout_blocks(app, body_w, ctx);

    // Total document height in rows; scroll == 0 follows the tail, larger scrolls
    // back into history. Clamp so over-scrolling never strands the viewport above
    // the content.
    let total: u16 = blocks.iter().map(Block::height).sum();
    let max_top = total.saturating_sub(inner.height);
    app.scroll = app.scroll.min(max_top);
    let top = max_top - app.scroll; // first visible document row
    let bottom = top + inner.height; // one past the last visible row

    // Walk the blocks top-to-bottom, rendering each one's intersection with the
    // visible window into its own sub-rect — the per-row math stays exact, and an
    // image rides in its own stacked rect (it can't live in a `Line`).
    let mut doc_y = 0u16;
    for block in &blocks {
        let h = block.height();
        let (b_start, b_end) = (doc_y, doc_y + h);
        doc_y = b_end;
        // Skip blocks entirely outside the window.
        if b_end <= top || b_start >= bottom {
            continue;
        }
        let visible_start = b_start.max(top);
        let visible_end = b_end.min(bottom);
        let screen_y = inner.y + (visible_start - top);
        let rect = Rect {
            x: inner.x,
            y: screen_y,
            width: inner.width,
            height: visible_end - visible_start,
        };
        match block {
            Block::Text(lines) => {
                let skip = visible_start - b_start;
                f.render_widget(Paragraph::new(lines.clone()).scroll((skip, 0)), rect);
            }
            Block::Image { idx, .. } => draw_image(f, rect, app, *idx, ctx),
        }
    }
}

/// Build the interleaved [`Block`] document: text entries wrapped into rows (with
/// the inter-turn rhythm and the live-stream cursor), broken by image previews
/// pinned after their transcript position.
fn layout_blocks(app: &App, body_w: usize, ctx: &RenderCtx) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut text: Vec<Line<'static>> = Vec::new();
    let mut prev: Option<Speaker> = None;
    let mut img = 0usize; // next un-emitted image (images are sorted by `after`)

    let mut flush_images_after = |n: usize, blocks: &mut Vec<Block>, text: &mut Vec<Line<'static>>| {
        while img < app.images.len() && app.images[img].after <= n {
            if !text.is_empty() {
                blocks.push(Block::Text(std::mem::take(text)));
            }
            let rows = image_rows(&app.images[img], body_w, ctx);
            blocks.push(Block::Image { idx: img, rows });
            img += 1;
        }
    };

    for (i, e) in app.transcript.iter().enumerate() {
        // Emit any images pinned at this transcript position (after == i) before
        // the entry that now sits at index i.
        flush_images_after(i, &mut blocks, &mut text);
        if gap_above(prev, e.speaker) {
            text.push(Line::from(""));
        }
        // The entry still being streamed gets a trailing cursor so live prose
        // reads as in-flight; it word-wraps with the text tail and vanishes the
        // moment the turn finalizes the entry.
        if app.live_assistant == Some(i) {
            let mut live = e.clone();
            live.text.push('▌');
            text.extend(render_entry(&live, body_w));
        } else {
            text.extend(render_entry(e, body_w));
        }
        prev = Some(e.speaker);
    }
    // Trailing images pinned at or after the transcript tail.
    flush_images_after(app.transcript.len(), &mut blocks, &mut text);
    if !text.is_empty() {
        blocks.push(Block::Text(text));
    }
    blocks
}

/// The row height an image preview claims: one row for its caption, plus the
/// image band itself (its pixel aspect scaled to cells via the picker's font
/// size, capped at [`MAX_IMAGE_ROWS`]). With no picker (the screenshot harness or
/// a dumb terminal) it is a single text-label row.
fn image_rows(cell: &super::super::app::ImageCell, body_w: usize, ctx: &RenderCtx) -> u16 {
    let Some(picker) = ctx.picker else {
        return 1; // text-label fallback
    };
    if matches!(cell.state, ImageState::Failed) {
        return 1;
    }
    // Width in cells: the image's own width, capped to the pane. Height follows
    // the pixel aspect, converted px→cells through the cell font size, then capped.
    let (fw, fh) = picker.font_size();
    let cols = (body_w as u16).min(60).max(1);
    let rows = match image_pixel_size(&cell.path) {
        Some((pw, ph)) if pw > 0 && fw > 0 && fh > 0 => {
            let target_px_w = cols as u32 * fw as u32;
            let scaled_h_px = target_px_w * ph / pw;
            (scaled_h_px / fh.max(1) as u32) as u16
        }
        _ => return 1, // unreadable → the text label only
    };
    // +1 for the caption row; clamp the image band into [1, MAX_IMAGE_ROWS].
    1 + rows.clamp(1, MAX_IMAGE_ROWS)
}

/// Read just the pixel dimensions of a PNG without fully decoding it.
fn image_pixel_size(path: &str) -> Option<(u32, u32)> {
    image::image_dimensions(path).ok()
}

/// Render one image cell into `rect`: a dim caption row, then the image (lazily
/// decoded + cached in the cell on first draw). Any failure degrades to the text
/// label — the preview never crashes the UI.
fn draw_image(f: &mut Frame, rect: Rect, app: &mut App, idx: usize, ctx: &mut RenderCtx) {
    // Text-only mode (screenshot harness / dumb terminal): just the stable label.
    let Some(picker) = ctx.picker else {
        let label = app.images[idx].label();
        f.render_widget(text_label(&label, Color::DarkGray), rect);
        return;
    };

    // Lazily build (and cache) the image protocol the first time we draw it.
    if matches!(app.images[idx].state, ImageState::Pending) {
        app.images[idx].state = match decode(picker, &app.images[idx].path) {
            Some(proto) => ImageState::Ready(Box::new(proto)),
            None => ImageState::Failed,
        };
    }

    if matches!(app.images[idx].state, ImageState::Failed) {
        let label = app.images[idx].label();
        f.render_widget(text_label(&label, Color::DarkGray), rect);
        return;
    }

    // A dim caption on row 0; the image fills the rest of the band. The caption
    // is read before the protocol is borrowed mutably (disjoint fields).
    let caption = format!("▸ board preview · {}", app.images[idx].caption);
    let (cap_rect, img_rect) = split_caption(rect);
    f.render_widget(text_label(&caption, Color::Cyan), cap_rect);
    if img_rect.height > 0 {
        if let ImageState::Ready(proto) = &mut app.images[idx].state {
            StatefulImage::default()
                .resize(Resize::Fit(None))
                .render(img_rect, f.buffer_mut(), proto.as_mut());
        }
    }
}

/// A one-line dim text label, the inline-image fallback and caption renderer.
fn text_label(text: &str, color: Color) -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(color).add_modifier(Modifier::DIM),
    )))
}

/// Split an image band into its caption row (row 0) and the image rect below it.
/// When the band is scrolled so only its tail is visible, the caption row may be
/// gone — then the whole rect is image.
fn split_caption(rect: Rect) -> (Rect, Rect) {
    if rect.height <= 1 {
        return (rect, Rect { height: 0, ..rect });
    }
    let cap = Rect { height: 1, ..rect };
    let img = Rect {
        y: rect.y + 1,
        height: rect.height - 1,
        ..rect
    };
    (cap, img)
}

/// Decode a PNG at `path` into a fitted [`StatefulProtocol`] via the picker.
/// `None` on any read/decode error (the caller falls back to the text label).
fn decode(picker: &ratatui_image::picker::Picker, path: &str) -> Option<ratatui_image::protocol::StatefulProtocol> {
    let img = image::ImageReader::open(path).ok()?.decode().ok()?;
    Some(picker.new_resize_protocol(img))
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
