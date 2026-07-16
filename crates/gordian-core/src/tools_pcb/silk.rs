//! Deterministic, DRC-oracled relocation of generated Reference silkscreen text.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use kicad_cli::{DrcReport, KicadCli, Violation};

use super::export::gate_drc;
use super::patch::{ReferencePosition, patch_reference_position, reference_position};

const MAX_DRC_RETRIES: usize = 16;
const MAX_CANDIDATES_PER_REFERENCE_PASS: usize = 4;
const MIN_REFERENCE_RADIUS_MM: f64 = 1.8;
const BATCH_CANDIDATE_INDICES: [usize; 4] = [3, 4, 5, 6];

pub(super) struct SilkCleanup {
    pub report: DrcReport,
    pub initial_warnings: usize,
    pub remaining_warnings: usize,
    pub moved_references: Vec<String>,
    pub attempts: usize,
}

pub(super) fn silk_warning_count(report: &DrcReport) -> usize {
    report
        .violations
        .iter()
        .filter(|violation| is_silk_warning(violation))
        .count()
}

pub(super) fn cleanup_reference_silkscreen(
    path: &Path,
    cli: &KicadCli,
    initial_report: DrcReport,
) -> Result<SilkCleanup, String> {
    let initial_warnings = silk_warning_count(&initial_report);
    let mut best_report = initial_report;
    let mut best_text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read board for silkscreen cleanup: {err}"))?;
    let mut best_warnings = initial_warnings;
    let mut attempts = 0usize;
    let mut moved = BTreeSet::new();
    let mut queue = offending_references(&best_report);
    let mut attempted: BTreeMap<String, BTreeSet<(i64, i64)>> = BTreeMap::new();

    // Move all current offenders per pass. These few DRC calls remove the bulk
    // of collisions without paying for one DRC process per reference. Later
    // passes use reference-hashed directions to spread dense label clusters.
    for candidate_index in BATCH_CANDIDATE_INDICES {
        if queue.is_empty() || attempts >= MAX_DRC_RETRIES {
            break;
        }
        let mut batch_text = best_text.clone();
        let mut batch_moves = Vec::new();
        for reference in &queue {
            let Some(origin) = reference_position(&batch_text, reference) else {
                continue;
            };
            let candidates = reference_candidates(reference, origin);
            let Some(&candidate) = candidates.get(candidate_index) else {
                continue;
            };
            batch_text = patch_reference_position(&batch_text, reference, candidate)?;
            batch_moves.push((reference.clone(), candidate));
        }
        if !batch_moves.is_empty() {
            write_board_text(path, &batch_text)?;
            attempts += 1;
            let batch_report = match cli.drc(path) {
                Ok(report) => report,
                Err(err) => {
                    write_board_text(path, &best_text)?;
                    return Err(format!("silkscreen batch cleanup DRC failed: {err}"));
                }
            };
            let batch_warnings = silk_warning_count(&batch_report);
            let best_gate = gate_drc(&best_report);
            let batch_gate = gate_drc(&batch_report);
            if batch_warnings < best_warnings
                && batch_gate.copper_violations <= best_gate.copper_violations
                && batch_gate.meaningful_unconnected <= best_gate.meaningful_unconnected
            {
                best_text = batch_text;
                best_report = batch_report;
                best_warnings = batch_warnings;
                for (reference, candidate) in batch_moves {
                    moved.insert(reference.clone());
                    attempted
                        .entry(reference)
                        .or_default()
                        .insert(candidate_key(candidate));
                }
                queue = offending_references(&best_report);
            } else {
                write_board_text(path, &best_text)?;
            }
        }
    }

    while let Some(reference) = queue.pop_first() {
        if attempts >= MAX_DRC_RETRIES || best_warnings == 0 {
            break;
        }
        let Some(origin) = reference_position(&best_text, &reference) else {
            continue;
        };
        let candidates = reference_candidates(&reference, origin)
            .into_iter()
            .filter(|candidate| {
                let key = candidate_key(*candidate);
                attempted
                    .get(&reference)
                    .is_none_or(|positions| !positions.contains(&key))
            })
            .take(MAX_CANDIDATES_PER_REFERENCE_PASS)
            .collect::<Vec<_>>();
        for candidate in candidates {
            if attempts >= MAX_DRC_RETRIES {
                break;
            }
            attempted
                .entry(reference.clone())
                .or_default()
                .insert(candidate_key(candidate));
            let candidate_text = patch_reference_position(&best_text, &reference, candidate)?;
            write_board_text(path, &candidate_text)?;
            attempts += 1;
            let candidate_report = match cli.drc(path) {
                Ok(report) => report,
                Err(err) => {
                    write_board_text(path, &best_text)?;
                    return Err(format!("silkscreen cleanup DRC failed: {err}"));
                }
            };
            let candidate_warnings = silk_warning_count(&candidate_report);
            let best_gate = gate_drc(&best_report);
            let candidate_gate = gate_drc(&candidate_report);
            let blocking_not_worse = candidate_gate.copper_violations
                <= best_gate.copper_violations
                && candidate_gate.meaningful_unconnected <= best_gate.meaningful_unconnected;
            if blocking_not_worse && candidate_warnings < best_warnings {
                best_text = candidate_text;
                best_report = candidate_report;
                best_warnings = candidate_warnings;
                moved.insert(reference.clone());
                queue.extend(offending_references(&best_report));
                break;
            }
            write_board_text(path, &best_text)?;
        }
    }

    // A rejected final candidate was already restored; write once more so every
    // exit path leaves exactly the best DRC-verified board on disk.
    write_board_text(path, &best_text)?;
    Ok(SilkCleanup {
        report: best_report,
        initial_warnings,
        remaining_warnings: best_warnings,
        moved_references: moved.into_iter().collect(),
        attempts,
    })
}

