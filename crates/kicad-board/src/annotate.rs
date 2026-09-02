//! Footprint annotations the workflow owns: the native `locked` flag and the
//! `gordian:` property namespace.
//!
//! Both live in the board file, so the whole partial state — what is staged and
//! why, what is locked and by whom — survives a process restart and is readable
//! by KiCad itself.

use std::collections::BTreeMap;

use crate::patch::{Node, apply_edits, child_nodes, node_head, root_body};

/// The property namespace every workflow annotation lives in.
pub const GORDIAN_PREFIX: &str = "gordian:";

/// Why a part is in the staging row.
pub const STAGED_REASON: &str = "gordian:staged_reason";

/// What the staging reason needs spelled out — a pin/pad mismatch, say.
pub const STAGED_DETAIL: &str = "gordian:staged_detail";

/// Who locked a part: `mechanical`, `agent`, or `user`.
pub const LOCKED_REASON: &str = "gordian:locked_reason";

/// One footprint annotation edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub reference: String,
    /// `Some(true)`/`Some(false)` set or clear KiCad's `(locked …)`; `None`
    /// leaves it alone.
    pub locked: Option<bool>,
    /// Property name → value, or `None` to remove the property.
    pub properties: BTreeMap<String, Option<String>>,
}

impl Annotation {
    /// An annotation that changes nothing about `reference` yet.
    pub fn new(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            locked: None,
            properties: BTreeMap::new(),
        }
    }

    pub fn locked(mut self, locked: bool) -> Self {
        self.locked = Some(locked);
        self
    }

    /// Set one `gordian:` property. Naming anything else panics: the write
    /// replaces the whole property node, so pointing it at `Reference` or
    /// `Value` would destroy a field KiCad owns.
    pub fn set(mut self, name: &str, value: impl Into<String>) -> Self {
        assert!(
            name.starts_with(GORDIAN_PREFIX),
            "annotations own the `{GORDIAN_PREFIX}` namespace only, not `{name}`"
        );
        self.properties.insert(name.to_owned(), Some(value.into()));
        self
    }

    /// Remove one `gordian:` property.
    pub fn clear(mut self, name: &str) -> Self {
        assert!(
            name.starts_with(GORDIAN_PREFIX),
            "annotations own the `{GORDIAN_PREFIX}` namespace only, not `{name}`"
        );
        self.properties.insert(name.to_owned(), None);
        self
    }
}

/// Escape a property value for the s-expression string it is written into.
fn quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn footprint_reference(text: &str, footprint: &Node) -> Option<String> {
    child_nodes(text, footprint.start + 1, footprint.end - 1)
        .iter()
        .filter(|node| node_head(text, node) == "property")
        .filter_map(|node| property_pair(text, node))
        .find(|(name, _)| name == "Reference")
        .map(|(_, value)| value)
}

/// `(property "NAME" "VALUE" …)` → `(NAME, VALUE)`, unescaped.
pub(crate) fn property_pair(text: &str, node: &Node) -> Option<(String, String)> {
    let body = &text[node.start..node.end];
    let rest = body.strip_prefix("(property")?.trim_start();
    let (name, rest) = read_string(rest)?;
    let (value, _) = read_string(rest.trim_start())?;
    Some((name, value))
}

