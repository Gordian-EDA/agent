//! Live-board validation through KiCAD DRC.

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

pub(super) struct DrcGate {
    pub copper_violations: usize,
    pub meaningful_unconnected: usize,
    pub ignored_zone_self_unconnected: usize,
}

impl DrcGate {
    pub fn is_ok(&self) -> bool {
        self.copper_violations == 0 && self.meaningful_unconnected == 0
    }
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

/// Save the active board and run KiCAD's PCB DRC against it.
pub fn check_board(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let path = ctx.pcb_path();
    if !path.exists() {
        return Ok(json!({ "error": "no board exists yet — run regenerate_board first" }));
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
    match materialize_zones_for_drc(&path, ctx.env(), ctx.kicad()) {
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
    let meaningful_unconnected: Vec<_> = report
        .unconnected_items
        .iter()
        .filter(|v| !is_zone_self_unconnected(v))
        .collect();
    let blocking_findings = gate.copper_violations + gate.meaningful_unconnected;
    let reported_findings = report.violations.len() + report.unconnected_items.len();
    Ok(json!({
        "ok": gate.is_ok(),
        "drc_clean": gate.is_ok(),
        "path": path.display().to_string(),
        "blocking_findings": blocking_findings,
        "reported_findings": reported_findings,
        "silk_warnings": silk_warnings,
        "silk_warnings_fixed": initial_silk_warnings.saturating_sub(silk_warnings),
        "silk_cleanup_attempts": silk_cleanup_attempts,
        "silk_references_moved": silk_references_moved,
        "silk_cleanup_error": silk_cleanup_error,
        "violations": report.violations.len(),
        "copper_violations": gate.copper_violations,
        "unconnected_items": gate.meaningful_unconnected,
        "ignored_zone_self_unconnected": gate.ignored_zone_self_unconnected,
        "top_violations": violation_summaries(
            report.violations.iter().filter(|v| !is_non_copper(v)),
            5,
        ),
        "top_silk_violations": violation_summaries(
            report.violations.iter().filter(|v| {
                v.severity == "warning" && matches!(v.kind.as_str(),
                    "silk_over_copper" | "silk_overlap" | "silk_edge_clearance" | "silk_over_silk")
            }),
            5,
        ),
        "top_unconnected": violation_summaries(meaningful_unconnected, 5),
        "note": if gate.is_ok() {
            format!("{note_prefix}KiCAD DRC passed; {silk_warnings} silkscreen warning(s) remain after bounded reference cleanup.")
        } else {
            format!("{note_prefix}KiCAD DRC reported issues; inspect violations/unconnected counts.")
        },
        "next": if gate.is_ok() {
            "DRC gate passed; finish the task. Do not regenerate, replace, or reroute this unchanged board. reported_findings may include tolerated non-copper warnings; blocking_findings is authoritative."
        } else {
            "Fix the top blocking violations/unconnected items, then call check_board again. Do not regenerate blindly."
        },
    }))
}
