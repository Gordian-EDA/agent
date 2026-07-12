//! Offline `.kicad_pcb` writes — the no-IPC fallback for `place_board`,
//! `route_board`, and `check_board`.
//!
//! The live path drives a running KiCAD over IPC, which needs a working GUI
//! session and (for footprint moves) KiCAD ≥ 9.0.3. Headless runs — LLM e2e
//! harnesses, CI — have neither, so these functions apply the same edits as
//! s-expression text operations on the board file itself. They only need to
//! handle the dialect Gordian's own seed writer emits plus KiCAD's resaves of
//! it: footprint blocks with a block-level `(at …)`, top-level
//! `(segment …)`/`(via …)` copper, and `(net N "NAME")` declarations.

use std::collections::BTreeMap;

use kicad_ipc::FootprintMove;
use pcb_model::{LayerRef, RouteSolution, ViaSpan};

/// One balanced s-expression node: byte range in the source text.
struct Node {
    start: usize,
    end: usize,
}

/// Iterate the top-level (depth-1) nodes of a `(kicad_pcb …)` document, or the
/// depth-1 children of any node body handed in.
fn child_nodes(text: &str, body_start: usize, body_end: usize) -> Vec<Node> {
    let bytes = text.as_bytes();
    let mut nodes = Vec::new();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut start = 0usize;
    let mut i = body_start;
    while i < body_end {
        match bytes[i] {
            b'"' if !in_str => in_str = true,
            b'"' if in_str => {
                // KiCAD escapes quotes as \"; skip escaped.
                if i == 0 || bytes[i - 1] != b'\\' {
                    in_str = false;
                }
            }
            b'(' if !in_str => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            b')' if !in_str => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    nodes.push(Node { start, end: i + 1 });
                }
            }
            _ => {}
        }
        i += 1;
    }
    nodes
}

/// The head atom of a node: `(footprint "x" …)` → `footprint`.
fn node_head<'a>(text: &'a str, node: &Node) -> &'a str {
    let inner = &text[node.start + 1..node.end];
    inner
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .find(|s| !s.is_empty())
        .unwrap_or("")
}

/// The body span of the document root `(kicad_pcb …)`: byte range strictly
/// inside its parens.
fn root_body(text: &str) -> Result<(usize, usize), String> {
    let start = text.find("(kicad_pcb").ok_or("not a kicad_pcb document")?;
    let root = child_nodes(text, start, text.len())
        .into_iter()
        .next()
        .ok_or("unbalanced kicad_pcb document")?;
    Ok((root.start + 1, root.end - 1))
}

