//! The staging row: parts that are on the board but not yet part of it.
//!
//! A board is built incrementally, so a legal board has parts nobody has placed
//! yet. They live in the seed row `sync_board` writes above the outline — the
//! existing staging-reason annotation records row membership and explains why
//! each part remains there. This module reads that state back as facts the
//! tools report: who is staged, why, and who is placed or locked.
//!
//! Staged parts are excluded from the DRC verdict and from fabrication export:
//! copper that does not exist yet is work outstanding, not a violation.

use std::collections::BTreeSet;

use geom::Point2;
use kicad_board::{BoardSnapshot, ImportedPart};
use serde_json::{Value, json};

/// Clear space kept between provisional footprint envelopes.
pub(crate) const STAGING_GAP_MM: f64 = 1.0;

/// Why a part is still in the staging row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagedReason {
    /// `sync_board` added it to a board that already had a layout.
    NewFromSync,
    /// Its symbol pins and its footprint pads disagree, so it was staged rather
    /// than blocking the whole sync.
    FootprintMismatch,
    /// The schematic has no footprint assignment for this reference yet.
    MissingFootprint,
    /// Nothing has laid it out yet.
    Unplaced,
}

impl StagedReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            StagedReason::NewFromSync => "new_from_sync",
            StagedReason::FootprintMismatch => "footprint_mismatch",
            StagedReason::MissingFootprint => "missing_footprint",
            StagedReason::Unplaced => "unplaced",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "new_from_sync" => Some(StagedReason::NewFromSync),
            "footprint_mismatch" => Some(StagedReason::FootprintMismatch),
            "missing_footprint" => Some(StagedReason::MissingFootprint),
            "unplaced" => Some(StagedReason::Unplaced),
            _ => None,
        }
    }
}

/// References whose staged reason prevents meaningful physical placement.
pub(crate) fn unplaceable_references(board: &BoardSnapshot) -> BTreeSet<String> {
    staged(board)
        .into_iter()
        .filter(|part| {
            matches!(
                part.reason,
                StagedReason::FootprintMismatch | StagedReason::MissingFootprint
            )
        })
        .map(|part| part.reference)
        .collect()
}

/// One part waiting in the staging row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StagedPart {
    pub(crate) reference: String,
    pub(crate) reason: StagedReason,
    /// What the reason needs spelled out — the pin/pad mismatch, say.
    pub(crate) detail: Option<String>,
    /// Physical footprint bounds at its current board pose.
    pub(crate) extent: Option<geom::Rect>,
}

impl StagedPart {
    fn to_json(&self) -> Value {
        json!({
            "ref": self.reference,
            "staged_reason": self.reason.as_str(),
            "detail": self.detail,
            "extent": self.extent.map(rect_json),
        })
    }
}

/// Compact min/max/size representation shared by board-state reports.
pub(crate) fn rect_json(rect: geom::Rect) -> Value {
    json!({
        "min": [rect.min_x, rect.min_y],
        "max": [rect.max_x, rect.max_y],
        "size_mm": [rect.width(), rect.height()],
    })
}

/// Bounding box of a footprint's physical courtyard or pads at its saved pose.
pub(crate) fn part_extent(part: &ImportedPart) -> Option<geom::Rect> {
    part_local_extent(part).map(|local| {
        crate::place::courtyard_at(
            local,
            part.at,
            f64::from(part.rotation),
            part.side == kicad_board::BoardSide::Back,
        )
    })
}

