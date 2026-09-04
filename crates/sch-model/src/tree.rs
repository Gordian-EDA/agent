//! The LAYOUT TREE: how a block is arranged, as the model composes it.
//!
//! A block is a nest of rows and columns — CSS flexbox for schematics — whose leaves are
//! parts. The model states the composition (what sits beside what, in which order, how far
//! apart); the typesetter measures the symbols and computes every coordinate. Nobody but
//! the typesetter ever writes a millimetre.
//!
//! ```json
//! {"row": [{"col": [{"part": "R1"}, {"part": "R2"}], "gap": 4},
//!          {"part": "U1"},
//!          {"col": [{"part": "C2"}, {"part": "D1", "rot": 90}]}],
//!  "gap": 10}
//! ```
//!
//! Gaps and every other length in a tree are GRID UNITS ([`UNIT_MM`]) — the spacing a
//! human reads off a schematic, not millimetres.

use std::collections::BTreeMap;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

/// One grid unit in millimetres: the 50-mil step a KiCAD symbol's pins are drawn on, and
/// the unit every length in a tree is quoted in (an 0603 resistor is 6 units long).
pub const UNIT_MM: f64 = 1.27;

/// Default gap between siblings, in grid units.
pub const DEFAULT_GAP: f64 = 8.0;

/// How wide a row may grow (grid units) before it wraps into stacked rows, and how tall a
/// column may grow before it wraps into side-by-side columns. A container longer than the
/// page is not something a reader can follow.
pub const WRAP_WIDTH: f64 = 150.0;
pub const WRAP_HEIGHT: f64 = 90.0;

/// How a container lines its children up on its CROSS axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    /// Alignment lines (pin lines, anchors) on one line — the readable default.
    #[default]
    Center,
    /// Leading edges flush.
    Start,
    /// Trailing edges flush.
    End,
}

/// A part at a leaf of the tree.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Leaf {
    pub part: String,
    /// 1-based unit of a multi-unit symbol; each unit is its own leaf.
    pub unit: Option<u8>,
    /// Explicit rotation in degrees (0/90/180/270). Omitted = the typesetter's
    /// convention for the part's role in its container.
    pub rot: Option<i32>,
    /// Flip left↔right.
    pub mirror: bool,
}

/// Which way a container stacks its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Left → right: one signal path.
    Row,
    /// Top → bottom: what hangs off a node.
    Col,
}

/// A row or column of children.
#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub axis: Axis,
    pub children: Vec<Tree>,
    /// Space between children in grid units; `None` = [`DEFAULT_GAP`].
    pub gap: Option<f64>,
    pub align: Align,
    /// Length (grid units) past which the container wraps into bands; `None` = the
    /// default for its axis ([`WRAP_WIDTH`] / [`WRAP_HEIGHT`]).
    pub wrap: Option<f64>,
}

/// A block's arrangement: a part, or a row/column of arrangements.
#[derive(Debug, Clone, PartialEq)]
pub enum Tree {
    Leaf(Leaf),
    Container(Container),
}

impl Tree {
    /// One part's leaf, at the unit it draws.
    pub fn leaf(part: impl Into<String>, unit: u8) -> Tree {
        Tree::Leaf(Leaf {
            part: part.into(),
            unit: (unit != 1).then_some(unit),
            ..Leaf::default()
        })
    }

    /// A row of `(part, unit)` with default spacing — what a block without an authored
    /// tree gets.
    pub fn row_of(parts: impl IntoIterator<Item = (String, u8)>) -> Tree {
        Tree::Container(Container {
            axis: Axis::Row,
            children: parts
                .into_iter()
                .map(|(part, unit)| Tree::leaf(part, unit))
                .collect(),
            gap: None,
            align: Align::Center,
            wrap: None,
        })
    }

    /// Every leaf in composition order.
    pub fn leaves(&self) -> Vec<&Leaf> {
        let mut out = Vec::new();
        self.walk(&mut |leaf| out.push(leaf));
        out
    }

    fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Leaf)) {
        match self {
            Tree::Leaf(leaf) => f(leaf),
            Tree::Container(c) => c.children.iter().for_each(|child| child.walk(f)),
        }
    }

    /// The leaves' `(part, unit)` keys, for membership checks against a block.
    pub fn keys(&self) -> Vec<(String, u8)> {
        self.leaves()
            .into_iter()
            .map(|leaf| (leaf.part.clone(), leaf.unit.unwrap_or(1)))
            .collect()
    }
}

/// Block name → its arrangement.
pub type Trees = BTreeMap<String, Tree>;

/// The wire form: one node with every key optional, so a malformed node is reported by
/// what it actually said rather than as "matched no variant" (what an untagged enum
/// gives).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    part: Option<String>,
    unit: Option<u8>,
    rot: Option<i32>,
    #[serde(default)]
    mirror: Mirror,
    row: Option<Vec<Tree>>,
    col: Option<Vec<Tree>>,
    gap: Option<f64>,
    align: Option<Align>,
    wrap: Option<f64>,
}