/// `(property "Reference" "R1" …)` value inside a footprint body, if any.
fn footprint_reference(text: &str, fp: &Node) -> Option<String> {
    let body = &text[fp.start + 1..fp.end - 1];
    let key = "(property \"Reference\" \"";
    let at = body.find(key)?;
    let rest = &body[at + key.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn fmt_num(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Parse an `(at x y [angle])` node's numbers.
fn parse_at(text: &str, node: &Node) -> Option<(f64, f64, Option<f64>)> {
    let inner = &text[node.start + 1..node.end - 1];
    let mut it = inner.split_whitespace();
    it.next()?; // "at"
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    let a = it.next().and_then(|s| s.parse().ok());
    Some((x, y, a))
}

/// Move footprints in a `.kicad_pcb` document by reference: rewrite each
/// matched footprint's block-level `(at x y [rot])` and shift the angle term of
/// every child `(at …)` that carries one (pads and text items sum the footprint
/// rotation into their own angle, so a rotation delta propagates to them).
pub fn patch_placements(text: &str, moves: &[FootprintMove]) -> Result<String, String> {
    let by_ref: BTreeMap<&str, &FootprintMove> =
        moves.iter().map(|m| (m.reference.as_str(), m)).collect();
    let (body_start, body_end) = root_body(text)?;
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut seen = 0usize;
    for fp in child_nodes(text, body_start, body_end) {
        if node_head(text, &fp) != "footprint" {
            continue;
        }
        let Some(reference) = footprint_reference(text, &fp) else {
            continue;
        };
        let Some(mv) = by_ref.get(reference.as_str()) else {
            continue;
        };
        seen += 1;
        let children = child_nodes(text, fp.start + 1, fp.end - 1);
        let Some(fp_at) = children.iter().find(|n| node_head(text, n) == "at") else {
            return Err(format!("footprint {reference}: no (at …) node"));
        };
        let (_, _, old_rot) = parse_at(text, fp_at)
            .ok_or_else(|| format!("footprint {reference}: malformed (at …)"))?;
        let old_rot = old_rot.unwrap_or(0.0);
        let new_rot = mv.rotation_deg.unwrap_or(old_rot);
        let (x, y) = (mv.x_nm as f64 / 1_000_000.0, mv.y_nm as f64 / 1_000_000.0);
        let new_at = if new_rot.rem_euclid(360.0).abs() < 1e-9 {
            format!("(at {} {})", fmt_num(x), fmt_num(y))
        } else {
            format!(
                "(at {} {} {})",
                fmt_num(x),
                fmt_num(y),
                fmt_num(new_rot.rem_euclid(360.0))
            )
        };
        edits.push((fp_at.start, fp_at.end, new_at));
        let delta = new_rot - old_rot;
        if delta.abs() > 1e-9 {
            rotate_child_angles(text, &fp, fp_at, delta, &mut edits);
        }
    }
    if seen != by_ref.len() {
        let found: Vec<String> = by_ref.keys().map(|s| s.to_string()).collect();
        return Err(format!(
            "matched {seen} of {} footprints for offline move (requested: {})",
            by_ref.len(),
            found.join(", ")
        ));
    }
    Ok(apply_edits(text, edits))
}

/// Add `delta` degrees to the angle term of every `(at x y a)` in the
/// footprint's pads/texts/properties (KiCAD stores those angles with the
/// footprint rotation summed in). An `(at x y)` without an angle gains one.
fn rotate_child_angles(
    text: &str,
    fp: &Node,
    fp_at: &Node,
    delta: f64,
    edits: &mut Vec<(usize, usize, String)>,
) {
    let mut stack = vec![(fp.start + 1, fp.end - 1)];
    while let Some((s, e)) = stack.pop() {
        for node in child_nodes(text, s, e) {
            if node.start == fp_at.start {
                continue;
            }
            let head = node_head(text, &node);
            if head == "at" {
                if let Some((x, y, a)) = parse_at(text, &node) {
                    let a = (a.unwrap_or(0.0) + delta).rem_euclid(360.0);
                    let new = if a.abs() < 1e-9 {
                        format!("(at {} {})", fmt_num(x), fmt_num(y))
                    } else {
                        format!("(at {} {} {})", fmt_num(x), fmt_num(y), fmt_num(a))
                    };
                    edits.push((node.start, node.end, new));
                }
            } else if matches!(head, "pad" | "property" | "fp_text") {
                stack.push((node.start + 1, node.end - 1));
            }
        }
    }
}

fn apply_edits(text: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(text.len() + 256);
    let mut pos = 0usize;
    for (start, end, replacement) in edits {
        out.push_str(&text[pos..start]);
        out.push_str(&replacement);
        pos = end;
    }
    out.push_str(&text[pos..]);
    out
}

/// Net name → net code from the document's top-level `(net N "NAME")`
/// declarations.
pub fn parse_net_codes(text: &str) -> Result<BTreeMap<String, i32>, String> {
    let (body_start, body_end) = root_body(text)?;
    let mut codes = BTreeMap::new();
    for node in child_nodes(text, body_start, body_end) {
        if node_head(text, &node) != "net" {
            continue;
        }
        let inner = &text[node.start + 1..node.end - 1];
        let mut it = inner.split_whitespace();
        it.next(); // "net"
        let Some(code) = it.next().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let rest = inner[inner.find('"').unwrap_or(inner.len())..].trim();
        let name = rest.trim_matches('"');
        if !name.is_empty() {
            codes.insert(name.to_string(), code);
        }
    }
    Ok(codes)
}

/// Remove all top-level `(segment …)` and `(via …)` copper. Returns the new
/// text and the removed (tracks, vias) counts.
pub fn strip_copper(text: &str) -> Result<(String, usize, usize), String> {
    let (body_start, body_end) = root_body(text)?;
    let mut edits = Vec::new();
    let (mut tracks, mut vias) = (0usize, 0usize);
    for node in child_nodes(text, body_start, body_end) {
        match node_head(text, &node) {
            "segment" => {
                tracks += 1;
                edits.push((line_start(text, node.start), node.end, String::new()));
            }
            "via" => {
                vias += 1;
                edits.push((line_start(text, node.start), node.end, String::new()));
            }
            _ => {}
        }
    }
    Ok((apply_edits(text, edits), tracks, vias))
}

fn line_start(text: &str, pos: usize) -> usize {
    text[..pos]
        .rfind('\n')
        .map(|nl| {
            if text[nl + 1..pos].trim().is_empty() {
                nl + 1
            } else {
                pos
            }
        })
        .unwrap_or(pos)
}

/// Append a routed [`RouteSolution`] as `(segment …)`/`(via …)` nodes before
/// the document's closing paren, mirroring the IPC writer's mapping (trace
/// polylines → per-window segments; via spans → layer pairs).
pub fn append_copper(
    text: &str,
    solution: &RouteSolution,
    layer_count: u32,
    layer_names: &[String],
) -> Result<String, String> {
    let codes = parse_net_codes(text)?;
    let net_code = |name: &str| -> Result<i32, String> {
        codes
            .get(name)
            .copied()
            .ok_or_else(|| format!("net {name} not declared in the board file"))
    };
    let layer_name = |layer: &LayerRef| -> &str {
        let idx = layer.index(layer_count).unwrap_or(0) as usize;
        layer_names.get(idx).map(String::as_str).unwrap_or("F.Cu")
    };
    let mut out = String::new();
    let mut uuid_n = 0usize;
    let mut uuid = |tag: &str| {
        uuid_n += 1;
        // Deterministic per-file placeholder ids, unique within this write.
        format!("offline-{tag}-{uuid_n:05}")
    };
    for trace in &solution.traces {
        let code = net_code(&trace.connection)?;
        let layer = layer_name(&trace.layer);
        for w in trace.path.windows(2) {
            if (w[0].x - w[1].x).abs() < 1e-9 && (w[0].y - w[1].y).abs() < 1e-9 {
                continue;
            }
            out.push_str(&format!(
                "\t(segment\n\t\t(start {} {})\n\t\t(end {} {})\n\t\t(width {})\n\t\t(layer \"{}\")\n\t\t(net {})\n\t\t(uuid \"{}\")\n\t)\n",
                fmt_num(w[0].x), fmt_num(w[0].y), fmt_num(w[1].x), fmt_num(w[1].y),
                fmt_num(trace.width), layer, code, uuid("seg"),
            ));
        }
    }
    for via in &solution.vias {
        let code = net_code(&via.connection)?;
        let (from, to, kind) = match &via.span {
            ViaSpan::Through => (
                layer_names.first().map(String::as_str).unwrap_or("F.Cu"),
                layer_names.last().map(String::as_str).unwrap_or("B.Cu"),
                None,
            ),
            ViaSpan::Partial { from, to, micro } => (
                layer_names
                    .get(*from as usize)
                    .map(String::as_str)
                    .unwrap_or("F.Cu"),
                layer_names
                    .get(*to as usize)
                    .map(String::as_str)
                    .unwrap_or("B.Cu"),
                Some(if *micro { "micro" } else { "blind" }),
            ),
        };
        let kind_line = kind.map(|k| format!("\t\t({k} yes)\n")).unwrap_or_default();
        out.push_str(&format!(
            "\t(via\n{kind_line}\t\t(at {} {})\n\t\t(size {})\n\t\t(drill {})\n\t\t(layers \"{}\" \"{}\")\n\t\t(net {})\n\t\t(uuid \"{}\")\n\t)\n",
            fmt_num(via.at.x), fmt_num(via.at.y), fmt_num(via.diameter), fmt_num(via.drill),
            from, to, code, uuid("via"),
        ));
    }
    // Insert at the root document's own close, not the last `)` byte in the
    // file.  KiCad files may legally carry trailing whitespace/comments; a
    // parenthesis there must not move newly routed copper outside the board.
    let (_, close) = root_body(text)?;
    let mut result = String::with_capacity(text.len() + out.len());
    result.push_str(&text[..close]);
    result.push_str(&out);
    result.push_str(&text[close..]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{Point2, Trace, Via};

    const BOARD: &str = r#"(kicad_pcb
	(version 20240108)
	(net 0 "")
	(net 1 "GND")
	(net 2 "VOUT")
	(footprint "Resistor_SMD:R_0603_1608Metric"
		(layer "F.Cu")
		(uuid "aaa")
		(at 12 20)
		(property "Reference" "R1"
			(at 0 -1.65 0)
		)
		(pad "1" smd roundrect
			(at -0.7875 0)
			(net 2 "VOUT")
		)
		(pad "2" smd roundrect
			(at 0.7875 0 180)
			(net 1 "GND")
		)
	)
	(segment
		(start 1 1)
		(end 2 1)
		(width 0.25)
		(layer "F.Cu")
		(net 1)
		(uuid "bbb")
	)
)
"#;

    #[test]
    fn patch_moves_and_rotates_footprint() {
        let moves = vec![FootprintMove {
            reference: "R1".to_string(),
            x_nm: 30_000_000,
            y_nm: 25_500_000,
            rotation_deg: Some(90.0),
        }];
        let out = patch_placements(BOARD, &moves).unwrap();
        assert!(out.contains("(at 30 25.5 90)"), "{out}");
        // pad angles gain the delta; pad locals stay put
        assert!(out.contains("(at -0.7875 0 90)"), "{out}");
        assert!(out.contains("(at 0.7875 0 270)"), "{out}");
        assert!(out.contains("(at 0 -1.65 90)"), "{out}");
    }

    #[test]
    fn patch_translation_only_keeps_angles() {
        let moves = vec![FootprintMove {
            reference: "R1".to_string(),
            x_nm: 5_000_000,
            y_nm: 6_000_000,
            rotation_deg: None,
        }];
        let out = patch_placements(BOARD, &moves).unwrap();
        assert!(out.contains("(at 5 6)"), "{out}");
        assert!(out.contains("(at 0.7875 0 180)"), "{out}");
    }

    #[test]
    fn patch_unknown_reference_errors() {
        let moves = vec![FootprintMove {
            reference: "R9".to_string(),
            x_nm: 0,
            y_nm: 0,
            rotation_deg: None,
        }];
        assert!(patch_placements(BOARD, &moves).is_err());
    }

    #[test]
    fn strip_and_append_copper_round_trip() {
        let (stripped, tracks, vias) = strip_copper(BOARD).unwrap();
        assert_eq!((tracks, vias), (1, 0));
        assert!(!stripped.contains("(segment"));

        let solution = RouteSolution {
            traces: vec![Trace {
                connection: "GND".to_string(),
                layer: LayerRef::top(),
                width: 0.3,
                path: vec![
                    Point2 { x: 1.0, y: 2.0 },
                    Point2 { x: 4.0, y: 2.0 },
                    Point2 { x: 4.0, y: 6.0 },
                ],
            }],
            vias: vec![Via {
                connection: "VOUT".to_string(),
                at: Point2 { x: 4.0, y: 6.0 },
                diameter: 0.6,
                drill: 0.3,
                span: ViaSpan::Through,
            }],
        };
        let layers = vec!["F.Cu".to_string(), "B.Cu".to_string()];
        let out = append_copper(&stripped, &solution, 2, &layers).unwrap();
        assert_eq!(out.matches("(segment").count(), 2);
        assert!(out.contains("(net 1)"), "GND code");
        assert!(out.contains("(via"), "{out}");
        assert!(out.contains("(layers \"F.Cu\" \"B.Cu\")"));
        // still balanced: root close paren last
        assert!(out.trim_end().ends_with(')'));
    }

    #[test]
    fn net_codes_parse_from_declarations() {
        let codes = parse_net_codes(BOARD).unwrap();
        assert_eq!(codes.get("GND"), Some(&1));
        assert_eq!(codes.get("VOUT"), Some(&2));
    }

    #[test]
    fn append_copper_targets_document_close_not_trailing_parenthesis() {
        let board = format!("{BOARD}; retained trailing comment )\n");
        let solution = RouteSolution {
            traces: vec![Trace {
                connection: "GND".to_owned(),
                layer: LayerRef::top(),
                width: 0.25,
                path: vec![Point2::new(2.0, 2.0), Point2::new(3.0, 2.0)],
            }],
            vias: vec![],
        };
        let out = append_copper(
            &board,
            &solution,
            2,
            &["F.Cu".to_owned(), "B.Cu".to_owned()],
        )
        .unwrap();

        let copper = out.find("\t(segment\n").unwrap();
        let root_close = out.find("\n)\n; retained").unwrap();
        assert!(
            copper < root_close,
            "new copper must remain inside kicad_pcb"
        );
        assert!(out.ends_with("; retained trailing comment )\n"));
    }
}
