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

use std::{collections::BTreeMap, path::Path};

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

/// Copper layers enabled by the board file's authoritative `(layers ...)`
/// table, in stack order. Unlike the live IPC stackup count, this cannot refer
/// to a board that was open immediately before the current project.
pub(super) fn board_copper_layer_names(text: &str) -> Result<Vec<String>, String> {
    let (body_start, body_end) = root_body(text)?;
    let layers = child_nodes(text, body_start, body_end)
        .into_iter()
        .find(|node| node_head(text, node) == "layers")
        .ok_or("board has no (layers ...) table")?;
    let mut names = Vec::new();
    for node in child_nodes(text, layers.start + 1, layers.end - 1) {
        let body = &text[node.start + 1..node.end - 1];
        let Some(first_quote) = body.find('"') else {
            continue;
        };
        let rest = &body[first_quote + 1..];
        let Some(end_quote) = rest.find('"') else {
            continue;
        };
        let name = &rest[..end_quote];
        if name == "F.Cu" || name == "B.Cu" || (name.starts_with("In") && name.ends_with(".Cu")) {
            names.push(name.to_owned());
        }
    }
    if names.len() < 2 || names.first().is_none_or(|name| name != "F.Cu") {
        return Err(format!(
            "board layer table has invalid copper stack ({})",
            names.join(", ")
        ));
    }
    // KiCad writes B.Cu before inner layers in some file versions. Normalize
    // by copper semantics rather than textual entry order.
    names.sort_by_key(|name| {
        if name == "F.Cu" {
            0
        } else if name == "B.Cu" {
            u32::MAX
        } else {
            name.strip_prefix("In")
                .and_then(|s| s.strip_suffix(".Cu"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(u32::MAX - 1)
        }
    });
    if names.last().is_none_or(|name| name != "B.Cu") {
        return Err(format!(
            "board layer table has invalid copper stack ({})",
            names.join(", ")
        ));
    }
    Ok(names)
}

/// Full-board rectangular copper zones, as authoritative plane-net assignments.
/// Gordian's seed writer emits its planes in exactly this form; requiring the
/// zone rectangle to cover the Edge.Cuts rectangle avoids promoting local pours.
pub(super) fn board_file_plane_nets(text: &str) -> Result<BTreeMap<String, u32>, String> {
    let layers = board_copper_layer_names(text)?;
    let (body_start, body_end) = root_body(text)?;
    let top = child_nodes(text, body_start, body_end);
    let outline = top
        .iter()
        .find(|node| {
            node_head(text, node) == "gr_rect"
                && text[node.start..node.end].contains("(layer \"Edge.Cuts\")")
        })
        .and_then(|node| {
            let children = child_nodes(text, node.start + 1, node.end - 1);
            let start = children.iter().find(|n| node_head(text, n) == "start")?;
            let end = children.iter().find(|n| node_head(text, n) == "end")?;
            let (x0, y0, _) = parse_at(text, start)?;
            let (x1, y1, _) = parse_at(text, end)?;
            Some((x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)))
        });
    let Some((min_x, min_y, max_x, max_y)) = outline else {
        return Ok(BTreeMap::new());
    };
    let mut planes = BTreeMap::new();
    for zone in top.iter().filter(|node| node_head(text, node) == "zone") {
        let block = &text[zone.start..zone.end];
        let Some(net) = quoted_field(block, "net_name") else {
            continue;
        };
        let Some(layer) = quoted_field(block, "layer") else {
            continue;
        };
        let Some(layer_idx) = layers.iter().position(|name| name == layer) else {
            continue;
        };
        let Some(polygon) = child_nodes(text, zone.start + 1, zone.end - 1)
            .into_iter()
            .find(|node| node_head(text, node) == "polygon")
        else {
            continue;
        };
        let points: Vec<_> = child_nodes(text, polygon.start + 1, polygon.end - 1)
            .into_iter()
            .flat_map(|node| child_nodes(text, node.start + 1, node.end - 1))
            .filter(|node| node_head(text, node) == "xy")
            .filter_map(|node| parse_at(text, &node).map(|(x, y, _)| (x, y)))
            .collect();
        let corners = [
            (min_x, min_y),
            (max_x, min_y),
            (max_x, max_y),
            (min_x, max_y),
        ];
        if points.len() == 4
            && corners.iter().all(|&(cx, cy)| {
                points
                    .iter()
                    .any(|&(x, y)| (x - cx).abs() <= 1e-6 && (y - cy).abs() <= 1e-6)
            })
        {
            planes.entry(net.to_owned()).or_insert(layer_idx as u32);
        }
    }
    Ok(planes)
}

fn quoted_field<'a>(block: &'a str, head: &str) -> Option<&'a str> {
    let prefix = format!("({head} \"");
    let rest = block.split_once(&prefix)?.1;
    Some(rest.split_once('"')?.0)
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

