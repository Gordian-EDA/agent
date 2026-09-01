//! Thin accessors, builders and the KiCAD-dialect printer over `kiutils_sexpr`.

use kiutils_sexpr::{Atom, Node, Span};

const NO_SPAN: Span = Span { start: 0, end: 0 };

/// `(head …)` — the leading symbol atom of a list, if any.
pub fn head(node: &Node) -> Option<&str> {
    match node {
        Node::List { items, .. } => match items.first() {
            Some(Node::Atom {
                atom: Atom::Symbol(s),
                ..
            }) => Some(s.as_str()),
            _ => None,
        },
        Node::Atom { .. } => None,
    }
}

/// Children of a list node (including the head atom), or an empty slice.
pub fn items(node: &Node) -> &[Node] {
    match node {
        Node::List { items, .. } => items,
        Node::Atom { .. } => &[],
    }
}

/// Mutable children of a list node; `None` for atoms.
pub fn items_mut(node: &mut Node) -> Option<&mut Vec<Node>> {
    match node {
        Node::List { items, .. } => Some(items),
        Node::Atom { .. } => None,
    }
}

/// Text of an atom, whether quoted or bare.
pub fn text(node: &Node) -> Option<&str> {
    match node {
        Node::Atom {
            atom: Atom::Symbol(s) | Atom::Quoted(s),
            ..
        } => Some(s.as_str()),
        Node::List { .. } => None,
    }
}

/// Numeric value of an atom.
pub fn number(node: &Node) -> Option<f64> {
    text(node).and_then(|s| s.parse().ok())
}

/// First child list with the given head.
pub fn child<'a>(node: &'a Node, name: &str) -> Option<&'a Node> {
    items(node).iter().find(|c| head(c) == Some(name))
}

/// First child list with the given head, mutable.
pub fn child_mut<'a>(node: &'a mut Node, name: &str) -> Option<&'a mut Node> {
    items_mut(node)?.iter_mut().find(|c| head(c) == Some(name))
}

/// `(name <atom>)` as text.
pub fn child_text<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    child(node, name).and_then(|c| items(c).get(1)).and_then(text)
}

/// `(name yes|no)`, absent reads as `None`.
pub fn child_flag(node: &Node, name: &str) -> Option<bool> {
    child_text(node, name).map(|s| s == "yes")
}

/// A bare `hide` atom or a `(hide yes)` list, tolerating both spellings KiCAD
/// has used for the same flag.
pub fn flag_present(node: &Node, name: &str) -> bool {
    items(node).iter().any(|c| match c {
        Node::List { items, .. } => {
            head(c) == Some(name) && items.get(1).and_then(text) == Some("yes")
        }
        Node::Atom { atom, .. } => matches!(atom, Atom::Symbol(s) if s == name),
    })
}

/// Bare symbol atom.
pub fn sym(value: impl Into<String>) -> Node {
    Node::Atom {
        atom: Atom::Symbol(value.into()),
        span: NO_SPAN,
    }
}

/// Quoted string atom.
pub fn quoted(value: impl Into<String>) -> Node {
    Node::Atom {
        atom: Atom::Quoted(value.into()),
        span: NO_SPAN,
    }
}

/// Numeric atom in KiCAD's 1e-4 mm dialect.
pub fn num(value: f64) -> Node {
    sym(fmt_number(value))
}

/// `(items…)`.
pub fn list(children: Vec<Node>) -> Node {
    Node::List {
        items: children,
        span: NO_SPAN,
    }
}

/// `(head children…)`.
pub fn tagged(name: &str, children: Vec<Node>) -> Node {
    let mut v = vec![sym(name)];
    v.extend(children);
    list(v)
}

/// Replace the first `(name …)` child, or append one.
pub fn set_child(node: &mut Node, replacement: Node) {
    let Some(name) = head(&replacement).map(str::to_string) else {
        return;
    };
    let Some(children) = items_mut(node) else {
        return;
    };
    match children.iter().position(|c| head(c) == Some(&name)) {
        Some(idx) => children[idx] = replacement,
        None => children.push(replacement),
    }
}

/// Remove every child named `name`, in both spellings [`flag_present`] accepts:
/// the `(name …)` list and the bare `name` atom older KiCAD wrote for flags.
pub fn remove_children(node: &mut Node, name: &str) {
    if let Some(children) = items_mut(node) {
        children.retain(|c| match c {
            Node::List { .. } => head(c) != Some(name),
            Node::Atom { atom, .. } => !matches!(atom, Atom::Symbol(s) if s == name),
        });
    }
}

