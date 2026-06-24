//! `export_fab` — turn a routed `.kicad_pcb` into a manufacturable deliverable:
//! Gerbers + Excellon drill + pick-and-place + (when a schematic is present) a
//! BOM, all under a single `fab/` directory the user can hand to a board house.
//!
//! This is the one-click bundle on top of [`export_board`](super::export_board):
//! `export_board` writes (and DRC-checks) the `.kicad_pcb`; `export_fab` runs
//! that board through the [`kicad_cli`] fabrication wrappers. It is read-only
//! over the design (it only reads the exported board) and never mutates the
//! draft, so it is a `ReadOnly` tool.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use kicad_cli::cli::KicadCli;

use crate::tools::PcbToolCtx;

/// Run the routed board through the fabrication exporters into `<project>/fab/`
/// and report the produced files.
///
/// Precondition: the board must already be exported (and routed) — run
/// `export_board` first. We require the `.kicad_pcb` to exist rather than
/// re-synthesizing it, so the bundle always reflects the board the user has
/// inspected. Optional `path` overrides the input board; optional `out_dir`
/// overrides the output directory (default `<project>/fab/`).
///
/// Produces, all keyed off the board: Gerbers (one `*.gbr` per layer), a
/// separate-PTH/NPTH Excellon drill set with drill maps, and a CSV
/// pick-and-place. When the project's `.kicad_sch` exists a grouped BOM CSV is
/// added; otherwise BOM is skipped with a note (the PCB carries no part values).
/// Returns the bundle directory and the full produced-file list. Individual
/// exporter failures are surfaced as a recoverable `{error}` value, not `Err`.
pub fn export_fab(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    let board = match input.get("path").and_then(Value::as_str) {
        Some(p) => PathBuf::from(p),
        None => ctx.pcb_path(),
    };
    if !board.is_file() {
        return Ok(json!({
            "error": format!(
                "no routed board at {} — run place_board → route_board → export_board first, \
                 then export_fab (it bundles the exported .kicad_pcb)",
                board.display()
            ),
        }));
    }

    let out_dir = match input.get("out_dir").and_then(Value::as_str) {
        Some(d) => PathBuf::from(d),
        None => ctx.project_dir().join("fab"),
    };
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        return Ok(json!({
            "error": format!("could not create fab dir {}: {e}", out_dir.display()),
        }));
    }

    let cli = KicadCli::new(ctx.env());
    let stem = board.file_stem().and_then(|s| s.to_str()).unwrap_or("board");

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
    let names: Vec<String> = files
        .iter()
        .filter_map(|p| file_name(p))
        .collect();

    let note = if files.is_empty() {
        "fab export produced no files — see errors.".to_string()
    } else {
        format!(
            "Fab bundle written to {} ({} files: Gerbers + Excellon drill + pick-and-place{}). \
             {bom_note} Hand the {} directory to a board house.",
            out_dir.display(),
            files.len(),
            if ctx.sch_path().is_file() { " + BOM" } else { "" },
            out_dir.display(),
        )
    };

    Ok(json!({
        "ok": errors.is_empty() && !files.is_empty(),
        "board": board.display().to_string(),
        "fab_dir": out_dir.display().to_string(),
        "files": names,
        "file_count": files.len(),
        "errors": errors,
        "note": note,
    }))
}

/// The file name of `p` as a `String`, or `None` if it has none.
fn file_name(p: &Path) -> Option<String> {
    p.file_name().and_then(|n| n.to_str()).map(str::to_string)
}
