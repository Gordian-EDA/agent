//! The staging row: parts that are on the board but not yet part of it.
//!
//! A board is built incrementally, so a legal board has parts nobody has placed
//! yet. They live in the seed row `sync_board` writes above the outline — the
//! row IS the staging area, there is no second flag — and this module reads
//! that row back as facts the tools report: who is staged, why, and who is
//! placed or locked.
//!
//! Staged parts are excluded from the DRC verdict and from fabrication export:
//! copper that does not exist yet is work outstanding, not a violation.

use std::collections::BTreeSet;

use kicad_board::{BoardSnapshot, ImportedPart};
use serde_json::{Value, json};

/// Why a part is still in the staging row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagedReason {
    /// `sync_board` added it to a board that already had a layout.
    NewFromSync,
    /// Its symbol pins and its footprint pads disagree, so it was staged rather
    /// than blocking the whole sync.
    FootprintMismatch,
    /// Nothing has laid it out yet.
    Unplaced,
}

impl StagedReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            StagedReason::NewFromSync => "new_from_sync",
            StagedReason::FootprintMismatch => "footprint_mismatch",
            StagedReason::Unplaced => "unplaced",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "new_from_sync" => Some(StagedReason::NewFromSync),
            "footprint_mismatch" => Some(StagedReason::FootprintMismatch),
            "unplaced" => Some(StagedReason::Unplaced),
            _ => None,
        }
    }
}

/// One part waiting in the staging row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedPart {
    pub(crate) reference: String,
    pub(crate) reason: StagedReason,
    /// What the reason needs spelled out — the pin/pad mismatch, say.
    pub(crate) detail: Option<String>,
}

impl StagedPart {
    fn to_json(&self) -> Value {
        json!({
            "ref": self.reference,
            "staged_reason": self.reason.as_str(),
            "detail": self.detail,
        })
    }
}

/// Every part still in the staging row, with the reason it is there.
///
/// Membership is positional (the seed row); the reason is the annotation
/// `sync_board` left on the footprint, defaulting to `unplaced` for a part that
/// was simply never laid out.
pub(crate) fn staged(board: &BoardSnapshot) -> Vec<StagedPart> {
    let row: BTreeSet<String> = kicad_board::seed_row_references(&board.imported)
        .into_iter()
        .collect();
    board
        .imported
        .parts
        .iter()
        .filter(|part| row.contains(&part.reference))
        .map(|part| StagedPart {
            reference: part.reference.clone(),
            reason: part
                .property(kicad_board::STAGED_REASON)
                .and_then(StagedReason::parse)
                .unwrap_or(StagedReason::Unplaced),
            detail: part.property(kicad_board::STAGED_DETAIL).map(str::to_owned),
        })
        .collect()
}

/// The references in the staging row.
pub(crate) fn staged_references(board: &BoardSnapshot) -> BTreeSet<String> {
    kicad_board::seed_row_references(&board.imported)
        .into_iter()
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
        let row_y = kicad_board::seed_row_y(0.0);
        let mut text = String::from(
            "(kicad_pcb\n\t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(2 \"B.Cu\" signal)\n\t\t(44 \"Edge.Cuts\" user)\n\t)\n\t(gr_rect\n\t\t(start 0 0)\n\t\t(end 40 40)\n\t\t(layer \"Edge.Cuts\")\n\t)\n",
        );
        for (index, reference) in ["R1", "R2"].iter().enumerate() {
            let x = kicad_board::seed_row_x(0.0, index);
            text.push_str(&format!(
                "\t(footprint \"L:R\"\n\t\t(layer \"F.Cu\")\n\t\t(at {x} {row_y})\n\t\t(property \"Reference\" \"{reference}\"\n\t\t\t(at 0 0 0)\n\t\t)\n\t)\n"
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
    fn the_seed_row_is_the_staging_area_and_carries_its_reason() {
        let board = board_with(&[kicad_board::Annotation::new("R2")
            .set(kicad_board::STAGED_REASON, "footprint_mismatch")
            .set(kicad_board::STAGED_DETAIL, "pin 3 has no pad")]);
        let state = BoardState::of(&board);

        assert_eq!(state.staged_references(), ["R1", "R2"]);
        assert_eq!(state.placed, ["U1"]);
        assert_eq!(state.staged[0].reason, StagedReason::Unplaced);
        assert_eq!(state.staged[0].detail, None);
        assert_eq!(state.staged[1].reason, StagedReason::FootprintMismatch);
        assert_eq!(state.staged[1].detail.as_deref(), Some("pin 3 has no pad"));
    }

    #[test]
    fn a_lock_reports_its_reason_and_a_hand_lock_reads_as_user() {
        let board = board_with(&[
            kicad_board::Annotation::new("U1")
                .locked(true)
                .set(kicad_board::LOCKED_REASON, "mechanical"),
            kicad_board::Annotation::new("R1").locked(true),
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
