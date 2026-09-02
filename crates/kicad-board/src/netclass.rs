//! Atomic offline KiCad net-class persistence.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::patch::{Node, atomic_write, child_nodes, node_head, parse_net_codes, root_body};

/// One net-class definition and the board nets assigned to it.
#[derive(Clone, Debug, PartialEq)]
pub struct NetClassUpdate {
    pub name: String,
    pub width: f64,
    pub clearance: f64,
    pub nets: Vec<String>,
}

/// Durable changes made by an offline net-class update.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetClassUpdateReport {
    pub changed: bool,
    pub board_changed: bool,
    pub project_changed: bool,
    pub nets: Vec<String>,
    pub classes: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct PatchFacts {
    settings_changed: bool,
    assignment_changed: bool,
    affected_nets: BTreeSet<String>,
}

/// Patch one board's legacy net-class records without changing unrelated bytes.
pub fn patch_board_net_class(
    text: &str,
    update: &NetClassUpdate,
) -> Result<(String, NetClassUpdateReport), String> {
    validate_update(update)?;
    let known: BTreeSet<_> = parse_net_codes(text)?.into_keys().collect();
    validate_nets(&known, &update.nets)?;
    let requested: BTreeSet<&str> = update.nets.iter().map(String::as_str).collect();
    let (body_start, body_end) = root_body(text)?;
    let classes: Vec<_> = child_nodes(text, body_start, body_end)
        .into_iter()
        .filter(|node| node_head(text, node) == "net_class")
        .collect();
    if classes.is_empty() {
        return Ok((text.to_owned(), NetClassUpdateReport::default()));
    }
    let mut edits = Vec::new();
    let mut target_found = false;
    let mut facts = PatchFacts::default();

    for class in &classes {
        let Some(name) = node_quoted_atom(text, class, 1) else {
            continue;
        };
        let block = &text[class.start..class.end];
        let members = class_members(block);
        if name == update.name {
            target_found = true;
            let (replacement, class_facts) = patch_target_class(block, update, &requested)?;
            facts.settings_changed |= class_facts.settings_changed;
            facts.assignment_changed |= class_facts.assignment_changed;
            facts.affected_nets.extend(class_facts.affected_nets);
            if replacement != block {
                edits.push((class.start, class.end, replacement));
            }
        } else if members
            .iter()
            .any(|member| requested.contains(member.as_str()))
        {
            let replacement = remove_members(block, &requested)?;
            facts.assignment_changed = true;
            facts.affected_nets.extend(
                members
                    .into_iter()
                    .filter(|member| requested.contains(member.as_str())),
            );
            edits.push((class.start, class.end, replacement));
        }
    }

    if !target_found {
        let insertion = classes.last().map_or(body_end, |class| class.end);
        let class = new_board_class(update, insertion == body_end);
        edits.push((insertion, insertion, class));
        facts.settings_changed = true;
        facts.assignment_changed = true;
        facts.affected_nets.extend(update.nets.iter().cloned());
    }

    let updated = apply_edits(text, edits);
    Ok((
        updated.clone(),
        report(text != updated, true, false, update, facts),
    ))
}

