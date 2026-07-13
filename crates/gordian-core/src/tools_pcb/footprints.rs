//! Footprint discovery + assignment tools: `search_footprints`,
//! `get_footprint_info`, and `assign_footprints` (fill missing footprints before
//! placement).

use serde_json::{Value, json};

use kicad_footprint::{FootprintId, SearchQuery};

use crate::AgentRuntime;
use crate::tools::{compile_report, current_sch_text, require_str};

pub fn search_footprints(input: Value, ctx: &AgentRuntime) -> anyhow::Result<Value> {
    let query = require_str(&input, "query")?;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(ctx.config().tools.default_search_limit);
    let normalized = query.to_ascii_lowercase();
    let (search_query, note) = if normalized.contains("rp2040") {
        (
            "QFN-56 7x7 0.4".to_string(),
            Some(
                "RP2040 uses a 56-pin QFN package in KiCad libraries; do not substitute a BGA footprint.",
            ),
        )
    } else {
        (query.clone(), None)
    };

    let hits: Vec<Value> = ctx
        .footprint_catalog()?
        .search(SearchQuery::new(search_query).limit(limit))
        .into_iter()
        .map(|h| json!({ "lib_id": h.id.to_string(), "pad_count": h.pad_count }))
        .collect();

    let mut out = json!({ "hits": hits });
    if let Some(note) = note {
        out["note"] = json!(note);
    }
    Ok(out)
}

pub fn get_footprint_info(input: Value, ctx: &AgentRuntime) -> anyhow::Result<Value> {
    let lib_id = require_str(&input, "lib_id")?;
    let catalog = ctx.footprint_catalog()?;

    let id = match FootprintId::parse(&lib_id) {
        Ok(id) => id,
        Err(_) => {
            return Ok(json!({ "error": format!("invalid footprint id `{lib_id}`") }));
        }
    };

    match catalog.footprint(&id) {
        Ok(fp) => {
            // The model binds nets by pad NUMBER and never needs each pad's coordinates (it
            // doesn't place pads), so return the number list + a compact shape SUMMARY rather
            // than the full per-pad table — for a 256-ball BGA the old table was ~20k chars
            // re-sent every turn. min_pitch + pad_size let the model judge fine-pitch (pick a
            // clearance/via); technologies/layers tell it SMD vs thru-hole.
            let pad_numbers: Vec<&str> = fp
                .pads
                .iter()
                .map(|p| p.number.as_str())
                .filter(|n| !n.is_empty())
                .collect();
            let mut min_pitch = f64::INFINITY;
            for (i, a) in fp.pads.iter().enumerate() {
                for b in &fp.pads[i + 1..] {
                    let d = a.at.dist(b.at);
                    if d > geom::EPS && d < min_pitch {
                        min_pitch = d;
                    }
                }
            }
            let (mut wmin, mut wmax) = (f64::INFINITY, 0.0_f64);
            for p in &fp.pads {
                let s = p.size.x.min(p.size.y);
                wmin = wmin.min(s);
                wmax = wmax.max(p.size.x.max(p.size.y));
            }
            let techs: std::collections::BTreeSet<&str> =
                fp.pads.iter().map(|p| p.technology.as_str()).collect();
            Ok(json!({
                "lib_id": id.to_string(),
                "name": fp.name,
                "descr": fp.descr,
                "pad_count": fp.pads.len(),
                "pad_numbers": pad_numbers,
                "min_pitch_mm": if min_pitch.is_finite() { (min_pitch * 1000.0).round() / 1000.0 } else { 0.0 },
                "pad_min_dim_mm": if wmin.is_finite() { (wmin * 1000.0).round() / 1000.0 } else { 0.0 },
                "pad_max_dim_mm": (wmax * 1000.0).round() / 1000.0,
                "technologies": techs,
                "courtyard": bbox_json(&fp.courtyard),
                "courtyard_source": fp.courtyard_source.as_str(),
                "bbox": bbox_json(&fp.bounds),
            }))
        }
        Err(e) if e.is_not_found() => Ok(json!({
            "error": format!("unknown footprint `{lib_id}`"),
            "suggestions": catalog.suggest(&id).iter().map(|i| i.to_string()).collect::<Vec<_>>(),
        })),
        Err(e) => Ok(json!({
            "error": format!("footprint `{lib_id}` could not be read: {e}"),
        })),
    }
}

