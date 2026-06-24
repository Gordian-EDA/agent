//! Paren-aware s-expression **text surgery** for footprint splicing — the kernel
//! the KiCAD emitter uses to transform a `.kicad_mod` body in place without a
//! full parse (kiutils' footprint AST does not round-trip through `write()`; see
//! [`crate::synth`]). Everything here is pure string manipulation over balanced
//! `( … )` nodes, honouring string literals, so any emitter targeting a textual
//! EDA format can reuse it. Deterministic; never panics.

use kicad_sexpr::fmt_num;

/// The whole `(footprint …)` block in `source` (from its opening paren to its
/// matching closing paren), or `None` if absent/unbalanced.
pub fn footprint_body(source: &str) -> Option<&str> {
    let start = source.find("(footprint ")?;
    let end = matching_close(source, start)?;
    Some(&source[start..=end])
}

/// The inner text of a `(footprint "NAME" … )` block: everything after the
/// `(footprint "NAME"` opener up to (not including) the block's final `)`.
pub fn footprint_inner(body: &str) -> Option<&str> {
    // Skip `(footprint ` then the quoted name token.
    let after_kw = body.strip_prefix("(footprint ")?;
    let rest = after_kw.strip_prefix('"')?;
    let name_close = rest.find('"')?;
    let inner_start = "(footprint ".len() + 1 + name_close + 1;
    // The body ends with its matching ')'; inner is everything between.
    let inner = &body[inner_start..body.len() - 1];
    Some(inner)
}

/// Split a footprint body's inner text into its top-level child nodes (each a
/// balanced `( … )` s-expression), trimming the whitespace between them.
pub fn top_level_nodes(inner: &str) -> Vec<&str> {
    let bytes = inner.as_bytes();
    let mut nodes = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'('
            && let Some(end) = matching_close(inner, i)
        {
            nodes.push(&inner[i..=end]);
            i = end + 1;
            continue;
        }
        i += 1;
    }
    nodes
}