/// Local position of a visible footprint text field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FieldPosition {
    pub x: f64,
    pub y: f64,
}

fn field_prefix(field: &str) -> String {
    format!("(property \"{field}\"")
}

pub(super) fn field_position(text: &str, reference: &str, field: &str) -> Option<FieldPosition> {
    let prefix = field_prefix(field);
    let (body_start, body_end) = root_body(text).ok()?;
    for fp in child_nodes(text, body_start, body_end) {
        if node_head(text, &fp) != "footprint"
            || footprint_reference(text, &fp).as_deref() != Some(reference)
        {
            continue;
        }
        let property = child_nodes(text, fp.start + 1, fp.end - 1)
            .into_iter()
            .find(|node| {
                node_head(text, node) == "property"
                    && text[node.start..node.end].starts_with(&prefix)
                    && !text[node.start..node.end].contains("(hide yes)")
            })?;
        let at = child_nodes(text, property.start + 1, property.end - 1)
            .into_iter()
            .find(|node| node_head(text, node) == "at")?;
        let (x, y, _) = parse_at(text, &at)?;
        return Some(FieldPosition { x, y });
    }
    None
}

/// Rewrite a visible text field's font size and stroke thickness.
pub(super) fn patch_field_text_size(
    text: &str,
    reference: &str,
    field: &str,
    size_mm: f64,
    thickness_mm: f64,
) -> Result<String, String> {
    let prefix = field_prefix(field);
    let (body_start, body_end) = root_body(text)?;
    for fp in child_nodes(text, body_start, body_end) {
        if node_head(text, &fp) != "footprint"
            || footprint_reference(text, &fp).as_deref() != Some(reference)
        {
            continue;
        }
        let property = child_nodes(text, fp.start + 1, fp.end - 1)
            .into_iter()
            .find(|node| {
                node_head(text, node) == "property"
                    && text[node.start..node.end].starts_with(&prefix)
                    && !text[node.start..node.end].contains("(hide yes)")
            })
            .ok_or_else(|| format!("footprint {reference}: no visible {field} property"))?;
        let effects = child_nodes(text, property.start + 1, property.end - 1)
            .into_iter()
            .find(|node| node_head(text, node) == "effects")
            .ok_or_else(|| format!("footprint {reference}: {field} has no effects"))?;
        let font = child_nodes(text, effects.start + 1, effects.end - 1)
            .into_iter()
            .find(|node| node_head(text, node) == "font")
            .ok_or_else(|| format!("footprint {reference}: {field} has no font"))?;
        let mut edits = Vec::new();
        for node in child_nodes(text, font.start + 1, font.end - 1) {
            match node_head(text, &node) {
                "size" => edits.push((
                    node.start,
                    node.end,
                    format!("(size {} {})", fmt_num(size_mm), fmt_num(size_mm)),
                )),
                "thickness" => edits.push((
                    node.start,
                    node.end,
                    format!("(thickness {})", fmt_num(thickness_mm)),
                )),
                _ => {}
            }
        }
        if edits.is_empty() {
            return Err(format!("footprint {reference}: {field} font has no size"));
        }
        return Ok(apply_edits(text, edits));
    }
    Err(format!("footprint {reference}: not found"))
}

