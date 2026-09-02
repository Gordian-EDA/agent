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
use kicad_board::BoardSnapshot;
use pcb_model::Finding as DrcViolation;
use pcb_model::{Point2, Polygon, Violation};

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
    /// Footprint courtyards that cross the physical board outline.
    outside_outline: BTreeMap<String, String>,
    /// Routed copper items that do not clear the physical board outline.
    copper_outside_outline: BTreeSet<String>,
}

impl Defects {
    /// Lint a board snapshot and split its findings into the three kinds the
    /// guard reasons about.
    pub(crate) fn of(board: &BoardSnapshot) -> (Self, Vec<Fault>) {
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
        let containment = outline_containment(board);
        defects.outside_outline = containment.outside_outline_keys;
        defects.copper_outside_outline = containment.copper_outside_keys;
        (defects, explained)
    }
}

/// Physical geometry that crosses a board outline.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct OutlineContainment {
    pub(crate) outside_outline: BTreeSet<String>,
    pub(crate) copper_outside_outline: usize,
    outside_outline_keys: BTreeMap<String, String>,
    copper_outside_keys: BTreeSet<String>,
}

impl OutlineContainment {
    pub(crate) fn is_clear(&self) -> bool {
        self.outside_outline.is_empty() && self.copper_outside_keys.is_empty()
    }
}

/// Check courtyards, pad copper, tracks and vias against the saved outline.
pub(crate) fn outline_containment(board: &BoardSnapshot) -> OutlineContainment {
    let Some(outline) = board.problem.outline.as_ref() else {
        return OutlineContainment::default();
    };
    outline_containment_against(board, outline)
}

/// Check saved board geometry against a proposed outline.
pub(crate) fn outline_containment_against(
    board: &BoardSnapshot,
    outline: &Polygon,
) -> OutlineContainment {
    let mut result = OutlineContainment::default();
    let outline_key = outline
        .points()
        .iter()
        .map(|point| format!("{:016x}:{:016x}", point.x.to_bits(), point.y.to_bits()))
        .collect::<Vec<_>>()
        .join("/");
    for part in &board.imported.parts {
        if let Some(local) = part.courtyard {
            let courtyard = crate::place::courtyard_at(
                local,
                part.at,
                f64::from(part.rotation),
                part.side == kicad_board::BoardSide::Back,
            );
            if !rect_inside_outline(courtyard, outline) {
                result.outside_outline.insert(part.reference.clone());
                result.outside_outline_keys.insert(
                    format!(
                        "{outline_key}|courtyard|{}|{:016x}:{:016x}:{:016x}:{:016x}",
                        part.reference,
                        courtyard.min_x.to_bits(),
                        courtyard.min_y.to_bits(),
                        courtyard.max_x.to_bits(),
                        courtyard.max_y.to_bits(),
                    ),
                    part.reference.clone(),
                );
            }
        }
    }

    let edge_clear = crate::sizing::EDGE_CLEAR_MM;
    for (index, obstacle) in board
        .problem
        .obstacles
        .iter()
        .filter(|obstacle| obstacle.kind.starts_with("pad:") || obstacle.kind == "zone")
        .enumerate()
    {
        let bounds = geom::Rect::from_center_half(
            obstacle.center,
            (obstacle.width / 2.0, obstacle.height / 2.0),
        );
        let required = if obstacle.kind.starts_with("pad:") {
            bounds.inflate(edge_clear)
        } else {
            bounds
        };
        if !rect_inside_outline(required, outline) {
            result.copper_outside_keys.insert(format!(
                "{outline_key}|obstacle|{index}|{}|{:016x}:{:016x}:{:016x}:{:016x}",
                obstacle.kind,
                bounds.min_x.to_bits(),
                bounds.min_y.to_bits(),
                bounds.max_x.to_bits(),
                bounds.max_y.to_bits(),
            ));
        }
    }
    for (trace_index, trace) in board.copper.traces.iter().enumerate() {
        let required = edge_clear + trace.width / 2.0;
        for (segment_index, points) in trace.path.windows(2).enumerate() {
            let segment = geom::Segment::new(points[0], points[1]);
            if !outline.contains_point(points[0])
                || !outline.contains_point(points[1])
                || outline.segment_dist_to_edge(segment) + geom::EPS < required
            {
                result.copper_outside_keys.insert(format!(
                    "{outline_key}|trace|{trace_index}|{segment_index}|{}|{:016x}:{:016x}:{:016x}:{:016x}:{:016x}",
                    trace.connection,
                    points[0].x.to_bits(),
                    points[0].y.to_bits(),
                    points[1].x.to_bits(),
                    points[1].y.to_bits(),
                    trace.width.to_bits(),
                ));
            }
        }
    }
    for (index, via) in board.copper.vias.iter().enumerate() {
        let required = edge_clear + via.diameter / 2.0;
        if !outline.contains_point(via.at) || outline.dist_to_edge(via.at) + geom::EPS < required {
            result.copper_outside_keys.insert(format!(
                "{outline_key}|via|{index}|{}|{:016x}:{:016x}:{:016x}",
                via.connection,
                via.at.x.to_bits(),
                via.at.y.to_bits(),
                via.diameter.to_bits(),
            ));
        }
    }
    result.copper_outside_outline = result.copper_outside_keys.len();
    result
}

