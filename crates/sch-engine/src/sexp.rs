//! Minimal, fast S-expression reader/writer for KiCad files.
//!
//! A parsed node is a [`Sexp::List`] whose first element is the tag. Atoms are bare symbols
//! ([`Sexp::Sym`]), quoted strings ([`Sexp::Str`]) or numbers.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Sexp {
    List(Vec<Sexp>),
    /// A quoted string atom.
    Str(String),
    /// A bare symbol atom.
    Sym(String),
    Int(i64),
    Float(f64),
}

impl Sexp {
    pub fn list(items: Vec<Sexp>) -> Sexp {
        Sexp::List(items)
    }
    pub fn sym(s: impl Into<String>) -> Sexp {
        Sexp::Sym(s.into())
    }
    pub fn str(s: impl Into<String>) -> Sexp {
        Sexp::Str(s.into())
    }

    pub fn as_list(&self) -> Option<&[Sexp]> {
        match self {
            Sexp::List(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_list_mut(&mut self) -> Option<&mut Vec<Sexp>> {
        match self {
            Sexp::List(v) => Some(v),
            _ => None,
        }
    }
    pub fn is_list(&self) -> bool {
        matches!(self, Sexp::List(_))
    }

    /// Text of any atom (numbers formatted the way they are written back out).
    pub fn text(&self) -> String {
        match self {
            Sexp::Str(s) | Sexp::Sym(s) => s.clone(),
            Sexp::Int(i) => i.to_string(),
            Sexp::Float(f) => fmt_num(*f),
            Sexp::List(_) => String::new(),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Sexp::Str(s) | Sexp::Sym(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Sexp::Int(i) => Some(*i as f64),
            Sexp::Float(f) => Some(*f),
            Sexp::Str(s) | Sexp::Sym(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// The node's tag (first atom of a list).
    pub fn tag(&self) -> &str {
        match self {
            Sexp::List(v) => v.first().and_then(|c| c.as_str()).unwrap_or(""),
            _ => "",
        }
    }

    /// All child lists with the given tag.
    pub fn children(&self, tag: &str) -> Vec<&Sexp> {
        self.as_list()
            .map(|v| v[1.min(v.len())..].iter().filter(|c| c.is_list() && c.tag() == tag).collect())
            .unwrap_or_default()
    }

    /// The first child list with the given tag.
    pub fn child(&self, tag: &str) -> Option<&Sexp> {
        let v = self.as_list()?;
        v[1.min(v.len())..].iter().find(|c| c.is_list() && c.tag() == tag)
    }

    /// Non-list children (the node's own atoms, tag excluded).
    pub fn atoms(&self) -> Vec<&Sexp> {
        self.as_list()
            .map(|v| v[1.min(v.len())..].iter().filter(|c| !c.is_list()).collect())
            .unwrap_or_default()
    }

    /// The `(property "name" "value" ...)` child.
    pub fn prop(&self, name: &str) -> Option<&Sexp> {
        let v = self.as_list()?;
        v[1.min(v.len())..].iter().find(|c| {
            c.tag() == "property"
                && c.as_list().map(|l| l.len() > 2 && l[1].as_str() == Some(name)).unwrap_or(false)
        })
    }

    pub fn prop_value(&self, name: &str) -> Option<String> {
        self.prop(name).and_then(|p| p.as_list()).map(|l| l[2].text())
    }
}

/// Parse the first complete s-expression in `text`.
pub fn loads(text: &str) -> Option<Sexp> {
    let b = text.as_bytes();
    let mut pos = 0usize;
    let n = b.len();
    let mut stack: Vec<Vec<Sexp>> = Vec::new();
    let mut root: Option<Sexp> = None;
    while pos < n {
        while pos < n && (b[pos] as char).is_whitespace() {
            pos += 1;
        }
        if pos >= n {
            break;
        }
        match b[pos] {
            b'(' => {
                pos += 1;
                stack.push(Vec::new());
            }
            b')' => {
                pos += 1;
                let node = Sexp::List(stack.pop()?);
                match stack.last_mut() {
                    Some(top) => top.push(node),
                    None => {
                        root = Some(node);
                        break;
                    }
                }
            }
            b'"' => {
                pos += 1;
                let start = pos;
                let mut escaped = false;
                while pos < n {
                    if b[pos] == b'\\' {
                        escaped = true;
                        pos += 2;
                        continue;
                    }
                    if b[pos] == b'"' {
                        break;
                    }
                    pos += 1;
                }
                let raw = &text[start..pos.min(n)];
                pos += 1;
                let s = if escaped {
                    raw.replace("\\\"", "\"").replace("\\n", "\n").replace("\\\\", "\\")
                } else {
                    raw.to_string()
                };
                stack.last_mut()?.push(Sexp::Str(s));
            }
            _ => {
                let start = pos;
                while pos < n
                    && !(b[pos] as char).is_whitespace()
                    && b[pos] != b'('
                    && b[pos] != b')'
                    && b[pos] != b'"'
                {
                    pos += 1;
                }
                let a = &text[start..pos];
                stack.last_mut()?.push(atom_of(a));
            }
        }
    }
    if root.is_none() && !stack.is_empty() {
        root = Some(Sexp::List(stack.swap_remove(0)));
    }
    root
}

fn atom_of(a: &str) -> Sexp {
    if let Ok(i) = a.parse::<i64>()
        && a.bytes().all(|c| c.is_ascii_digit() || c == b'-')
    {
        return Sexp::Int(i);
    }
    if is_plain_float(a)
        && let Ok(f) = a.parse::<f64>()
    {
        return Sexp::Float(f);
    }
    Sexp::Sym(a.to_string())
}

/// Mirrors Python's `-?\d*\.?\d+(e-?\d+)?` guard: only such tokens become numbers.
fn is_plain_float(a: &str) -> bool {
    let b = a.as_bytes();
    let mut i = 0;
    if i < b.len() && b[i] == b'-' {
        i += 1;
    }
    let d0 = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
    } else if i == d0 {
        return false;
    }
    let d1 = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == d1 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && b[i] == b'-' {
            i += 1;
        }
        let e0 = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e0 {
            return false;
        }
    }
    i == b.len()
}

pub fn fmt_num(v: f64) -> String {
    let mut s = format!("{v:.6}");
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if s == "-0" || s.is_empty() { "0".into() } else { s }
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn atom(v: &Sexp) -> String {
    match v {
        Sexp::Str(s) => esc(s),
        Sexp::Sym(s) => s.clone(),
        Sexp::Int(i) => i.to_string(),
        Sexp::Float(f) => fmt_num(*f),
        Sexp::List(_) => String::new(),
    }
}

/// Pretty-print roughly in KiCad's style (one child node per line).
pub fn dumps(node: &Sexp, indent: usize) -> String {
    let mut out = String::new();
    dump(node, indent, &mut out);
    out
}

fn dump(node: &Sexp, indent: usize, out: &mut String) {
    let pad = "\t".repeat(indent);
    let Sexp::List(items) = node else {
        out.push_str(&pad);
        out.push_str(&atom(node));
        return;
    };
    let _ = write!(out, "{pad}(");
    let mut first = true;
    let mut children_started = false;
    for c in items {
        if c.is_list() {
            out.push('\n');
            dump(c, indent + 1, out);
            children_started = true;
        } else if children_started {
            let _ = write!(out, "\n{pad}\t{}", atom(c));
        } else {
            if !first {
                out.push(' ');
            }
            out.push_str(&atom(c));
        }
        first = false;
    }
    if children_started {
        let _ = write!(out, "\n{pad})");
    } else {
        out.push(')');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let t = "(kicad_sch (version 20250114) (at 1.27 -2.54 90) (name \"a \\\"b\\\"\"))";
        let n = loads(t).unwrap();
        assert_eq!(n.tag(), "kicad_sch");
        assert_eq!(n.child("version").unwrap().as_list().unwrap()[1], Sexp::Int(20250114));
        let at = n.child("at").unwrap().as_list().unwrap();
        assert_eq!(at[1].as_f64(), Some(1.27));
        assert_eq!(at[2].as_f64(), Some(-2.54));
        assert_eq!(n.child("name").unwrap().as_list().unwrap()[1], Sexp::Str("a \"b\"".into()));
        assert!(dumps(&n, 0).contains("\\\"b\\\""));
    }

    #[test]
    fn symbols_stay_symbols() {
        let n = loads("(pin passive line 1e 2x)").unwrap();
        assert_eq!(n.atoms().iter().map(|a| a.text()).collect::<Vec<_>>(), ["passive", "line", "1e", "2x"]);
    }
}
