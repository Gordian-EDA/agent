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
    let path = match super::active::save_live_board(ctx) {
        Ok(path) => path,
        Err(err) => return Ok(json!({ "error": err })),
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
    Ok(json!({
        "ok": copper_violations == 0 && report.unconnected_items.is_empty(),
        "path": path.display().to_string(),
        "violations": report.violations.len(),
        "copper_violations": copper_violations,
        "unconnected_items": report.unconnected_items.len(),
        "top_violations": violation_summaries(
            report.violations.iter().filter(|v| !is_non_copper(v)),
            5,
        ),
        "top_unconnected": violation_summaries(report.unconnected_items.iter(), 5),
        "note": if copper_violations == 0 && report.unconnected_items.is_empty() {
            "KiCAD DRC passed for the saved live board."
        } else {
            "KiCAD DRC reported issues; inspect violations/unconnected counts."
        },
    }))
}