fn rect_inside_outline(rect: geom::Rect, outline: &Polygon) -> bool {
    let corners_inside = [
        Point2::new(rect.min_x, rect.min_y),
        Point2::new(rect.max_x, rect.min_y),
        Point2::new(rect.max_x, rect.max_y),
        Point2::new(rect.min_x, rect.max_y),
    ]
    .into_iter()
    .all(|point| outline.contains_point(point));
    corners_inside
        && !outline
            .edges()
            .any(|edge| segment_hits_rect_interior(edge, rect))
}

fn segment_hits_rect_interior(segment: geom::Segment, rect: geom::Rect) -> bool {
    let inner = geom::Rect::new(
        rect.min_x + geom::EPS,
        rect.min_y + geom::EPS,
        rect.max_x - geom::EPS,
        rect.max_y - geom::EPS,
    );
    if inner.min_x >= inner.max_x || inner.min_y >= inner.max_y {
        return false;
    }
    let direction = Point2::new(segment.b.x - segment.a.x, segment.b.y - segment.a.y);
    let mut enter = 0.0_f64;
    let mut exit = 1.0_f64;
    for (origin, delta, low, high) in [
        (segment.a.x, direction.x, inner.min_x, inner.max_x),
        (segment.a.y, direction.y, inner.min_y, inner.max_y),
    ] {
        if delta.abs() <= f64::EPSILON {
            if origin < low || origin > high {
                return false;
            }
            continue;
        }
        let first = (low - origin) / delta;
        let second = (high - origin) / delta;
        enter = enter.max(first.min(second));
        exit = exit.min(first.max(second));
        if enter > exit {
            return false;
        }
    }
    exit >= 0.0 && enter <= 1.0
}

/// What an edit added to a board's defects. Everything the board already
/// carried is excused: a mutator answers for its own damage, not for arriving at
/// a board someone else broke.
struct Introduced<'a> {
    shorts: Vec<&'a (String, String)>,
    faults: Vec<&'a Fault>,
    outside_outline: Vec<&'a String>,
    copper_outside_outline: Vec<&'a String>,
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
        outside_outline: after
            .outside_outline
            .iter()
            .filter(|(key, _)| !before.outside_outline.contains_key(*key))
            .map(|(_, reference)| reference)
            .collect(),
        copper_outside_outline: after
            .copper_outside_outline
            .difference(&before.copper_outside_outline)
            .collect(),
    }
}

/// What a mutator declares before it writes: who it is, which files it may
/// touch, which references it acts on, and the revision the caller believed the
/// board was at.
pub(crate) struct Edit<'a> {
    pub(crate) tool: &'static str,
    pub(crate) summary: &'a str,
    pub(crate) files: &'a [PathBuf],
    pub(crate) refs: Vec<String>,
    /// The caller's `expect_revision`: when it is not the board's current
    /// revision, someone else wrote since and this edit is refused instead of
    /// silently landing on top of theirs.
    pub(crate) expect_revision: Option<RevisionId>,
}

impl<'a> Edit<'a> {
    pub(crate) fn new(tool: &'static str, summary: &'a str, files: &'a [PathBuf]) -> Self {
        Self {
            tool,
            summary,
            files,
            refs: Vec::new(),
            expect_revision: None,
        }
    }

    pub(crate) fn refs(mut self, refs: impl IntoIterator<Item = String>) -> Self {
        self.refs = refs.into_iter().collect();
        self
    }