/// Patch project net settings while retaining bytes outside the two changed values.
pub fn patch_project_net_class(
    text: &str,
    update: &NetClassUpdate,
) -> Result<(String, NetClassUpdateReport), String> {
    validate_update(update)?;
    let root: Value =
        serde_json::from_str(text).map_err(|err| format!("invalid project JSON: {err}"))?;
    let net_settings = root
        .get("net_settings")
        .and_then(Value::as_object)
        .ok_or("project has no net_settings object")?;
    let mut classes = net_settings
        .get("classes")
        .and_then(Value::as_array)
        .cloned()
        .ok_or("project net_settings.classes is not an array")?;
    let mut assignments = match net_settings.get("netclass_assignments") {
        Some(Value::Object(value)) => value.clone(),
        Some(Value::Null) | None => Map::new(),
        Some(_) => return Err("project net_settings.netclass_assignments is not an object".into()),
    };
    let desired_priority = classes
        .iter()
        .filter(|class| class.get("name").and_then(Value::as_str) != Some(update.name.as_str()))
        .filter_map(|class| class.get("priority").and_then(Value::as_i64))
        .min()
        .unwrap_or(0_i64)
        .saturating_sub(1)
        .max(i64::from(i32::MIN));
    let mut facts = PatchFacts::default();
    let target = classes
        .iter_mut()
        .find(|class| class.get("name").and_then(Value::as_str) == Some(update.name.as_str()));
    if let Some(class) = target {
        let object = class
            .as_object_mut()
            .ok_or("project net class is not an object")?;
        let old_width = object.get("track_width").and_then(Value::as_f64);
        let old_clearance = object.get("clearance").and_then(Value::as_f64);
        let old_priority = object.get("priority").and_then(Value::as_i64);
        if old_width != Some(update.width)
            || old_clearance != Some(update.clearance)
            || old_priority != Some(desired_priority)
        {
            object.insert("track_width".into(), json!(update.width));
            object.insert("clearance".into(), json!(update.clearance));
            object.insert("priority".into(), json!(desired_priority));
            facts.settings_changed = true;
            for (net, assigned) in &assignments {
                if assignment_contains(assigned, &update.name) {
                    facts.affected_nets.insert(net.clone());
                }
            }
        }
    } else {
        classes.push(json!({
            "clearance": update.clearance,
            "name": update.name,
            "priority": desired_priority,
            "track_width": update.width,
        }));
        facts.settings_changed = true;
    }

    for net in &update.nets {
        let desired = json!([update.name]);
        if assignments.get(net) != Some(&desired) {
            assignments.insert(net.clone(), desired);
            facts.assignment_changed = true;
            facts.affected_nets.insert(net.clone());
        }
    }
    if facts.settings_changed {
        facts.affected_nets.extend(update.nets.iter().cloned());
    }

    let net_settings_span = json_object_member(text, 0..text.len(), "net_settings")?
        .ok_or("project has no net_settings member")?;
    let classes_span = json_object_member(text, net_settings_span.clone(), "classes")?
        .ok_or("project net_settings has no classes member")?;
    let assignments_span = json_object_member(text, net_settings_span, "netclass_assignments")?;
    let mut edits = Vec::new();
    if facts.settings_changed {
        let classes_json = indent_json_value(&serde_json::to_string_pretty(&classes).unwrap(), 4);
        edits.push((classes_span.start, classes_span.end, classes_json));
    }
    if facts.assignment_changed {
        let assignments_json = indent_json_value(
            &serde_json::to_string_pretty(&Value::Object(assignments)).unwrap(),
            4,
        );
        if let Some(span) = assignments_span {
            edits.push((span.start, span.end, assignments_json));
        } else {
            let insertion = object_close(
                text,
                &json_object_member(text, 0..text.len(), "net_settings")?
                    .ok_or("project has no net_settings member")?,
            )?;
            edits.push((
                insertion,
                insertion,
                format!(",\n    \"netclass_assignments\": {assignments_json}"),
            ));
        }
    }
    let updated = apply_edits(text, edits);
    Ok((
        updated.clone(),
        report(text != updated, false, true, update, facts),
    ))
}

/// Read exact legacy board net-class assignments as per-net widths.
pub fn board_net_widths(text: &str) -> Result<BTreeMap<String, f64>, String> {
    let (body_start, body_end) = root_body(text)?;
    let mut widths = BTreeMap::new();
    for class in child_nodes(text, body_start, body_end)
        .into_iter()
        .filter(|node| node_head(text, node) == "net_class")
    {
        let block = &text[class.start..class.end];
        if node_quoted_atom(text, &class, 1).as_deref() == Some("Default") {
            continue;
        }
        let Some(width) = numeric_child(block, "trace_width") else {
            continue;
        };
        for net in class_members(block) {
            widths.insert(net, width);
        }
    }
    Ok(widths)
}

/// Read KiCad project net-class assignments as per-net widths.
pub fn project_net_widths(text: &str) -> Result<BTreeMap<String, f64>, String> {
    let root: Value =
        serde_json::from_str(text).map_err(|err| format!("invalid project JSON: {err}"))?;
    let Some(settings) = root.get("net_settings").and_then(Value::as_object) else {
        return Ok(BTreeMap::new());
    };
    let mut classes = BTreeMap::new();
    for class in settings
        .get("classes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(name), Some(width)) = (
            class.get("name").and_then(Value::as_str),
            class.get("track_width").and_then(Value::as_f64),
        ) else {
            continue;
        };
        if name == "Default" {
            continue;
        }
        let priority = class
            .get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(i64::from(i32::MAX) - 1);
        classes.insert(name, (priority, width));
    }
    let mut net_classes: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    if let Some(assignments) = settings
        .get("netclass_assignments")
        .and_then(Value::as_object)
    {
        for (net, assigned) in assignments {
            net_classes.entry(net.clone()).or_default().extend(
                assigned
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            );
        }
    }
    for pattern in settings
        .get("netclass_patterns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(net), Some(class)) = (
            pattern.get("pattern").and_then(Value::as_str),
            pattern.get("netclass").and_then(Value::as_str),
        ) else {
            continue;
        };
        net_classes.entry(net.to_owned()).or_default().insert(class);
    }
    let mut widths = BTreeMap::new();
    for (net, assigned) in net_classes {
        let selected = assigned
            .into_iter()
            .filter_map(|name| classes.get(name))
            .min_by_key(|(priority, _)| *priority);
        if let Some((_, width)) = selected {
            widths.insert(net, *width);
        }
    }
    Ok(widths)
}