fn read_string(text: &str) -> Option<(String, &str)> {
    let bytes = text.as_bytes();
    if bytes.first()? != &b'"' {
        return None;
    }
    let mut out = String::new();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            // KiCad escapes exactly two characters. Anything else after a
            // backslash is literal text, and copying it as a byte would split a
            // multi-byte character and de-sync the rest of the walk.
            b'\\' if matches!(bytes.get(index + 1), Some(b'"' | b'\\')) => {
                out.push(bytes[index + 1] as char);
                index += 2;
            }
            b'"' => return Some((out, &text[index + 1..])),
            _ => {
                let ch = text[index..].chars().next()?;
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    None
}

/// The `gordian:` properties one footprint carries.
pub(crate) fn gordian_properties(text: &str, footprint: &Node) -> BTreeMap<String, String> {
    child_nodes(text, footprint.start + 1, footprint.end - 1)
        .into_iter()
        .filter(|node| node_head(text, node) == "property")
        .filter_map(|node| property_pair(text, &node))
        .filter(|(name, _)| name.starts_with(GORDIAN_PREFIX))
        .collect()
}

/// Apply every annotation to a `.kicad_pcb` document.
///
/// Every named reference must exist: annotating a part the board does not carry
/// is a caller error, not something to silently drop.
pub fn patch_annotations(text: &str, annotations: &[Annotation]) -> Result<String, String> {
    let by_reference: BTreeMap<&str, &Annotation> = annotations
        .iter()
        .map(|annotation| (annotation.reference.as_str(), annotation))
        .collect();
    let (body_start, body_end) = root_body(text)?;
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut matched: BTreeMap<String, ()> = BTreeMap::new();
    let mut seen = 0usize;
    for footprint in child_nodes(text, body_start, body_end) {
        if node_head(text, &footprint) != "footprint" {
            continue;
        }
        let Some(reference) = footprint_reference(text, &footprint) else {
            continue;
        };
        let Some(annotation) = by_reference.get(reference.as_str()) else {
            continue;
        };
        seen += 1;
        matched.insert(reference.clone(), ());
        let children = child_nodes(text, footprint.start + 1, footprint.end - 1);
        let indent = block_indent(text, &footprint);
        if let Some(locked) = annotation.locked {
            edits.extend(locked_edit(text, &footprint, &children, locked, &indent));
        }
        for (name, value) in &annotation.properties {
            edits.extend(property_edit(
                text,
                &footprint,
                &children,
                name,
                value.as_deref(),
                &indent,
            ));
        }
    }
    report_matches(seen, &matched, by_reference.keys().copied(), "annotate")?;
    Ok(apply_edits(text, edits))
}

/// Say exactly which references a walk could not find, and call a board that
/// carries the same refdes twice what it is.
fn report_matches<'a>(
    seen: usize,
    matched: &BTreeMap<String, ()>,
    requested: impl Iterator<Item = &'a str>,
    verb: &str,
) -> Result<(), String> {
    let missing: Vec<&str> = requested
        .filter(|reference| !matched.contains_key(*reference))
        .collect();
    if !missing.is_empty() {
        return Err(format!("no footprint to {verb} for {}", missing.join(", ")));
    }
    if seen != matched.len() {
        return Err(format!(
            "the board carries {} footprints for {} distinct reference(s); fix the duplicate \
             refdes before editing",
            seen,
            matched.len()
        ));
    }
    Ok(())
}

/// Drop whole footprints from a document.
///
/// Fabrication output must not carry parts that are not on the board yet, so
/// the export copy is the board minus its staging row.
pub fn remove_footprints(text: &str, references: &[String]) -> Result<String, String> {
    let wanted: BTreeMap<&str, ()> = references
        .iter()
        .map(|reference| (reference.as_str(), ()))
        .collect();
    let (body_start, body_end) = root_body(text)?;
    let mut edits = Vec::new();
    let mut matched: BTreeMap<String, ()> = BTreeMap::new();
    let mut seen = 0usize;
    for footprint in child_nodes(text, body_start, body_end) {
        if node_head(text, &footprint) != "footprint" {
            continue;
        }
        let Some(reference) = footprint_reference(text, &footprint) else {
            continue;
        };
        if !wanted.contains_key(reference.as_str()) {
            continue;
        }
        seen += 1;
        matched.insert(reference.clone(), ());
        let mut start = footprint.start;
        while start > body_start && matches!(text.as_bytes()[start - 1], b' ' | b'\t' | b'\n') {
            start -= 1;
        }
        edits.push((start, footprint.end, String::new()));
    }
    report_matches(seen, &matched, wanted.keys().copied(), "remove")?;
    Ok(apply_edits(text, edits))
}

/// The whitespace a footprint's children are indented by, so an inserted node
/// reads like the rest of the file.
fn block_indent(text: &str, footprint: &Node) -> String {
    let body = &text[footprint.start..footprint.end];
    body.find('\n')
        .map(|at| {
            body[at + 1..]
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect()
        })
        .unwrap_or_else(|| "\t\t".to_owned())
}

/// Where a new child node goes: after the footprint's existing properties, so
/// `Reference` stays the first one KiCad and this module both look for.
fn insertion_point(text: &str, footprint: &Node, children: &[Node]) -> usize {
    children
        .iter()
        .rfind(|node| node_head(text, node) == "property")
        .map(|last| last.end)
        .or_else(|| children.first().map(|first| first.start))
        .unwrap_or(footprint.end - 1)
}

fn locked_edit(
    text: &str,
    footprint: &Node,
    children: &[Node],
    locked: bool,
    indent: &str,
) -> Vec<(usize, usize, String)> {
    let existing = children
        .iter()
        .find(|node| node_head(text, node) == "locked");
    match (existing, locked) {
        (Some(node), true) => vec![(node.start, node.end, "(locked yes)".to_owned())],
        (Some(node), false) => {
            // Remove the node and the whitespace that introduced it.
            let mut start = node.start;
            while start > footprint.start
                && matches!(text.as_bytes()[start - 1], b' ' | b'\t' | b'\n')
            {
                start -= 1;
            }
            vec![(start, node.end, String::new())]
        }
        (None, true) => {
            let at = children
                .first()
                .map_or(footprint.end - 1, |first| first.start);
            vec![(at, at, format!("(locked yes)\n{indent}"))]
        }
        (None, false) => Vec::new(),
    }
}