/// Hide a footprint text field entirely.
pub(super) fn patch_field_hidden(
    text: &str,
    reference: &str,
    field: &str,
) -> Result<String, String> {
    let prefix = field_prefix(field);
    let (body_start, body_end) = root_body(text)?;
    for fp in child_nodes(text, body_start, body_end) {
        if node_head(text, &fp) != "footprint"
            || footprint_reference(text, &fp).as_deref() != Some(reference)
        {
            continue;
        }
        let property = child_nodes(text, fp.start + 1, fp.end - 1)
            .into_iter()
            .find(|node| {
                node_head(text, node) == "property"
                    && text[node.start..node.end].starts_with(&prefix)
            })
            .ok_or_else(|| format!("footprint {reference}: no {field} property"))?;
        if text[property.start..property.end].contains("(hide yes)") {
            return Ok(text.to_string());
        }
        let effects = child_nodes(text, property.start + 1, property.end - 1)
            .into_iter()
            .find(|node| node_head(text, node) == "effects")
            .ok_or_else(|| format!("footprint {reference}: {field} has no effects"))?;
        let insertion = effects.start;
        let mut out = String::with_capacity(text.len() + 12);
        out.push_str(&text[..insertion]);
        out.push_str("(hide yes) ");
        out.push_str(&text[insertion..]);
        return Ok(out);
    }
    Err(format!("footprint {reference}: not found"))
}

/// Footprints carrying a visible silkscreen text field named `field`.
///
/// KiCad 9 DRC reports omit `PCB_FIELD` items other than Reference/Value, so
/// violations caused by generated fields arrive without attribution; this scan
/// recovers the candidate owners directly from the board text.
pub(super) fn silk_field_owners(text: &str, field: &str) -> Vec<String> {
    let prefix = field_prefix(field);
    let Ok((body_start, body_end)) = root_body(text) else {
        return Vec::new();
    };
    let mut owners = Vec::new();
    for fp in child_nodes(text, body_start, body_end) {
        if node_head(text, &fp) != "footprint" {
            continue;
        }
        let on_silk = child_nodes(text, fp.start + 1, fp.end - 1)
            .into_iter()
            .any(|node| {
                let body = &text[node.start..node.end];
                node_head(text, &node) == "property"
                    && body.starts_with(&prefix)
                    && !body.contains("(hide yes)")
                    && (body.contains("(layer \"F.SilkS\")")
                        || body.contains("(layer \"B.SilkS\")"))
            });
        if on_silk && let Some(reference) = footprint_reference(text, &fp) {
            owners.push(reference);
        }
    }
    owners
}

/// A footprint's board placement: position and rotation in degrees.
pub(super) fn footprint_placement(text: &str, reference: &str) -> Option<(f64, f64, f64)> {
    let (body_start, body_end) = root_body(text).ok()?;
    for fp in child_nodes(text, body_start, body_end) {
        if node_head(text, &fp) != "footprint"
            || footprint_reference(text, &fp).as_deref() != Some(reference)
        {
            continue;
        }
        let at = child_nodes(text, fp.start + 1, fp.end - 1)
            .into_iter()
            .find(|node| node_head(text, node) == "at")?;
        let (x, y, angle) = parse_at(text, &at)?;
        return Some((x, y, angle.unwrap_or(0.0)));
    }
    None
}