/// Atomically update both KiCad board and project net-class representations.
pub fn write_net_class_update(
    board_path: &Path,
    project_path: &Path,
    update: &NetClassUpdate,
) -> Result<NetClassUpdateReport, String> {
    let board = std::fs::read_to_string(board_path)
        .map_err(|err| format!("could not read board {}: {err}", board_path.display()))?;
    let project = std::fs::read_to_string(project_path)
        .map_err(|err| format!("could not read project {}: {err}", project_path.display()))?;
    let (new_board, board_report) = patch_board_net_class(&board, update)?;
    let (new_project, project_report) = patch_project_net_class(&project, update)?;
    let mut report = merge_reports(board_report, project_report, update);
    if !report.changed {
        return Ok(report);
    }

    if report.project_changed {
        atomic_write(project_path, new_project.as_bytes()).map_err(|err| {
            format!(
                "could not replace project {}: {err}",
                project_path.display()
            )
        })?;
    }
    if report.board_changed
        && let Err(err) = atomic_write(board_path, new_board.as_bytes())
    {
        if !report.project_changed {
            return Err(format!(
                "could not replace board {}: {err}",
                board_path.display()
            ));
        }
        let rollback = atomic_write(project_path, project.as_bytes());
        return Err(match rollback {
            Ok(()) => format!(
                "could not replace board {}: {err}; restored the project file",
                board_path.display()
            ),
            Err(rollback) => format!(
                "could not replace board {}: {err}; project rollback also failed: {rollback}",
                board_path.display()
            ),
        });
    }
    report.board_changed = board != new_board;
    report.project_changed = project != new_project;
    Ok(report)
}

fn validate_update(update: &NetClassUpdate) -> Result<(), String> {
    if update.name.is_empty() || update.name == "Default" {
        return Err("net class name must be non-empty and not Default".into());
    }
    if !update.width.is_finite() || update.width <= 0.0 {
        return Err(format!(
            "net width must be greater than zero, got {}",
            update.width
        ));
    }
    if !update.clearance.is_finite() || update.clearance < 0.0 {
        return Err(format!(
            "net clearance must be non-negative, got {}",
            update.clearance
        ));
    }
    if update.nets.is_empty() {
        return Err("at least one net is required".into());
    }
    Ok(())
}

fn validate_nets(known: &BTreeSet<String>, requested: &[String]) -> Result<(), String> {
    let missing: Vec<_> = requested
        .iter()
        .filter(|net| !known.contains(*net))
        .cloned()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("board has no net(s): {}", missing.join(", ")))
    }
}

fn patch_target_class(
    block: &str,
    update: &NetClassUpdate,
    requested: &BTreeSet<&str>,
) -> Result<(String, PatchFacts), String> {
    let mut edits = Vec::new();
    let mut facts = PatchFacts::default();
    let nodes = child_nodes(block, 1, block.len() - 1);
    let mut has_width = false;
    let mut has_clearance = false;
    let members = class_members(block);
    if numeric_child(block, "trace_width") != Some(update.width)
        || numeric_child(block, "clearance") != Some(update.clearance)
    {
        facts.settings_changed = true;
        facts.affected_nets.extend(members.iter().cloned());
    }
    for node in nodes {
        match node_head(block, &node) {
            "trace_width" => {
                has_width = true;
                if numeric_node(block, &node) != Some(update.width) {
                    edits.push((
                        node.start,
                        node.end,
                        format!("(trace_width {})", fmt_num(update.width)),
                    ));
                }
            }
            "clearance" => {
                has_clearance = true;
                if numeric_node(block, &node) != Some(update.clearance) {
                    edits.push((
                        node.start,
                        node.end,
                        format!("(clearance {})", fmt_num(update.clearance)),
                    ));
                }
            }
            _ => {}
        }
    }
    let existing: BTreeSet<_> = members.iter().map(String::as_str).collect();
    let missing: Vec<_> = requested.difference(&existing).copied().collect();
    if !missing.is_empty() {
        facts.assignment_changed = true;
        facts
            .affected_nets
            .extend(missing.iter().map(|net| (*net).to_owned()));
    }
    let mut insertion = String::new();
    if !has_clearance {
        insertion.push_str(&format!("\n\t\t(clearance {})", fmt_num(update.clearance)));
    }
    if !has_width {
        insertion.push_str(&format!("\n\t\t(trace_width {})", fmt_num(update.width)));
    }
    for net in missing {
        insertion.push_str(&format!("\n\t\t(add_net \"{}\")", escape_sexpr(net)));
    }
    if !insertion.is_empty() {
        edits.push((block.len() - 1, block.len() - 1, insertion));
    }
    Ok((apply_edits(block, edits), facts))
}

