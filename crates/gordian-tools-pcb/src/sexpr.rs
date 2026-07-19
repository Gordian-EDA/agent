//! Minimal board-file s-expression scanning shared by the outline and render
//! readers: balanced-node extents and `(key x y)` point lookup over raw text.

use geom::Point2;

/// End index (exclusive) of the balanced node beginning at `start`, which must
/// sit on its `(`. String literals and their escapes are skipped.
pub(crate) fn sexpr_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes[start..].iter().copied().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(start + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// First `(key x y …)` point inside `block`.
pub(crate) fn sexpr_point(block: &str, key: &str) -> Option<Point2> {
    let marker = format!("({key} ");
    let rest = block.split_once(&marker)?.1;
    let mut values = rest
        .split(|ch: char| ch.is_ascii_whitespace() || ch == ')')
        .filter(|value| !value.is_empty());
    Some(Point2::new(
        values.next()?.parse().ok()?,
        values.next()?.parse().ok()?,
    ))
}
