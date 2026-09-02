//! Saved-board validation through KiCad 10 DRC.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use kicad::Violation;

use gordian_runtime::AgentRuntime;

use crate::board::guard::{Edit, Guard};

/// DRC findings KiCAD raises that are independent of routed copper.
const NON_COPPER_WARNINGS: &[&str] = &[
    "lib_footprint_mismatch",
    "lib_footprint_issues",
    "silk_over_copper",
    "silk_overlap",
    "silk_edge_clearance",
    "silk_over_silk",
];

pub(super) fn is_non_copper(v: &Violation) -> bool {
    v.severity == "warning" && NON_COPPER_WARNINGS.contains(&v.kind.as_str())
}

pub(super) fn is_zone_self_unconnected(v: &Violation) -> bool {
    if v.kind != "unconnected_items" || v.items.len() < 2 {
        return false;
    }
    let Some(first) = v.items.first() else {
        return false;
    };
    first.description.starts_with("Zone [")
        && v.items.iter().all(|item| {
            item.description == first.description && item.uuid.as_deref() == first.uuid.as_deref()
        })
}

/// A badly broken board can have hundreds of unconnected items; enough of them
/// to act on is enough.
const MAX_UNCONNECTED_PAIRS: usize = 20;

pub(super) fn violation_summaries<'a>(
    violations: impl IntoIterator<Item = &'a Violation>,
    limit: usize,
) -> Vec<Value> {
    violations
        .into_iter()
        .take(limit)
        .map(|v| {
            json!({
                "type": v.kind,
                "severity": v.severity,
                "description": v.description,
                "items": v.items.iter().take(3).map(|i| i.description.clone()).collect::<Vec<_>>(),
            })
        })
        .collect()
}

#[derive(Clone, Copy)]
struct ClassifiedViolation<'a> {
    classification: &'static str,
    violation: &'a Violation,
}

impl ClassifiedViolation<'_> {
    /// Whether this finding is the board's to answer for. A finding that only
    /// names parts still in the staging row is not: they are not part of the
    /// board yet, and DRC has no rule for "never laid out".
    fn blocks(&self) -> bool {
        self.classification != "staged"
    }
}

/// A finding kind that is settled purely by placing the part it names — copper
/// that has nowhere to go yet, or a courtyard sitting in the staging row.
fn is_settled_by_placing(kind: &str) -> bool {
    matches!(
        kind,
        "unconnected_items" | "courtyards_overlap" | "footprint_type_mismatch"
    )
}

/// Re-classify the findings a staged part is answerable for, so the DRC verdict
/// is about the board being built and not about the row waiting to join it.
///
/// For a finding that placing the part settles — an unrouted pair, two
/// courtyards in the staging row — naming ONE staged part is enough. A copper
/// defect is different: a clearance fault or a short between a placed track and
/// a staged pad is real copper on the placed board, and it is excused only when
/// every part it names is staged. `referenced_parts` is a prose heuristic, so
/// erring toward keeping copper defects in the verdict is the safe direction.
///
/// The list is re-sorted afterwards: what the caller must act on leads, then
/// what the board arrived with, then the staging row it has not reached yet.
fn excuse_staged<'a>(
    findings: &mut Vec<ClassifiedViolation<'a>>,
    staged: &std::collections::BTreeSet<String>,
) {
    if staged.is_empty() {
        return;
    }
    for finding in findings.iter_mut() {
        let (kind, refs, _) = violation_key(finding.violation);
        let excused = if is_settled_by_placing(&kind) {
            refs.iter().any(|reference| staged.contains(reference))
        } else {
            !refs.is_empty() && refs.iter().all(|reference| staged.contains(reference))
        };
        if excused {
            finding.classification = "staged";
        }
    }
    findings.sort_by_key(|finding| match finding.classification {
        "introduced" => 0,
        "pre_existing" => 1,
        _ => 2,
    });
}

fn bracketed_names(text: &str) -> impl Iterator<Item = &str> {
    text.split('[')
        .skip(1)
        .filter_map(|tail| tail.split_once(']').map(|(name, _)| name))
}

