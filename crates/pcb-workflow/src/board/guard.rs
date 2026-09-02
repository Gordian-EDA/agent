//! The one gate every board mutator passes through.
//!
//! The invariant is one-directional: the board's copper may connect *less* than
//! the schematic asks — an unrouted net is honest work still to do — but never
//! *more*. A short is a defect no result payload can excuse, and neither is a
//! clearance fault the edit itself introduced.
//!
//! So a mutator never writes unguarded. It opens a [`Guard`] (which saves any
//! live session, snapshots the `.kicad_pcb`, and records the defects the board
//! *already* had), makes its edit, and hands the result to [`Guard::commit`].
//! The guard re-reads the board, diffs its defects against the baseline, and
//! either stamps the revision on the result or restores the snapshot and
//! refuses with the violations it found.
//!
//! Faults are compared as `(rule, the nets involved)`, not as exact payloads:
//! copper moves, so coordinates move with it, but the *fault* does not. A board
//! that arrived shorted stays the agent's problem to fix, not a reason to refuse
//! every later edit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{Value, json};

use gordian_runtime::AgentRuntime;
use gordian_runtime::revisions::RevisionId;
use kicad_board::IpcBoardSnapshot;
use pcb_model::Finding as DrcViolation;
use pcb_model::Violation;

use crate::diagnose::{Fault, FaultKey, faults};

/// The defects a board carries at one moment.
#[derive(Debug, Default, Clone)]
pub(crate) struct Defects {
    /// Pairs of nets the copper electrically merges.
    shorts: BTreeSet<(String, String)>,
    /// Geometry faults, as `(rule, nets)` with their multiplicity.
    faults: BTreeMap<FaultKey, usize>,
    /// Connections whose copper does not join all their pads.
    unrouted: BTreeSet<String>,
}

impl Defects {
    /// Lint a board snapshot and split its findings into the three kinds the
    /// guard reasons about.
    pub(crate) fn of(board: &IpcBoardSnapshot) -> (Self, Vec<Fault>) {
        // The board's own copper appears twice in a snapshot: once as true
        // geometry in `copper`, and once as the bounding boxes KiCAD hands over
        // as router keep-outs. Lint the geometry; a diagonal trace's bounding
        // box swallows foreign pads and would read as a short that is not there.
        let mut problem = board.problem.clone();
        crate::route::remove_existing_copper_obstacles(&mut problem);
        let violations = pcb_engine::check(&problem, &board.copper);
        let mut defects = Defects::default();
        let mut geometry = Vec::new();
        for violation in &violations {
            match violation {
                DrcViolation::Connectivity {
                    violation: Violation::CrossNetMerge { a, b },
                } => {
                    defects.shorts.insert((a.clone(), b.clone()));
                }
                DrcViolation::Connectivity {
                    violation: Violation::Unconnected { connection, .. },
                } => {
                    defects.unrouted.insert(connection.clone());
                }
                other => geometry.push(other.clone()),
            }
        }
        let explained = faults(&geometry, &problem, &board.imported.parts);
        for fault in &explained {
            *defects.faults.entry(fault.key.clone()).or_default() += 1;
        }
        (defects, explained)
    }
}

/// What an edit added to a board's defects. Everything the board already
/// carried is excused: a mutator answers for its own damage, not for arriving at
/// a board someone else broke.
struct Introduced<'a> {
    shorts: Vec<&'a (String, String)>,
    faults: Vec<&'a Fault>,
}

fn introduced<'a>(
    before: &'a Defects,
    after: &'a Defects,
    explained: &'a [Fault],
) -> Introduced<'a> {
    let mut budget = before.faults.clone();
    Introduced {
        shorts: after.shorts.difference(&before.shorts).collect(),
        faults: explained
            .iter()
            .filter(|fault| match budget.get_mut(&fault.key) {
                Some(remaining) if *remaining > 0 => {
                    *remaining -= 1;
                    false
                }
                _ => true,
            })
            .collect(),
    }
}

