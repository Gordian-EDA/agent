//! Board intent: what the model says about a layout, lowered into constraints.
//!
//! The model does not author board coordinates. `move_parts{to}` is the one
//! place a literal position belongs, and only because a user asked for that
//! part to sit there. Everything else is stated the way it is on the schematic
//! side — as intent:
//!
//! ```text
//! intent: {
//!   edge:      { "J1": "left", "J2": "right" },   // which board edge a part faces
//!   keep_near: [["C3", "U1"], …],                 // pairs that must stay close
//!   group:     [["U1", "C3", "C4"], …],           // parts that belong together
//!   zones:     ["GND"]                            // nets that get a copper pour
//! }
//! ```
//!
//! This module is the single lowering from that vocabulary. The placement half
//! becomes [`PlacementHints`], which `pcb-place` honours as cost terms, so the
//! annealer cannot quietly reverse what the seed arranged. `zones` are board rules
//! rather than placement, so they belong to `sync_board`; each tool applies its
//! own half and names the other rather than dropping it silently.

use std::collections::{BTreeMap, BTreeSet};

use pcb_place::{Edge, GroupHint, PlacementHints};
use serde_json::Value;

/// One parsed `intent` object, split into the half each tool can act on.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct BoardIntent {
    /// The placement half: edges, proximities, groups.
    pub(crate) hints: PlacementHints,
    /// Nets that should be poured as a copper zone.
    pub(crate) zones: Vec<String>,
    /// Reference → canonical `left|right|top|bottom` for reported edge input.
    pub(crate) normalized_edges: BTreeMap<String, String>,
}

impl BoardIntent {
    /// Every reference the intent names, so a caller can check them against the
    /// board before the placer silently ignores a typo.
    pub(crate) fn references(&self) -> BTreeSet<&str> {
        self.hints
            .groups
            .iter()
            .flat_map(|group| group.members.iter())
            .chain(self.hints.keep_near.iter().flatten())
            .map(String::as_str)
            .collect()
    }

    /// The references the caller pinned to a board edge. A connector or
    /// mounting hole there is a physical fixing, so placing it earns a
    /// `mechanical` lock.
    pub(crate) fn edge_references(&self) -> BTreeSet<&str> {
        self.hints
            .groups
            .iter()
            .filter(|group| group.edge.is_some())
            .flat_map(|group| group.members.iter())
            .map(String::as_str)
            .collect()
    }

    /// Fold this intent's placement half into hints the caller already has.
    pub(crate) fn merge_into(self, hints: &mut PlacementHints) {
        hints.groups.extend(self.hints.groups);
        hints.keep_near.extend(self.hints.keep_near);
    }

    /// Drop references that are not on the current board while preserving the
    /// usable portion of a model-authored intent.
    pub(crate) fn retain_references(&mut self, known: &BTreeSet<String>) {
        self.hints
            .keep_near
            .retain(|pair| pair.iter().all(|reference| known.contains(reference)));
        for group in &mut self.hints.groups {
            group.members.retain(|reference| known.contains(reference));
        }
        self.hints.groups.retain(|group| !group.members.is_empty());
        self.normalized_edges
            .retain(|reference, _| known.contains(reference));
    }
}

/// Read the `intent` field of a tool input. Absent intent is empty intent.
pub(crate) fn parse(input: &Value) -> std::result::Result<BoardIntent, String> {
    let Some(intent) = input.get("intent") else {
        return Ok(BoardIntent::default());
    };
    let object = intent
        .as_object()
        .ok_or_else(|| "intent must be an object".to_owned())?;
    if let Some(unknown) = object
        .keys()
        .find(|key| !["edge", "keep_near", "group", "zones"].contains(&key.as_str()))
    {
        return Err(format!(
            "intent has no `{unknown}` field; it takes edge, keep_near, group and zones"
        ));
    }

    let mut hints = PlacementHints::default();
    let mut normalized_edges = BTreeMap::new();
    for (reference, side, normalized) in edge_entries(object.get("edge"))? {
        normalized_edges.insert(reference.clone(), normalized.to_owned());
        hints.groups.push(GroupHint {
            name: format!("edge:{reference}"),
            members: vec![reference],
            region: None,
            edge: Some(side),
            grid: false,
            rotation: None,
            surround: None,
        });
    }
    hints.keep_near = pairs(object.get("keep_near"))?;
    for (index, members) in groups(object.get("group"))?.into_iter().enumerate() {
        hints.groups.push(GroupHint {
            name: format!("group:{index}"),
            members,
            region: None,
            edge: None,
            grid: false,
            rotation: None,
            surround: None,
        });
    }
    Ok(BoardIntent {
        hints,
        zones: strings(object.get("zones"), "zones")?,
        normalized_edges,
    })
}