fn is_silk_warning(violation: &Violation) -> bool {
    violation.severity == "warning"
        && matches!(
            violation.kind.as_str(),
            "silk_over_copper" | "silk_overlap" | "silk_edge_clearance" | "silk_over_silk"
        )
}

fn offending_references(report: &DrcReport) -> BTreeSet<String> {
    report
        .violations
        .iter()
        .filter(|violation| is_silk_warning(violation))
        .flat_map(|violation| &violation.items)
        .filter_map(|item| item.description.strip_prefix("Reference field of "))
        .map(str::to_owned)
        .collect()
}

fn reference_candidates(reference: &str, origin: ReferencePosition) -> Vec<ReferencePosition> {
    let radius = origin.x.hypot(origin.y).max(MIN_REFERENCE_RADIUS_MM);
    let phase = reference
        .bytes()
        .fold(0usize, |sum, byte| sum.wrapping_add(byte as usize))
        % 8;
    let directions = [
        (0.0, -1.0),
        (1.0, 0.0),
        (0.0, 1.0),
        (-1.0, 0.0),
        (0.707106781, -0.707106781),
        (0.707106781, 0.707106781),
        (-0.707106781, 0.707106781),
        (-0.707106781, -0.707106781),
    ];
    let mut candidates = vec![origin]; // first retry only makes rotated text upright
    let norm = origin.x.hypot(origin.y);
    if norm > 1e-9 {
        let ux = origin.x / norm;
        let uy = origin.y / norm;
        // Most library references begin just outside one body edge. Try the
        // opposite and the two adjacent edges before a generic hash-spread ring.
        for (dx, dy) in [(-ux, -uy), (-uy, ux), (uy, -ux)] {
            candidates.push(ReferencePosition {
                x: (dx * radius * 100.0).round() / 100.0,
                y: (dy * radius * 100.0).round() / 100.0,
            });
        }
    }
    for extra in [0.0, 1.0, 2.0] {
        for offset in 0..directions.len() {
            let (dx, dy) = directions[(phase + offset) % directions.len()];
            let r = radius + extra;
            let candidate = ReferencePosition {
                x: (dx * r * 100.0).round() / 100.0,
                y: (dy * r * 100.0).round() / 100.0,
            };
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

fn candidate_key(candidate: ReferencePosition) -> (i64, i64) {
    (
        (candidate.x * 1000.0).round() as i64,
        (candidate.y * 1000.0).round() as i64,
    )
}

fn write_board_text(path: &Path, text: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("board path {} has no parent", path.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|err| format!("could not stage silkscreen cleanup: {err}"))?;
    temp.write_all(text.as_bytes())
        .map_err(|err| format!("could not write staged silkscreen cleanup: {err}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|err| format!("could not sync staged silkscreen cleanup: {err}"))?;
    temp.persist(path).map_err(|err| {
        format!(
            "could not replace board after silkscreen cleanup: {}",
            err.error
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_cli::ViolationItem;

    fn violation(kind: &str, items: &[&str]) -> Violation {
        Violation {
            severity: "warning".to_owned(),
            kind: kind.to_owned(),
            description: String::new(),
            items: items
                .iter()
                .map(|description| ViolationItem {
                    description: (*description).to_owned(),
                    uuid: None,
                })
                .collect(),
        }
    }

    #[test]
    fn silk_census_and_reference_extraction_are_specific() {
        let report = DrcReport {
            violations: vec![
                violation("silk_over_copper", &["Reference field of R2", "Pad 1"]),
                violation("silk_overlap", &["Segment of U1", "Reference field of C1"]),
                violation("lib_footprint_mismatch", &["Footprint R2"]),
            ],
            unconnected_items: vec![],
        };

        assert_eq!(silk_warning_count(&report), 2);
        assert_eq!(
            offending_references(&report)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["C1", "R2"]
        );
    }

    #[test]
    fn reference_candidates_are_deterministic_nearby_and_bounded() {
        let origin = ReferencePosition { x: 0.0, y: -1.5 };
        let a = reference_candidates("R17", origin);
        let b = reference_candidates("R17", origin);

        assert_eq!(a, b);
        assert_eq!(a[0], origin);
        assert!(a.len() <= 25);
        assert!(
            a.iter()
                .all(|candidate| candidate.x.hypot(candidate.y) <= 3.81)
        );
    }
}