/// A board mutation in flight: the pre-edit file, its revision snapshot, and the
/// defects the board already carried.
pub(crate) struct Guard {
    tool: &'static str,
    /// Every file the mutator declared, with the bytes it had. A rollback puts
    /// all of them back: `set_net_width` writes the project's net classes as
    /// well as the board, and restoring one without the other leaves the two
    /// disagreeing about the same net.
    original: Vec<(PathBuf, Option<String>)>,
    revision: RevisionId,
    /// `None` when the board could not be read before the edit — the guard then
    /// has no baseline to compare against and must not refuse on a guess.
    before: Option<Defects>,
}

impl Guard {
    /// Save any live session, snapshot the board, and record its defects.
    ///
    /// The `Err` payload is the mutator's refusal, ready to return: the board
    /// has not been touched.
    pub(crate) fn open(
        ctx: &AgentRuntime,
        tool: &'static str,
        summary: &str,
        files: &[PathBuf],
    ) -> Result<Self, Value> {
        let path = ctx.pcb_path();
        if !path.exists() {
            return Err(json!({
                "error": format!("{tool}: this project has no board yet — run sync_board first"),
            }));
        }
        // THE capture call site for every board mutator: one revision per edit.
        // It is taken BEFORE the live session is saved, so a session's unsaved
        // work is recoverable too, not overwritten on the way in.
        let revision = ctx.revisions().capture(tool, summary, files).map_err(
            |error| json!({ "error": format!("{tool}: could not capture the board: {error}") }),
        )?;
        ctx.kicad().save_if_open().map_err(|e| {
            json!({
                "error": format!("{tool}: could not save the open KiCAD board first: {e}"),
                "revision": revision,
            })
        })?;
        // What rollback restores: the board as this mutator found it, which is
        // the saved state — the session's own edits are not this tool's to undo.
        let original = files
            .iter()
            .map(|file| (file.clone(), std::fs::read_to_string(file).ok()))
            .collect::<Vec<_>>();
        if !original
            .iter()
            .any(|(file, text)| file == &path && text.is_some())
        {
            return Err(json!({
                "error": format!("{tool}: could not read the board"),
                "revision": revision,
            }));
        }
        // Read the baseline from a fresh session. The save above put the live
        // board on disk, so the two agree — but a cached pcbnew can still be
        // serving an older document, and a baseline from one board compared
        // against a check on another invents defects the edit never caused.
        ctx.close_kicad_session();
        let before = crate::active_board(ctx)
            .ok()
            .map(|board| Defects::of(&board).0);
        Ok(Self {
            tool,
            original,
            revision,
            before,
        })
    }

    /// The revision this edit can be undone to.
    pub(crate) fn revision(&self) -> RevisionId {
        self.revision
    }

    /// Put the board back as it was and return `error` with the revision on it.
    /// For an edit that failed on its own terms, before the guard's check.
    pub(crate) fn rollback(self, ctx: &AgentRuntime, error: Value) -> Value {
        let restored = self.restore(ctx);
        merge_into(
            error,
            json!({ "revision": self.revision, "restored": restored }),
        )
    }

    /// Check the edited board. Returns `result` stamped with the revision when
    /// the edit kept the board honest, or the refusal after rolling back.
    pub(crate) fn commit(self, ctx: &AgentRuntime, result: Value) -> Value {
        let stamped = |result: Value, revision: RevisionId| {
            merge_into(result, json!({ "revision": revision }))
        };
        let Some(before) = self.before.as_ref() else {
            return stamped(result, self.revision);
        };
        // A board that cannot be read AFTER the edit is the one case rollback
        // exists for: the check cannot run, so the edit cannot be trusted.
        let board = match crate::active_board(ctx) {
            Ok(board) => board,
            Err(error) => {
                let tool = self.tool;
                let restored = self.restore(ctx);
                return json!({
                    "ok": false,
                    "error": format!("{tool}: the edited board could not be read back: {error}"),
                    "code": "board_unreadable_after_edit",
                    "restored": restored,
                    "revision": self.revision,
                });
            }
        };
        let (after, explained) = Defects::of(&board);
        let Introduced {
            shorts,
            faults: introduced,
        } = introduced(before, &after, &explained);
        if shorts.is_empty() && introduced.is_empty() {
            return stamped(result, self.revision);
        }
        let shorts: Vec<Value> = shorts
            .iter()
            .map(|(a, b)| json!({ "a": a, "b": b }))
            .collect();

        let restored = self.restore(ctx);
        let tool = self.tool;
        let headline = if shorts.is_empty() {
            format!(
                "{tool} refused: the edit introduced {} design-rule violation(s) the board did \
                 not have",
                introduced.len()
            )
        } else {
            format!(
                "{tool} refused: the edit would short {} net pair(s) the schematic keeps apart",
                shorts.len()
            )
        };
        json!({
            "ok": false,
            "error": headline,
            "code": "board_guard_refused",
            "shorts": shorts,
            "violations": introduced.iter().map(|fault| fault.json.clone()).collect::<Vec<_>>(),
            // Honest, not blocking: what the board still has left to route. A
            // net here is a to-do, and it is reported so the caller can tell it
            // apart from the defects above.
            "unrouted": after.unrouted.iter().collect::<Vec<_>>(),
            "restored": restored,
            "revision": self.revision,
            "note": if restored {
                "the board is back to its pre-edit state; nothing was written. Fix what the \
                 violations name — delete the offending copper, move the part, or widen the \
                 board — then try again."
            } else {
                "the board could NOT be put back — undo this revision before editing further."
            },
        })
    }

