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

/// Encode the characters KiCAD cannot store literally.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ESCAPES.iter().find(|(_, escaped)| *escaped == ch) {
            Some((token, _)) => {
                out.push('{');
                out.push_str(token);
                out.push('}');
            }
            None => out.push(ch),
        }
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
    fn round_trips_every_escaped_character() {
        for (_, ch) in ESCAPES {
            let plain = format!("A{ch}B");
            assert_eq!(unescape(&escape(&plain)), plain);
        }
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
