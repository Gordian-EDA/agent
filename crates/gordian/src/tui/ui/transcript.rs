//! The scrollable chat pane: [`draw_transcript`] lays the entries out and
//! [`render_entry`] styles one entry into wrapped [`Line`]s (markdown for
//! assistant prose, recessed cards for tool/system lines). [`wrap_segments`] is
//! the styled word-wrap that keeps the scroll arithmetic exact in visual rows.
//! The first-launch [`draw_welcome`] splash lives here too, since it fills this
//! pane until the first turn.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Entry, ImageCell, NoticeLevel, PreviewZone, Speaker};
use super::super::md::{self, LineKind, MdLine, WrapMode};
use super::super::theme;
use super::body;

const TOP_PADDING_ROWS: u16 = 1;

/// A clickable span's position within one transcript block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreviewLink {
    line: usize,
    col: u16,
    width: u16,
    idx: usize,
}

/// One vertical band of styled transcript rows and its inline preview links.
#[derive(Default)]
struct Block {
    lines: Vec<Line<'static>>,
    links: Vec<PreviewLink>,
}

impl Block {
    fn height(&self) -> u16 {
        self.lines.len() as u16
    }

    fn push_line(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn extend(&mut self, mut other: Self) {
        let line_offset = self.lines.len();
        for link in &mut other.links {
            link.line += line_offset;
        }
        self.lines.append(&mut other.lines);
        self.links.append(&mut other.links);
    }
}

pub(super) fn draw_transcript(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = transcript_body(area);
    app.viewport_h = inner.height;
    // Preview click zones are republished every draw, with the current layout.
    app.preview_zones.clear();
    // Before the first real exchange, fill the pane with a welcome splash rather
    // than leaving it blank (the Codex first-launch idiom).
    let started = app
        .transcript
        .iter()
        .any(|e| matches!(e.speaker, Speaker::User | Speaker::Assistant));
    if !started {
        app.scroll_max = 0;
        draw_welcome(f, inner, app);
        return;
    }

    let body_w = inner.width.max(1) as usize;
    let blocks = layout_blocks(app, body_w);

    // Total document height in rows; scroll == 0 follows the tail, larger scrolls
    // back into history. Clamp so over-scrolling never strands the viewport above
    // the content.
    let total: u16 = blocks.iter().map(Block::height).sum();
    let max_top = total.saturating_sub(inner.height);
    app.scroll_max = max_top;
    app.scroll = app.scroll.min(max_top);
    let top = max_top - app.scroll; // first visible document row
    let bottom = top + inner.height; // one past the last visible row

    // Walk the blocks top-to-bottom, rendering each one's intersection with the
    // visible window into its own sub-rect — the per-row math stays exact.
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
        let skip = visible_start - b_start;
        f.render_widget(Paragraph::new(block.lines.clone()).scroll((skip, 0)), rect);
        for link in &block.links {
            let line = link.line as u16;
            if line < skip || line >= skip + rect.height || link.col >= rect.width {
                continue;
            }
            app.preview_zones.push(PreviewZone {
                x: rect.x + link.col,
                y: rect.y + line - skip,
                width: link.width.min(rect.width - link.col),
                height: 1,
                idx: link.idx,
            });
        }
    }
}

fn transcript_body(area: Rect) -> Rect {
    let inner = body(area);
    if inner.height <= TOP_PADDING_ROWS {
        return inner;
    }
    Rect {
        y: inner.y + TOP_PADDING_ROWS,
        height: inner.height - TOP_PADDING_ROWS,
        ..inner
    }
}