fn bbox_json(b: &geom::Rect) -> Value {
    json!({
        "min_x": b.min_x,
        "min_y": b.min_y,
        "max_x": b.max_x,
        "max_y": b.max_y,
        "width": b.width(),
        "height": b.height(),
    })
}

/// Assign a footprint in the working circuit-YAML draft.
///
/// Footprint assignment belongs to the schematic/circuit YAML, not PCB state.
/// Earlier this helper returned only prose instructions, which live models often
/// treated as a completed state change and then looped on regenerate_board. It now
/// performs the draft edit directly and returns a compact edit result, adding
/// compile diagnostics only when the edited draft has errors or warnings.
pub fn assign_footprints(input: Value, ctx: &AgentRuntime) -> anyhow::Result<Value> {
    let assignments = footprint_assignments(&input)?;
    let catalog = ctx.footprint_catalog()?;
    for assignment in &assignments {
        let id = match FootprintId::parse(&assignment.footprint) {
            Ok(id) => id,
            Err(_) => {
                return Ok(json!({
                    "error": format!(
                        "part {}: invalid footprint id `{}`",
                        assignment.reference, assignment.footprint
                    )
                }));
            }
        };
        if let Err(e) = catalog.footprint(&id) {
            if e.is_not_found() {
                return Ok(json!({
                    "error": format!(
                        "part {}: unknown footprint `{}`",
                        assignment.reference, assignment.footprint
                    ),
                    "suggestions": catalog.suggest(&id).iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                }));
            }
            return Ok(json!({
                "error": format!(
                    "part {}: footprint `{}` could not be read: {e}",
                    assignment.reference, assignment.footprint
                ),
            }));
        }
    }

    let Some(draft) = ctx.workspace().read_draft()? else {
        return Ok(json!({
            "error": "no draft exists — call read_schematic({source:\"draft\"}) (seeds a draft from the current schematic) or create_design first",
        }));
    };
    let mut edited = draft;
    let mut applied = Vec::new();
    for assignment in &assignments {
        let (next, edit_kind) =
            match patch_footprint(&edited, &assignment.reference, &assignment.footprint) {
                Ok(patched) => patched,
                Err(msg) => return Ok(json!({ "error": msg })),
            };
        edited = next;
        applied.push(json!({
            "reference": assignment.reference,
            "footprint": assignment.footprint,
            "edit": edit_kind,
        }));
    }
    ctx.workspace()
        .write_draft(&edited, current_sch_text(ctx).as_deref())?;

    let report = compile_report(&circuit_lang::compile(&edited, ctx.provider()).diagnostics);
    let errors = report.get("errors").and_then(Value::as_u64).unwrap_or(0);
    let warnings = report.get("warnings").and_then(Value::as_u64).unwrap_or(0);
    let mut out = json!({
        "ok": errors == 0,
        "assigned": applied,
        "count": assignments.len(),
        "next": "apply_design(), then regenerate_board",
    });
    if errors > 0 || warnings > 0 {
        out["errors"] = json!(errors);
        out["warnings"] = json!(warnings);
        out["diagnostics"] = report["diagnostics"].clone();
    }
    Ok(out)
}

struct FootprintAssignment {
    reference: String,
    footprint: String,
}

fn footprint_assignments(input: &Value) -> anyhow::Result<Vec<FootprintAssignment>> {
    let Some(items) = input.get("assignments") else {
        anyhow::bail!("missing required `assignments` array");
    };
    let Some(items) = items.as_array() else {
        anyhow::bail!("assignments must be an array of {{reference, footprint}}");
    };
    if items.is_empty() {
        anyhow::bail!("assignments must contain at least one item");
    }
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let reference = require_str(item, "reference")
                .map_err(|e| anyhow::anyhow!("assignments[{idx}].reference: {e}"))?;
            let footprint = require_str(item, "footprint")
                .map_err(|e| anyhow::anyhow!("assignments[{idx}].footprint: {e}"))?;
            Ok(FootprintAssignment {
                reference,
                footprint,
            })
        })
        .collect()
}

