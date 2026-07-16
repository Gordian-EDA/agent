//! Audit one timed natural-language schematic-to-fabrication run.
//!
//! Usage: `cargo run -p gordian-core --example audit_live_e2e -- \
//!   /tmp/gordian-hard10/run01-can 45 180`

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use kicad_cli::KicadCli;
use kicad_env::KicadEnv;
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().context("missing run root")?);
    let minimum_parts: usize = args
        .next()
        .context("missing minimum physical-part count")?
        .parse()
        .context("minimum part count must be an integer")?;
    let maximum_wall_seconds: u64 = args
        .next()
        .unwrap_or_else(|| "180".to_owned())
        .parse()
        .context("maximum wall time must be an integer")?;
    if args.next().is_some() {
        bail!("usage: audit_live_e2e <run-root> <minimum-parts> [maximum-wall-seconds]");
    }

    let project = root.join("project");
    let schematic = project.join("design.kicad_sch");
    let board = project.join("design.kicad_pcb");
    let wall =
        parse_wall_seconds(&std::fs::read_to_string(root.join("time.txt")).unwrap_or_default());
    let board_text = std::fs::read_to_string(&board).unwrap_or_default();
    let physical_parts = count_footprints(&board_text);
    let renders = project.join(".gordian/renders");
    let render_count = count_files(&renders, Some("png"));
    let fab_count = count_files(&project.join("fab"), None);

    let env = KicadEnv::detect().context("no KiCad environment detected")?;
    let cli = KicadCli::new(&env);
    let erc = schematic.exists().then(|| cli.erc(&schematic)).transpose();
    let drc = board.exists().then(|| cli.drc(&board)).transpose();
    let erc_report = erc.as_ref().ok().and_then(Option::as_ref);
    let drc_report = drc.as_ref().ok().and_then(Option::as_ref);
    let erc_errors = erc_report.map(|r| r.error_count());
    let erc_warnings = erc_report.map(|r| r.warning_count());
    let drc_violations = drc_report.map(|r| r.violations.len());
    let unconnected = drc
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .map(|r| r.unconnected_items.len());

    let gates = json!({
        "within_time": wall.is_some_and(|seconds| seconds <= maximum_wall_seconds),
        "minimum_parts": physical_parts >= minimum_parts,
        "erc_clean": erc_errors == Some(0) && erc_warnings == Some(0),
        "drc_clean": drc_violations == Some(0) && unconnected == Some(0),
        "four_renders": render_count >= 4,
        "fab_bundle": fab_count >= 10,
    });
    let passed = gates
        .as_object()
        .is_some_and(|values| values.values().all(|value| value == &json!(true)));
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "passed": passed,
            "run_root": root,
            "wall_seconds": wall,
            "maximum_wall_seconds": maximum_wall_seconds,
            "physical_parts": physical_parts,
            "minimum_physical_parts": minimum_parts,
            "erc_errors": erc_errors,
            "erc_warnings": erc_warnings,
            "drc_violations": drc_violations,
            "unconnected_items": unconnected,
            "render_count": render_count,
            "fab_file_count": fab_count,
            "gates": gates,
        }))?
    );
    if !passed {
        bail!("live E2E acceptance failed");
    }
    Ok(())
}

fn parse_wall_seconds(text: &str) -> Option<u64> {
    text.split_whitespace()
        .find_map(|field| field.strip_prefix("wall=")?.parse().ok())
}

fn count_footprints(board: &str) -> usize {
    board
        .lines()
        .filter(|line| line.trim_start().starts_with("(footprint \""))
        .count()
}

fn count_files(dir: &Path, extension: Option<&str>) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && extension.is_none_or(|wanted| {
                    entry.path().extension().and_then(|ext| ext.to_str()) == Some(wanted)
                })
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shell_timer_and_counts_only_footprint_nodes() {
        assert_eq!(parse_wall_seconds("wall=94 exit=1\n"), Some(94));
        assert_eq!(
            count_footprints("(kicad_pcb\n  (footprint \"A:B\"\n  (footprint \"C:D\"\n)"),
            2
        );
    }
}