/// Index of the `)` matching the `(` at `open` in `s`, honoring string literals
/// (parens inside `"…"` do not count). `None` if unbalanced.
pub fn matching_close(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    debug_assert_eq!(bytes[open], b'(');
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_str = !in_str,
            b'(' if !in_str => depth += 1,
            b')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The head symbol of an s-expression node `(<head> …)`, e.g. `"pad"`,
/// `"fp_line"`, `"property"`. Empty string if the node is malformed.
pub fn node_head(node: &str) -> &str {
    let rest = node.strip_prefix('(').unwrap_or(node);
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Whether a graphic node sits on a silkscreen layer (`*.SilkS`).
pub fn on_silk(node: &str) -> bool {
    // A graphic's layer appears as `(layer "F.SilkS")` / `"B.SilkS"`.
    node.contains("(layer \"F.SilkS\")") || node.contains("(layer \"B.SilkS\")")
}

/// The pad number token of a `(pad "N" …)` node, if present.
pub fn pad_number(node: &str) -> Option<String> {
    let rest = node.strip_prefix("(pad ")?;
    let rest = rest.strip_prefix('"')?;
    let close = rest.find('"')?;
    Some(rest[..close].to_owned())
}

/// Insert `insertion` on its own line immediately before the final closing paren
/// of the balanced s-expression `node`. The inserted line is indented to match
/// the node's children (the indentation of the first child line). `None` if the
/// node has no closing paren.
pub fn inject_before_close(node: &str, insertion: &str) -> Option<String> {
    let close = node.rfind(')')?;
    // Child indentation: the whitespace run after the first newline.
    let indent = child_indent(node);
    let mut out = String::with_capacity(node.len() + insertion.len() + indent.len() + 2);
    out.push_str(&node[..close]);
    // Ensure we start the inserted line cleanly (node[..close] ends with the
    // child block's trailing newline+indent before the ')').
    out.push_str(insertion);
    out.push('\n');
    out.push_str(&indent);
    out.push_str(&node[close..]);
    Some(out)
}

/// The indentation (leading whitespace) of the first child line of a multi-line
/// node — i.e. the run of tabs/spaces after the node's first `\n`. Empty for a
/// single-line node.
fn child_indent(node: &str) -> String {
    let Some(nl) = node.find('\n') else {
        return String::new();
    };
    node[nl + 1..]
        .chars()
        .take_while(|c| *c == '\t' || *c == ' ')
        .collect()
}

/// Append `node` to `out` with one extra leading tab on every non-empty line, so
/// a `.kicad_mod` child (indented one level) sits at the board-footprint depth.
pub fn push_reindented(out: &mut String, node: &str) {
    for (i, line) in node.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.is_empty() {
            out.push('\t');
        }
        out.push_str(line);
    }
    out.push('\n');
}

/// Cap the first `(size W H)` in `body` to `max` mm on each axis (shrink only —
/// a smaller library value is left alone). Used to keep refdes text compact.
pub fn cap_font_size(body: &str, max: f64) -> String {
    let Some(start) = body.find("(size ") else { return body.to_owned() };
    let open = start + "(size ".len();
    let Some(rel_close) = body[open..].find(')') else { return body.to_owned() };
    let inner = &body[open..open + rel_close];
    let nums: Vec<f64> = inner.split_whitespace().filter_map(|t| t.parse().ok()).collect();
    if nums.len() != 2 {
        return body.to_owned();
    }
    let (w, h) = (nums[0].min(max), nums[1].min(max));
    format!("{}(size {} {}){}", &body[..start], fmt_num(w), fmt_num(h), &body[open + rel_close + 1..])
}

/// Add `fp_rot` (CCW degrees) to a pad's stored `(at x y [rot])` rotation. KiCAD
/// pad rotation is absolute (footprint angle folded in), so a rotated footprint
/// needs each pad's angle bumped. The pad `(at …)` is the first `(at ` on its
/// own line inside the pad node.
pub fn bump_pad_rotation(node: &str, fp_rot: i32) -> String {
    const AT: &str = "(at ";
    // Find the pad-level `(at …)` — the first one inside the node.
    let Some(at_pos) = node.find(AT) else {
        return node.to_owned();
    };
    let after = &node[at_pos + AT.len()..];
    let Some(line_end) = after.find(')') else {
        return node.to_owned();
    };
    let inside = &after[..line_end]; // "x y" or "x y rot"
    let nums: Vec<&str> = inside.split_whitespace().collect();
    let (x, y) = match (nums.first(), nums.get(1)) {
        (Some(x), Some(y)) => (*x, *y),
        _ => return node.to_owned(),
    };
    let pad_rot: f64 = nums.get(2).and_then(|r| r.parse().ok()).unwrap_or(0.0);
    let new_rot = (pad_rot + fp_rot as f64).rem_euclid(360.0);
    let replacement = if new_rot == 0.0 {
        format!("(at {x} {y}")
    } else {
        format!("(at {x} {y} {})", fmt_num(new_rot))
    };
    // Rebuild: prefix + new "(at …" + the rest after the original "(at …" up to
    // and including its ')'. We replace the substring `(at <inside>)`.
    let mut out = String::with_capacity(node.len() + 8);
    out.push_str(&node[..at_pos]);
    out.push_str(&replacement);
    out.push_str(&node[at_pos + AT.len() + line_end + 1..]); // after the ')'
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_close_handles_nested_and_strings() {
        let s = "(a (b \"x)y\") c)";
        assert_eq!(matching_close(s, 0), Some(s.len() - 1));
    }

    #[test]
    fn node_head_and_silk() {
        assert_eq!(node_head("(pad \"1\" smd)"), "pad");
        assert_eq!(node_head("(fp_line\n\t(layer \"F.SilkS\")\n)"), "fp_line");
        assert!(on_silk("(fp_line\n\t(layer \"F.SilkS\")\n)"));
        assert!(!on_silk("(fp_rect\n\t(layer \"F.CrtYd\")\n)"));
    }

    #[test]
    fn inject_net_into_thru_hole_pad_with_drill() {
        let pad = "(pad \"1\" thru_hole rect\n\t(at 0 0)\n\t(size 1.7 1.7)\n\t(drill 1)\n\t(layers \"*.Cu\" \"*.Mask\")\n)";
        let out = inject_before_close(pad, "(net 2 \"VIN\")").unwrap();
        assert!(out.contains("(net 2 \"VIN\")"));
        // The (drill 1) child is untouched and the net lands before the close.
        let net_at = out.find("(net 2").unwrap();
        let close = out.rfind(')').unwrap();
        assert!(net_at < close);
        assert!(out.contains("(drill 1)"));
    }

    #[test]
    fn bump_pad_rotation_adds_footprint_angle() {
        let pad = "(pad \"1\" smd roundrect\n\t(at -0.9375 -0.95)\n\t(size 1.475 0.6)\n)";
        let out = bump_pad_rotation(pad, 90);
        assert!(out.contains("(at -0.9375 -0.95 90)"), "{out}");
        // A pad already at 90 + footprint 90 → 180.
        let pad2 = "(pad \"1\" smd\n\t(at 0 0 90)\n)";
        assert!(bump_pad_rotation(pad2, 90).contains("(at 0 0 180)"));
    }
}
