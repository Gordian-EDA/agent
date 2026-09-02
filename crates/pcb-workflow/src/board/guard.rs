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
use kicad_board::IpcBoardSnapshot;
use pcb_drc::connectivity::Violation;
use pcb_drc::lint::DrcViolation;

use crate::diagnose::{Fault, FaultKey, faults};

/// Where a board mutator's pre-edit copies live, one per committed edit.
fn undo_dir(ctx: &AgentRuntime) -> PathBuf {
    ctx.project_dir().join(".gordian").join("pcb-undo")
}

/// Snapshot the pre-edit board text and return its revision id.
///
/// THE SINGLE CAPTURE CALL SITE for every board mutator: the unified revision
/// system replaces this body with `revisions::capture(tool, summary, &paths)`.
pub(crate) fn snapshot(ctx: &AgentRuntime, original: &str) -> std::io::Result<String> {
    let dir = undo_dir(ctx);
    std::fs::create_dir_all(&dir)?;
    let next = 1 + std::fs::read_dir(&dir)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()?
                .to_str()?
                .strip_prefix("pcb-")?
                .parse::<u32>()
                .ok()
        })
        .max()
        .unwrap_or(0);
    let id = format!("pcb-{next}");
    std::fs::write(dir.join(format!("{id}.kicad_pcb")), original)?;
    Ok(id)
}

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
        let violations = pcb_drc::lint::lint(&board.problem, &board.copper);
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
        let explained = faults(&geometry, &board.problem, &board.imported.parts);
        for fault in &explained {
            *defects.faults.entry(fault.key.clone()).or_default() += 1;
        }
        (defects, explained)
    }
}

/// A board mutation in flight: the pre-edit file, its revision snapshot, and the
/// defects the board already carried.
pub(crate) struct Guard {
    tool: &'static str,
    path: PathBuf,
    original: String,
    revision: String,
    /// `None` when the board could not be read before the edit — the guard then
    /// has no baseline to compare against and must not refuse on a guess.
    before: Option<Defects>,
}

impl Guard {
    /// Save any live session, snapshot the board, and record its defects.
    ///
    /// The `Err` payload is the mutator's refusal, ready to return: the board
    /// has not been touched.
    pub(crate) fn open(ctx: &AgentRuntime, tool: &'static str) -> Result<Self, Value> {
        let path = ctx.pcb_path();
        if !path.exists() {
            return Err(json!({
                "error": format!("{tool}: this project has no board yet — run sync_board first"),
            }));
        }
        ctx.kicad().save_if_open().map_err(|e| {
            json!({ "error": format!("{tool}: could not save the open KiCAD board first: {e}") })
        })?;
        let original = std::fs::read_to_string(&path)
            .map_err(|e| json!({ "error": format!("{tool}: could not read the board: {e}") }))?;
        let revision = snapshot(ctx, &original).map_err(
            |e| json!({ "error": format!("{tool}: could not snapshot the board: {e}") }),
        )?;
        let before = crate::active_board(ctx)
            .ok()
            .map(|board| Defects::of(&board).0);
        Ok(Self {
            tool,
            path,
            original,
            revision,
            before,
        })
    }

    /// Put the board back as it was and return `error` with the revision on it.
    /// For an edit that failed on its own terms, before the guard's check.
    pub(crate) fn rollback(self, ctx: &AgentRuntime, error: Value) -> Value {
        let restored = self.restore(ctx);
        merge_into(error, json!({ "revision": self.revision, "restored": restored }))
    }

    /// Check the edited board. Returns `result` stamped with the revision when
    /// the edit kept the board honest, or the refusal after rolling back.
    pub(crate) fn commit(self, ctx: &AgentRuntime, result: Value) -> Value {
        let stamped = |result: Value, revision: &str| {
            merge_into(result, json!({ "revision": revision }))
        };
        let (Some(before), Ok(board)) = (self.before.as_ref(), crate::active_board(ctx)) else {
            return stamped(result, &self.revision);
        };
        let (after, explained) = Defects::of(&board);

        let shorts: Vec<Value> = after
            .shorts
            .difference(&before.shorts)
            .map(|(a, b)| json!({ "a": a, "b": b }))
            .collect();
        let mut budget = before.faults.clone();
        let introduced: Vec<&Fault> = explained
            .iter()
            .filter(|fault| match budget.get_mut(&fault.key) {
                Some(remaining) if *remaining > 0 => {
                    *remaining -= 1;
                    false
                }
                _ => true,
            })
            .collect();
        if shorts.is_empty() && introduced.is_empty() {
            return stamped(result, &self.revision);
        }

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
            "note": "the board is back to its pre-edit state; nothing was written. Fix what the \
                     violations name — delete the offending copper, move the part, or widen the \
                     board — then try again.",
        })
    }

    fn restore(&self, ctx: &AgentRuntime) -> bool {
        ctx.close_kicad_session();
        std::fs::write(&self.path, &self.original).is_ok()
    }
}

/// Fold `extra`'s fields into `base` when `base` is an object.
fn merge_into(mut base: Value, extra: Value) -> Value {
    if let (Value::Object(base), Value::Object(extra)) = (&mut base, extra) {
        base.extend(extra);
    }
    base
}
