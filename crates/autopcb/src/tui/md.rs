//! Minimal markdown rendering for assistant prose: a few block forms (code
//! fences, headings, bullets, blockquotes) plus inline `code`, **bold** and
//! *italic*. Deliberately not a full CommonMark parser — LLM chat prose only
//! ever uses this small dialect, and a hand-rolled renderer keeps the styled
//! output predictable and the wrapping math in `ui` exact.
//!
//! [`render_markdown`] turns text into **logical lines** of styled segments;
//! the renderer wraps them to the viewport ([`super::ui`]), so nothing here
//! needs to know the terminal width.

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

/// Render markdown-ish text into logical lines styled relative to `base`
/// (the speaker's body style).
pub fn render_markdown(text: &str, base: Style) -> Vec<MdLine> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for raw in text.split('\n') {
        if raw.trim_start().starts_with("```") {
            // The fence markers themselves aren't shown; an unclosed fence
            // (mid-stream text) just styles the rest as code.
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push(MdLine {
                segments: vec![(raw.to_string(), code_style(base))],
                wrap: WrapMode::Preserve,
            });
            continue;
        }
        out.push(block_line(raw, base));
    }
    out
}

/// Classify one non-fence line and parse its inline spans.
fn block_line(raw: &str, base: Style) -> MdLine {
    let indent_len = raw.len() - raw.trim_start().len();
    let (indent, rest) = raw.split_at(indent_len);

    // Heading: strip the hashes, render bold.
    if let Some(text) = heading_text(rest) {
        return MdLine {
            segments: parse_inline(text, base.add_modifier(Modifier::BOLD)),
            wrap: WrapMode::Word,
        };
    }

    // Bullet: normalize the marker to "•", keep the nesting indent.
    if let Some(item) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
        let mut segments = vec![(format!("{indent}• "), base)];
        segments.extend(parse_inline(item, base));
        return MdLine {
            segments,
            wrap: WrapMode::Word,
        };
    }

    // Blockquote: a dim bar gutter, italic body.
    if let Some(quote) = rest.strip_prefix("> ") {
        let mut segments = vec![("▌ ".to_string(), Style::default().fg(Color::DarkGray))];
        segments.extend(parse_inline(quote, base.add_modifier(Modifier::ITALIC)));
        return MdLine {
            segments,
            wrap: WrapMode::Word,
        };
    }

    MdLine {
        segments: parse_inline(raw, base),
        wrap: WrapMode::Word,
    }
}

/// `# `..`###### ` heading? Return the text after the hashes.
fn heading_text(s: &str) -> Option<&str> {
    let hashes = s.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        s[hashes..].strip_prefix(' ')
    } else {
        None
    }
}

fn code_style(base: Style) -> Style {
    base.fg(Color::Cyan)
}

/// Parse inline markup — `` `code` ``, `**bold**`, `*italic*` — into styled
/// segments. Unterminated markers stay literal.
fn parse_inline(s: &str, base: Style) -> Vec<(String, Style)> {
    let chars: Vec<char> = s.chars().collect();
    let mut segments: Vec<(String, Style)> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;

    let flush = |plain: &mut String, segments: &mut Vec<(String, Style)>| {
        if !plain.is_empty() {
            segments.push((std::mem::take(plain), base));
        }
    };

    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if let Some(j) = find(&chars, i + 1, &['`']) {
                flush(&mut plain, &mut segments);
                segments.push((chars[i + 1..j].iter().collect(), code_style(base)));
                i = j + 1;
                continue;
            }
        } else if c == '*' {
            if chars.get(i + 1) == Some(&'*') {
                if let Some(j) = find(&chars, i + 2, &['*', '*']) {
                    flush(&mut plain, &mut segments);
                    segments.push((
                        chars[i + 2..j].iter().collect(),
                        base.add_modifier(Modifier::BOLD),
                    ));
                    i = j + 2;
                    continue;
                }
            } else if let Some(j) = find(&chars, i + 1, &['*'])
                // A real emphasis opener: non-empty and not "* " (a stray
                // bullet or multiplication sign).
                && j > i + 1
                && chars[i + 1] != ' '
            {
                flush(&mut plain, &mut segments);
                segments.push((
                    chars[i + 1..j].iter().collect(),
                    base.add_modifier(Modifier::ITALIC),
                ));
                i = j + 1;
                continue;
            }
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut segments);
    if segments.is_empty() {
        segments.push((String::new(), base));
    }
    segments
}

/// First index `j >= from` where `needle` occurs in `haystack`.
fn find(haystack: &[char], from: usize, needle: &[char]) -> Option<usize> {
    (from..=haystack.len().saturating_sub(needle.len()))
        .find(|&j| &haystack[j..j + needle.len()] == needle)
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

    #[test]
    fn plain_text_passes_through() {
        let lines = render_markdown("hello world", base());
        assert_eq!(lines.len(), 1);
        assert_eq!(text_of(&lines[0]), "hello world");
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
    fn blockquotes_get_a_bar_gutter() {
        let lines = render_markdown("> careful", base());
        assert!(text_of(&lines[0]).starts_with("▌ "));
        assert!(text_of(&lines[0]).contains("careful"));
    }

    #[test]
    fn code_fences_hide_markers_and_preserve_content() {
        let lines = render_markdown("before\n```yaml\nnets:\n  VCC: [U1.1]\n```\nafter", base());
        let texts: Vec<String> = lines.iter().map(text_of).collect();
        assert_eq!(texts, vec!["before", "nets:", "  VCC: [U1.1]", "after"]);
        assert_eq!(
            lines[1].wrap,
            WrapMode::Preserve,
            "code is preserve-wrapped"
        );
        assert_eq!(lines[2].segments[0].1.fg, Some(Color::Cyan));
    }

    #[test]
    fn unclosed_fence_styles_the_remainder_as_code() {
        let lines = render_markdown("```\nlet x = 1;", base());
        assert_eq!(lines.len(), 1);
        assert_eq!(text_of(&lines[0]), "let x = 1;");
        assert_eq!(lines[0].wrap, WrapMode::Preserve);
    }

    #[test]
    fn empty_lines_survive() {
        let lines = render_markdown("a\n\nb", base());
        assert_eq!(lines.len(), 3);
        assert_eq!(text_of(&lines[1]), "");
    }
}
