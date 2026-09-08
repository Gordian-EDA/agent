//! A minimal s-expression reader for KiCad files.

/// One s-expression node: a head atom and the items that follow it.
#[derive(Debug, Clone, PartialEq)]
pub enum Sexp {
    Atom(String),
    List(Vec<Sexp>),
}

impl Sexp {
    pub fn as_atom(&self) -> Option<&str> {
        match self {
            Sexp::Atom(text) => Some(text),
            Sexp::List(_) => None,
        }
    }

    pub fn items(&self) -> &[Sexp] {
        match self {
            Sexp::List(items) => items,
            Sexp::Atom(_) => &[],
        }
    }

    /// The list's first atom, which names it.
    pub fn head(&self) -> Option<&str> {
        self.items().first()?.as_atom()
    }

    /// Every direct child list named `name`.
    pub fn children<'a: 'n, 'n>(&'a self, name: &'n str) -> impl Iterator<Item = &'a Sexp> + 'n {
        self.items()
            .iter()
            .filter(move |item| item.head() == Some(name))
    }

    /// The first direct child list named `name`.
    pub fn child(&self, name: &str) -> Option<&Sexp> {
        self.children(name).next()
    }

    /// The first atom after the head of the first child named `name`.
    pub fn text(&self, name: &str) -> Option<&str> {
        self.child(name)?.items().get(1)?.as_atom()
    }

    pub fn number(&self, index: usize) -> Option<f64> {
        self.items().get(index)?.as_atom()?.parse().ok()
    }

    /// A `(name yes|no)` flag.
    pub fn flag(&self, name: &str) -> Option<bool> {
        Some(matches!(self.text(name)?, "yes" | "true"))
    }

    /// Every descendant list named `name`, depth first.
    pub fn descendants<'a>(&'a self, name: &'a str, out: &mut Vec<&'a Sexp>) {
        for item in self.items() {
            if item.head() == Some(name) {
                out.push(item);
            }
            item.descendants(name, out);
        }
    }
}

/// Parse a whole file into its top-level node.
pub fn parse(text: &str) -> Result<Sexp, String> {
    let bytes = text.as_bytes();
    let mut at = 0;
    skip_space(bytes, &mut at);
    let node = parse_node(bytes, &mut at)?;
    Ok(node)
}

fn skip_space(bytes: &[u8], at: &mut usize) {
    while *at < bytes.len() && bytes[*at].is_ascii_whitespace() {
        *at += 1;
    }
}

fn parse_node(bytes: &[u8], at: &mut usize) -> Result<Sexp, String> {
    skip_space(bytes, at);
    match bytes.get(*at) {
        None => Err("unexpected end of s-expression".into()),
        Some(b'(') => {
            *at += 1;
            let mut items = Vec::new();
            loop {
                skip_space(bytes, at);
                match bytes.get(*at) {
                    None => return Err("unclosed list".into()),
                    Some(b')') => {
                        *at += 1;
                        return Ok(Sexp::List(items));
                    }
                    _ => items.push(parse_node(bytes, at)?),
                }
            }
        }
        Some(b'"') => {
            *at += 1;
            let mut out = String::new();
            while let Some(&byte) = bytes.get(*at) {
                *at += 1;
                match byte {
                    b'"' => return Ok(Sexp::Atom(out)),
                    b'\\' => {
                        let escaped = *bytes.get(*at).ok_or("unclosed string")?;
                        *at += 1;
                        out.push(match escaped {
                            b'n' => '\n',
                            b'r' => '\r',
                            b't' => '\t',
                            other => other as char,
                        });
                    }
                    _ => push_utf8(bytes, at, byte, &mut out),
                }
            }
            Err("unclosed string".into())
        }
        Some(_) => {
            let start = *at;
            while let Some(&byte) = bytes.get(*at) {
                if byte.is_ascii_whitespace() || byte == b'(' || byte == b')' {
                    break;
                }
                *at += 1;
            }
            Ok(Sexp::Atom(
                String::from_utf8_lossy(&bytes[start..*at]).into_owned(),
            ))
        }
    }
}

/// Copy one UTF-8 code point starting at the byte already consumed.
fn push_utf8(bytes: &[u8], at: &mut usize, first: u8, out: &mut String) {
    let extra = match first {
        0x00..=0x7f => 0,
        0xc0..=0xdf => 1,
        0xe0..=0xef => 2,
        _ => 3,
    };
    let start = *at - 1;
    *at = (*at + extra).min(bytes.len());
    out.push_str(&String::from_utf8_lossy(&bytes[start..*at]));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nested_lists_and_strings() {
        let node = parse(r#"(a (b "x y") (c 1.5))"#).unwrap();
        assert_eq!(node.head(), Some("a"));
        assert_eq!(node.text("b"), Some("x y"));
        assert_eq!(node.child("c").unwrap().number(1), Some(1.5));
    }

    #[test]
    fn keeps_escapes_and_utf8() {
        let node = parse(r#"(a "±\"q\"")"#).unwrap();
        assert_eq!(node.items()[1].as_atom(), Some("±\"q\""));
    }
}
