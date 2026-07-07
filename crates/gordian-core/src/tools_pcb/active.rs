//! Live KiCAD-board views used while KiCAD IPC is the PCB source of truth.

use std::path::PathBuf;

use kicad_ipc::snapshot::{ImportedBoard, IpcBoardSnapshot};
use pcb_place::placement::Placement;

use crate::AgentRuntime;

/// Save the active KiCAD board and return its project PCB path.
pub fn save_live_board(ctx: &AgentRuntime) -> std::result::Result<PathBuf, String> {
    let path = ctx.pcb_path();
    if !path.exists() {
        return Err("no board exists yet — run regenerate_board first".to_owned());
    }
    ctx.kicad()
        .with_session(&path, |session| session.kicad().save())
        .map_err(|e| format!("could not open/save live KiCAD board: {e}"))?;
    Ok(path)
}

/// Read the active board as a routing problem.
pub fn board_problem(ctx: &AgentRuntime) -> std::result::Result<IpcBoardSnapshot, String> {
    read_snapshot(ctx)
}

fn read_snapshot(ctx: &AgentRuntime) -> std::result::Result<IpcBoardSnapshot, String> {
    let path = ctx.pcb_path();
    if !path.exists() {
        return Err("no board exists yet — run regenerate_board first".to_owned());
    }
    let mut last_ready_err = None;
    for _ in 0..6 {
        match ctx
            .kicad()
            .with_session(&path, |session| session.kicad().board_snapshot())
        {
            Ok(snapshot) => return Ok(snapshot),
            Err(err) if err.is_transient_api_ready_error() => {
                last_ready_err = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(750));
            }
            Err(err) if err.is_transport_timeout() => {
                ctx.close_kicad_session();
                return ctx
                    .kicad()
                    .with_session(&path, |session| session.kicad().board_snapshot())
                    .map_err(|retry| format!("could not read live KiCAD board over IPC: {retry}"));
            }
            Err(err) => return Err(format!("could not read live KiCAD board over IPC: {err}")),
        }
    }
    if let Some(err) = last_ready_err {
        Err(format!("could not read live KiCAD board over IPC: {err}"))
    } else {
        ctx.kicad()
            .with_session(&path, |session| session.kicad().board_snapshot())
            .map_err(|err| format!("could not read live KiCAD board over IPC: {err}"))
    }
}

pub(super) fn imported_placements(board: &ImportedBoard) -> Vec<Placement> {
    board
        .parts
        .iter()
        .map(|p| Placement {
            reference: p.reference.clone(),
            at: p.at,
            rotation: p.rotation as f64,
        })
        .collect()
}

pub(super) fn is_seed_imported_board(board: &ImportedBoard) -> bool {
    is_seed_placement(&board.bounds, &imported_placements(board))
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
        (x - expected_x).abs() < geom::EPS
            && (y - expected_y).abs() < geom::EPS
            && rotation.abs() < geom::EPS
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
