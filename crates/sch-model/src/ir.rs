//! The Layout IR — the geometry-free "frame" the LLM emits: the global flow,
//! which nets are rails (and their band), where the anchors (ICs) sit, which nets
//! exit as ports. The engine (`sch-floorplan`) turns this into exact millimetre
//! placement; the LLM never sees a coordinate. This module is just the data
//! vocabulary; the inference that *produces* an IR lives in the engine.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::result::IdiomReport;

/// Global signal-flow direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Flow {
    /// Left → right (signals flow horizontally). The common case.
    #[default]
    Lr,
    /// Top → bottom.
    Tb,
}

/// Which horizontal band a rail net occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Band {
    Top,
    Bottom,
}

/// Which sheet edge a port net exits toward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// Orientation of a 2-pin part, stated as the direction its pins run — from its
/// first connected net (pin 1) toward its second (pin 2). The engine works out
/// the exact rotation from the symbol's own pin geometry, so the LLM never
/// reasons about a symbol's native axis or KiCAD angles; it just says which way
/// the part points. `down` (pin 1 on top, e.g. a divider leg from VCC down to
/// GND) is the common default. ICs/connectors ignore this (they stay at 0°; use
/// `mirror` to flip them left-to-right).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Orient {
    /// Pin 1 at the bottom, pin 2 at the top.
    Up,
    /// Pin 1 at the top, pin 2 at the bottom (the usual passive orientation).
    #[default]
    Down,
    /// Pin 1 on the right, pin 2 on the left.
    Left,
    /// Pin 1 on the left, pin 2 on the right (a series element along the flow).
    Right,
}

/// A coarse, unitless placement cell + orientation. The engine maps the
/// (col,row) grid to mm — each column sized to its widest part, each row to its
/// tallest — and places the symbol at the cell centre. `col` grows right, `row`
/// grows down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub col: i32,
    pub row: i32,
    #[serde(default)]
    pub orient: Orient,
}


/// The axis a set of parts is aligned ALONG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// Members sit on one horizontal line — a shared row (equal `y`).
    Horizontal,
    /// Members sit on one vertical line — a shared column (equal `x`).
    Vertical,
}

/// One piece of RELATIONAL layout intent the LLM authors: a statement about parts
/// relative to each other, never a coordinate.
///
/// Orthogonal to the other [`LayoutIr`] keys by design:
/// - [`LayoutIr::grid`] is the DENSE form (a full authored 2D arrangement, already
///   enforced by `grid_order_viol`); relations are the SPARSE pairwise form an LLM can
///   state about two parts without laying out the whole sheet. They share the same
///   comparison convention (part origins, `x` grows right, `y` grows down).
/// - [`LayoutIr::zone`] is an ABSOLUTE coarse bias (a fraction of the board bbox);
///   relations say nothing about where on the sheet the parts land.
/// - [`LayoutIr::frozen`] pins recognized idiom clusters at engine-chosen poses;
///   [`Relation::Group`] asks the engine to FIND a cohesive arrangement.
///
/// A refdes naming a multi-unit part refers to the centroid of its units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Relation {
    /// `a` sits strictly left of `b`.
    LeftOf { a: String, b: String },
    /// `a` sits strictly right of `b`.
    RightOf { a: String, b: String },
    /// `a` sits strictly above `b` (smaller `y`).
    Above { a: String, b: String },
    /// `a` sits strictly below `b`.
    Below { a: String, b: String },
    /// `members` are placed as one cohesive cluster — packed together with nothing
    /// foreign between them — optionally on `side` of the `anchor` refdes.
    Group {
        name: String,
        members: Vec<String>,
        #[serde(default)]
        side: Option<GroupSide>,
    },
    /// `members` share one row (`Horizontal`) or column (`Vertical`).
    Align { members: Vec<String>, axis: Axis },
}

/// Where a [`Relation::Group`] sits, optionally relative to an anchor part.
///
/// Written either positionally (`["left", "U1"]`), by name
/// (`{"side": "left", "anchor": "U1"}`), or as a bare edge (`"left"`) when the
/// group has no anchor to hang off. All three round-trip to the positional form
/// when an anchor is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GroupSide {
    Anchored(Side, String),
    Named { side: Side, anchor: String },
    Edge(Side),
}

impl GroupSide {
    /// The edge, and the anchor refdes when one was named.
    pub fn parts(&self) -> (Side, Option<&str>) {
        match self {
            GroupSide::Anchored(side, anchor) => (*side, Some(anchor.as_str())),
            GroupSide::Named { side, anchor } => (*side, Some(anchor.as_str())),
            GroupSide::Edge(side) => (*side, None),
        }
    }
}

impl Relation {
    /// Every refdes this relation constrains.
    pub fn refdes(&self) -> Vec<&str> {
        match self {
            Relation::LeftOf { a, b }
            | Relation::RightOf { a, b }
            | Relation::Above { a, b }
            | Relation::Below { a, b } => vec![a.as_str(), b.as_str()],
            Relation::Group {
                members, side, ..
            } => members
                .iter()
                .map(String::as_str)
                .chain(side.iter().filter_map(|side| side.parts().1))
                .collect(),
            Relation::Align { members, .. } => members.iter().map(String::as_str).collect(),
        }
    }
}