fn referenced_parts(text: &str) -> Vec<&str> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    words
        .windows(2)
        .filter_map(|pair| {
            let marker = pair[0].trim_matches(|character: char| !character.is_alphanumeric());
            if !matches!(marker, "of" | "Footprint") {
                return None;
            }
            let candidate = pair[1]
                .trim_matches(|character: char| !character.is_alphanumeric() && character != '_');
            let digit = candidate
                .chars()
                .any(|character| character.is_ascii_digit());
            let alpha = candidate
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic());
            (digit && alpha).then_some(candidate)
        })
        .collect()
}

fn violation_key(violation: &Violation) -> (String, Vec<String>, Vec<String>) {
    let texts = std::iter::once(violation.description.as_str())
        .chain(violation.items.iter().map(|item| item.description.as_str()));
    let mut refs = BTreeSet::new();
    let mut nets = BTreeSet::new();
    for text in texts {
        refs.extend(referenced_parts(text).into_iter().map(str::to_owned));
        nets.extend(bracketed_names(text).map(str::to_owned));
    }
    (
        violation.kind.clone(),
        refs.into_iter().collect(),
        nets.into_iter().collect(),
    )
}

fn classify_violations<'a>(
    current: &'a [Violation],
    baseline: &[Violation],
) -> Vec<ClassifiedViolation<'a>> {
    let mut available = BTreeMap::new();
    for violation in baseline {
        *available.entry(violation_key(violation)).or_insert(0usize) += 1;
    }
    let mut classified = current
        .iter()
        .map(|violation| {
            let count = available.entry(violation_key(violation)).or_default();
            let classification = if *count > 0 {
                *count -= 1;
                "pre_existing"
            } else {
                "introduced"
            };
            ClassifiedViolation {
                classification,
                violation,
            }
        })
        .collect::<Vec<_>>();
    classified.sort_by_key(|finding| finding.classification != "introduced");
    classified
}

fn classified_finding(finding: ClassifiedViolation<'_>) -> Value {
    let (_, refs, nets) = violation_key(finding.violation);
    json!({
        "classification": finding.classification,
        "code": finding.violation.kind,
        "type": finding.violation.kind,
        "severity": finding.violation.severity,
        "description": finding.violation.description,
        "refs": refs,
        "nets": nets,
        "items": finding.violation.items.iter().map(|item| item.description.clone()).collect::<Vec<_>>(),
    })
}

fn classified_line(finding: ClassifiedViolation<'_>) -> String {
    let (_, refs, nets) = violation_key(finding.violation);
    let subjects = refs.into_iter().chain(nets).collect::<Vec<_>>().join(", ");
    let subjects = if subjects.is_empty() {
        String::new()
    } else {
        format!(" {subjects}")
    };
    format!(
        "{} {}[{}]{}: {}",
        finding.classification,
        finding.violation.severity,
        finding.violation.kind,
        subjects,
        finding.violation.description
    )
}

fn classified_summaries<'a>(
    findings: impl IntoIterator<Item = &'a ClassifiedViolation<'a>>,
    limit: usize,
) -> Vec<Value> {
    findings
        .into_iter()
        .take(limit)
        .copied()
        .map(classified_finding)
        .collect()
}

