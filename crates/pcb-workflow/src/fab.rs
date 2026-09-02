//! `export_fab` — turn a routed `.kicad_pcb` into a manufacturable deliverable:
//! Gerbers + Excellon drill + pick-and-place + (when a schematic is present) a
//! BOM, all under a single `fab/` directory the user can hand to a board house.
//!
//! This is the one-click bundle for the saved project board.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use gordian_runtime::AgentRuntime;

/// Run the routed board through the fabrication exporters into `<project>/fab/`
/// and report the produced files.
///
/// Precondition: the board must already exist and should have passed
/// `check_board`. The bundle always lands in `<project>/fab/`.
///
/// Produces, all keyed off the board: Gerbers (one `*.gbr` per layer), a
/// separate-PTH/NPTH Excellon drill set with drill maps, and a CSV
/// pick-and-place. When the project's `.kicad_sch` exists a grouped BOM CSV is
/// added; otherwise BOM is skipped with a note (the PCB carries no part values).
/// Returns the bundle directory and the full produced-file list. Individual
/// exporter failures are surfaced as a recoverable `{error}` value, not `Err`.
pub fn export_fab(_input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let board = ctx.pcb_path();
    if !board.is_file() {
        return Ok(json!({
            "error": format!(
                "no routed board at {} — run sync_board → place_board → route_board → check_board first, \
                 then export_fab (it bundles the saved .kicad_pcb)",
                board.display()
            ),
        }));
    }

    let cli = ctx.env();
    let drc = match cli.refill_zones(&board, true) {
        Ok(report) => report,
        Err(e) => {
            return Ok(
                json!({ "error": format!("kicad-cli pcb drc --refill-zones --save-board failed before fab export: {e}") }),
            );
        }
    };
    let copper_violations = drc
        .violations
        .iter()
        .filter(|v| !super::export::is_non_copper(v))
        .count();
    let meaningful_unconnected: Vec<_> = drc
        .unconnected_items
        .iter()
        .filter(|v| !super::export::is_zone_self_unconnected(v))
        .collect();
    if copper_violations > 0 || !meaningful_unconnected.is_empty() {
        return Ok(json!({
            "ok": false,
            "error": "PCB DRC is not clean; fix copper violations/unconnected items before export_fab",
            "copper_violations": copper_violations,
            "unconnected_items": meaningful_unconnected.len(),
            "ignored_zone_self_unconnected": drc.unconnected_items.len().saturating_sub(meaningful_unconnected.len()),
            "top_violations": super::export::violation_summaries(
                drc.violations.iter().filter(|v| !super::export::is_non_copper(v)),
                5,
            ),
            "top_unconnected": super::export::violation_summaries(meaningful_unconnected, 5),
        }));
    }

    let out_dir = ctx.project_dir().join("fab");
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        return Ok(json!({
            "error": format!("could not create fab dir {}: {e}", out_dir.display()),
        }));
    }

    let stem = board
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board");

    // Each exporter is independent; a failure of one (e.g. a board with no
    // through-holes still yields a drill file) is reported per-artifact rather
    // than aborting the bundle.
    let mut files: Vec<PathBuf> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    match cli.export_gerbers(&board, &out_dir) {
        Ok(mut gerbers) => files.append(&mut gerbers),
        Err(e) => errors.push(format!("gerbers: {e}")),
    }
    match cli.export_drill(&board, &out_dir) {
        Ok(mut drills) => files.append(&mut drills),
        Err(e) => errors.push(format!("drill: {e}")),
    }
    let pos = out_dir.join(format!("{stem}-pos.csv"));
    match cli.export_pos(&board, &pos) {
        Ok(p) => files.push(p),
        Err(e) => errors.push(format!("pos: {e}")),
    }

    // BOM is a SCHEMATIC export — only attempt it when the source `.kicad_sch`
    // exists (the board-only flow may have no schematic).
    let bom_note = if ctx.sch_path().is_file() {
        let bom = out_dir.join(format!("{stem}-bom.csv"));
        match cli.export_bom(ctx.sch_path(), &bom) {
            Ok(p) => {
                files.push(p);
                "BOM exported from the project schematic."
            }
            Err(e) => {
                errors.push(format!("bom: {e}"));
                "BOM export failed (see errors)."
            }
        }
    } else {
        "BOM skipped — no .kicad_sch in the project (the board carries no part values)."
    };

    files.sort();
    let names: Vec<String> = files.iter().filter_map(|p| file_name(p)).collect();
    let (verified_files, missing_files) = verify_files(&files);
    let all_files_exist = !files.is_empty() && missing_files.is_empty();
    for missing in &missing_files {
        errors.push(format!(
            "exporter reported a file that does not exist: {missing}"
        ));
    }

    let note = if files.is_empty() {
        "fab export produced no files — see errors.".to_string()
    } else {
        format!(
            "Fab bundle written to {} ({} files: Gerbers + Excellon drill + pick-and-place{}). \
             {bom_note} Hand the {} directory to a board house.",
            out_dir.display(),
            files.len(),
            if ctx.sch_path().is_file() {
                " + BOM"
            } else {
                ""
            },
            out_dir.display(),
        )
    };

    Ok(json!({
        "ok": errors.is_empty() && all_files_exist,
        "board": board.display().to_string(),
        "fab_dir": out_dir.display().to_string(),
        "files": names,
        "verified_files": verified_files,
        "all_files_exist": all_files_exist,
        "missing_files": missing_files,
        "file_count": files.len(),
        "errors": errors,
        "note": note,
    }))
}

fn verify_files(files: &[PathBuf]) -> (Vec<Value>, Vec<String>) {
    let verified = files
        .iter()
        .map(|path| {
            json!({
                "path": path.display().to_string(),
                "exists": path.is_file(),
            })
        })
        .collect::<Vec<_>>();
    let missing = files
        .iter()
        .filter(|path| !path.is_file())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    (verified, missing)
}

/// The file name of `p` as a `String`, or `None` if it has none.
fn file_name(p: &Path) -> Option<String> {
    p.file_name().and_then(|n| n.to_str()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fabrication_file_report_verifies_every_path() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("board-F_Cu.gbr");
        let missing = dir.path().join("board.drl");
        std::fs::write(&present, "gerber").unwrap();

        let (verified, absent) = verify_files(&[present, missing.clone()]);

        assert_eq!(verified[0]["exists"], true);
        assert_eq!(verified[1]["exists"], false);
        assert_eq!(absent, [missing.display().to_string()]);
    }
}