    fn restore(&self, ctx: &AgentRuntime) -> bool {
        ctx.close_kicad_session();
        self.original.iter().all(|(path, text)| match text {
            Some(text) => std::fs::write(path, text).is_ok(),
            // The file did not exist before the edit; an edit that created one
            // is undone by removing it again.
            None => !path.exists() || std::fs::remove_file(path).is_ok(),
        })
    }
}

/// Fold `extra`'s fields into `base` when `base` is an object.
fn merge_into(mut base: Value, extra: Value) -> Value {
    if let (Value::Object(base), Value::Object(extra)) = (&mut base, extra) {
        base.extend(extra);
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(a: &str, b: &str) -> (String, String) {
        (a.to_owned(), b.to_owned())
    }

    fn fault(rule: &'static str, nets: &[&str]) -> Fault {
        Fault {
            key: (rule, nets.iter().map(|net| (*net).to_owned()).collect()),
            json: json!({ "rule": rule, "nets": nets }),
        }
    }

    fn defects(shorts: &[(String, String)], faults: &[Fault]) -> Defects {
        let mut counts: BTreeMap<FaultKey, usize> = BTreeMap::new();
        for fault in faults {
            *counts.entry(fault.key.clone()).or_default() += 1;
        }
        Defects {
            shorts: shorts.iter().cloned().collect(),
            faults: counts,
            unrouted: BTreeSet::new(),
        }
    }

    #[test]
    fn a_short_the_edit_created_is_refused() {
        let before = defects(&[], &[]);
        let after = defects(&[short("GND", "VBUS")], &[]);
        let introduced = introduced(&before, &after, &[]);
        assert_eq!(introduced.shorts, [&short("GND", "VBUS")]);
    }

    #[test]
    fn a_short_the_board_arrived_with_is_not_this_edit_to_answer_for() {
        let already = defects(&[short("GND", "VBUS")], &[]);
        let introduced = introduced(&already, &already, &[]);
        assert!(introduced.shorts.is_empty());
        assert!(introduced.faults.is_empty());
    }

    #[test]
    fn a_fault_is_matched_by_rule_and_nets_so_moved_copper_does_not_look_new() {
        let clearance = fault("clearance (track to track)", &["GND", "SDA"]);
        let before = defects(&[], std::slice::from_ref(&clearance));
        // Same fault, reported at a different place after the copper moved.
        let mut moved = fault("clearance (track to track)", &["GND", "SDA"]);
        moved.json = json!({ "rule": "clearance (track to track)", "at_mm": [9.0, 9.0] });
        let after = defects(&[], std::slice::from_ref(&moved));
        assert!(
            introduced(&before, &after, std::slice::from_ref(&moved))
                .faults
                .is_empty()
        );

        // A SECOND one of the same fault is one the edit added.
        let two = [moved.clone(), moved.clone()];
        let after_two = defects(&[], &two);
        assert_eq!(introduced(&before, &after_two, &two).faults.len(), 1);
    }
}