/// Bounding box of the board outline: every `(start/end/mid/center …)` point of
/// top-level Edge.Cuts graphics.
pub(super) fn board_outline_bbox(text: &str) -> Option<(f64, f64, f64, f64)> {
    let (body_start, body_end) = root_body(text).ok()?;
    let mut bbox: Option<(f64, f64, f64, f64)> = None;
    for node in child_nodes(text, body_start, body_end) {
        let body = &text[node.start..node.end];
        if !node_head(text, &node).starts_with("gr_") || !body.contains("(layer \"Edge.Cuts\")") {
            continue;
        }
        for point in child_nodes(text, node.start + 1, node.end - 1) {
            if !matches!(node_head(text, &point), "start" | "end" | "mid" | "center") {
                continue;
            }
            let inner = &text[point.start + 1..point.end - 1];
            let mut it = inner.split_whitespace().skip(1);
            let (Some(Ok(x)), Some(Ok(y))) = (
                it.next().map(str::parse::<f64>),
                it.next().map(str::parse::<f64>),
            ) else {
                continue;
            };
            bbox = Some(match bbox {
                None => (x, y, x, y),
                Some((min_x, min_y, max_x, max_y)) => {
                    (min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y))
                }
            });
        }
    }
    bbox
}

/// Relocate one visible text field in footprint-local coordinates and
/// counter-rotate it so the rendered board text remains upright.
pub(super) fn patch_field_position(
    text: &str,
    reference: &str,
    field: &str,
    position: FieldPosition,
) -> Result<String, String> {
    let prefix = field_prefix(field);
    let (body_start, body_end) = root_body(text)?;
    for fp in child_nodes(text, body_start, body_end) {
        if node_head(text, &fp) != "footprint"
            || footprint_reference(text, &fp).as_deref() != Some(reference)
        {
            continue;
        }
        let property = child_nodes(text, fp.start + 1, fp.end - 1)
            .into_iter()
            .find(|node| {
                node_head(text, node) == "property"
                    && text[node.start..node.end].starts_with(&prefix)
            })
            .ok_or_else(|| format!("footprint {reference}: no {field} property"))?;
        if text[property.start..property.end].contains("(hide yes)") {
            return Err(format!("footprint {reference}: {field} property is hidden"));
        }
        let footprint_at = child_nodes(text, fp.start + 1, fp.end - 1)
            .into_iter()
            .find(|node| node_head(text, node) == "at")
            .ok_or_else(|| format!("footprint {reference}: no (at …) node"))?;
        let (_, _, footprint_angle) = parse_at(text, &footprint_at)
            .ok_or_else(|| format!("footprint {reference}: invalid (at …) node"))?;
        let upright_angle = (-footprint_angle.unwrap_or(0.0)).rem_euclid(360.0);
        let at = child_nodes(text, property.start + 1, property.end - 1)
            .into_iter()
            .find(|node| node_head(text, node) == "at")
            .ok_or_else(|| format!("footprint {reference}: {field} has no (at …) node"))?;
        let replacement = format!(
            "(at {} {} {})",
            fmt_num(position.x),
            fmt_num(position.y),
            fmt_num(upright_angle)
        );
        return Ok(apply_edits(text, vec![(at.start, at.end, replacement)]));
    }
    Err(format!("footprint {reference}: not found"))
}