/// Build the transcript document with wrapped text and per-row preview links.
fn layout_blocks(app: &App, body_w: usize) -> Vec<Block> {
    let mut block = Block::default();
    let mut prev: Option<Speaker> = None;

    let mut i = 0usize;
    while i < app.transcript.len() {
        let e = &app.transcript[i];
        if e.speaker == Speaker::Tool {
            if gap_above(prev, e) {
                block.push_line(Line::from(""));
            }

            let mut end = i + 1;
            while end < app.transcript.len() && app.transcript[end].speaker == Speaker::Tool {
                end += 1;
            }
            block.extend(render_tool_group(
                &app.transcript[i..end],
                i,
                &app.images,
                body_w,
            ));
            prev = Some(Speaker::Tool);
            i = end;
            continue;
        }

        if gap_above(prev, e) {
            block.push_line(Line::from(""));
        }
        block.lines.extend(render_entry(e, body_w));
        // The divider is usually followed by the next turn, whose own leading
        // gap already separates the two — but when it's still the newest thing
        // in the transcript (the common moment right after a turn finishes),
        // nothing follows to open that gap, and the rule would otherwise sit
        // flush against the composer below it.
        if is_worked_divider(e) && i + 1 == app.transcript.len() {
            block.push_line(Line::from(""));
        }
        prev = Some(e.speaker);
        i += 1;
    }
    if block.lines.is_empty() {
        Vec::new()
    } else {
        vec![block]
    }
}

/// The first-launch splash, shown in the transcript pane until the first turn:
/// the brand, a tagline, a few example prompts, and the key hints — vertically
/// centred so an empty cockpit feels intentional rather than blank.
/// The schematic's project directory, tildified when it sits under `$HOME` so
/// the welcome line reads as a place rather than a full path.
fn project_dir(sch_path: &str) -> String {
    let dir = std::path::Path::new(sch_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| ".".to_string());
    if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
        && let Ok(rest) = std::path::Path::new(&dir).strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    dir
}