fn remove_members(block: &str, requested: &BTreeSet<&str>) -> Result<String, String> {
    let mut edits = Vec::new();
    for node in child_nodes(block, 1, block.len() - 1) {
        if node_head(block, &node) == "add_net"
            && node_quoted_atom(block, &node, 1).is_some_and(|net| requested.contains(net.as_str()))
        {
            let mut start = node.start;
            while start > 0 && matches!(block.as_bytes()[start - 1], b' ' | b'\t') {
                start -= 1;
            }
            if start > 0 && block.as_bytes()[start - 1] == b'\n' {
                start -= 1;
            }
            edits.push((start, node.end, String::new()));
        }
    }
    Ok(apply_edits(block, edits))
}

fn new_board_class(update: &NetClassUpdate, before_root_close: bool) -> String {
    let mut out = format!(
        "{}\t(net_class \"{}\" \"Gordian net width\"\n\t\t(clearance {})\n\t\t(trace_width {})",
        if before_root_close { "" } else { "\n" },
        escape_sexpr(&update.name),
        fmt_num(update.clearance),
        fmt_num(update.width)
    );
    for net in &update.nets {
        out.push_str(&format!("\n\t\t(add_net \"{}\")", escape_sexpr(net)));
    }
    out.push_str(if before_root_close {
        "\n\t)\n"
    } else {
        "\n\t)"
    });
    out
}

fn class_members(block: &str) -> Vec<String> {
    child_nodes(block, 1, block.len() - 1)
        .into_iter()
        .filter(|node| node_head(block, node) == "add_net")
        .filter_map(|node| node_quoted_atom(block, &node, 1))
        .collect()
}

fn numeric_child(block: &str, head: &str) -> Option<f64> {
    let node = child_nodes(block, 1, block.len() - 1)
        .into_iter()
        .find(|node| node_head(block, node) == head)?;
    numeric_node(block, &node)
}

fn numeric_node(block: &str, node: &Node) -> Option<f64> {
    block[node.start + 1..node.end - 1]
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn node_quoted_atom(text: &str, node: &Node, position: usize) -> Option<String> {
    let body = &text[node.start + 1..node.end - 1];
    let mut atoms = Vec::new();
    let mut chars = body.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '"' {
            let mut value = String::new();
            let mut escaped = false;
            for (_, ch) in chars.by_ref() {
                if escaped {
                    value.push(ch);
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    break;
                } else {
                    value.push(ch);
                }
            }
            atoms.push(value);
        } else {
            let mut value = String::from(ch);
            while let Some((_, next)) = chars.peek() {
                if next.is_whitespace() {
                    break;
                }
                value.push(*next);
                chars.next();
            }
            atoms.push(value);
        }
    }
    atoms.get(position).cloned()
}

fn assignment_contains(value: &Value, name: &str) -> bool {
    value
        .as_array()
        .is_some_and(|classes| classes.iter().any(|class| class.as_str() == Some(name)))
}

fn merge_reports(
    board: NetClassUpdateReport,
    project: NetClassUpdateReport,
    update: &NetClassUpdate,
) -> NetClassUpdateReport {
    let nets = board
        .nets
        .into_iter()
        .chain(project.nets)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let changed = board.changed || project.changed;
    NetClassUpdateReport {
        changed,
        board_changed: board.changed,
        project_changed: project.changed,
        nets,
        classes: if changed {
            vec![update.name.clone()]
        } else {
            Vec::new()
        },
    }
}