impl Wire {
    /// What this node actually said, so a refusal is a correction rather than a
    /// restatement of the rule. A node carrying only spacing is the common near
    /// miss: a gap belongs on the container, not beside its children.
    fn said(&self) -> String {
        let mut keys = Vec::new();
        for (name, present) in [
            ("part", self.part.is_some()),
            ("unit", self.unit.is_some()),
            ("rot", self.rot.is_some()),
            ("row", self.row.is_some()),
            ("col", self.col.is_some()),
            ("gap", self.gap.is_some()),
            ("align", self.align.is_some()),
            ("wrap", self.wrap.is_some()),
        ] {
            if present {
                keys.push(name);
            }
        }
        if keys.is_empty() {
            return "; this node said nothing".into();
        }
        format!("; this one said {}", keys.join(", "))
    }
}

/// `mirror` written either as a flag or, as the reference sheets write it, the axis
/// string `"y"`.
#[derive(Default, Deserialize)]
#[serde(untagged)]
enum Mirror {
    #[default]
    Absent,
    Flag(bool),
    Axis(String),
}

impl Mirror {
    fn flag(&self) -> bool {
        match self {
            Mirror::Absent => false,
            Mirror::Flag(on) => *on,
            Mirror::Axis(axis) => !axis.is_empty(),
        }
    }
}

impl<'de> Deserialize<'de> for Tree {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Tree, D::Error> {
        let wire = Wire::deserialize(d)?;
        let named = [
            wire.part.is_some(),
            wire.row.is_some(),
            wire.col.is_some(),
        ]
        .iter()
        .filter(|set| **set)
        .count();
        if named != 1 {
            return Err(de::Error::custom(format!(
                "a layout node is exactly one of `part`, `row` or `col`{}",
                wire.said()
            )));
        }
        if let Some(part) = wire.part {
            if !matches!(wire.rot, None | Some(0) | Some(90) | Some(180) | Some(270)) {
                return Err(de::Error::custom(format!(
                    "{part}: rot must be 0, 90, 180 or 270"
                )));
            }
            return Ok(Tree::Leaf(Leaf {
                part,
                unit: wire.unit,
                rot: wire.rot,
                mirror: wire.mirror.flag(),
            }));
        }
        let (axis, children) = match (wire.row, wire.col) {
            (Some(children), _) => (Axis::Row, children),
            (_, Some(children)) => (Axis::Col, children),
            _ => unreachable!("exactly one of part/row/col is set"),
        };
        if children.is_empty() {
            return Err(de::Error::custom("a `row`/`col` needs at least one child"));
        }
        Ok(Tree::Container(Container {
            axis,
            children,
            gap: wire.gap,
            align: wire.align.unwrap_or_default(),
            wrap: wire.wrap,
        }))
    }
}

impl Serialize for Tree {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Tree::Leaf(leaf) => {
                let mut map = s.serialize_map(None)?;
                map.serialize_entry("part", &leaf.part)?;
                if let Some(unit) = leaf.unit {
                    map.serialize_entry("unit", &unit)?;
                }
                if let Some(rot) = leaf.rot {
                    map.serialize_entry("rot", &rot)?;
                }
                if leaf.mirror {
                    map.serialize_entry("mirror", &true)?;
                }
                map.end()
            }
            Tree::Container(c) => {
                let mut map = s.serialize_map(None)?;
                let key = match c.axis {
                    Axis::Row => "row",
                    Axis::Col => "col",
                };
                map.serialize_entry(key, &c.children)?;
                if let Some(gap) = c.gap {
                    map.serialize_entry("gap", &gap)?;
                }
                if c.align != Align::Center {
                    map.serialize_entry("align", &c.align)?;
                }
                if let Some(wrap) = c.wrap {
                    map.serialize_entry("wrap", &wrap)?;
                }
                map.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nested_tree_round_trips() {
        let src = r#"{"row":[{"col":[{"part":"R1"},{"part":"C1"}],"gap":4.0},
                             {"part":"U1"},
                             {"part":"D1","rot":90,"mirror":"y"}],"gap":10.0}"#;
        let tree: Tree = serde_json::from_str(src).unwrap();
        assert_eq!(
            tree.keys(),
            vec![
                ("R1".into(), 1),
                ("C1".into(), 1),
                ("U1".into(), 1),
                ("D1".into(), 1)
            ]
        );
        let back: Tree = serde_json::from_str(&serde_json::to_string(&tree).unwrap()).unwrap();
        assert_eq!(back, tree);
    }

    #[test]
    fn a_node_naming_no_arrangement_is_told_what_it_said() {
        let err = serde_json::from_str::<Tree>(r#"{"gap":4}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("this one said gap"), "{err}");
    }

    #[test]
    fn an_ambiguous_node_is_refused_by_what_it_said() {
        let err = serde_json::from_str::<Tree>(r#"{"part":"R1","row":[{"part":"R2"}]}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exactly one of"), "{err}");
        let err = serde_json::from_str::<Tree>(r#"{"part":"R1","rot":45}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("R1: rot must be"), "{err}");
    }
}