fn fmt_num(v: f64) -> String {
    if v.abs() < 0.0000005 {
        return "0".to_string();
    }
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
        let (x, y) = (kicad_ipc::units::nm_to_mm(mv.x_nm), kicad_ipc::units::nm_to_mm(mv.y_nm));
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
    validate_route_layers(solution, layer_count, layer_names)?;
    let codes = parse_net_codes(text)?;
    let net_code = |name: &str| -> Result<i32, String> {
        codes
            .get(name)
            .copied()
            .ok_or_else(|| format!("net {name} not declared in the board file"))
    };
    let layer_name = |layer: &LayerRef| -> Result<&str, String> {
        let idx = layer.index(layer_count).ok_or_else(|| {
            format!(
                "invalid route layer `{}` for {layer_count}-layer board",
                layer.0
            )
        })? as usize;
        Ok(layer_names[idx].as_str())
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
        let layer = layer_name(&trace.layer)?;
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

/// Append copper to a board on disk without exposing a partially-written PCB.
///
/// The caller must close any live KiCad session before calling this: an open
/// editor still owns an older in-memory document and could overwrite this file
/// on a later save. Existing copper is retained because [`append_copper`] only
/// inserts the supplied solution before the root document close.
pub fn append_copper_file(
    path: &Path,
    solution: &RouteSolution,
    layer_count: u32,
    layer_names: &[String],
) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read board {}: {err}", path.display()))?;
    let updated = append_copper(&text, solution, layer_count, layer_names)?;
    gordian_runtime::workspace::atomic_write(path, updated.as_bytes())
        .map_err(|err| format!("could not replace board {}: {err}", path.display()))?;
    Ok(())
}

fn validate_route_layers(
    solution: &RouteSolution,
    layer_count: u32,
    layer_names: &[String],
) -> Result<(), String> {
    if layer_count < 2 || layer_names.len() != layer_count as usize {
        return Err(format!(
            "route stackup mismatch: problem has {layer_count} copper layers but board exposes {} ({})",
            layer_names.len(),
            layer_names.join(", ")
        ));
    }
    for (idx, actual) in layer_names.iter().enumerate() {
        let expected = if idx == 0 {
            "F.Cu".to_owned()
        } else if idx + 1 == layer_count as usize {
            "B.Cu".to_owned()
        } else {
            format!("In{idx}.Cu")
        };
        if actual != &expected {
            return Err(format!(
                "route stackup mismatch: layer {idx} is `{actual}`, expected `{expected}`"
            ));
        }
    }
    for trace in &solution.traces {
        if trace.layer.index(layer_count).is_none() {
            return Err(format!(
                "invalid route layer `{}` for {layer_count}-layer board",
                trace.layer.0
            ));
        }
    }
    for via in &solution.vias {
        if let ViaSpan::Partial { from, to, .. } = via.span
            && (from >= layer_count || to >= layer_count)
        {
            return Err(format!(
                "invalid via span {from}..{to} for {layer_count}-layer board"
            ));
        }
    }
    Ok(())
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
    fn field_relocation_stays_visible_and_upright() {
        assert_eq!(
            field_position(BOARD, "R1", "Reference"),
            Some(FieldPosition { x: 0.0, y: -1.65 })
        );

        let moved =
            patch_field_position(BOARD, "R1", "Reference", FieldPosition { x: 2.5, y: 1.5 })
                .unwrap();

        assert!(moved.contains("(property \"Reference\" \"R1\"\n\t\t\t(at 2.5 1.5 0)"));
        assert!(!moved.contains("(hide yes)"));
        assert!(moved.contains("(at 12 20)"), "footprint must not move");
        assert!(moved.contains("(at -0.7875 0)"), "pads must not move");

        let rotated_board = BOARD.replacen("(at 12 20)", "(at 12 20 90)", 1);
        let rotated = patch_field_position(
            &rotated_board,
            "R1",
            "Reference",
            FieldPosition { x: 2.5, y: 1.5 },
        )
        .unwrap();
        assert!(rotated.contains("(property \"Reference\" \"R1\"\n\t\t\t(at 2.5 1.5 270)"));
        assert!(rotated.contains("(at 12 20 90)"), "footprint must not move");
    }

    #[test]
    fn silk_field_owners_finds_visible_silk_fields_only() {
        let board = BOARD.replacen(
            "(property \"Reference\" \"R1\"\n\t\t\t(at 0 -1.65 0)\n\t\t)",
            "(property \"Reference\" \"R1\"\n\t\t\t(at 0 -1.65 0)\n\t\t)\n\t\t(property \"Function\" \"GND/VOUT\"\n\t\t\t(at 0 3 0)\n\t\t\t(layer \"F.SilkS\")\n\t\t)",
            1,
        );
        assert_eq!(silk_field_owners(&board, "Function"), vec!["R1"]);
        assert!(silk_field_owners(BOARD, "Function").is_empty());
        let hidden = board.replacen(
            "(property \"Function\" \"GND/VOUT\"\n\t\t\t(at 0 3 0)",
            "(property \"Function\" \"GND/VOUT\"\n\t\t\t(hide yes)\n\t\t\t(at 0 3 0)",
            1,
        );
        assert!(silk_field_owners(&hidden, "Function").is_empty());
    }

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
    fn board_layer_table_is_the_authoritative_copper_stack() {
        let board = BOARD.replacen(
            "\t(net 0 \"\")",
            "\t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(2 \"B.Cu\" signal)\n\t\t(5 \"F.SilkS\" user)\n\t)\n\t(net 0 \"\")",
            1,
        );
        assert_eq!(
            board_copper_layer_names(&board).unwrap(),
            vec!["F.Cu", "B.Cu"]
        );
    }

    #[test]
    fn full_board_zone_is_an_authoritative_plane_but_local_pour_is_not() {
        let board = r#"(kicad_pcb
            (layers (0 "F.Cu" signal) (4 "In1.Cu" signal) (6 "In2.Cu" signal) (2 "B.Cu" signal))
            (gr_rect (start 0 0) (end 20 10) (layer "Edge.Cuts"))
            (zone (net 1) (net_name "GND") (layer "In1.Cu")
                (polygon (pts (xy 0 0) (xy 20 0) (xy 20 10) (xy 0 10))))
            (zone (net 2) (net_name "+3V3") (layer "In2.Cu")
                (polygon (pts (xy 2 2) (xy 18 2) (xy 18 8) (xy 2 8))))
        )"#;

        assert_eq!(
            board_file_plane_nets(board).unwrap(),
            BTreeMap::from([("GND".to_owned(), 1)])
        );
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
    fn append_copper_file_preserves_existing_copper() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, BOARD).unwrap();
        let solution = RouteSolution {
            traces: vec![Trace {
                connection: "VOUT".to_owned(),
                layer: LayerRef::bottom(),
                width: 0.25,
                path: vec![Point2::new(4.0, 5.0), Point2::new(6.0, 5.0)],
            }],
            vias: vec![],
        };

        append_copper_file(&path, &solution, 2, &["F.Cu".to_owned(), "B.Cu".to_owned()]).unwrap();

        let out = std::fs::read_to_string(path).unwrap();
        assert_eq!(out.matches("(segment").count(), 2, "{out}");
        assert!(out.contains("(layer \"B.Cu\")"), "{out}");
        assert!(out.contains("(net 2)"), "{out}");
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

    #[test]
    fn append_copper_rejects_stale_or_disabled_route_layers() {
        let bottom = RouteSolution {
            traces: vec![Trace {
                connection: "GND".to_owned(),
                layer: LayerRef::bottom(),
                width: 0.25,
                path: vec![Point2::new(2.0, 2.0), Point2::new(3.0, 2.0)],
            }],
            vias: vec![],
        };
        let stale_stack = [
            "F.Cu".to_owned(),
            "In1.Cu".to_owned(),
            "In2.Cu".to_owned(),
            "B.Cu".to_owned(),
        ];
        assert!(
            append_copper(BOARD, &bottom, 2, &stale_stack)
                .unwrap_err()
                .contains("stackup mismatch")
        );
        assert!(
            append_copper(BOARD, &bottom, 2, &["F.Cu".to_owned(), "In1.Cu".to_owned()],)
                .unwrap_err()
                .contains("expected `B.Cu`")
        );

        let disabled_inner = RouteSolution {
            traces: vec![Trace {
                layer: LayerRef("inner1".to_owned()),
                ..bottom.traces[0].clone()
            }],
            vias: vec![],
        };
        assert!(
            append_copper(
                BOARD,
                &disabled_inner,
                2,
                &["F.Cu".to_owned(), "B.Cu".to_owned()],
            )
            .unwrap_err()
            .contains("invalid route layer")
        );
    }
}