fn patch_footprint(
    draft: &str,
    reference: &str,
    footprint: &str,
) -> Result<(String, &'static str), String> {
    let mut lines: Vec<String> = draft.lines().map(str::to_owned).collect();
    let had_trailing_newline = draft.ends_with('\n');
    let target = format!("{reference}:");
    for i in 0..lines.len() {
        let line = &lines[i];
        let trimmed = line.trim_start();
        if let Some(target_pos) = line.find(&target)
            && line[target_pos..].contains('{')
            && line[target_pos..].contains('}')
        {
            let (line, kind) = patch_inline_component(line, target_pos, footprint)?;
            lines[i] = line;
            return Ok((join_lines(lines, had_trailing_newline), kind));
        }
        if !trimmed.starts_with(&target) {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let child_indent = " ".repeat(indent + 2);
        let mut end = i + 1;
        while end < lines.len() {
            let next = &lines[end];
            let next_trimmed = next.trim_start();
            if !next_trimmed.is_empty()
                && next.len() - next_trimmed.len() <= indent
                && next_trimmed.ends_with(':')
            {
                break;
            }
            end += 1;
        }
        for line in lines.iter_mut().take(end).skip(i + 1) {
            if line.trim_start().starts_with("footprint:") {
                let existing_indent = line.len() - line.trim_start().len();
                *line = format!(
                    "{}footprint: {}",
                    " ".repeat(existing_indent),
                    yaml_string(footprint)
                );
                return Ok((join_lines(lines, had_trailing_newline), "updated"));
            }
        }
        lines.insert(
            i + 1,
            format!("{child_indent}footprint: {}", yaml_string(footprint)),
        );
        return Ok((join_lines(lines, had_trailing_newline), "inserted"));
    }
    Err(format!(
        "component `{reference}` was not found in the working draft"
    ))
}

fn patch_inline_component(
    line: &str,
    target_pos: usize,
    footprint: &str,
) -> Result<(String, &'static str), String> {
    let open = line[target_pos..]
        .find('{')
        .map(|p| target_pos + p)
        .ok_or_else(|| "inline component map has no opening `{`".to_string())?;
    let close = matching_brace(line, open)
        .ok_or_else(|| "inline component map has no matching `}`".to_string())?;
    let body_start = open + 1;
    let body = &line[body_start..close];
    let value = format!("footprint: {}", yaml_string(footprint));
    if let Some(rel_pos) = body.find("footprint:") {
        let pos = body_start + rel_pos;
        let after = pos + "footprint:".len();
        let rel_end = line[after..close]
            .find([',', '}'])
            .ok_or_else(|| "inline component footprint field is malformed".to_string())?;
        let end = after + rel_end;
        let mut out = String::new();
        out.push_str(&line[..pos]);
        out.push_str(&value);
        out.push_str(&line[end..]);
        return Ok((out, "updated"));
    }
    let before = line[..close].trim_end();
    let sep = if before.ends_with('{') { "" } else { "," };
    Ok((
        format!("{before}{sep} {value}{}", &line[close..]),
        "inserted",
    ))
}

fn matching_brace(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_quote = false;
    let mut escape = false;
    for (offset, ch) in s[open..].char_indices() {
        if in_quote {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_quote = false;
            }
            continue;
        }
        match ch {
            '"' => in_quote = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn yaml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn join_lines(lines: Vec<String>, trailing_newline: bool) -> String {
    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_footprint_inserts_into_inline_component() {
        let draft = "version: 1\nblocks: {main: {components: {R1: {part: R, between: [A, B]}}}}\n";
        let (patched, kind) = patch_footprint(draft, "R1", "Fixtures:R_0603_1608Metric").unwrap();
        assert_eq!(kind, "inserted");
        assert!(
            patched.contains(
                "R1: {part: R, between: [A, B], footprint: \"Fixtures:R_0603_1608Metric\"}"
            )
        );
    }

    #[test]
    fn patch_footprint_updates_block_component() {
        let draft = "\
version: 1
blocks:
  main:
    components:
      R1:
        part: R
        footprint: \"Old:Footprint\"
        between: [A, B]
";
        let (patched, kind) = patch_footprint(draft, "R1", "Fixtures:R_0603_1608Metric").unwrap();
        assert_eq!(kind, "updated");
        assert!(patched.contains("        footprint: \"Fixtures:R_0603_1608Metric\"\n"));
        assert!(!patched.contains("Old:Footprint"));
    }

    #[test]
    fn patch_footprint_inserts_into_block_component() {
        let draft = "\
version: 1
blocks:
  main:
    components:
      R1:
        part: R
        between: [A, B]
";
        let (patched, kind) = patch_footprint(draft, "R1", "Fixtures:R_0603_1608Metric").unwrap();
        assert_eq!(kind, "inserted");
        assert!(patched.contains(
            "      R1:\n        footprint: \"Fixtures:R_0603_1608Metric\"\n        part: R"
        ));
    }
}
