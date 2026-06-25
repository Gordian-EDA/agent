//! Live KiCAD-board views used while `.kicad_pcb` is the PCB source of truth.

use std::collections::BTreeMap;
use std::path::PathBuf;

use kicad_sexpr::pcb::{BoardProblem, extract_copper, read_board, read_problem};
use pcb_model::RouteSolution;
use pcb_place::placement::Placement;

use crate::tools::PcbToolCtx;

use super::draft::{BoardDraft, DraftPart, DraftRules};

/// Save the active KiCAD board and return its project PCB path.
pub fn save_live_board(ctx: &PcbToolCtx) -> std::result::Result<PathBuf, String> {
    let path = ctx.pcb_path();
    ctx.kicad()
        .with_session(&path, |session| session.kicad().save())
        .map_err(|e| format!("could not open/save live KiCAD board: {e}"))?;
    Ok(path)
}

/// Read the active board as a routing problem.
pub fn board_problem(ctx: &PcbToolCtx) -> std::result::Result<BoardProblem, String> {
    let path = save_live_board(ctx)?;
    read_problem(&path).map_err(|e| format!("could not read saved KiCAD board: {e}"))
}

/// Extract existing copper from the active board.
pub fn copper_solution(ctx: &PcbToolCtx) -> std::result::Result<RouteSolution, String> {
    let path = save_live_board(ctx)?;
    extract_copper(&path).map_err(|e| format!("could not read routed copper: {e}"))
}

/// Build the transient board model the placer consumes from the active KiCAD board.
///
/// Sidecar PCB state is intentionally ignored. Rules are recovered only to the fidelity currently exposed by
/// `kicad-sexpr::read_board`/`read_problem`; richer rule read-back is a follow-up item.
pub fn draft_from_live(ctx: &PcbToolCtx) -> std::result::Result<BoardDraft, String> {
    let path = save_live_board(ctx)?;
    let imported =
        read_board(&path).map_err(|e| format!("could not read saved KiCAD board: {e}"))?;
    let problem =
        read_problem(&path).map_err(|e| format!("could not read saved KiCAD board: {e}"))?;
    let placements = imported
        .parts
        .iter()
        .map(|p| Placement {
            reference: p.reference.clone(),
            at: p.at.clone(),
            rotation: p.rotation as f64,
        })
        .collect::<Vec<_>>();
    let last_placement = (!is_seed_placement(&imported.bounds, &placements)).then_some(placements);
    let parts = imported
        .parts
        .into_iter()
        .map(|p| DraftPart {
            reference: p.reference,
            footprint: p.lib_id,
            pad_nets: p
                .pads
                .into_iter()
                .filter_map(|(pad, net)| net.map(|n| (pad, n)))
                .collect::<BTreeMap<_, _>>(),
            locked: None,
        })
        .collect();
    Ok(BoardDraft {
        bounds: imported.bounds,
        rules: DraftRules {
            clearance: problem.problem.clearance,
            min_trace_width: problem.problem.min_trace_width,
            via_diameter: problem.problem.via_diameter,
            via_drill: problem.problem.via_drill,
            layer_count: problem.problem.layer_count,
            net_widths: problem.problem.net_widths,
            ..Default::default()
        },
        parts,
        keepouts: Vec::new(),
        hints: Default::default(),
        last_placement,
        last_place_illegal: false,
        outline: None,
    })
}

fn is_seed_placement(bounds: &pcb_model::Rect, placements: &[Placement]) -> bool {
    if placements.is_empty() {
        return false;
    }
    let mut coords: Vec<_> = placements
        .iter()
        .map(|p| (p.at.x, p.at.y, p.rotation))
        .collect();
    coords.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    coords.iter().enumerate().all(|(idx, (x, y, rotation))| {
        let expected_x = bounds.min_x + 2.0 + 2.54 * idx as f64;
        let expected_y = bounds.min_y + 2.0;
        (x - expected_x).abs() < 1e-6 && (y - expected_y).abs() < 1e-6 && rotation.abs() < 1e-6
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{Point2, Rect};

    fn placement(reference: &str, x: f64, y: f64, rotation: f64) -> Placement {
        Placement {
            reference: reference.to_owned(),
            at: Point2 { x, y },
            rotation,
        }
    }

    #[test]
    fn seed_row_is_not_a_real_placement() {
        let bounds = Rect {
            min_x: 10.0,
            max_x: 30.0,
            min_y: 5.0,
            max_y: 20.0,
        };
        let seeded = vec![
            placement("C1", 14.54, 7.0, 0.0),
            placement("R1", 12.0, 7.0, 0.0),
        ];
        let moved = vec![
            placement("R1", 12.0, 7.0, 0.0),
            placement("C1", 16.0, 9.0, 90.0),
        ];

        assert!(is_seed_placement(&bounds, &seeded));
        assert!(!is_seed_placement(&bounds, &moved));
    }
}