    /// Read `expect_revision` off a tool's own input.
    pub(crate) fn expecting(mut self, input: &Value) -> Self {
        self.expect_revision = input
            .get("expect_revision")
            .and_then(Value::as_u64)
            .map(RevisionId::new);
        self
    }
}

/// The refusal a stale `expect_revision` earns: the current revision and what
/// the writer that took it touched, so the caller can re-read and retry.
fn conflict_refusal(
    ctx: &AgentRuntime,
    tool: &'static str,
    expected: RevisionId,
) -> Option<Value> {
    let current = ctx.revisions().conflict(expected).ok()??;
    Some(json!({
        "error": format!(
            "{tool} expected revision {expected}, but the project is at revision {} \
             (written by {}); nothing was written",
            current.id, current.tool
        ),
        "code": "revision_conflict",
        "expected_revision": expected,
        "current_revision": current.id,
        "current_tool": current.tool,
        "refs_touched": current.refs_touched,
        "note": "Re-read the board (get_board / check_board) and retry against the current \
                 revision, or drop expect_revision to write regardless.",
    }))
}

/// A board mutation in flight: the pre-edit file, its revision snapshot, and the
/// defects the board already carried.
pub(crate) struct Guard {
    tool: &'static str,
    phase: crate::WorkflowPhase,
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
    /// Snapshot the board and record its defects.
    ///
    /// The `Err` payload is the mutator's refusal, ready to return: the board
    /// has not been touched.
    pub(crate) fn open(ctx: &AgentRuntime, edit: Edit<'_>) -> Result<Self, Value> {
        let Edit {
            tool,
            summary,
            files,
            refs,
            expect_revision,
        } = edit;
        let path = ctx.pcb_path();
        if !path.exists() {
            return Err(json!({
                "error": format!("{tool}: this project has no board yet — run sync_board first"),
            }));
        }
        if let Some(expected) = expect_revision
            && let Some(refusal) = conflict_refusal(ctx, tool, expected)
        {
            return Err(refusal);
        }
        let revision = ctx
            .revisions()
            .capture(gordian_runtime::revisions::Capture::new(tool, summary, files).refs(refs))
            .map_err(
                |error| json!({ "error": format!("{tool}: could not capture the board: {error}") }),
            )?;
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
        let before = crate::active_board(ctx)
            .ok()
            .map(|board| Defects::of(&board).0);
        Ok(Self {
            tool,
            phase: crate::WorkflowPhase::start("guard", 0, 0),
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
        self.phase.facts(None, None, Some(1));
        tracing::info!(tool = self.tool, reason = %error, revision = %self.revision, "board guard rollback");
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
            self.phase.facts(None, None, Some(0));
            return stamped(result, self.revision);
        };
        // A board that cannot be read AFTER the edit is the one case rollback
        // exists for: the check cannot run, so the edit cannot be trusted.
        let board = match crate::active_board(ctx) {
            Ok(board) => board,
            Err(error) => {
                let tool = self.tool;
                self.phase.facts(None, None, Some(1));
                tracing::info!(tool, reason = %error, revision = %self.revision, "board guard refusal");
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
            outside_outline,
            copper_outside_outline,
        } = introduced(before, &after, &explained);
        if shorts.is_empty()
            && introduced.is_empty()
            && outside_outline.is_empty()
            && copper_outside_outline.is_empty()
        {
            self.phase.facts(None, None, Some(0));
            return stamped(result, self.revision);
        }
        let shorts: Vec<Value> = shorts
            .iter()
            .map(|(a, b)| json!({ "a": a, "b": b }))
            .collect();

        let restored = self.restore(ctx);
        let tool = self.tool;
        let headline = if !outside_outline.is_empty() || !copper_outside_outline.is_empty() {
            format!("{tool} refused: the edit moved board geometry outside the outline")
        } else if shorts.is_empty() {
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
        self.phase.facts(
            None,
            None,
            Some(
                shorts.len()
                    + introduced.len()
                    + outside_outline.len()
                    + usize::from(!copper_outside_outline.is_empty()),
            ),
        );
        tracing::info!(tool, reason = %headline, revision = %self.revision, "board guard refusal");
        json!({
            "ok": false,
            "error": headline,
            "code": "board_guard_refused",
            "shorts": shorts,
            "violations": introduced.iter().map(|fault| fault.json.clone()).collect::<Vec<_>>(),
            "outside_outline": outside_outline,
            "copper_outside_outline": !copper_outside_outline.is_empty(),
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

    fn restore(&self, _ctx: &AgentRuntime) -> bool {
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

    fn containment_snapshot(part_x: f64, trace_end: f64) -> BoardSnapshot {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(
            &path,
            format!(
                r#"(kicad_pcb
 (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (44 "Edge.Cuts" user))
 (net 0 "") (net 1 "SIG")
 (gr_rect (start 0 0) (end 10 10) (layer "Edge.Cuts"))
 (footprint "Test:Pad" (layer "F.Cu") (at {part_x} 5)
   (property "Reference" "R1")
   (fp_rect (start -1 -1) (end 1 1) (layer "F.CrtYd"))
   (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "SIG")))
 (segment (start 2 2) (end {trace_end} 2) (width 0.2) (layer "F.Cu") (net 1)))"#,
            ),
        )
        .unwrap();
        kicad_board::read_snapshot(&path).unwrap()
    }

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
            outside_outline: BTreeMap::new(),
            copper_outside_outline: BTreeSet::new(),
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

    #[test]
    fn outline_containment_names_courtyards_and_copper_separately() {
        assert!(outline_containment(&containment_snapshot(5.0, 8.0)).is_clear());

        let outside = outline_containment(&containment_snapshot(9.5, 9.8));
        assert_eq!(outside.outside_outline, BTreeSet::from(["R1".to_owned()]));
        assert!(outside.copper_outside_outline >= 2, "{outside:?}");
    }

    #[test]
    fn newly_outside_geometry_is_a_guard_defect() {
        let before = defects(&[], &[]);
        let mut after = defects(&[], &[]);
        after
            .outside_outline
            .insert("R1:moved".to_owned(), "R1".to_owned());
        after
            .copper_outside_outline
            .insert("via:old-outline".to_owned());

        let added = introduced(&before, &after, &[]);
        assert_eq!(added.outside_outline, [&"R1".to_owned()]);
        assert_eq!(
            added.copper_outside_outline,
            [&"via:old-outline".to_owned()]
        );

        let mut before = defects(&[], &[]);
        before
            .copper_outside_outline
            .insert("trace:before".to_owned());
        let mut after = defects(&[], &[]);
        after
            .copper_outside_outline
            .insert("trace:after".to_owned());
        assert_eq!(
            introduced(&before, &after, &[]).copper_outside_outline,
            [&"trace:after".to_owned()],
            "an equal-count replacement is still newly outside geometry"
        );

        let mut before = defects(&[], &[]);
        before
            .outside_outline
            .insert("R1:before".to_owned(), "R1".to_owned());
        let mut after = defects(&[], &[]);
        after
            .outside_outline
            .insert("R1:after".to_owned(), "R1".to_owned());
        assert_eq!(
            introduced(&before, &after, &[]).outside_outline,
            [&"R1".to_owned()],
            "moving the same reference farther out is still a new defect"
        );
    }

    #[test]
    fn a_concave_outline_notch_cannot_cross_a_rectangle() {
        let outline = Polygon::new(vec![
            Point2::new(0.0, 0.0),
            Point2::new(10.0, 0.0),
            Point2::new(10.0, 10.0),
            Point2::new(6.0, 10.0),
            Point2::new(6.0, 4.0),
            Point2::new(4.0, 4.0),
            Point2::new(4.0, 10.0),
            Point2::new(0.0, 10.0),
        ])
        .unwrap();
        let across_notch = geom::Rect::new(3.0, 3.0, 7.0, 5.0);

        assert!(
            [
                Point2::new(3.0, 3.0),
                Point2::new(7.0, 3.0),
                Point2::new(7.0, 5.0),
                Point2::new(3.0, 5.0),
            ]
            .into_iter()
            .all(|corner| outline.contains_point(corner)),
            "the regression requires all four corners to look valid"
        );
        assert!(!rect_inside_outline(across_notch, &outline));

        let diagonal = Polygon::new(vec![
            Point2::new(-10.0, -10.0),
            Point2::new(20.0, -10.0),
            Point2::new(20.0, 20.0),
            Point2::new(-10.0, 20.0),
            Point2::new(-10.0, 6.0),
            Point2::new(11.0, 5.0),
            Point2::new(-10.0, 4.0),
        ])
        .unwrap();
        assert!(!rect_inside_outline(
            geom::Rect::new(0.0, 0.0, 10.0, 10.0),
            &diagonal,
        ));
    }
}
