//! Live-board validation through KiCAD DRC.

use anyhow::Result;
use serde_json::{Value, json};

use kicad_cli::{KicadCli, Violation};

use crate::AgentRuntime;

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

/// Save the active board and run KiCAD's PCB DRC against it.
pub fn check_board(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let path = ctx.pcb_path();
    if !path.exists() {
        return Ok(json!({ "error": "no board exists yet — run regenerate_board first" }));
    }
    let note_prefix = if ctx.kicad().is_open() {
        match super::active::save_live_board(ctx) {
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
    let cli = KicadCli::new(ctx.env());
    let report = match cli.drc(&path) {
        Ok(report) => report,
        Err(e) => return Ok(json!({ "error": format!("kicad-cli pcb drc failed: {e}") })),
    };
    let copper_violations = report
        .violations
        .iter()
        .filter(|v| !is_non_copper(v))
        .count();
    let meaningful_unconnected: Vec<_> = report
        .unconnected_items
        .iter()
        .filter(|v| !is_zone_self_unconnected(v))
        .collect();
    Ok(json!({
        "ok": copper_violations == 0 && meaningful_unconnected.is_empty(),
        "path": path.display().to_string(),
        "violations": report.violations.len(),
        "copper_violations": copper_violations,
        "unconnected_items": meaningful_unconnected.len(),
        "ignored_zone_self_unconnected": report.unconnected_items.len().saturating_sub(meaningful_unconnected.len()),
        "top_violations": violation_summaries(
            report.violations.iter().filter(|v| !is_non_copper(v)),
            5,
        ),
        "top_unconnected": violation_summaries(meaningful_unconnected, 5),
        "note": if copper_violations == 0 && report.unconnected_items.iter().all(is_zone_self_unconnected) {
            format!("{note_prefix}KiCAD DRC passed.")
        } else {
            format!("{note_prefix}KiCAD DRC reported issues; inspect violations/unconnected counts.")
        },
    }))
}