/// The geometry-free floorplan. Four keys; everything else is inferred from
/// connectivity by the compiler's fixed rule set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutIr {
    #[serde(default)]
    pub flow: Flow,
    /// Net → band. Nets drawn as spanning rails.
    #[serde(default)]
    pub rails: BTreeMap<String, Band>,
    /// Refdes → coarse cell. Usually only ICs; any refdes may be pinned.
    #[serde(default)]
    pub place: BTreeMap<String, Cell>,
    /// Net → edge side. Nets that exit as labelled ports.
    #[serde(default)]
    pub ports: BTreeMap<String, Side>,
    /// Anchors (ICs) to flip left-to-right, so the pins facing their neighbours
    /// point the right way (e.g. a level translator's B-side toward a connector).
    #[serde(default)]
    pub mirror: BTreeSet<String>,
    /// Refdes → authored grid bounding box `[col_min, row_min, col_max, row_max]`
    /// in composed grid-ordinal coords (from the per-block `layout:`). The search
    /// holds gridded parts in this RELATIVE order — left/right by column, top/bottom
    /// by row — so the author's arrangement is "relatively rigid"; a part spanning a
    /// column range floats within it. Empty on the sidecar/baseline paths (no
    /// authored grid ⇒ no ordering constraint, so tuned references are unaffected).
    #[serde(default)]
    pub grid: BTreeMap<String, [i32; 4]>,
    /// Idioms the engine recognized from connectivity and co-placed as cohesive
    /// clusters (crystal+load-caps, decoupling bank, op-amp feedback). Surfaced to
    /// the agent via `EmitOutput.detected_idioms`. `#[serde(default)]` so existing
    /// sidecar `layout.json` files (which never carry it) still deserialize.
    #[serde(default)]
    pub idioms: Vec<IdiomReport>,
    /// Refdes the placement search must NOT move — an idiom cluster's members,
    /// pinned so their recognized arrangement ships intact.
    #[serde(default)]
    pub frozen: BTreeSet<String>,
    /// Power nets the author wants drawn as DISTRIBUTED LOCAL grounds/supplies — one
    /// power symbol per pin (the professional "drop a GND triangle at each pin" style)
    /// — instead of one sheet-spanning rail. The signal is the author declaring ≥2
    /// power symbols for the net (`GND1`, `GND2`, …); a board with one keeps the rail.
    /// Tames the long-rail sprawl of a dense MCU. `#[serde(default)]` so sidecars (one
    /// symbol per rail) deserialize empty and the tuned references stay rails.
    #[serde(default)]
    pub rail_locals: BTreeSet<String>,
    /// The inverse of [`Self::rail_locals`]: power nets to draw as ONE shared trunk even when
    /// the per-pin heuristic would distribute them. The cluster engine sets this for a power
    /// net whose pins it laid in a single aligned row (the "modules between rails" idiom), then
    /// keeps it only if its safety net confirms the trunk de-sprawls without colliding. Empty
    /// elsewhere ⇒ references byte-identical.
    #[serde(default)]
    pub rail_force: BTreeSet<String>,
    /// HYBRID VLM placement: refdes → a COARSE target position as a fraction of the
    /// board bbox, `[fx, fy]` in 0..1 (fx: 0=left,1=right; fy: 0=top,1=bottom). A vision
    /// LLM is good at rough DIRECTION ("power left, MCU centre") but not millimetre
    /// positions, so this is applied as a SOFT bias in the annealer's placement cost (its
    /// `zbias` term), NOT a forced cell — the engine still does the precise placement, just
    /// nudged toward the LLM's zones. Empty on every existing path ⇒ no bias ⇒ unchanged.
    #[serde(default)]
    pub zone: BTreeMap<String, [f64; 2]>,
    /// RELATIONAL layout intent: the author's statements about parts relative to each
    /// other ([`Relation`]). `#[serde(default)]` so every existing sidecar `layout.json`
    /// deserializes to an empty list ⇒ no constraint ⇒ tuned references unaffected.
    ///
    /// ## What each engine guarantees
    ///
    /// | engine | ordering (`LeftOf`/`RightOf`/`Above`/`Below`) | `Group` side | `Group` cohesion | `Align` |
    /// |---|---|---|---|---|
    /// | `anneal-place` | HARD — seeded by projection, moves that break it are rejected, plus a heavy cost term | HARD, same route | soft cost (group bbox) | HARD, same route |
    /// | `cluster-place` | inherits anneal, and its pose/de-sprawl/rail steps self-reject on any regression | as anneal | as anneal | as anneal |
    /// | `spine-place` | projection at the end of typesetting, kept only if its A/B gate agrees; violations rank above aesthetics in every pass | same | not modelled — the grammar owns cohesion | same |
    ///
    /// "HARD" means the shipped placement satisfies the relation whenever a feasible
    /// placement exists; contradictory intent (a cycle) is left alone rather than
    /// resolved arbitrarily, and a relation whose parts are all frozen cannot be met.
    #[serde(default)]
    pub relations: Vec<Relation>,
}

impl LayoutIr {
    /// Deserialize an IR from JSON (the subagent's structured output / a test
    /// fixture sidecar).
    pub fn from_json(s: &str) -> serde_json::Result<LayoutIr> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod group_side_tests {
    use super::*;

    #[test]
    fn a_group_side_is_accepted_in_all_three_written_forms() {
        let positional: GroupSide = serde_json::from_str(r#"["left","U1"]"#).unwrap();
        let named: GroupSide =
            serde_json::from_str(r#"{"side":"left","anchor":"U1"}"#).unwrap();
        assert_eq!(positional.parts(), (Side::Left, Some("U1")));
        assert_eq!(named.parts(), (Side::Left, Some("U1")));

        // The bare edge is what two of four campaign agents actually wrote.
        let bare: GroupSide = serde_json::from_str(r#""top""#).unwrap();
        assert_eq!(bare.parts(), (Side::Top, None));
    }

    #[test]
    fn a_group_relation_deserializes_with_a_bare_side() {
        let rel: Relation = serde_json::from_str(
            r#"{"kind":"group","name":"power","members":["J1","F1"],"side":"left"}"#,
        )
        .unwrap();
        assert_eq!(rel.refdes(), vec!["J1", "F1"]);
    }
}
