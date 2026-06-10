//! Markdown rendering for assistant prose, built on `pulldown-cmark` (a real
//! CommonMark parser — correct nesting, escapes, and edge cases). This module
//! owns only the **terminal styling policy**: which ratatui styles each
//! construct gets, and which lines must preserve whitespace.
//!
//! [`render_markdown`] turns text into **logical lines** of styled segments;
//! the renderer wraps them to the viewport ([`super::ui`]), so nothing here
//! needs to know the terminal width.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};

/// How the renderer may wrap a logical line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapMode {
    /// Prose: wrap at word boundaries, collapsing runs of whitespace.
    Word,
    /// Code: hard-wrap by character, preserving every space.
    Preserve,
}

/// One logical (unwrapped) line: styled segments plus its wrap mode.
#[derive(Clone, Debug, PartialEq)]
pub struct MdLine {
    pub segments: Vec<(String, Style)>,
    pub wrap: WrapMode,
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

fn code_style(base: Style) -> Style {
    base.fg(Color::Cyan)
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
                self.current.push((t.to_string(), code_style(self.base)));
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
                    .push(("─".repeat(24), Style::default().fg(Color::DarkGray)));
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
            Tag::CodeBlock(_) => {
                self.block_start();
                self.code_block = true;
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
        if self.bold > 0 || self.heading {
            s = s.add_modifier(Modifier::BOLD);
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
                    .push((piece.to_string(), code_style(self.base)));
            }
        }
    }

    /// Push the pending code row even when blank (blank lines inside a code
    /// block are content).
    fn flush_code_row(&mut self) {
        self.lines.push(MdLine {
            segments: std::mem::take(&mut self.current),
            wrap: WrapMode::Preserve,
        });
        self.line_open = false;
    }

    /// Start a fresh line: lay down the blockquote gutter, if any.
    fn begin_line(&mut self) {
        self.line_open = true;
        if self.quote_depth > 0 {
            self.current.push((
                "▌ ".repeat(self.quote_depth),
                Style::default().fg(Color::DarkGray),
            ));
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
        let wrap = if self.code_block {
            WrapMode::Preserve
        } else {
            WrapMode::Word
        };
        self.lines.push(MdLine {
            segments: std::mem::take(&mut self.current),
            wrap,
        });
        self.line_open = false;
    }

    /// A new block begins: pay out the owed blank separator.
    fn block_start(&mut self) {
        if self.sep_pending && !self.lines.is_empty() {
            self.lines.push(MdLine {
                segments: vec![(String::new(), self.base)],
                wrap: WrapMode::Word,
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
        assert_eq!(code.1.fg, Some(Color::Cyan));
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
    fn headings_strip_hashes_and_bold() {
        let lines = render_markdown("## Power section", base());
        assert_eq!(text_of(&lines[0]), "Power section");
        assert!(lines[0].segments[0].1.add_modifier.contains(Modifier::BOLD));
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
        assert_eq!(lines[3].segments[0].1.fg, Some(Color::Cyan));
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