/// A footprint's courtyard-or-pad envelope in its own unrotated coordinates.
pub(crate) fn part_local_extent(part: &ImportedPart) -> Option<geom::Rect> {
    if let Some(local) = part.courtyard {
        return Some(local);
    }
    let back = part.side == kicad_board::BoardSide::Back;
    part.pads.iter().fold(None, |extent, pad| {
        let mut local = Point2::new(pad.at.x - part.at.x, pad.at.y - part.at.y)
            .rotate(-f64::from(part.rotation));
        if back {
            local.x = -local.x;
        }
        let half = Point2::new(pad.size.x / 2.0, pad.size.y / 2.0)
            .rotated_half_extents(f64::from(part.rotation));
        let pad = geom::Rect::from_center_half(local, (half.x, half.y));
        Some(match extent {
            None => pad,
            Some(current) => geom::Rect::new(
                current.min_x.min(pad.min_x),
                current.min_y.min(pad.min_y),
                current.max_x.max(pad.max_x),
                current.max_y.max(pad.max_y),
            ),
        })
    })
}

/// Outline bounds in the same shape used for footprint extents.
pub(crate) fn outline_json(board: &BoardSnapshot) -> Value {
    rect_json(board.problem.bounds)
}

/// References outside the outline paired with their physical extents.
pub(crate) fn outside_json(
    board: &BoardSnapshot,
    references: impl IntoIterator<Item = String>,
) -> Vec<Value> {
    let references: BTreeSet<String> = references.into_iter().collect();
    board
        .imported
        .parts
        .iter()
        .filter(|part| references.contains(&part.reference))
        .map(|part| {
            json!({
                "ref": part.reference,
                "extent": part_extent(part).map(rect_json),
            })
        })
        .collect()
}

/// Every part still in the staging row, with the reason it is there.
///
/// The staging-reason annotation is membership as well as explanation, so
/// resizing an auto outline cannot accidentally turn staged geometry into
/// placed geometry. Placement clears the annotation when the part leaves the
/// row.
pub(crate) fn staged(board: &BoardSnapshot) -> Vec<StagedPart> {
    board
        .imported
        .parts
        .iter()
        .filter(|part| {
            part.property(kicad_board::STAGED_REASON)
                .and_then(StagedReason::parse)
                .is_some()
        })
        .map(|part| StagedPart {
            reference: part.reference.clone(),
            reason: part
                .property(kicad_board::STAGED_REASON)
                .and_then(StagedReason::parse)
                .unwrap_or(StagedReason::Unplaced),
            detail: part.property(kicad_board::STAGED_DETAIL).map(str::to_owned),
            extent: part_extent(part),
        })
        .collect()
}

/// The references in the staging row.
pub(crate) fn staged_references(board: &BoardSnapshot) -> BTreeSet<String> {
    staged(board)
        .into_iter()
        .map(|part| part.reference)
        .collect()
}

/// Why one part is locked. `locked_reason` is revocable metadata beside
/// KiCad's own `locked` flag; a part locked in KiCad by hand has no reason
/// property and reads as `user`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockReason {
    /// A connector, mounting hole or other part whose position is physical.
    Mechanical,
    /// A pose the agent decided to keep.
    Agent,
    /// Locked outside this tool surface.
    User,
}

impl LockReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LockReason::Mechanical => "mechanical",
            LockReason::Agent => "agent",
            LockReason::User => "user",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "mechanical" => Some(LockReason::Mechanical),
            "agent" => Some(LockReason::Agent),
            "user" => Some(LockReason::User),
            _ => None,
        }
    }
}

/// The reason a locked part carries, or `user` when nothing recorded one.
pub(crate) fn lock_reason(part: &ImportedPart) -> Option<LockReason> {
    part.locked.then(|| {
        part.property(kicad_board::LOCKED_REASON)
            .and_then(LockReason::parse)
            .unwrap_or(LockReason::User)
    })
}

/// The board's parts split into the three states a partial board has.
pub(crate) struct BoardState {
    pub(crate) staged: Vec<StagedPart>,
    pub(crate) placed: Vec<String>,
    pub(crate) locked: Vec<Value>,
}

