//! Markdown rendering for assistant prose, built on `pulldown-cmark` (a real
//! CommonMark parser — correct nesting, escapes, and edge cases). This module
//! owns only the **terminal styling policy**: which ratatui styles each
//! construct gets, and which lines must preserve whitespace.
//!
//! [`render_markdown`] turns text into **logical lines** of styled segments;
//! the renderer wraps them to the viewport ([`super::ui`]), so nothing here
//! needs to know the terminal width.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};

use crate::tui::theme;

/// How the renderer may wrap a logical line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapMode {
    /// Prose: wrap at word boundaries, collapsing runs of whitespace.
    Word,
    /// Code: hard-wrap by character, preserving every space.
    Preserve,
}

/// What kind of block a logical line belongs to, so the renderer can frame it
/// (a fenced code block gets a slate background and a gutter rule; prose does
/// not).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LineKind {
    /// Ordinary prose / list / quote line.
    #[default]
    Prose,
    /// A line inside a fenced code block. `lang` is `Some` only on the opening
    /// row, carrying the fence's info string (e.g. `yaml`) for a dim label.
    Code { lang: Option<String> },
}

/// One logical (unwrapped) line: styled segments, its wrap mode, and its block
/// kind (so the renderer can frame code blocks distinctly).
#[derive(Clone, Debug, PartialEq)]
pub struct MdLine {
    pub segments: Vec<(String, Style)>,
    pub wrap: WrapMode,
    pub kind: LineKind,
}

/// Render markdown text into logical lines styled relative to `base`
/// (the speaker's body style).
pub fn render_markdown(text: &str, base: Style) -> Vec<MdLine> {
    let mut r = Renderer::new(base);
    for ev in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH) {
        r.event(ev);
    }
    r.finish()
}

/// Inline `code` takes the structural accent so it reads as a token in prose.
fn inline_code_style(base: Style) -> Style {
    base.patch(theme::INLINE_CODE)
}

/// Fenced-block code is plain text on a raised surface — the block's background
/// already marks it as code, so recolouring every line would be noise.
fn block_code_style(base: Style) -> Style {
    base.patch(theme::CODE)
}

/// Walks the pulldown-cmark event stream, accumulating styled segments into
/// the line being built and flushing it on block/line boundaries.
struct Renderer {
    base: Style,
    lines: Vec<MdLine>,
    /// Segments of the line being built.
    current: Vec<(String, Style)>,
    /// Whether the current line has been started (its prefix laid down).
    line_open: bool,
    /// One blank separator is owed before the next top-level block.
    sep_pending: bool,
    // Inline state (counters: constructs can nest).
    bold: usize,
    italic: usize,
    strike: usize,
    link: usize,
    heading: bool,
    code_block: bool,
    /// The fenced block's info string (language), pending until its first row is
    /// emitted; `None` thereafter so only the opening row carries the label.
    code_lang: Option<String>,
    quote_depth: usize,
    /// Open lists; `Some(n)` is the next ordered-item number.
    lists: Vec<Option<u64>>,
}

