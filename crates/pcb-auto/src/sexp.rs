//! Structurally lossless S-expression tree for KiCad files.
//!
//! Numbers keep their original token text until replaced, so untouched values are written back
//! byte-identical. KiCad accepts any whitespace, so the writer only needs to be readable.

use std::fmt::Write as _;
use std::path::Path;

/// One token: a bare symbol, a quoted string, a number that remembers its spelling, or a list.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Sym(String),
    Str(String),
    Num(String),
    List(SList),
}

/// A parenthesised list. Its head is the leading symbol, when it has one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SList {
    pub items: Vec<Node>,
}

impl Node {
    pub fn sym(s: impl Into<String>) -> Node {
        Node::Sym(s.into())
    }
    pub fn str(s: impl Into<String>) -> Node {
        Node::Str(s.into())
    }
    pub fn num(v: f64) -> Node {
        Node::Num(fmt_num(v))
    }
    pub fn int(v: i64) -> Node {
        Node::Num(v.to_string())
    }
    pub fn flag(v: bool) -> Node {
        Node::Sym(if v { "yes" } else { "no" }.into())
    }

    /// The token's text, whatever its kind (a list yields the empty string).
    pub fn text(&self) -> &str {
        match self {
            Node::Sym(s) | Node::Str(s) | Node::Num(s) => s,
            Node::List(_) => "",
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Node::Num(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&SList> {
        match self {
            Node::List(l) => Some(l),
            _ => None,
        }
    }
}

/// Build `(head items...)`.
pub fn l(head: &str, items: Vec<Node>) -> Node {
    let mut v = vec![Node::sym(head)];
    v.extend(items);
    Node::List(SList { items: v })
}

/// `(head x y)` with two numbers — the shape most KiCad geometry takes.
pub fn lxy(head: &str, x: f64, y: f64) -> Node {
    l(head, vec![Node::num(x), Node::num(y)])
}

impl SList {
    pub fn head(&self) -> Option<&str> {
        match self.items.first() {
            Some(Node::Sym(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn is(&self, head: &str) -> bool {
        self.head() == Some(head)
    }

    /// Child lists, optionally filtered by head symbol.
    pub fn lists(&self, head: Option<&str>) -> Vec<&SList> {
        self.items
            .iter()
            .filter_map(|c| c.as_list())
            .filter(|c| head.is_none_or(|h| c.is(h)))
            .collect()
    }

    pub fn find(&self, head: &str) -> Option<&SList> {
        self.items
            .iter()
            .filter_map(|c| c.as_list())
            .find(|c| c.is(head))
    }

    pub fn find_mut(&mut self, head: &str) -> Option<&mut SList> {
        self.items.iter_mut().find_map(|c| match c {
            Node::List(l) if l.is(head) => Some(l),
            _ => None,
        })
    }

    /// Atoms after the head.
    pub fn args(&self) -> Vec<&Node> {
        self.items
            .iter()
            .filter(|c| c.as_list().is_none())
            .skip(1)
            .collect()
    }

    pub fn arg(&self, i: usize) -> Option<&Node> {
        self.args().into_iter().nth(i)
    }

    pub fn arg_f64(&self, i: usize) -> Option<f64> {
        self.arg(i).and_then(|a| a.as_f64())
    }

    pub fn arg_text(&self, i: usize) -> Option<&str> {
        self.arg(i).map(|a| a.text())
    }

    /// Replace every atom after the head, keeping child lists in place.
    pub fn set_args(&mut self, values: Vec<Node>) {
        let head = self.items.first().cloned();
        let children: Vec<Node> = self
            .items
            .iter()
            .filter(|c| c.as_list().is_some())
            .cloned()
            .collect();
        let mut items = Vec::new();
        if let Some(h) = head {
            items.push(h);
        }
        items.extend(values);
        items.extend(children);
        self.items = items;
    }

    /// Set the child list `(head values...)`, creating it if missing.
    pub fn set(&mut self, head: &str, values: Vec<Node>) {
        if self.find_mut(head).is_none() {
            self.items.push(l(head, vec![]));
        }
        self.find_mut(head).unwrap().set_args(values);
    }

    pub fn remove(&mut self, head: &str) {
        self.items
            .retain(|c| !c.as_list().map(|s| s.is(head)).unwrap_or(false));
    }

    pub fn push(&mut self, node: Node) {
        self.items.push(node);
    }
}

/// KiCad-style number: up to 6 decimals, trailing zeros stripped, `-0` avoided.
pub fn fmt_num(v: f64) -> String {
    let v = if v.abs() < 5e-7 { 0.0 } else { v };
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() || s == "-0" {
        "0".into()
    } else {
        s
    }
}

fn looks_numeric(tok: &str) -> bool {
    let c = tok.as_bytes().first().copied().unwrap_or(b' ');
    (c.is_ascii_digit() || c == b'+' || c == b'-' || c == b'.') && tok.parse::<f64>().is_ok()
}

/// Parse one top-level S-expression.
pub fn parse(text: &str) -> anyhow::Result<SList> {
    let b = text.as_bytes();
    let mut i = 0usize;
    let n = b.len();
    let mut stack: Vec<Vec<Node>> = Vec::new();
    let mut items: Vec<Node> = Vec::new();
    while i < n {
        match b[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'(' => {
                stack.push(std::mem::take(&mut items));
                i += 1;
            }
            b')' => {
                let done = SList {
                    items: std::mem::take(&mut items),
                };
                items = stack.pop().ok_or_else(|| anyhow::anyhow!("unbalanced )"))?;
                items.push(Node::List(done));
                i += 1;
            }
            b'"' => {
                i += 1;
                let mut out = String::new();
                loop {
                    if i >= n {
                        anyhow::bail!("unclosed string");
                    }
                    let c = b[i];
                    if c == b'\\' && i + 1 < n {
                        let e = b[i + 1];
                        out.push(match e {
                            b'n' => '\n',
                            b'r' => '\r',
                            b't' => '\t',
                            _ => e as char,
                        });
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if c == b'"' {
                        break;
                    }
                    // multi-byte UTF-8 passes through one byte at a time via the slice below
                    let start = i - 1;
                    let ch_len = utf8_len(b[start]);
                    out.push_str(std::str::from_utf8(&b[start..start + ch_len])?);
                    i = start + ch_len;
                }
                items.push(Node::Str(out));
            }
            _ => {
                let start = i;
                while i < n && !matches!(b[i], b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'"') {
                    i += 1;
                }
                let tok = std::str::from_utf8(&b[start..i])?;
                items.push(if looks_numeric(tok) {
                    Node::Num(tok.to_string())
                } else {
                    Node::Sym(tok.to_string())
                });
            }
        }
    }
    if !stack.is_empty() || items.len() != 1 {
        anyhow::bail!("malformed s-expression");
    }
    match items.pop() {
        Some(Node::List(top)) => Ok(top),
        _ => anyhow::bail!("top level is not a list"),
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn atom_text(a: &Node) -> String {
    match a {
        Node::Num(s) | Node::Sym(s) => s.clone(),
        Node::Str(s) => format!(
            "\"{}\"",
            s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
        ),
        Node::List(_) => String::new(),
    }
}

pub fn dumps(node: &SList) -> String {
    let mut out = String::with_capacity(1 << 16);
    write_node(node, &mut out, 0);
    out.push('\n');
    out
}

fn write_node(node: &SList, out: &mut String, depth: usize) {
    let ind = "\t".repeat(depth);
    if !node.items.iter().any(|c| c.as_list().is_some()) {
        let _ = write!(
            out,
            "{ind}({})",
            node.items
                .iter()
                .map(atom_text)
                .collect::<Vec<_>>()
                .join(" ")
        );
        return;
    }
    out.push_str(&ind);
    out.push('(');
    let mut first = true;
    for c in &node.items {
        match c {
            Node::List(child) => {
                out.push('\n');
                write_node(child, out, depth + 1);
            }
            atom => {
                if !first {
                    out.push(' ');
                }
                out.push_str(&atom_text(atom));
            }
        }
        first = false;
    }
    out.push('\n');
    out.push_str(&ind);
    out.push(')');
}

pub fn load(path: &Path) -> anyhow::Result<SList> {
    parse(&std::fs::read_to_string(path)?)
}

/// Atomic write: the file is replaced only once the new content is complete.
pub fn save(node: &SList, path: &Path) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::Builder::new().prefix(".pcb-auto-").tempfile_in(dir)?;
    use std::io::Write;
    tmp.write_all(dumps(node).as_bytes())?;
    tmp.flush()?;
    tmp.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_numbers_and_strings() {
        let s = parse("(kicad_pcb (version 20241229) (name \"a b\") (at 1.50 -0))").unwrap();
        assert_eq!(s.head(), Some("kicad_pcb"));
        assert_eq!(s.find("version").unwrap().arg_f64(0), Some(20241229.0));
        assert_eq!(s.find("name").unwrap().arg_text(0), Some("a b"));
        let text = dumps(&s);
        assert!(text.contains("1.50"), "number spelling is preserved: {text}");
    }
}
