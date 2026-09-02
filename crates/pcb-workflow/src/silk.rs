//! Deterministic, DRC-oracled relocation of generated silkscreen text fields.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use kicad::{DrcReport, KicadInstallation, Violation};

use super::export::gate_drc;
use kicad_board::{
    FieldPosition, board_outline_bbox, field_position, footprint_placement, patch_field_hidden,
    patch_field_position, patch_field_text_size, silk_field_owners,
};

const MAX_DRC_RETRIES: usize = 16;
const MAX_CANDIDATES_PER_TARGET_PASS: usize = 4;
const MIN_TEXT_RADIUS_MM: f64 = 1.8;
const BOARD_INSET_MM: f64 = 0.5;
const BATCH_CANDIDATE_INDICES: [usize; 4] = [3, 4, 5, 6];
const REFERENCE_FIELD: &str = "Reference";
const FUNCTION_FIELD: &str = "Function";

/// Fallback text sizes for stubborn dense clusters, applied the way a layout
/// engineer would: smaller readable silk beats overlapping silk. Each step is
/// one batched DRC call outside the positional retry budget.
const SHRINK_STEPS_MM: [(f64, f64); 2] = [(0.8, 0.12), (0.6, 0.1)];

pub(super) struct SilkCleanup {
    pub report: DrcReport,
    pub initial_warnings: usize,
    pub remaining_warnings: usize,
    pub moved_references: Vec<String>,
    pub attempts: usize,
}

/// One movable silkscreen text: a footprint's Reference or generated Function
/// legend.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SilkTarget {
    reference: String,
    field: String,
}

impl SilkTarget {
    fn label(&self) -> String {
        if self.field == REFERENCE_FIELD {
            self.reference.clone()
        } else {
            format!("{}.{}", self.reference, self.field)
        }
    }
}

pub(super) fn silk_warning_count(report: &DrcReport) -> usize {
    report
        .violations
        .iter()
        .filter(|violation| is_silk_warning(violation))
        .count()
}

