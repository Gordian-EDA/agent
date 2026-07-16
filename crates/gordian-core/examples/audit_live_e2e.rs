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
    let timing = std::fs::read_to_string(root.join("time.txt")).unwrap_or_default();
    let wall = parse_named_u64(&timing, "wall");
    let process_exit = parse_named_u64(&timing, "exit");
    let board_text = std::fs::read_to_string(&board).unwrap_or_default();
    let physical_parts = count_footprints(&board_text);
    let renders = project.join(".gordian/renders");
    let render_count = count_files(&renders, Some("png"));
    let valid_render_count = count_valid_pngs(&renders, 800, 600);
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
        "process_success": process_exit == Some(0),
        "within_time": wall.is_some_and(|seconds| seconds <= maximum_wall_seconds),
        "minimum_parts": physical_parts >= minimum_parts,
        "erc_clean": erc_errors == Some(0) && erc_warnings == Some(0),
        "drc_clean": drc_violations == Some(0) && unconnected == Some(0),
        "four_valid_renders": render_count >= 4 && valid_render_count >= 4,
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
            "process_exit": process_exit,
            "maximum_wall_seconds": maximum_wall_seconds,
            "physical_parts": physical_parts,
            "minimum_physical_parts": minimum_parts,
            "erc_errors": erc_errors,
            "erc_warnings": erc_warnings,
            "drc_violations": drc_violations,
            "unconnected_items": unconnected,
            "render_count": render_count,
            "valid_render_count": valid_render_count,
            "fab_file_count": fab_count,
            "gates": gates,
        }))?
    );
    if !passed {
        bail!("live E2E acceptance failed");
    }
    Ok(())
}

fn parse_named_u64(text: &str, name: &str) -> Option<u64> {
    text.split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{name}="))?.parse().ok())
}

fn count_valid_pngs(dir: &Path, minimum_width: u32, minimum_height: u32) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| png_dimensions(&bytes))
        .filter(|&(width, height)| width >= minimum_width && height >= minimum_height)
        .count()
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    (bytes.get(..8)? == SIGNATURE).then_some(())?;
    (bytes.get(12..16)? == b"IHDR").then_some(())?;
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((width, height))
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
        assert_eq!(parse_named_u64("wall=94 exit=1\n", "wall"), Some(94));
        assert_eq!(parse_named_u64("wall=94 exit=1\n", "exit"), Some(1));
        assert_eq!(
            count_footprints("(kicad_pcb\n  (footprint \"A:B\"\n  (footprint \"C:D\"\n)"),
            2
        );
    }

    #[test]
    fn validates_png_signature_and_ihdr_dimensions() {
        let mut bytes = vec![0; 24];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&1600u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&900u32.to_be_bytes());
        assert_eq!(png_dimensions(&bytes), Some((1600, 900)));
        bytes[0] = 0;
        assert_eq!(png_dimensions(&bytes), None);
    }
}
