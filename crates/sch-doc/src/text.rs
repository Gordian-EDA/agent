//! KiCAD's `{token}` escapes for characters that cannot appear literally in a
//! label or field.
//!
//! The table is the one KiCAD 10 actually applies, read back from
//! `kicad-cli sch export netlist` on a sheet of labels — one per candidate
//! token — rather than transcribed from its source.

const ESCAPES: [(&str, char); 14] = [
    ("slash", '/'),
    ("backslash", '\\'),
    ("quote", '\''),
    ("dblquote", '"'),
    ("dollar", '$'),
    ("brace", '{'),
    ("lt", '<'),
    ("gt", '>'),
    ("colon", ':'),
    ("tab", '\t'),
    ("return", '\r'),
    ("space", ' '),
    ("bar", '|'),
    ("comma", ','),
];

/// Decode `{slash}` and friends. Any other `{…}` run is ordinary text and is
/// left alone, which is how KiCAD reads a label like `{cs}`.
pub fn unescape(text: &str) -> String {
    if !text.contains('{') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        match tail
            .find('}')
            .and_then(|close| lookup(&tail[1..close]).map(|ch| (ch, close)))
        {
            Some((ch, close)) => {
                out.push(ch);
                rest = &tail[close + 1..];
            }
            None => {
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Encode the two characters that would otherwise change what a label says:
/// `/`, which separates sheet from net in a net name, and `{`, which would
/// start an escape of its own.
///
/// The decode table above is larger because KiCAD accepts all of it; in a label
/// KiCAD itself writes nothing else, and neither does this.
///
/// A `{` opening one of KiCAD's markup runs — `~{…}` overbar, `^{…}` superscript,
/// `_{…}` subscript — is left alone: `~{RST}` is how a sheet says active-low RST,
/// and escaping its brace turns the name into the literal `~{brace}RST}`.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous = None;
    for ch in text.chars() {
        match ch {
            '/' => out.push_str("{slash}"),
            '{' if !matches!(previous, Some('~' | '^' | '_')) => out.push_str("{brace}"),
            _ => out.push(ch),
        }
        previous = Some(ch);
    }
    out
}

fn lookup(token: &str) -> Option<char> {
    ESCAPES
        .iter()
        .find(|(name, _)| *name == token)
        .map(|(_, ch)| *ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_what_it_encodes() {
        for plain in ["A/B", "A{B", "{slash}", "plain"] {
            assert_eq!(unescape(&escape(plain)), plain);
        }
    }

    /// An overbarred name is markup, not an escape: `~{RST}` is what a sheet
    /// writes for active-low RST, and it has to survive being written out.
    #[test]
    fn keeps_kicads_own_markup_runs() {
        assert_eq!(escape("~{RST}"), "~{RST}");
        assert_eq!(escape("D_{0}"), "D_{0}");
        assert_eq!(escape("X^{2}"), "X^{2}");
        assert_eq!(unescape(&escape("~{RST}")), "~{RST}");
        assert_eq!(escape("A{B"), "A{brace}B");
    }

    /// Everything KiCAD leaves alone in a label, this leaves alone too.
    #[test]
    fn encodes_only_what_would_change_the_meaning() {
        assert_eq!(escape("MY SIG,A"), "MY SIG,A");
        assert_eq!(escape("D<0>"), "D<0>");
        assert_eq!(escape("A/B"), "A{slash}B");
    }

    #[test]
    fn decodes_what_kicad_writes() {
        assert_eq!(unescape("FB{slash}VSET"), "FB/VSET");
        assert_eq!(escape("FB/VSET"), "FB{slash}VSET");
    }

    /// A brace run that is not an escape is ordinary text.
    #[test]
    fn leaves_unknown_tokens_alone() {
        assert_eq!(unescape("CLK{cs}"), "CLK{cs}");
        assert_eq!(unescape("no braces"), "no braces");
        assert_eq!(unescape("{unclosed"), "{unclosed");
    }
}