fn draw_welcome(f: &mut Frame, area: Rect, app: &App) {
    let logo = theme::LOGO;
    let wordmark = theme::LOGO;
    let dim = theme::META;
    let caret = Style::default().fg(theme::ACC);
    let example = |s: &'static str| {
        Line::from(vec![
            Span::styled("    ❯ ", caret),
            Span::styled(s, theme::PROSE),
        ])
    };

    let mut lines: Vec<Line<'static>> = [
        "         .",
        "      :++;++:",
        "     ;+     +;",
        "    .+.     ...",
        "    :+. +++++;.",
        " .+  +;       :++",
        ":+:  :+:   :+:  :+:",
        "+;    ..  ++.    ;+",
        "+:;    :++:     ;:;",
        "  .:;;:.   .:;;:.",
    ]
    .into_iter()
    .map(|row| Line::from(Span::styled(row, logo)))
    .collect();
    lines.extend([
        Line::from(""),
        Line::from(Span::styled("Gordian", wordmark)),
        Line::from(vec![
            Span::styled("the schematic & PCB design copilot", theme::SUBTLE),
            Span::styled(
                format!("  ·  {}", project_dir(&app.status.sch_path)),
                theme::META,
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Try:", dim)),
        example("design a 3.3V LDO regulator with input and output caps"),
        example("add a USB-C connector with CC pull-down resistors"),
        example("lay out and route the PCB for this schematic"),
        Line::from(""),
        Line::from(Span::styled("  /help for commands  ·  Tab completes", dim)),
    ]);

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
/// Whether a blank rhythm row belongs *before* `cur` when it follows class
/// `prev` (`None` = top of the transcript): a new user/assistant turn, the first
/// tool cards, loud system notices, and turn-summary lines after
/// assistant/tool output all open with a gap; other consecutive same-class lines
/// stay tight.
fn gap_above(prev: Option<Speaker>, cur: &Entry) -> bool {
    let Some(prev) = prev else { return false };
    match cur.speaker {
        Speaker::User => true,
        Speaker::Assistant => prev != Speaker::Assistant,
        Speaker::Tool => true,
        Speaker::System => {
            matches!(cur.level, NoticeLevel::Error)
                || matches!(prev, Speaker::Assistant | Speaker::Tool)
        }
    }
}

/// Whether `e` renders as the "Worked for Ns" turn-summary rule rather than an
/// ordinary system notice.
fn is_worked_divider(e: &Entry) -> bool {
    e.speaker == Speaker::System
        && e.level == NoticeLevel::Plain
        && e.text.starts_with("Worked for ")
}

/// `first` is the row-0 marker, `cont` the indent repeated on wrapped rows.
fn render_entry(e: &Entry, width: usize) -> Vec<Line<'static>> {
    if is_worked_divider(e) {
        return vec![render_worked_divider(&e.text, width, e.level)];
    }

    // (first_marker, cont_marker, marker_style, body_style, markdown) — the
    // inter-entry blank is owned by `draw_transcript` (see `gap_above`).
    let (first, cont, marker_style, body_style, markdown) = match e.speaker {
        // The user's turn: a cyan caret and bold text — the one thing the eye
        // should land on when scanning back through the transcript.
        Speaker::User => ("❯ ", "  ", theme::USER_CARET, theme::USER, false),
        // Assistant prose: plain markdown at a blank 2-col gutter, aligned under
        // the user's text. No bullet — the user's caret alone marks the turns, so
        // the transcript stays lean (the Codex idiom).
        Speaker::Assistant => ("  ", "  ", theme::PROSE, theme::PROSE, true),
        // Tool calls are grouped by `render_tool_group`; this fallback is only for
        // direct unit use of `render_entry`.
        Speaker::Tool => ("  ", "  ", theme::META, theme::META, false),
        Speaker::System => {
            // A notice reads as a callout: a level glyph leads it, the text takes
            // the level colour, and a warn/error gets a colored left rule on every
            // row so failures stand out from the recessed metadata around them.
            let (glyph, color) = match e.level {
                NoticeLevel::Plain => ("• ", theme::FAINT),
                NoticeLevel::Error => ("✗ ", theme::ERR),
            };
            let body = Style::default().fg(color);
            // Continuation rows of a loud notice keep a colored rule; quiet ones
            // just indent under the glyph.
            let loud = matches!(e.level, NoticeLevel::Error);
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

    // Error notices get a one-cell background tint so the whole line reads
    // as a callout band, not just a colored glyph.
    let notice_tint = match (e.speaker, e.level) {
        (Speaker::System, NoticeLevel::Error) => Some(theme::ERR_BG),
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

fn render_worked_divider(text: &str, width: usize, level: NoticeLevel) -> Line<'static> {
    let (rule, label_style) = match level {
        NoticeLevel::Error => (theme::DANGER, theme::DANGER),
        NoticeLevel::Plain => (theme::RULE, theme::META),
    };
    let label = format!(" {text} ");
    let label_w = label.chars().count();
    if width <= 1 {
        return Line::from(Span::styled("─", rule));
    }
    if width <= label_w + 1 {
        return Line::from(Span::styled(
            label.chars().take(width).collect::<String>(),
            label_style,
        ));
    }
    let right = width.saturating_sub(1 + label_w);
    Line::from(vec![
        Span::styled("─", rule),
        Span::styled(label, label_style),
        Span::styled("─".repeat(right), rule),
    ])
}

fn render_tool_group(
    entries: &[Entry],
    first_entry: usize,
    images: &[ImageCell],
    width: usize,
) -> Block {
    let title = tool_group_title(entries);
    let mut block = Block::default();
    block.push_line(Line::from(vec![
        Span::styled("• ", theme::TOOL_GUTTER),
        Span::styled(title, theme::TOOL_GROUP),
    ]));

    for (idx, e) in entries.iter().enumerate() {
        let last = idx + 1 == entries.len();
        let tool_entry = first_entry + idx;
        let preview = images.iter().position(|cell| cell.tool_entry == tool_entry);
        block.extend(render_tool_row(e, last, width, preview));
    }
    block
}

fn tool_group_title(entries: &[Entry]) -> &'static str {
    let mut title = None;
    for e in entries {
        let (name, _) = split_tool_text(&e.text);
        let candidate = tool_title_for_name(name);
        if title.is_some_and(|seen| seen != candidate) {
            return "Used tools";
        }
        title = Some(candidate);
    }
    title.unwrap_or("Used tools")
}

fn tool_title_for_name(name: &str) -> &'static str {
    match name
        .split_once('_')
        .map(|(prefix, _)| prefix)
        .unwrap_or(name)
    {
        "apply" | "edit" | "update" | "write" => "Updated",
        "check" | "drc" | "erc" | "lint" | "review" | "validate" => "Checked",
        "create" | "make" | "new" => "Created",
        "export" | "save" => "Exported",
        "footprint" | "get" | "list" | "load" | "open" | "read" | "search" => "Searched",
        "place" => "Placed",
        "render" | "screenshot" => "Rendered",
        "route" => "Routed",
        "seed" => "Seeded",
        "summarize" => "Summarized",
        _ => "Explored",
    }
}

fn render_tool_row(e: &Entry, last: bool, width: usize, preview: Option<usize>) -> Block {
    let first_prefix = if last { "  └ " } else { "  ├ " };
    let cont_prefix = if last { "    " } else { "  │ " };
    let (name, detail) = split_tool_text(&e.text);
    let action = human_tool_name(name);
    let mut segments = vec![(action, theme::TOOL_NAME)];
    if preview.is_some() {
        segments.push((" → ".to_string(), theme::SUBTLE));
        segments.push(("preview".to_string(), theme::LINK));
    } else if !detail.is_empty() {
        segments.push((format!(" {detail}"), theme::SUBTLE));
    }

    let body_w = width.saturating_sub(first_prefix.chars().count()).max(1);
    let mut block = Block::default();
    for (row, spans) in wrap_segments(&segments, body_w, false)
        .into_iter()
        .enumerate()
    {
        let prefix = if row == 0 { first_prefix } else { cont_prefix };
        if let Some(idx) = preview {
            let mut col = prefix.chars().count() as u16;
            for span in &spans {
                let width = span.content.chars().count() as u16;
                if span.style == theme::LINK {
                    block.links.push(PreviewLink {
                        line: row,
                        col,
                        width,
                        idx,
                    });
                }
                col += width;
            }
        }
        let mut out = vec![Span::styled(prefix, theme::TOOL_GUTTER)];
        out.extend(spans);
        block.push_line(Line::from(out));
    }
    block
}

fn split_tool_text(text: &str) -> (&str, String) {
    if let Some((name, summary)) = text.split_once(" → ") {
        return (name, format!("→ {summary}"));
    }
    if let Some(name) = text.strip_suffix("(…) running…") {
        return (name, "running...".into());
    }
    (text, String::new())
}

fn human_tool_name(name: &str) -> String {
    let mut out = String::new();
    for (i, part) in name.split('_').filter(|p| !p.is_empty()).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            if i == 0 {
                out.extend(first.to_uppercase());
            } else {
                out.extend(first.to_lowercase());
            }
            out.extend(chars.flat_map(char::to_lowercase));
        }
    }
    if out.is_empty() { name.into() } else { out }
}

/// Frame one wrapped row of a fenced code block: a slate background across the
/// row, a DarkGray left rule in place of the speaker gutter, and — on the
/// opening row — a dim language label so the block reads as code without a
/// boxed container.
fn code_row_spans(ml: &MdLine, row: Vec<Span<'static>>, opening: bool) -> Vec<Span<'static>> {
    let bg = |st: Style| st.bg(theme::BG2);
    let mut spans = vec![Span::styled("▎ ", theme::CODE_GUTTER)];
    for s in row {
        spans.push(Span::styled(s.content, bg(s.style)));
    }
    if opening && let LineKind::Code { lang: Some(lang) } = &ml.kind {
        spans.push(Span::styled(format!("  {lang}"), theme::CODE_LANG));
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
    use ratatui::style::Modifier;

    #[test]
    fn project_dir_is_the_schematics_parent() {
        // Not asserting the `~` form here — it depends on this machine's actual
        // $HOME, which the test can't control without changing process env.
        assert_eq!(
            project_dir("/opt/projects/buck/design.kicad_sch"),
            "/opt/projects/buck"
        );
        assert_eq!(project_dir("design.kicad_sch"), ".");
    }

    /// Plain text of one wrapped row.
    fn row_text(row: &[Span]) -> String {
        row.iter().map(|s| s.content.as_ref()).collect()
    }

    fn plain(s: &str) -> Vec<(String, Style)> {
        vec![(s.to_string(), Style::default())]
    }

    #[test]
    fn transcript_body_keeps_a_top_margin() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let inner = transcript_body(area);
        assert_eq!(inner.y, 1);
        assert_eq!(inner.height, 23);
    }

    #[test]
    fn tool_calls_get_a_gap_even_in_a_run() {
        let tool = Entry::tool("search_symbols -> 5 hits");
        assert!(gap_above(Some(Speaker::Tool), &tool));
    }

    #[test]
    fn tool_group_title_varies_by_tool_kind() {
        assert_eq!(
            tool_group_title(&[Entry::tool("search_symbols → 5 hits")]),
            "Searched"
        );
        assert_eq!(
            tool_group_title(&[Entry::tool("route_board → ok")]),
            "Routed"
        );
        assert_eq!(
            tool_group_title(&[
                Entry::tool("search_symbols → 5 hits"),
                Entry::tool("route_board → ok"),
            ]),
            "Used tools"
        );
    }

    #[test]
    fn tool_group_attaches_preview_to_its_render_row() {
        let entries = [Entry::tool("render_schematic → rendered schematic to PNG")];
        let images = [ImageCell::new(4, "/tmp/renders/schematic.png")];
        let block = render_tool_group(&entries, 4, &images, 80);

        assert_eq!(
            row_text(&block.lines[1].spans),
            "  └ Render schematic → preview"
        );
        assert_eq!(
            block.links,
            vec![PreviewLink {
                line: 1,
                col: 23,
                width: 7,
                idx: 0,
            }]
        );
        assert!(
            !block
                .lines
                .iter()
                .any(|line| row_text(&line.spans).contains("schematic.png"))
        );
        assert_eq!(block.lines[1].spans.last().unwrap().style, theme::LINK);
    }

    #[test]
    fn loud_system_notices_get_a_gap_after_user_messages() {
        let user = Entry::user("hey");
        let error = Entry::notice(NoticeLevel::Error, "Stopped after 0s");
        let plain = Entry::system("agent unavailable");

        assert!(gap_above(Some(user.speaker), &error));
        assert!(!gap_above(Some(user.speaker), &plain));
    }

    #[test]
    fn worked_notice_renders_as_codex_divider() {
        let rows = render_entry(&Entry::notice(NoticeLevel::Plain, "Worked for 1m 38s"), 48);
        assert_eq!(rows.len(), 1);
        let text = row_text(&rows[0].spans);
        assert!(text.starts_with("─ Worked for 1m 38s "), "{text}");
        assert_eq!(text.chars().count(), 48);
    }

    #[test]
    fn worked_error_wraps_instead_of_truncating() {
        let rows = render_entry(
            &Entry::notice(
                NoticeLevel::Error,
                "Worked for 2s — Web stream error for model 'openai/gpt-5.4-nano (adapter: OpenAI)'. Caused by: response body ended unexpectedly",
            ),
            40,
        );
        let text = rows
            .iter()
            .map(|row| row_text(&row.spans))
            .collect::<Vec<_>>()
            .join("\n");
        let flat_text = text.replace('\n', " ");

        assert!(rows.len() > 1, "{text}");
        assert!(text.contains("Caused by:"), "{text}");
        assert!(flat_text.contains("response body"), "{text}");
        assert!(flat_text.contains("ended unexpectedly"), "{text}");
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