fn edge_entries(
    value: Option<&Value>,
) -> std::result::Result<Vec<(String, Edge, &'static str)>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let object = value
        .as_object()
        .ok_or_else(|| "intent.edge maps a reference to a board side".to_owned())?;
    object
        .iter()
        .map(|(reference, side)| {
            let side = side
                .as_str()
                .ok_or_else(|| format!("intent.edge.{reference} must be a board side"))?;
            let (edge, normalized) = match side {
                "left" | "west" | "W" | "w" => (Edge::W, "left"),
                "right" | "east" | "E" | "e" => (Edge::E, "right"),
                "top" | "north" | "N" | "n" => (Edge::N, "top"),
                "bottom" | "south" | "S" | "s" => (Edge::S, "bottom"),
                other => {
                    return Err(format!(
                        "intent.edge.{reference} is `{other}`; a board side is left/west, \
                         right/east, top/north or bottom/south"
                    ));
                }
            };
            Ok((reference.clone(), edge, normalized))
        })
        .collect()
}

fn pairs(value: Option<&Value>) -> std::result::Result<Vec<[String; 2]>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| "intent.keep_near is an array of [reference, reference] pairs".to_owned())?;
    items
        .iter()
        .map(|item| {
            let rule = "each intent.keep_near entry is exactly two references";
            match strings(Some(item), rule)?.as_slice() {
                [a, b] => Ok([a.clone(), b.clone()]),
                _ => Err(rule.to_owned()),
            }
        })
        .collect()
}

fn groups(value: Option<&Value>) -> std::result::Result<Vec<Vec<String>>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| "intent.group is an array of reference lists".to_owned())?;
    items
        .iter()
        .map(|item| {
            let members = strings(Some(item), "intent.group entry")?;
            if members.len() < 2 {
                return Err("each intent.group names at least two references".to_owned());
            }
            Ok(members)
        })
        .collect()
}

fn strings(value: Option<&Value>, what: &str) -> std::result::Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| format!("{what} must be an array of strings"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{what} must be an array of strings"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_edge_intent_becomes_a_one_part_edge_group() {
        let intent = parse(&json!({ "intent": { "edge": { "J1": "left" } } })).unwrap();
        assert_eq!(intent.hints.groups.len(), 1);
        assert_eq!(intent.hints.groups[0].members, ["J1"]);
        assert_eq!(intent.hints.groups[0].edge, Some(Edge::W));
    }

    #[test]
    fn keep_near_and_group_carry_their_references() {
        let intent = parse(&json!({
            "intent": {
                "keep_near": [["C3", "U1"]],
                "group": [["U1", "C3", "C4"]],
                "zones": ["GND"],
            }
        }))
        .unwrap();
        assert_eq!(intent.hints.keep_near, [["C3".to_owned(), "U1".to_owned()]]);
        assert_eq!(intent.hints.groups[0].members, ["U1", "C3", "C4"]);
        assert_eq!(intent.zones, ["GND"]);
        assert_eq!(
            intent.references(),
            ["C3", "C4", "U1"].into_iter().collect()
        );
    }

    #[test]
    fn a_misspelled_side_is_refused_by_name() {
        let error = parse(&json!({ "intent": { "edge": { "J1": "diagonal" } } })).unwrap_err();
        assert!(error.contains("left/west"), "{error}");
    }

    #[test]
    fn compass_edge_aliases_are_accepted_and_reported_canonically() {
        let intent = parse(&json!({
            "intent": {
                "edge": {
                    "J1": "south",
                    "J2": "east",
                    "J3": "N",
                    "J4": "W"
                }
            }
        }))
        .unwrap();

        assert_eq!(intent.hints.groups[0].edge, Some(Edge::S));
        assert_eq!(intent.hints.groups[1].edge, Some(Edge::E));
        assert_eq!(intent.hints.groups[2].edge, Some(Edge::N));
        assert_eq!(intent.hints.groups[3].edge, Some(Edge::W));
        assert_eq!(
            intent.normalized_edges,
            BTreeMap::from([
                ("J1".to_owned(), "bottom".to_owned()),
                ("J2".to_owned(), "right".to_owned()),
                ("J3".to_owned(), "top".to_owned()),
                ("J4".to_owned(), "left".to_owned()),
            ])
        );
    }

    #[test]
    fn an_unknown_intent_field_is_refused() {
        let error = parse(&json!({ "intent": { "rotate": [] } })).unwrap_err();
        assert!(error.contains("`rotate`"), "{error}");
    }
}