pub(super) fn cleanup_silk_text(
    path: &Path,
    cli: &KicadInstallation,
    initial_report: DrcReport,
) -> Result<SilkCleanup, String> {
    let initial_warnings = silk_warning_count(&initial_report);
    let mut best_report = initial_report;
    let mut best_text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read board for silkscreen cleanup: {err}"))?;
    let mut best_warnings = initial_warnings;
    let mut attempts = 0usize;
    let mut moved = BTreeSet::new();
    let mut attempted: BTreeMap<String, BTreeSet<(i64, i64)>> = BTreeMap::new();

    // Each tier re-runs the full oracle-gated position search; tiers beyond
    // the first shrink the still-offending texts first, the way a layout
    // engineer trades text size for clean silk in a dense cluster, and clear
    // their attempted positions since a spot rejected at full size can accept
    // the smaller text.
    for tier in 0..=SHRINK_STEPS_MM.len() {
        if best_warnings == 0 {
            break;
        }
        if tier > 0 {
            let (size, thickness) = SHRINK_STEPS_MM[tier - 1];
            let mut shrunk_text = best_text.clone();
            let mut shrunk_labels = Vec::new();
            for target in offending_targets(&best_report, &best_text) {
                if let Ok(next) = patch_field_text_size(
                    &shrunk_text,
                    &target.reference,
                    &target.field,
                    size,
                    thickness,
                ) {
                    shrunk_text = next;
                    shrunk_labels.push(target.label());
                }
            }
            if shrunk_labels.is_empty() {
                break;
            }
            write_board_text(path, &shrunk_text)?;
            attempts += 1;
            let shrunk_report = match cli.drc(path) {
                Ok(report) => report,
                Err(err) => {
                    write_board_text(path, &best_text)?;
                    return Err(format!("silkscreen shrink cleanup DRC failed: {err}"));
                }
            };
            let shrunk_warnings = silk_warning_count(&shrunk_report);
            let best_gate = gate_drc(&best_report);
            let shrunk_gate = gate_drc(&shrunk_report);
            if shrunk_warnings <= best_warnings
                && shrunk_gate.copper_violations <= best_gate.copper_violations
                && shrunk_gate.meaningful_unconnected <= best_gate.meaningful_unconnected
            {
                best_text = shrunk_text;
                best_report = shrunk_report;
                best_warnings = shrunk_warnings;
                moved.extend(shrunk_labels.iter().cloned());
                for label in shrunk_labels {
                    attempted.remove(&label);
                }
            } else {
                write_board_text(path, &best_text)?;
            }
        }
        let tier_budget = (tier + 1) * MAX_DRC_RETRIES;
        let mut queue = offending_targets(&best_report, &best_text);

        // Move all current offenders per pass. These few DRC calls remove the bulk
        // of collisions without paying for one DRC process per target. Later
        // passes use label-hashed directions to spread dense text clusters.
        for candidate_index in BATCH_CANDIDATE_INDICES {
            if queue.is_empty() || attempts >= tier_budget {
                break;
            }
            let mut batch_text = best_text.clone();
            let mut batch_moves = Vec::new();
            for target in &queue {
                let Some(origin) = field_position(&batch_text, &target.reference, &target.field)
                else {
                    continue;
                };
                let candidates = on_board_candidates(&batch_text, target, origin);
                let Some(&candidate) = candidates.get(candidate_index) else {
                    continue;
                };
                batch_text =
                    patch_field_position(&batch_text, &target.reference, &target.field, candidate)?;
                batch_moves.push((target.clone(), candidate));
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
                let batch_improves = batch_warnings < best_warnings
                    && batch_gate.copper_violations <= best_gate.copper_violations
                    && batch_gate.meaningful_unconnected <= best_gate.meaningful_unconnected;
                if batch_improves {
                    for (target, candidate) in &batch_moves {
                        attempted
                            .entry(target.label())
                            .or_default()
                            .insert(candidate_key(*candidate));
                    }
                    best_text = batch_text;
                    best_report = batch_report;
                    best_warnings = batch_warnings;
                    for (target, _) in batch_moves {
                        moved.insert(target.label());
                    }
                    queue = offending_targets(&best_report, &best_text);
                } else {
                    // A rejected singleton batch tested the exact same state as an
                    // individual move, so do not spend the fallback budget retrying
                    // it. Multi-target batches may fail through interactions even
                    // when one move is useful, and therefore remain individually
                    // eligible.
                    if let [(target, candidate)] = batch_moves.as_slice() {
                        attempted
                            .entry(target.label())
                            .or_default()
                            .insert(candidate_key(*candidate));
                    }
                    write_board_text(path, &best_text)?;
                }
            }
        }

        let mut queue: VecDeque<_> = queue.into_iter().collect();
        while let Some(target) = queue.pop_front() {
            if attempts >= tier_budget || best_warnings == 0 {
                break;
            }
            let Some(origin) = field_position(&best_text, &target.reference, &target.field) else {
                continue;
            };
            let viable = on_board_candidates(&best_text, &target, origin);
            let candidates = untried_candidates(&viable, attempted.get(&target.label()))
                .into_iter()
                .take(MAX_CANDIDATES_PER_TARGET_PASS)
                .collect::<Vec<_>>();
            let mut accepted = false;
            for candidate in candidates {
                if attempts >= tier_budget {
                    break;
                }
                attempted
                    .entry(target.label())
                    .or_default()
                    .insert(candidate_key(candidate));
                let candidate_text =
                    patch_field_position(&best_text, &target.reference, &target.field, candidate)?;
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
                    moved.insert(target.label());
                    queue = offending_targets(&best_report, &best_text)
                        .into_iter()
                        .collect();
                    accepted = true;
                    break;
                }
                write_board_text(path, &best_text)?;
            }
            // Work in small per-target passes for fairness, but do not discard a
            // stubborn target while fresh positions and the global retry budget
            // remain. Re-queueing also lets a dense label reach the wider rings.
            if !accepted
                && attempts < tier_budget
                && !untried_candidates(&viable, attempted.get(&target.label())).is_empty()
            {
                queue.push_back(target);
            }
        }
    }

    // Texts that survive every size tier and ring have no legal silk spot on
    // this board. Industry practice for such ultra-dense clusters is to omit
    // the silk reference (it stays in the fab drawing), which reads cleaner
    // than clipped or overlapping text. The DRC oracle still gates the result.
    if best_warnings > 0 {
        let mut hidden_text = best_text.clone();
        let mut hidden_labels = Vec::new();
        for target in offending_targets(&best_report, &best_text) {
            if let Ok(next) = patch_field_hidden(&hidden_text, &target.reference, &target.field) {
                hidden_text = next;
                hidden_labels.push(target.label());
            }
        }
        if !hidden_labels.is_empty() {
            write_board_text(path, &hidden_text)?;
            attempts += 1;
            let hidden_report = match cli.drc(path) {
                Ok(report) => report,
                Err(err) => {
                    write_board_text(path, &best_text)?;
                    return Err(format!("silkscreen hide cleanup DRC failed: {err}"));
                }
            };
            let hidden_warnings = silk_warning_count(&hidden_report);
            let best_gate = gate_drc(&best_report);
            let hidden_gate = gate_drc(&hidden_report);
            if hidden_warnings < best_warnings
                && hidden_gate.copper_violations <= best_gate.copper_violations
                && hidden_gate.meaningful_unconnected <= best_gate.meaningful_unconnected
            {
                best_text = hidden_text;
                best_report = hidden_report;
                best_warnings = hidden_warnings;
                moved.extend(hidden_labels);
            } else {
                write_board_text(path, &best_text)?;
            }
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

/// Movable texts implicated by the report.
///
/// References come straight from violation items. KiCad 10.0.4 omits
/// `PCB_FIELD` items other than Reference/Value from some DRC reports, so a
/// violation caused by a generated Function legend can surface with no items
/// or only its collision partner. Whenever a silk violation has no movable
/// attribution, every visible Function legend becomes a relocation candidate;
/// the DRC oracle keeps only moves that reduce warnings.
fn offending_targets(report: &DrcReport, board_text: &str) -> BTreeSet<SilkTarget> {
    let mut targets = BTreeSet::new();
    let mut unattributed = false;
    for violation in report.violations.iter().filter(|v| is_silk_warning(v)) {
        let mut attributed = false;
        for item in &violation.items {
            if let Some(reference) = item.description.strip_prefix("Reference field of ") {
                targets.insert(SilkTarget {
                    reference: reference.to_owned(),
                    field: REFERENCE_FIELD.to_owned(),
                });
                attributed = true;
            }
        }
        unattributed |= !attributed;
    }
    if unattributed {
        for reference in silk_field_owners(board_text, FUNCTION_FIELD) {
            targets.insert(SilkTarget {
                reference,
                field: FUNCTION_FIELD.to_owned(),
            });
        }
    }
    targets
}

/// Candidate positions whose text center stays on the board.
///
/// KiCad flags silk crossing the outline but not silk placed entirely outside
/// it, so an unconstrained search can "fix" an edge warning by pushing a label
/// off the board — DRC-clean, but the label vanishes from fabrication.
fn on_board_candidates(
    text: &str,
    target: &SilkTarget,
    origin: FieldPosition,
) -> Vec<FieldPosition> {
    let candidates = candidate_positions(target, origin);
    let Some((min_x, min_y, max_x, max_y)) = board_outline_bbox(text) else {
        return candidates;
    };
    let Some((fx, fy, angle)) = footprint_placement(text, &target.reference) else {
        return candidates;
    };
    let (sin, cos) = angle.to_radians().sin_cos();
    candidates
        .into_iter()
        .filter(|candidate| {
            let x = fx + candidate.x * cos + candidate.y * sin;
            let y = fy - candidate.x * sin + candidate.y * cos;
            x >= min_x + BOARD_INSET_MM
                && x <= max_x - BOARD_INSET_MM
                && y >= min_y + BOARD_INSET_MM
                && y <= max_y - BOARD_INSET_MM
        })
        .collect()
}

fn candidate_positions(target: &SilkTarget, origin: FieldPosition) -> Vec<FieldPosition> {
    let radius = origin.x.hypot(origin.y).max(MIN_TEXT_RADIUS_MM);
    let phase = target
        .label()
        .bytes()
        .fold(0usize, |sum, byte| sum.wrapping_add(byte as usize))
        % 8;
    let diag = std::f64::consts::FRAC_1_SQRT_2;
    let directions = [
        (0.0, -1.0),
        (1.0, 0.0),
        (0.0, 1.0),
        (-1.0, 0.0),
        (diag, -diag),
        (diag, diag),
        (-diag, diag),
        (-diag, -diag),
    ];
    let mut candidates = vec![origin]; // first retry only makes rotated text upright
    // The body center: pad-free on most IC packages, and the spot a layout
    // engineer uses when the perimeter is packed.
    candidates.push(FieldPosition { x: 0.0, y: 0.0 });
    let norm = origin.x.hypot(origin.y);
    if norm > 1e-9 {
        let ux = origin.x / norm;
        let uy = origin.y / norm;
        // Most library texts begin just outside one body edge. Try the
        // opposite and the two adjacent edges before a generic hash-spread ring.
        for (dx, dy) in [(-ux, -uy), (-uy, ux), (uy, -ux)] {
            candidates.push(FieldPosition {
                x: (dx * radius * 100.0).round() / 100.0,
                y: (dy * radius * 100.0).round() / 100.0,
            });
        }
    }
    for extra in [0.0, 1.0, 2.0, 3.5] {
        for offset in 0..directions.len() {
            let (dx, dy) = directions[(phase + offset) % directions.len()];
            let r = radius + extra;
            let candidate = FieldPosition {
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

fn candidate_key(candidate: FieldPosition) -> (i64, i64) {
    (
        (candidate.x * 1000.0).round() as i64,
        (candidate.y * 1000.0).round() as i64,
    )
}

fn untried_candidates(
    candidates: &[FieldPosition],
    attempted: Option<&BTreeSet<(i64, i64)>>,
) -> Vec<FieldPosition> {
    candidates
        .iter()
        .copied()
        .filter(|candidate| {
            attempted.is_none_or(|positions| !positions.contains(&candidate_key(*candidate)))
        })
        .collect()
}

fn write_board_text(path: &Path, text: &str) -> Result<(), String> {
    gordian_runtime::workspace::atomic_write(path, text.as_bytes())
        .map_err(|err| format!("could not replace board after silkscreen cleanup: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kicad::ViolationItem;

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
                    pos: None,
                })
                .collect(),
        }
    }

    fn target(reference: &str, field: &str) -> SilkTarget {
        SilkTarget {
            reference: reference.to_owned(),
            field: field.to_owned(),
        }
    }

    const FUNCTION_BOARD: &str = r#"(kicad_pcb
	(footprint "Connector:Header"
		(at 38 11.93)
		(property "Reference" "J1"
			(at 0 -2.38 0)
			(layer "F.SilkS")
		)
		(property "Function" "+3V3/GND"
			(at 0 12.54 0)
			(layer "F.SilkS")
		)
	)
)
"#;

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
            offending_targets(&report, "(kicad_pcb\n)")
                .into_iter()
                .collect::<Vec<_>>(),
            vec![target("C1", "Reference"), target("R2", "Reference")]
        );
    }

    #[test]
    fn unattributed_silk_violations_enqueue_function_legends() {
        // KiCad reports some Function-field collisions with empty items or only
        // the partner item, never the field itself.
        let report = DrcReport {
            violations: vec![
                violation("silk_over_copper", &[]),
                violation("silk_overlap", &["Segment of C7 on F.Silkscreen"]),
                violation("silk_edge_clearance", &["Rectangle on Edge.Cuts"]),
            ],
            unconnected_items: vec![],
        };

        let targets = offending_targets(&report, FUNCTION_BOARD)
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(targets, vec![target("J1", "Function")]);
        assert_eq!(targets[0].label(), "J1.Function");
    }

    #[test]
    fn attributed_silk_violations_leave_function_legends_alone() {
        let report = DrcReport {
            violations: vec![violation(
                "silk_overlap",
                &["Reference field of C1", "Reference field of C2"],
            )],
            unconnected_items: vec![],
        };

        assert_eq!(
            offending_targets(&report, FUNCTION_BOARD)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![target("C1", "Reference"), target("C2", "Reference")]
        );
    }

    #[test]
    fn candidate_positions_are_deterministic_nearby_and_bounded() {
        let origin = FieldPosition { x: 0.0, y: -1.5 };
        let a = candidate_positions(&target("R17", "Reference"), origin);
        let b = candidate_positions(&target("R17", "Reference"), origin);

        assert_eq!(a, b);
        assert_eq!(a[0], origin);
        assert_eq!(a[1], FieldPosition { x: 0.0, y: 0.0 });
        assert!(a.len() <= 40);
        assert!(
            a.iter()
                .all(|candidate| candidate.x.hypot(candidate.y) <= 5.31)
        );
    }

    #[test]
    fn rejected_batch_candidates_do_not_starve_wider_target_ring() {
        let origin = FieldPosition { x: -1.8, y: 0.0 };
        let c6 = target("C6", "Reference");
        let all = candidate_positions(&c6, origin);
        let mut attempted = BTreeSet::new();
        attempted.insert(candidate_key(origin));
        for index in [5, 6, 7, 8] {
            attempted.insert(candidate_key(all[index]));
        }

        let first_pass = untried_candidates(&all, Some(&attempted))
            .into_iter()
            .take(MAX_CANDIDATES_PER_TARGET_PASS)
            .collect::<Vec<_>>();
        for candidate in &first_pass {
            attempted.insert(candidate_key(*candidate));
        }
        let second_pass = untried_candidates(&all, Some(&attempted));

        assert_eq!(first_pass.len(), MAX_CANDIDATES_PER_TARGET_PASS);
        assert_eq!(second_pass[0], FieldPosition { x: 2.8, y: 0.0 });
    }

    #[test]
    fn candidates_that_leave_the_board_are_rejected() {
        // J1 sits 1.77mm from the 80mm edge; a 12.54mm-radius ring reaches far
        // past the outline, where KiCad DRC no longer sees the text at all.
        let board = "(kicad_pcb\n\
            \t(footprint \"Connector:Header\"\n\
            \t\t(at 78.23 29.5)\n\
            \t\t(property \"Reference\" \"J1\"\n\
            \t\t\t(at 0 -2.38 0)\n\
            \t\t\t(layer \"F.SilkS\")\n\
            \t\t)\n\
            \t)\n\
            \t(gr_rect\n\
            \t\t(start 0 0)\n\
            \t\t(end 80 58)\n\
            \t\t(layer \"Edge.Cuts\")\n\
            \t)\n\
            )\n";
        let function = target("J1", "Function");
        let origin = FieldPosition { x: 0.0, y: 12.54 };
        let viable = on_board_candidates(board, &function, origin);

        assert!(!viable.is_empty());
        assert!(candidate_positions(&function, origin).len() > viable.len());
        for candidate in viable {
            let x = 78.23 + candidate.x;
            let y = 29.5 + candidate.y;
            assert!((0.5..=79.5).contains(&x) && (0.5..=57.5).contains(&y));
        }
    }
}