/// KiCAD's numeric spelling: up to four decimals, trailing zeros trimmed.
pub fn fmt_number(value: f64) -> String {
    let v = if value == 0.0 { 0.0 } else { value }; // normalise -0
    let mut s = format!("{v:.4}");
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    s
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// One-line rendering of a node.
#[cfg(test)]
pub fn flat(node: &Node) -> String {
    let mut out = String::new();
    flat_into(node, &mut out);
    out
}

fn flat_into(node: &Node, out: &mut String) {
    match node {
        Node::Atom {
            atom: Atom::Symbol(s),
            ..
        } => out.push_str(s),
        Node::Atom {
            atom: Atom::Quoted(s),
            ..
        } => out.push_str(&escape(s)),
        Node::List { items, .. } => {
            out.push('(');
            for (idx, child) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(' ');
                }
                flat_into(child, out);
            }
            out.push(')');
        }
    }
}

/// Pretty-print `node` in the KiCAD 9/10 printer dialect at `indent` tabs.
///
/// Three rules reproduce the shape KiCAD writes: a list whose children are all
/// atoms stays on one line; `(pts …)` puts every point on a single indented
/// line; anything else breaks after its leading atoms and indents its list
/// children. KiCAD additionally wraps very long lines at a width we do not
/// reproduce — untouched nodes are re-emitted from their original bytes, so the
/// difference only ever shows on nodes an edit actually rewrote.
pub fn print(node: &Node, indent: usize, out: &mut String) {
    for _ in 0..indent {
        out.push('\t');
    }
    let Node::List { items, .. } = node else {
        flat_into(node, out);
        out.push('\n');
        return;
    };
    if items.iter().all(|c| !matches!(c, Node::List { .. })) {
        flat_into(node, out);
        out.push('\n');
        return;
    }
    if head(node) == Some("pts") {
        out.push_str("(pts\n");
        for _ in 0..indent + 1 {
            out.push('\t');
        }
        for (idx, child) in items[1..].iter().enumerate() {
            if idx > 0 {
                out.push(' ');
            }
            flat_into(child, out);
        }
        out.push('\n');
        for _ in 0..indent {
            out.push('\t');
        }
        out.push_str(")\n");
        return;
    }
    let split = items
        .iter()
        .position(|c| matches!(c, Node::List { .. }))
        .unwrap_or(items.len());
    out.push('(');
    for (idx, child) in items[..split].iter().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        flat_into(child, out);
    }
    out.push('\n');
    for child in &items[split..] {
        print(child, indent + 1, out);
    }
    for _ in 0..indent {
        out.push('\t');
    }
    out.push_str(")\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiutils_sexpr::parse_one;

    fn render(src: &str) -> String {
        let doc = parse_one(src).expect("parse");
        let mut out = String::new();
        print(&doc.nodes[0], 0, &mut out);
        out
    }

    /// Atoms are re-emitted verbatim; only values the typed model rewrites go
    /// through `fmt_number`.
    #[test]
    fn all_atom_lists_stay_on_one_line() {
        assert_eq!(render("(at  1.0   2.0  90 )"), "(at 1.0 2.0 90)\n");
    }

    #[test]
    fn pts_children_share_one_line() {
        assert_eq!(
            render("(pts (xy 1 2) (xy 3 4))"),
            "(pts\n\t(xy 1 2) (xy 3 4)\n)\n"
        );
    }

    #[test]
    fn leading_atoms_ride_the_head_line() {
        assert_eq!(
            render("(property \"Reference\" \"R1\" (at 0 0 0))"),
            "(property \"Reference\" \"R1\"\n\t(at 0 0 0)\n)\n"
        );
    }

    #[test]
    fn printing_is_a_fixed_point() {
        let src = "(symbol (lib_id \"Device:R\") (at 1.5 2.25 90) (pts (xy 0 0) (xy 1 1)))";
        let once = render(src);
        let twice = render(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn numbers_use_kicad_spelling() {
        assert_eq!(fmt_number(0.0), "0");
        assert_eq!(fmt_number(-0.0), "0");
        assert_eq!(fmt_number(1.27), "1.27");
        assert_eq!(fmt_number(179.03135), "179.0314");
    }

    #[test]
    fn quotes_and_backslashes_round_trip() {
        let src = r#"(x "a\"b\\c")"#;
        assert_eq!(render(src), "(x \"a\\\"b\\\\c\")\n");
    }
}