fn report(
    changed: bool,
    board_changed: bool,
    project_changed: bool,
    update: &NetClassUpdate,
    facts: PatchFacts,
) -> NetClassUpdateReport {
    NetClassUpdateReport {
        changed,
        board_changed: changed && board_changed,
        project_changed: changed && project_changed,
        nets: facts.affected_nets.into_iter().collect(),
        classes: if changed {
            vec![update.name.clone()]
        } else {
            Vec::new()
        },
    }
}

fn json_object_member(
    text: &str,
    object: Range<usize>,
    wanted: &str,
) -> Result<Option<Range<usize>>, String> {
    let bytes = text.as_bytes();
    let start = skip_ws(bytes, object.start);
    if bytes.get(start) != Some(&b'{') {
        return Err("JSON value is not an object".into());
    }
    let close = object_close(text, &(start..object.end))?;
    let mut cursor = start + 1;
    while cursor < close {
        cursor = skip_ws_and_commas(bytes, cursor);
        if cursor >= close {
            break;
        }
        let key_end = json_string_end(bytes, cursor)?;
        let key: String = serde_json::from_str(&text[cursor..key_end])
            .map_err(|err| format!("invalid JSON object key: {err}"))?;
        cursor = skip_ws(bytes, key_end);
        if bytes.get(cursor) != Some(&b':') {
            return Err("invalid JSON object member".into());
        }
        let value_start = skip_ws(bytes, cursor + 1);
        let value_end = json_value_end(bytes, value_start)?;
        if key == wanted {
            return Ok(Some(value_start..value_end));
        }
        cursor = value_end;
    }
    Ok(None)
}

fn object_close(text: &str, object: &Range<usize>) -> Result<usize, String> {
    let bytes = text.as_bytes();
    let start = skip_ws(bytes, object.start);
    let end = json_value_end(bytes, start)?;
    if bytes.get(start) != Some(&b'{') || bytes.get(end - 1) != Some(&b'}') {
        return Err("JSON value is not an object".into());
    }
    Ok(end - 1)
}

fn json_value_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    match bytes.get(start) {
        Some(b'"') => json_string_end(bytes, start),
        Some(b'{') | Some(b'[') => {
            let open = bytes[start];
            let close = if open == b'{' { b'}' } else { b']' };
            let mut depth = 0usize;
            let mut cursor = start;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'"' => cursor = json_string_end(bytes, cursor)?,
                    byte if byte == open => {
                        depth += 1;
                        cursor += 1;
                    }
                    byte if byte == close => {
                        depth -= 1;
                        cursor += 1;
                        if depth == 0 {
                            return Ok(cursor);
                        }
                    }
                    _ => cursor += 1,
                }
            }
            Err("unterminated JSON container".into())
        }
        Some(_) => {
            let mut cursor = start;
            while cursor < bytes.len()
                && !matches!(
                    bytes[cursor],
                    b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t'
                )
            {
                cursor += 1;
            }
            Ok(cursor)
        }
        None => Err("missing JSON value".into()),
    }
}

fn json_string_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    if bytes.get(start) != Some(&b'"') {
        return Err("expected JSON string".into());
    }
    let mut escaped = false;
    for (cursor, byte) in bytes.iter().enumerate().skip(start + 1) {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Ok(cursor + 1);
        }
    }
    Err("unterminated JSON string".into())
}

fn skip_ws(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    cursor
}

fn skip_ws_and_commas(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b',')
    {
        cursor += 1;
    }
    cursor
}

fn indent_json_value(value: &str, spaces: usize) -> String {
    value.replace('\n', &format!("\n{}", " ".repeat(spaces)))
}

fn apply_edits(text: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.0));
    let mut result = text.to_owned();
    for (start, end, replacement) in edits {
        result.replace_range(start..end, &replacement);
    }
    result
}

fn fmt_num(value: f64) -> String {
    format!("{}", if value == 0.0 { 0.0 } else { value })
}