impl BoardState {
    pub(crate) fn of(board: &BoardSnapshot) -> Self {
        let staged = staged(board);
        let in_row: BTreeSet<&str> = staged.iter().map(|part| part.reference.as_str()).collect();
        Self {
            placed: board
                .imported
                .parts
                .iter()
                .filter(|part| !in_row.contains(part.reference.as_str()))
                .map(|part| part.reference.clone())
                .collect(),
            locked: board
                .imported
                .parts
                .iter()
                .filter_map(|part| {
                    lock_reason(part).map(
                        |reason| json!({ "ref": part.reference, "locked_reason": reason.as_str() }),
                    )
                })
                .collect(),
            staged,
        }
    }

    pub(crate) fn staged_json(&self) -> Vec<Value> {
        self.staged.iter().map(StagedPart::to_json).collect()
    }

    pub(crate) fn staged_references(&self) -> Vec<String> {
        self.staged
            .iter()
            .map(|part| part.reference.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board_with(annotations: &[kicad_board::Annotation]) -> BoardSnapshot {
        let mut text = String::from(
            "(kicad_pcb\n\t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(2 \"B.Cu\" signal)\n\t\t(44 \"Edge.Cuts\" user)\n\t)\n\t(gr_rect\n\t\t(start 0 0)\n\t\t(end 40 40)\n\t\t(layer \"Edge.Cuts\")\n\t)\n",
        );
        for (index, reference) in ["R1", "R2"].iter().enumerate() {
            let x = 2.0 + index as f64 * 4.0;
            text.push_str(&format!(
                "\t(footprint \"L:R\"\n\t\t(layer \"F.Cu\")\n\t\t(at {x} -2)\n\t\t(property \"Reference\" \"{reference}\"\n\t\t\t(at 0 0 0)\n\t\t)\n\t)\n"
            ));
        }
        // A part nothing staged: laid out in the middle of the board.
        text.push_str(
            "\t(footprint \"L:U\"\n\t\t(layer \"F.Cu\")\n\t\t(at 20 20)\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 0 0 0)\n\t\t)\n\t)\n)",
        );
        let text = kicad_board::patch_annotations(&text, annotations).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.kicad_pcb");
        std::fs::write(&path, text).unwrap();
        kicad_board::read_snapshot(&path).unwrap()
    }

    #[test]
    fn staging_annotations_record_membership_and_reason() {
        let board = board_with(&[
            kicad_board::Annotation::new("R1").set(kicad_board::STAGED_REASON, "unplaced"),
            kicad_board::Annotation::new("R2")
                .set(kicad_board::STAGED_REASON, "footprint_mismatch")
                .set(kicad_board::STAGED_DETAIL, "pin 3 has no pad"),
        ]);
        let state = BoardState::of(&board);

        assert_eq!(state.staged_references(), ["R1", "R2"]);
        assert_eq!(state.placed, ["U1"]);
        assert_eq!(state.staged[0].reason, StagedReason::Unplaced);
        assert_eq!(state.staged[0].detail, None);
        assert!(state.staged[0].extent.is_none());
        assert_eq!(state.staged[1].reason, StagedReason::FootprintMismatch);
        assert_eq!(state.staged[1].detail.as_deref(), Some("pin 3 has no pad"));
    }

    #[test]
    fn a_lock_reports_its_reason_and_a_hand_lock_reads_as_user() {
        let board = board_with(&[
            kicad_board::Annotation::new("U1")
                .locked(true)
                .set(kicad_board::LOCKED_REASON, "mechanical"),
            kicad_board::Annotation::new("R1")
                .locked(true)
                .set(kicad_board::STAGED_REASON, "unplaced"),
            kicad_board::Annotation::new("R2").set(kicad_board::STAGED_REASON, "unplaced"),
        ]);
        let state = BoardState::of(&board);

        assert_eq!(
            state.locked,
            [
                json!({ "ref": "R1", "locked_reason": "user" }),
                json!({ "ref": "U1", "locked_reason": "mechanical" }),
            ]
        );
    }
}