fn classified_unconnected(
    parts: &[kicad_board::ImportedPart],
    findings: &[ClassifiedViolation<'_>],
) -> Vec<Value> {
    findings
        .iter()
        .take(MAX_UNCONNECTED_PAIRS)
        .filter_map(|finding| {
            let mut pair = crate::diagnose::unconnected_pair(parts, finding.violation)?;
            pair["classification"] = json!(finding.classification);
            Some(pair)
        })
        .collect()
}

pub(super) struct DrcGate {
    pub copper_violations: usize,
    pub meaningful_unconnected: usize,
    pub ignored_zone_self_unconnected: usize,
}

/// Apply the same production DRC policy to project and corpus boards.
/// Library/silkscreen warnings and KiCad's zone-self artifacts do not
/// describe routed-copper correctness; every other finding blocks the gate.
pub(super) fn gate_drc(report: &kicad::DrcReport) -> DrcGate {
    let copper_violations = report
        .violations
        .iter()
        .filter(|v| !is_non_copper(v))
        .count();
    let meaningful_unconnected = report
        .unconnected_items
        .iter()
        .filter(|v| !is_zone_self_unconnected(v))
        .count();
    DrcGate {
        copper_violations,
        meaningful_unconnected,
        ignored_zone_self_unconnected: report
            .unconnected_items
            .len()
            .saturating_sub(meaningful_unconnected),
    }
}

/// Recompute generated copper zones through KiCad 10 before a DRC decision.
pub(crate) fn materialize_zones_for_drc(
    path: &Path,
    env: &kicad::KicadInstallation,
) -> std::result::Result<bool, String> {
    let has_zones = std::fs::read_to_string(path)
        .map(|text| text.contains("\n\t(zone") || text.contains("\n  (zone"))
        .map_err(|e| format!("could not inspect routed board: {e}"))?;
    if !has_zones {
        return Ok(false);
    }
    env.refill_zones(path, false)
        .map_err(|e| format!("kicad-cli pcb drc --refill-zones failed: {e}"))?;
    Ok(true)
}

/// Refill every copper zone in the saved board and persist KiCad's fill cache.
#[tracing::instrument(skip_all, fields(project = %ctx.project_dir().display()))]
pub fn refill_zones(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let path = ctx.pcb_path();
    let gate = match Guard::open(
        ctx,
        Edit::new(
            "refill_zones",
            "Refill board copper zones",
            std::slice::from_ref(&path),
        )
        .expecting(&input),
    ) {
        Ok(gate) => gate,
        Err(refusal) => return Ok(refusal),
    };
    let has_zones = std::fs::read_to_string(&path)
        .map(|text| text.contains("\n\t(zone") || text.contains("\n  (zone"))
        .unwrap_or(false);
    let result = if has_zones {
        ctx.env()
            .refill_zones(&path, true)
            .map(|_| true)
            .map_err(|error| {
                format!("kicad-cli pcb drc --refill-zones --save-board failed: {error}")
            })
    } else {
        Ok(false)
    };
    match result {
        Ok(refilled) => Ok(gate.commit(
            ctx,
            json!({
                "ok": true,
                "refilled": refilled,
                "path": path.display().to_string(),
                "note": if refilled {
                    "KiCad refilled and saved every board zone."
                } else {
                    "The board has no copper zones to refill."
                },
            }),
        )),
        Err(error) => Ok(gate.rollback(
            ctx,
            json!({
                "error": error,
                "next": "Fix the reported board error, then run refill_zones again."
            }),
        )),
    }
}

fn isolated_copper<'a>(
    parts: &[kicad_board::ImportedPart],
    violations: impl IntoIterator<Item = &'a Violation>,
) -> Vec<Value> {
    violations
        .into_iter()
        .filter_map(|violation| {
            let nets = violation
                .items
                .iter()
                .flat_map(|item| bracketed_names(&item.description))
                .collect::<BTreeSet<_>>();
            if nets.len() != 1 {
                return None;
            }
            let net = *nets.iter().next()?;
            let mut pads = Vec::new();
            let mut pad_at = None;
            for item in &violation.items {
                let Some((pad, pad_net, at)) =
                    crate::diagnose::pad_handle(parts, &item.description)
                else {
                    continue;
                };
                if pad_net == net {
                    pads.push(pad);
                    pad_at.get_or_insert(at);
                }
            }
            pads.sort();
            pads.dedup();
            let at = violation
                .items
                .iter()
                .find_map(|item| item.pos.map(|pos| [pos.x, pos.y]))
                .or_else(|| pad_at.map(|at| [at.x, at.y]));
            Some(json!({ "net": net, "at": at, "pads": pads }))
        })
        .collect()
}