fn escape_sexpr(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: &str = "(kicad_pcb\n\t(version 20240108)\n\t(net 0 \"\")\n\t(net 1 \"GND\")\n\t(net 2 \"SIG\")\n\t(net_class \"Default\" \"default\"\n\t\t(clearance 0.2)\n\t\t(trace_width 0.25)\n\t\t(add_net \"GND\")\n\t\t(add_net \"SIG\")\n\t)\n\t(gr_line (start 0 0) (end 10 0) (layer \"Edge.Cuts\"))\n)\n";
    const PROJECT: &str = "{\n  \"board\": {\"sentinel\": \"unchanged\"},\n  \"net_settings\": {\n    \"classes\": [\n      {\n        \"clearance\": 0.2,\n        \"name\": \"Default\",\n        \"priority\": 2147483647,\n        \"track_width\": 0.25\n      }\n    ],\n    \"meta\": {\"version\": 4},\n    \"netclass_assignments\": null,\n    \"netclass_patterns\": []\n  },\n  \"meta\": {\"filename\": \"board.kicad_pro\", \"version\": 1}\n}\n";

    fn update() -> NetClassUpdate {
        NetClassUpdate {
            name: "Width_0_5".into(),
            width: 0.5,
            clearance: 0.2,
            nets: vec!["SIG".into()],
        }
    }

    #[test]
    fn board_round_trip_changes_only_net_class_bytes() {
        let (patched, report) = patch_board_net_class(BOARD, &update()).unwrap();
        assert!(report.changed);
        assert_eq!(report.nets, vec!["SIG"]);
        assert_eq!(report.classes, vec!["Width_0_5"]);
        assert_eq!(board_net_widths(&patched).unwrap()["SIG"], 0.5);
        assert!(!board_net_widths(&patched).unwrap().contains_key("GND"));
        assert!(patched.contains("\t(gr_line (start 0 0) (end 10 0) (layer \"Edge.Cuts\"))\n"));
        assert_eq!(
            patched.rsplit_once("\t(gr_line").unwrap().1,
            BOARD.rsplit_once("\t(gr_line").unwrap().1
        );

        let (again, second) = patch_board_net_class(&patched, &update()).unwrap();
        assert_eq!(again, patched);
        assert!(!second.changed);
    }

    #[test]
    fn project_round_trip_preserves_unrelated_bytes_and_resolves_width() {
        let (patched, report) = patch_project_net_class(PROJECT, &update()).unwrap();
        assert!(report.changed);
        assert_eq!(project_net_widths(&patched).unwrap()["SIG"], 0.5);
        assert!(patched.contains("  \"board\": {\"sentinel\": \"unchanged\"},\n"));
        assert!(patched.contains("    \"meta\": {\"version\": 4},\n"));
        assert!(
            patched.contains("  \"meta\": {\"filename\": \"board.kicad_pro\", \"version\": 1}\n")
        );

        let (again, second) = patch_project_net_class(&patched, &update()).unwrap();
        assert_eq!(again, patched);
        assert!(!second.changed);
    }

    #[test]
    fn project_class_outranks_legacy_default_patterns() {
        let project = PROJECT
            .replace("\"priority\": 2147483647", "\"priority\": -1")
            .replace(
                "\"netclass_patterns\": []",
                "\"netclass_patterns\": [{\"netclass\": \"Default\", \"pattern\": \"SIG\"}]",
            );

        let (patched, _) = patch_project_net_class(&project, &update()).unwrap();

        let parsed: Value = serde_json::from_str(&patched).unwrap();
        let class = parsed["net_settings"]["classes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|class| class["name"] == "Width_0_5")
            .unwrap();
        assert_eq!(class["priority"], -2);
        assert_eq!(project_net_widths(&patched).unwrap()["SIG"], 0.5);
    }

    #[test]
    fn modern_board_without_legacy_classes_stays_byte_identical() {
        let board = "(kicad_pcb\n\t(net 0 \"\")\n\t(net 1 \"SIG\")\n)\n";

        let (patched, report) = patch_board_net_class(board, &update()).unwrap();

        assert_eq!(patched, board);
        assert!(!report.changed);
    }

    #[test]
    fn file_update_atomically_round_trips_board_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let board_path = dir.path().join("board.kicad_pcb");
        let project_path = dir.path().join("board.kicad_pro");
        std::fs::write(&board_path, BOARD).unwrap();
        std::fs::write(&project_path, PROJECT).unwrap();

        let report = write_net_class_update(&board_path, &project_path, &update()).unwrap();
        assert!(report.board_changed);
        assert!(report.project_changed);
        let board = std::fs::read_to_string(board_path).unwrap();
        let project = std::fs::read_to_string(project_path).unwrap();
        assert_eq!(board_net_widths(&board).unwrap()["SIG"], 0.5);
        assert_eq!(project_net_widths(&project).unwrap()["SIG"], 0.5);
    }
}