fn property_edit(
    text: &str,
    footprint: &Node,
    children: &[Node],
    name: &str,
    value: Option<&str>,
    indent: &str,
) -> Vec<(usize, usize, String)> {
    let existing = children.iter().find(|node| {
        node_head(text, node) == "property"
            && property_pair(text, node).is_some_and(|(existing, _)| existing == name)
    });
    match (existing, value) {
        (Some(node), Some(value)) => vec![(
            node.start,
            node.end,
            format!(
                "(property \"{}\" \"{}\"\n{indent}\t(at 0 0 0)\n{indent}\t(unlocked yes)\n{indent}\t(layer \"F.Fab\")\n{indent}\t(hide yes)\n{indent})",
                quote(name),
                quote(value)
            ),
        )],
        (Some(node), None) => {
            let mut start = node.start;
            while start > footprint.start
                && matches!(text.as_bytes()[start - 1], b' ' | b'\t' | b'\n')
            {
                start -= 1;
            }
            vec![(start, node.end, String::new())]
        }
        (None, Some(value)) => {
            let at = insertion_point(text, footprint, children);
            vec![(
                at,
                at,
                format!(
                    "\n{indent}(property \"{}\" \"{}\"\n{indent}\t(at 0 0 0)\n{indent}\t(unlocked yes)\n{indent}\t(layer \"F.Fab\")\n{indent}\t(hide yes)\n{indent})",
                    quote(name),
                    quote(value)
                ),
            )]
        }
        (None, None) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: &str = "(kicad_pcb\n\t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(2 \"B.Cu\" signal)\n\t\t(44 \"Edge.Cuts\" user)\n\t)\n\t(gr_rect\n\t\t(start 0 0)\n\t\t(end 20 20)\n\t\t(layer \"Edge.Cuts\")\n\t)\n\t(footprint \"L:R\"\n\t\t(layer \"F.Cu\")\n\t\t(uuid \"u\")\n\t\t(at 1 2)\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 0 0 0)\n\t\t)\n\t)\n)";

    fn snapshot_of(text: &str) -> crate::BoardSnapshot {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.kicad_pcb");
        std::fs::write(&path, text).unwrap();
        crate::read_snapshot(&path).unwrap()
    }

    #[test]
    fn locking_and_unlocking_round_trips_through_the_file() {
        let locked = patch_annotations(BOARD, &[Annotation::new("R1").locked(true)]).unwrap();
        assert!(locked.contains("(locked yes)"));
        assert!(snapshot_of(&locked).imported.parts[0].locked);

        let unlocked = patch_annotations(&locked, &[Annotation::new("R1").locked(false)]).unwrap();
        assert!(!unlocked.contains("(locked yes)"));
        assert!(!snapshot_of(&unlocked).imported.parts[0].locked);
    }

    #[test]
    fn a_gordian_property_is_written_read_and_removed() {
        let with = patch_annotations(
            BOARD,
            &[Annotation::new("R1")
                .set(LOCKED_REASON, "mechanical")
                .set(STAGED_DETAIL, "pin \"3\" has no pad")],
        )
        .unwrap();
        let part = &snapshot_of(&with).imported.parts[0];
        assert_eq!(
            part.properties.get(LOCKED_REASON).map(String::as_str),
            Some("mechanical")
        );
        assert_eq!(
            part.properties.get(STAGED_DETAIL).map(String::as_str),
            Some("pin \"3\" has no pad"),
            "an escaped quote survives the round trip"
        );

        let without =
            patch_annotations(&with, &[Annotation::new("R1").clear(LOCKED_REASON)]).unwrap();
        let part = &snapshot_of(&without).imported.parts[0];
        assert!(!part.properties.contains_key(LOCKED_REASON));
        assert!(part.properties.contains_key(STAGED_DETAIL));
        assert_eq!(part.reference, "R1", "the Reference property is untouched");
    }

    #[test]
    fn removing_a_footprint_takes_the_whole_block() {
        let stripped = remove_footprints(BOARD, &["R1".to_owned()]).unwrap();
        assert!(!stripped.contains("footprint"), "{stripped}");
        assert!(snapshot_of(&stripped).imported.parts.is_empty());
        assert!(remove_footprints(BOARD, &["R9".to_owned()]).is_err());
    }

    #[test]
    fn annotating_a_reference_the_board_lacks_is_an_error() {
        let error = patch_annotations(BOARD, &[Annotation::new("R9").locked(true)]).unwrap_err();
        assert!(error.contains("R9"), "{error}");
    }
}