/// Run DRC and classify findings against this turn's first board snapshot.
///
/// `ok` and `drc_clean` consider introduced blocking findings only; inherited
/// findings do not block completion of an otherwise clean focused edit.
#[tracing::instrument(skip_all, fields(project = %ctx.project_dir().display()))]
pub fn check_board(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let path = ctx.pcb_path();
    if !path.exists() {
        return Ok(json!({ "error": "no board exists yet — run sync_board first" }));
    }
    let mut note_prefix = "Ran KiCad 10 DRC on the saved board file. ";
    match materialize_zones_for_drc(&path, ctx.env()) {
        Ok(true) => note_prefix = "Recomputed zones with KiCad 10, then ran DRC. ",
        Ok(false) => {}
        Err(e) => return Ok(json!({ "error": e })),
    }
    let cli = ctx.env();
    let initial_report = match cli.drc(&path) {
        Ok(report) => report,
        Err(e) => return Ok(json!({ "error": format!("kicad-cli pcb drc failed: {e}") })),
    };
    let initial_silk_warnings = super::silk::silk_warning_count(&initial_report);
    let mut silk_cleanup_attempts = 0usize;
    let mut silk_references_moved = Vec::new();
    let mut silk_cleanup_error = None;
    let report = if initial_silk_warnings > 0 {
        match super::silk::cleanup_silk_text(&path, cli, initial_report.clone()) {
            Ok(cleanup) => {
                silk_cleanup_attempts = cleanup.attempts;
                silk_references_moved = cleanup.moved_references;
                debug_assert_eq!(cleanup.initial_warnings, initial_silk_warnings);
                debug_assert_eq!(
                    cleanup.remaining_warnings,
                    super::silk::silk_warning_count(&cleanup.report)
                );
                cleanup.report
            }
            Err(err) => {
                silk_cleanup_error = Some(err);
                initial_report
            }
        }
    } else {
        initial_report
    };
    let silk_warnings = super::silk::silk_warning_count(&report);
    let gate = gate_drc(&report);
    let baseline = ctx.revisions().turn_baseline(&path)?;
    let (baseline_report, baseline_error) = match baseline
        .as_ref()
        .and_then(|baseline| baseline.path.as_deref())
        .map(|baseline_path| cli.drc(baseline_path))
        .transpose()
    {
        Ok(report) => (report, None),
        Err(error) => (None, Some(format!("running baseline DRC: {error}"))),
    };
    let baseline_violations = baseline_report
        .as_ref()
        .map(|report| report.violations.as_slice())
        .unwrap_or_default();
    let baseline_unconnected = baseline_report
        .as_ref()
        .map(|report| report.unconnected_items.as_slice())
        .unwrap_or_default();
    let mut violations = classify_violations(&report.violations, baseline_violations);
    let mut unconnected_findings =
        classify_violations(&report.unconnected_items, baseline_unconnected);
    let board = match crate::active_board(ctx) {
        Ok(board) => board,
        Err(error) => {
            return Ok(json!({
                "ok": false,
                "drc_clean": false,
                "error": format!("could not verify board outline containment: {error}"),
                "code": "outline_containment_unavailable",
                "blocking_findings": 1,
            }));
        }
    };
    // A part still in the staging row is progress outstanding, not a defect:
    // excuse its findings before anything counts them.
    let state = crate::staging::BoardState::of(&board);
    let staged_refs: std::collections::BTreeSet<String> = state
        .staged
        .iter()
        .map(|part| part.reference.clone())
        .collect();
    excuse_staged(&mut violations, &staged_refs);
    excuse_staged(&mut unconnected_findings, &staged_refs);
    let violations = violations;
    let unconnected_findings = unconnected_findings;
    let meaningful_unconnected = unconnected_findings
        .iter()
        .filter(|finding| !is_zone_self_unconnected(finding.violation))
        .copied()
        .collect::<Vec<_>>();
    let containment = crate::board::guard::outline_containment(&board);
    let outline_blocking =
        containment.outside_outline.len() + usize::from(containment.copper_outside_outline > 0);
    let introduced_copper = violations
        .iter()
        .filter(|finding| {
            finding.classification == "introduced" && !is_non_copper(finding.violation)
        })
        .count();
    let introduced_unconnected = meaningful_unconnected
        .iter()
        .filter(|finding| finding.classification == "introduced")
        .count();
    // Staged parts are outside the verdict, so they are outside every count of
    // it too: the gate's raw totals are reduced by what was excused.
    let staged_copper = violations
        .iter()
        .filter(|finding| !finding.blocks() && !is_non_copper(finding.violation))
        .count();
    let staged_unconnected = meaningful_unconnected
        .iter()
        .filter(|finding| !finding.blocks())
        .count();
    let copper_violations = gate.copper_violations - staged_copper;
    let unconnected_items = gate.meaningful_unconnected - staged_unconnected;
    let pre_existing_copper = copper_violations - introduced_copper;
    let pre_existing_unconnected = unconnected_items - introduced_unconnected;
    let blocking_findings = introduced_copper + introduced_unconnected + outline_blocking;
    let blocking_findings_absolute = copper_violations + unconnected_items + outline_blocking;
    let introduced = violations
        .iter()
        .chain(&unconnected_findings)
        .filter(|finding| finding.classification == "introduced")
        .count();
    let reported_findings =
        report.violations.len() + report.unconnected_items.len() + outline_blocking;
    let pre_existing = reported_findings - introduced;
    let introduced_silk_warnings = violations
        .iter()
        .filter(|finding| {
            finding.classification == "introduced"
                && finding.violation.severity == "warning"
                && matches!(
                    finding.violation.kind.as_str(),
                    "silk_over_copper" | "silk_overlap" | "silk_edge_clearance" | "silk_over_silk"
                )
        })
        .count();
    // The board's own part list is what turns KiCAD's prose into pad handles and
    // says which footprints are still in the seed row. Reading it reopens the
    // session this function closed; DRC stands either way, so it is best-effort.
    // A part left in the seed row is not a DRC finding — KiCAD has no rule for
    // "never laid out" — but it is exactly what the next `place_board({refs})`
    // call must name, so the completion signal has to say it.
    let ratsnest = crate::ratsnest::build(&board, &board.problem, &[], None);
    let blocked = ratsnest.blocked();
    let staged = state.staged_json();
    let staged_refs_list = state.staged_references();
    let unconnected = if meaningful_unconnected.is_empty() {
        Vec::new()
    } else {
        classified_unconnected(&board.imported.parts, &meaningful_unconnected)
    };
    let islands = isolated_copper(
        &board.imported.parts,
        meaningful_unconnected
            .iter()
            .map(|finding| finding.violation),
    );
    let mut findings = violations
        .iter()
        .chain(&unconnected_findings)
        .copied()
        .map(classified_finding)
        .collect::<Vec<_>>();
    if !containment.outside_outline.is_empty() {
        findings.push(json!({
            "classification": "absolute",
            "code": "outside_outline",
            "type": "outside_outline",
            "severity": "error",
            "description": "footprint courtyards cross the physical board outline",
            "refs": containment.outside_outline,
            "nets": [],
            "items": [],
        }));
    }
    if containment.copper_outside_outline > 0 {
        findings.push(json!({
            "classification": "absolute",
            "code": "copper_outside_outline",
            "type": "copper_outside_outline",
            "severity": "error",
            "description": format!(
                "{} copper item(s) cross or do not clear the physical board outline",
                containment.copper_outside_outline,
            ),
            "refs": [],
            "nets": [],
            "items": [],
        }));
    }
    let mut diagnostics = vec![format!(
        "{introduced} introduced, {pre_existing} pre-existing"
    )];
    diagnostics.extend(
        violations
            .iter()
            .chain(&unconnected_findings)
            .take(10)
            .copied()
            .map(classified_line),
    );
    if !containment.outside_outline.is_empty() {
        diagnostics.push(format!(
            "blocking error[outside_outline] {}",
            containment
                .outside_outline
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if containment.copper_outside_outline > 0 {
        diagnostics.push(format!(
            "blocking error[copper_outside_outline] {} copper item(s)",
            containment.copper_outside_outline
        ));
    }
    let note = if blocking_findings == 0 {
        format!(
            "{note_prefix}Routed {}/{}; {} part(s) staged. No introduced blocking DRC \
             findings; {pre_existing} pre-existing finding(s) and {silk_warnings} absolute \
             silkscreen warning(s) remain.",
            ratsnest.routed,
            ratsnest.total,
            staged_refs_list.len(),
        )
    } else {
        format!(
            "{note_prefix}Board checks reported {blocking_findings} blocking finding(s); inspect the introduced DRC findings and absolute outline containment first."
        )
    };
    let next = if !staged_refs_list.is_empty() {
        format!(
            "{} footprint(s) are still staged: call place_board({{\"refs\": {}}}) \
             to lay them out, then route_board, then check_board again.",
            staged_refs_list.len(),
            serde_json::to_string(&staged_refs_list).unwrap_or_else(|_| "[]".to_owned()),
        )
    } else if blocking_findings == 0 {
        "export_fab".to_owned()
    } else {
        "Fix the introduced blocking violations/unconnected items, then call check_board again. Leave pre-existing findings alone and do not resync blindly.".to_owned()
    };
    let text = diagnostics.join("\n");
    // The DRC detail and the silkscreen pass each get their own object: the
    // board's verdict and its progress stay at the top level, where a caller
    // reads them, and no single `json!` grows past what the macro can expand.
    let drc = json!({
        "baseline_revision": baseline.as_ref().map(|baseline| baseline.revision),
        "baseline_error": baseline_error,
        "introduced": introduced,
        "pre_existing": pre_existing,
        "blocking_findings_absolute": blocking_findings_absolute,
        "reported_findings": reported_findings,
        "violations": report.violations.len(),
        "copper_violations": copper_violations,
        "introduced_copper_violations": introduced_copper,
        "pre_existing_copper_violations": pre_existing_copper,
        "unconnected_items": unconnected_items,
        "introduced_unconnected_items": introduced_unconnected,
        "pre_existing_unconnected_items": pre_existing_unconnected,
        "ignored_zone_self_unconnected": gate.ignored_zone_self_unconnected,
        "outside_outline": containment.outside_outline,
        "copper_outside_outline": containment.copper_outside_outline > 0,
        "top_violations": classified_summaries(
            violations.iter().filter(|finding| !is_non_copper(finding.violation)),
            5,
        ),
        "top_unconnected": classified_summaries(meaningful_unconnected.iter(), 5),
    });
    let silk = json!({
        "warnings": silk_warnings,
        "introduced_warnings": introduced_silk_warnings,
        "warnings_fixed": initial_silk_warnings.saturating_sub(silk_warnings),
        "cleanup_attempts": silk_cleanup_attempts,
        "references_moved": silk_references_moved,
        "cleanup_error": silk_cleanup_error,
        "top_violations": classified_summaries(
            violations.iter().filter(|finding| {
                finding.violation.severity == "warning" && matches!(finding.violation.kind.as_str(),
                    "silk_over_copper" | "silk_overlap" | "silk_edge_clearance" | "silk_over_silk")
            }),
            5,
        ),
    });
    Ok(json!({
        "ok": blocking_findings == 0,
        "drc_clean": blocking_findings == 0,
        "path": path.display().to_string(),
        "blocking_findings": blocking_findings,
        // Progress, not pass/fail: how much of the board is routed, what is
        // standing in the way of the rest, and who is still in the staging row.
        "routed": format!("{}/{}", ratsnest.routed, ratsnest.total),
        "routed_connection_count": ratsnest.routed,
        "total_connection_count": ratsnest.total,
        "blocked": blocked,
        "staged": staged,
        "staged_count": staged_refs_list.len(),
        "drc": drc,
        "silk": silk,
        "findings": findings,
        "diagnostics": diagnostics,
        "text": text,
        // Never a bare count: the two pads that should be joined are what say
        // which route_track / route_board{nets} call repairs the board.
        "unconnected": unconnected,
        "islands": islands,
        "note": note,
        "next": next,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{LayerRef, Point2};

    fn violation(code: &str, reference: &str, net: &str) -> Violation {
        Violation {
            severity: "warning".to_owned(),
            kind: code.to_owned(),
            description: "finding".to_owned(),
            items: vec![kicad::ViolationItem {
                description: format!("Pad 1 [{net}] of {reference} on F.Cu"),
                uuid: None,
                pos: None,
            }],
        }
    }

    #[test]
    fn board_edit_classifies_one_introduced_and_two_pre_existing_findings() {
        let baseline = vec![
            violation("clearance", "R1", "A"),
            violation("clearance", "R2", "B"),
        ];
        let current = vec![
            violation("clearance", "R1", "A"),
            violation("clearance", "R2", "B"),
            violation("clearance", "R3", "C"),
        ];

        let classified = classify_violations(&current, &baseline);

        assert_eq!(
            classified
                .iter()
                .filter(|finding| finding.classification == "introduced")
                .count(),
            1
        );
        assert_eq!(
            classified
                .iter()
                .filter(|finding| finding.classification == "pre_existing")
                .count(),
            2
        );
    }

    #[test]
    fn board_undo_to_baseline_has_no_introduced_findings() {
        let baseline = vec![
            violation("clearance", "R1", "A"),
            violation("clearance", "R2", "B"),
        ];
        let restored = vec![
            violation("clearance", "R1", "A"),
            violation("clearance", "R2", "B"),
        ];

        let classified = classify_violations(&restored, &baseline);

        assert!(
            classified
                .iter()
                .all(|finding| finding.classification == "pre_existing")
        );
    }

    #[test]
    fn fresh_board_without_baseline_has_only_introduced_findings() {
        let current = vec![
            violation("clearance", "R1", "A"),
            violation("clearance", "R2", "B"),
        ];

        let classified = classify_violations(&current, &[]);

        assert!(
            classified
                .iter()
                .all(|finding| finding.classification == "introduced")
        );
    }

    #[test]
    fn isolated_copper_reports_only_same_net_items_and_pads() {
        let parts = vec![kicad_board::ImportedPart {
            reference: "U1".to_owned(),
            lib_id: "Package:Test".to_owned(),
            at: Point2::new(4.0, 5.0),
            rotation: 0,
            side: kicad_board::BoardSide::Front,
            locked: false,
            courtyard: None,
            pads: vec![kicad_board::ImportedPad {
                number: "1".to_owned(),
                net: Some("GND".to_owned()),
                at: Point2::new(4.0, 5.0),
                layers: vec![LayerRef::top()],
                shape: "rect".to_owned(),
                size: Point2::new(1.0, 1.0),
                drill: None,
            }],
            properties: Default::default(),
        }];
        let same_net = Violation {
            severity: "error".to_owned(),
            kind: "unconnected_items".to_owned(),
            description: "Missing connection between items".to_owned(),
            items: vec![
                kicad::ViolationItem {
                    description: "Via [GND] on F.Cu - B.Cu".to_owned(),
                    uuid: None,
                    pos: None,
                },
                kicad::ViolationItem {
                    description: "Pad 1 [GND] of U1 on F.Cu".to_owned(),
                    uuid: None,
                    pos: None,
                },
            ],
        };
        let cross_net = Violation {
            items: vec![
                kicad::ViolationItem {
                    description: "Via [GND] on F.Cu - B.Cu".to_owned(),
                    uuid: None,
                    pos: None,
                },
                kicad::ViolationItem {
                    description: "Via [/OP2_OUT] on F.Cu - B.Cu".to_owned(),
                    uuid: None,
                    pos: None,
                },
            ],
            ..same_net.clone()
        };

        let islands = isolated_copper(&parts, [&same_net, &cross_net]);

        assert_eq!(islands.len(), 1);
        assert_eq!(islands[0]["net"], "GND");
        assert_eq!(islands[0]["at"], json!([4.0, 5.0]));
        assert_eq!(islands[0]["pads"], json!(["U1.1"]));
    }
}