impl Renderer {
    fn new(base: Style) -> Self {
        Self {
            base,
            lines: Vec::new(),
            current: Vec::new(),
            line_open: false,
            sep_pending: false,
            bold: 0,
            italic: 0,
            strike: 0,
            link: 0,
            heading: false,
            code_block: false,
            code_lang: None,
            quote_depth: 0,
            lists: Vec::new(),
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) if self.code_block => self.code_text(&t),
            Event::Text(t) => self.push_text(&t),
            Event::Code(t) => {
                self.ensure_line();
                self.current.push((t.to_string(), inline_code_style(self.base)));
            }
            Event::SoftBreak | Event::HardBreak => {
                self.flush_line();
                self.begin_line();
                if !self.lists.is_empty() {
                    // Hang the continuation under the item's marker.
                    self.current
                        .push(("  ".repeat(self.lists.len()), self.base));
                }
            }
            Event::Rule => {
                self.block_start();
                self.ensure_line();
                self.current
                    .push(("─".repeat(24), theme::RULE));
                self.flush_line();
                self.mark_sep();
            }
            // Raw HTML in chat prose: show it literally.
            Event::Html(t) | Event::InlineHtml(t) => self.push_text(&t),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                self.block_start();
                self.begin_line();
            }
            Tag::Heading { .. } => {
                self.block_start();
                self.heading = true;
                self.begin_line();
            }
            Tag::BlockQuote(_) => {
                self.block_start();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.block_start();
                self.code_block = true;
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let lang = info.split_whitespace().next().unwrap_or("");
                        (!lang.is_empty()).then(|| lang.to_string())
                    }
                    CodeBlockKind::Indented => None,
                };
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block_start();
                } else {
                    // A nested list interrupts its parent item's line.
                    self.flush_line();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.begin_line();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.current
                    .push((format!("{}{marker}", "  ".repeat(depth)), self.base));
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { .. } => self.link += 1,
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.mark_sep();
            }
            TagEnd::Heading(_) => {
                self.heading = false;
                self.flush_line();
                self.mark_sep();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.mark_sep();
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.code_block = false;
                self.mark_sep();
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.lists.pop();
                self.mark_sep();
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.link = self.link.saturating_sub(1),
            _ => {}
        }
    }

    /// The style inline text takes right now.
    fn style(&self) -> Style {
        let mut s = self.base;
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        // Headings carry the structural accent so they read as structure, not
        // just bold prose; inline **strong** stays plain bold (no recolour).
        if self.heading {
            s = s.patch(theme::HEADING);
        }
        if self.italic > 0 || self.quote_depth > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.link > 0 {
            s = s.add_modifier(Modifier::UNDERLINED);
        }
        s
    }

    fn push_text(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        self.ensure_line();
        self.current.push((t.to_string(), self.style()));
    }

    /// Code-block text: every `\n` terminates a preserve-wrapped row (interior
    /// blank rows included), styled as code.
    fn code_text(&mut self, t: &str) {
        for (i, piece) in t.split('\n').enumerate() {
            if i > 0 {
                self.flush_code_row();
            }
            if !piece.is_empty() {
                self.line_open = true;
                self.current
                    .push((piece.to_string(), block_code_style(self.base)));
            }
        }
    }

    /// Push the pending code row even when blank (blank lines inside a code
    /// block are content). The fence's language rides on the first row only.
    fn flush_code_row(&mut self) {
        self.lines.push(MdLine {
            segments: std::mem::take(&mut self.current),
            wrap: WrapMode::Preserve,
            kind: LineKind::Code {
                lang: self.code_lang.take(),
            },
        });
        self.line_open = false;
    }

    /// Start a fresh line: lay down the blockquote gutter, if any.
    fn begin_line(&mut self) {
        self.line_open = true;
        if self.quote_depth > 0 {
            self.current
                .push(("▌ ".repeat(self.quote_depth), theme::QUOTE_GUTTER));
        }
    }

    fn ensure_line(&mut self) {
        if !self.line_open {
            self.begin_line();
        }
    }

    /// Close the line being built, if one is open.
    fn flush_line(&mut self) {
        if !self.line_open && self.current.is_empty() {
            return;
        }
        let (wrap, kind) = if self.code_block {
            (
                WrapMode::Preserve,
                LineKind::Code {
                    lang: self.code_lang.take(),
                },
            )
        } else {
            (WrapMode::Word, LineKind::Prose)
        };
        self.lines.push(MdLine {
            segments: std::mem::take(&mut self.current),
            wrap,
            kind,
        });
        self.line_open = false;
    }

    /// A new block begins: pay out the owed blank separator.
    fn block_start(&mut self) {
        if self.sep_pending && !self.lines.is_empty() {
            self.lines.push(MdLine {
                segments: vec![(String::new(), self.base)],
                wrap: WrapMode::Word,
                kind: LineKind::Prose,
            });
        }
        self.sep_pending = false;
    }

    /// A top-level block ended: owe a blank line before the next one.
    fn mark_sep(&mut self) {
        if self.lists.is_empty() && self.quote_depth == 0 {
            self.sep_pending = true;
        }
    }

    fn finish(mut self) -> Vec<MdLine> {
        self.flush_line();
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Style {
        Style::default()
    }

    /// Concatenated plain text of a line (markup stripped).
    fn text_of(l: &MdLine) -> String {
        l.segments.iter().map(|(t, _)| t.as_str()).collect()
    }

    fn texts(lines: &[MdLine]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    #[test]
    fn plain_text_passes_through() {
        let lines = render_markdown("hello world", base());
        assert_eq!(texts(&lines), vec!["hello world"]);
        assert_eq!(lines[0].wrap, WrapMode::Word);
    }

    #[test]
    fn bold_and_code_are_styled_and_unwrapped_of_markers() {
        let lines = render_markdown("use **R1** and `10k`", base());
        let l = &lines[0];
        assert_eq!(text_of(l), "use R1 and 10k");
        let bold = l.segments.iter().find(|(t, _)| t == "R1").unwrap();
        assert!(bold.1.add_modifier.contains(Modifier::BOLD));
        let code = l.segments.iter().find(|(t, _)| t == "10k").unwrap();
        assert_eq!(code.1.fg, Some(theme::INFO), "inline code takes the structural accent");
    }

    #[test]
    fn italic_is_styled() {
        let lines = render_markdown("an *important* note", base());
        let seg = lines[0]
            .segments
            .iter()
            .find(|(t, _)| t == "important")
            .unwrap();
        assert!(seg.1.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn nested_emphasis_combines_modifiers() {
        // The hand-rolled parser couldn't do this; CommonMark can.
        let lines = render_markdown("**bold with *both* in it**", base());
        let seg = lines[0].segments.iter().find(|(t, _)| t == "both").unwrap();
        assert!(seg.1.add_modifier.contains(Modifier::BOLD));
        assert!(seg.1.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn escapes_render_literally() {
        let lines = render_markdown(r"a \*literal\* star", base());
        assert_eq!(text_of(&lines[0]), "a *literal* star");
    }

    #[test]
    fn unterminated_markers_stay_literal() {
        let lines = render_markdown("3 * 4 and a `tick", base());
        assert_eq!(text_of(&lines[0]), "3 * 4 and a `tick");
    }

    #[test]
    fn headings_are_accented_bold_distinct_from_inline_strong() {
        let lines = render_markdown("## Power section", base());
        assert_eq!(text_of(&lines[0]), "Power section");
        let h = lines[0].segments[0].1;
        assert!(h.add_modifier.contains(Modifier::BOLD), "heading is bold");
        assert_eq!(h.fg, Some(theme::INFO), "heading carries the accent");

        // Inline **strong** is plain bold — no recolour — so the two are distinct.
        let strong = render_markdown("a **bold** word", base());
        let seg = strong[0]
            .segments
            .iter()
            .find(|(t, _)| t == "bold")
            .unwrap();
        assert!(seg.1.add_modifier.contains(Modifier::BOLD));
        assert_ne!(
            seg.1.fg,
            Some(theme::INFO),
            "strong stays plain, not accented"
        );
    }

    #[test]
    fn bullets_normalize_to_dots_and_keep_indent() {
        let lines = render_markdown("- top\n  - nested", base());
        assert_eq!(text_of(&lines[0]), "• top");
        assert_eq!(text_of(&lines[1]), "  • nested");
    }

    #[test]
    fn ordered_lists_keep_their_numbers() {
        let lines = render_markdown("1. first\n2. second", base());
        assert_eq!(text_of(&lines[0]), "1. first");
        assert_eq!(text_of(&lines[1]), "2. second");
    }

    #[test]
    fn blockquotes_get_a_bar_gutter() {
        let lines = render_markdown("> careful", base());
        assert!(text_of(&lines[0]).starts_with("▌ "));
        assert!(text_of(&lines[0]).contains("careful"));
    }

    #[test]
    fn code_fences_hide_markers_and_preserve_content() {
        let lines = render_markdown("before\n```yaml\nnets:\n  VCC: [U1.1]\n```\nafter", base());
        // Blocks are separated by blank lines, like any markdown renderer.
        assert_eq!(
            texts(&lines),
            vec!["before", "", "nets:", "  VCC: [U1.1]", "", "after"]
        );
        assert_eq!(
            lines[2].wrap,
            WrapMode::Preserve,
            "code is preserve-wrapped"
        );
        assert_eq!(
            lines[3].segments[0].1.fg,
            Some(theme::FG),
            "fenced code is plain text on the raised surface"
        );
        // The fence language rides the opening code row only.
        assert_eq!(
            lines[2].kind,
            LineKind::Code {
                lang: Some("yaml".into())
            }
        );
        assert_eq!(lines[3].kind, LineKind::Code { lang: None });
    }

    #[test]
    fn blank_lines_inside_code_blocks_survive() {
        let lines = render_markdown("```\na\n\nb\n```", base());
        assert_eq!(texts(&lines), vec!["a", "", "b"]);
        assert_eq!(lines[1].wrap, WrapMode::Preserve);
    }

    #[test]
    fn unclosed_fence_styles_the_remainder_as_code() {
        let lines = render_markdown("```\nlet x = 1;", base());
        assert_eq!(texts(&lines), vec!["let x = 1;"]);
        assert_eq!(lines[0].wrap, WrapMode::Preserve);
    }

    #[test]
    fn paragraphs_are_separated_by_a_blank_line() {
        let lines = render_markdown("a\n\nb", base());
        assert_eq!(texts(&lines), vec!["a", "", "b"]);
    }

    #[test]
    fn soft_breaks_keep_the_authors_line_breaks() {
        let lines = render_markdown("line one\nline two", base());
        assert_eq!(texts(&lines), vec!["line one", "line two"]);
    }
}
