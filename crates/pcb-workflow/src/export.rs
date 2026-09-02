//! Live-board validation through KiCAD DRC.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use kicad::Violation;

use gordian_runtime::AgentRuntime;

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
    let violations = findings
        .iter()
        .take(MAX_UNCONNECTED_PAIRS)
        .map(|finding| finding.violation)
        .collect::<Vec<_>>();
    let mut pairs = crate::diagnose::unconnected_pairs(parts, violations.iter().copied());
    for (pair, finding) in pairs.iter_mut().zip(findings) {
        pair["classification"] = json!(finding.classification);
    }
    pairs
}

pub(super) struct DrcGate {
    pub copper_violations: usize,
    pub meaningful_unconnected: usize,
    pub ignored_zone_self_unconnected: usize,
}

/// Apply the same production DRC policy to live boards and offline corpus
/// boards. Library/silkscreen warnings and KiCad's zone-self artifacts do not
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

/// Ensure generated copper zones have cached fills before a headless DRC run.
/// KiCad 10+ can refill through the CLI; KiCad 9 needs its board IPC API.
pub(super) fn materialize_zones_for_drc(
    path: &Path,
    env: &kicad::KicadInstallation,
    sessions: &kicad_ipc::SessionManager,
    attach_running: bool,
) -> std::result::Result<bool, String> {
    let has_zones = std::fs::read_to_string(path)
        .map(|text| text.contains("\n\t(zone") || text.contains("\n  (zone"))
        .map_err(|e| format!("could not inspect routed board: {e}"))?;
    if !has_zones {
        return Ok(false);
    }
    let major = env
        .version()
        .split('.')
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    if major >= 10 {
        // drc uses `--refill-zones` when this version supports it.
        return Ok(false);
    }
    // KiCad 9 has no CLI refill operation, and DRC on unfilled zones reports
    // every stitching via as dangling. Refilling is the one explicit reason a
    // check may open a (headless, or attached when configured) pcbnew session.
    let _ = attach_running;
    if major < 9 {
        return Err(format!(
            "KiCad {} cannot refill generated zones headlessly; KiCad 9+ is required to DRC boards with zones",
            env.version()
        ));
    }
    sessions
        .with_session(path, |session| {
            session.kicad().refill_zones()?;
            session.kicad().save()
        })
        .map_err(|e| format!("could not refill board zones over KiCad IPC: {e}"))?;
    Ok(true)
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
    let mut note_prefix = if ctx.kicad().is_open() {
        match crate::save_active_board(ctx) {
            Ok(_) => "Saved the live KiCAD board, then ran DRC. ",
            Err(_) => {
                // A wedged live session must not block DRC: the offline write
                // paths keep the file current, so lint the file itself and drop
                // the session so the next tool reopens from disk.
                ctx.close_kicad_session();
                "Live KiCAD save failed; dropped the session and ran DRC on the board file. "
            }
        }
    } else {
        "No live KiCAD session was open; ran DRC directly on the board file. "
    };
    match materialize_zones_for_drc(
        &path,
        ctx.env(),
        ctx.kicad(),
        ctx.config().kicad.attach_running,
    ) {
        Ok(true) => note_prefix = "Refilled and saved the live KiCAD board zones, then ran DRC. ",
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
        // Cleanup edits the durable board between CLI DRC passes. Drop any live
        // editor session first so stale in-memory state cannot overwrite it.
        ctx.close_kicad_session();
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
    let violations = classify_violations(&report.violations, baseline_violations);
    let unconnected_findings = classify_violations(&report.unconnected_items, baseline_unconnected);
    let meaningful_unconnected = unconnected_findings
        .iter()
        .filter(|finding| !is_zone_self_unconnected(finding.violation))
        .copied()
        .collect::<Vec<_>>();
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
    let pre_existing_copper = gate.copper_violations - introduced_copper;
    let pre_existing_unconnected = gate.meaningful_unconnected - introduced_unconnected;
    let blocking_findings = introduced_copper + introduced_unconnected;
    let blocking_findings_absolute = gate.copper_violations + gate.meaningful_unconnected;
    let introduced = violations
        .iter()
        .chain(&unconnected_findings)
        .filter(|finding| finding.classification == "introduced")
        .count();
    let reported_findings = report.violations.len() + report.unconnected_items.len();
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
    let board = crate::active_board(ctx).ok();
    let unplaced = board
        .as_ref()
        .map(|board| kicad_board::seed_row_references(&board.imported))
        .unwrap_or_default();
    let unconnected = if meaningful_unconnected.is_empty() {
        Vec::new()
    } else {
        let parts = board.map(|board| board.imported.parts).unwrap_or_default();
        classified_unconnected(&parts, &meaningful_unconnected)
    };
    let findings = violations
        .iter()
        .chain(&unconnected_findings)
        .copied()
        .map(classified_finding)
        .collect::<Vec<_>>();
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
    let text = diagnostics.join("\n");
    Ok(json!({
        "ok": blocking_findings == 0,
        "drc_clean": blocking_findings == 0,
        "path": path.display().to_string(),
        "baseline_revision": baseline.as_ref().map(|baseline| baseline.revision),
        "baseline_error": baseline_error,
        "introduced": introduced,
        "pre_existing": pre_existing,
        "blocking_findings": blocking_findings,
        "blocking_findings_absolute": blocking_findings_absolute,
        "reported_findings": reported_findings,
        "silk_warnings": silk_warnings,
        "introduced_silk_warnings": introduced_silk_warnings,
        "silk_warnings_fixed": initial_silk_warnings.saturating_sub(silk_warnings),
        "silk_cleanup_attempts": silk_cleanup_attempts,
        "silk_references_moved": silk_references_moved,
        "silk_cleanup_error": silk_cleanup_error,
        "violations": report.violations.len(),
        "copper_violations": gate.copper_violations,
        "introduced_copper_violations": introduced_copper,
        "pre_existing_copper_violations": pre_existing_copper,
        "unconnected_items": gate.meaningful_unconnected,
        "introduced_unconnected_items": introduced_unconnected,
        "pre_existing_unconnected_items": pre_existing_unconnected,
        "ignored_zone_self_unconnected": gate.ignored_zone_self_unconnected,
        "findings": findings,
        "diagnostics": diagnostics,
        "text": text,
        "top_violations": classified_summaries(
            violations.iter().filter(|finding| !is_non_copper(finding.violation)),
            5,
        ),
        "top_silk_violations": classified_summaries(
            violations.iter().filter(|finding| {
                finding.violation.severity == "warning" && matches!(finding.violation.kind.as_str(),
                    "silk_over_copper" | "silk_overlap" | "silk_edge_clearance" | "silk_over_silk")
            }),
            5,
        ),
        "top_unconnected": classified_summaries(meaningful_unconnected.iter(), 5),
        // Never a bare count: the two pads that should be joined are what say
        // which route_track / route_board{nets} call repairs the board.
        "unconnected": unconnected,
        // Footprints still in the seed row: place them with
        // place_board({refs}) before routing expects copper to reach them.
        "unplaced": unplaced,
        "note": if blocking_findings == 0 {
            format!("{note_prefix}No introduced blocking DRC findings; {pre_existing} pre-existing finding(s) and {silk_warnings} absolute silkscreen warning(s) remain.")
        } else {
            format!("{note_prefix}KiCAD DRC reported {blocking_findings} introduced blocking finding(s); inspect the introduced violations/unconnected pairs first.")
        },
        "next": if !unplaced.is_empty() {
            format!(
                "{} footprint(s) are still in the seed row: call place_board({{\"refs\": {}}}) \
                 to lay them out, then route_board, then check_board again.",
                unplaced.len(),
                serde_json::to_string(&unplaced).unwrap_or_else(|_| "[]".to_owned()),
            )
        } else if blocking_findings == 0 {
            "DRC gate passed for this turn; finish the task and leave pre-existing findings alone unless asked. blocking_findings is the introduced gate; blocking_findings_absolute is informational.".to_owned()
        } else {
            "Fix the introduced blocking violations/unconnected items, then call check_board again. Leave pre-existing findings alone and do not resync blindly.".to_owned()
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
