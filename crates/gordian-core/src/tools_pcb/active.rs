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
    // A wedged live session must not block file-based consumers: the offline
    // write paths keep the on-disk board current, so drop the session and hand
    // back the file.
    if ctx
        .kicad()
        .with_session(&path, |session| session.kicad().save())
        .is_err()
    {
        ctx.close_kicad_session();
    }
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
            Ok(mut snapshot) => {
                reconcile_file_stackup(&path, &mut snapshot)?;
                return Ok(snapshot);
            }
            Err(err) if err.is_transient_api_ready_error() => {
                last_ready_err = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(750));
            }
            Err(err) if err.is_transport_timeout() => {
                ctx.close_kicad_session();
                let mut snapshot = ctx
                    .kicad()
                    .with_session(&path, |session| session.kicad().board_snapshot())
                    .map_err(|retry| {
                        format!("could not read live KiCAD board over IPC: {retry}")
                    })?;
                reconcile_file_stackup(&path, &mut snapshot)?;
                return Ok(snapshot);
            }
            Err(err) => return Err(format!("could not read live KiCAD board over IPC: {err}")),
        }
    }
    if let Some(err) = last_ready_err {
        Err(format!("could not read live KiCAD board over IPC: {err}"))
    } else {
        let mut snapshot = ctx
            .kicad()
            .with_session(&path, |session| session.kicad().board_snapshot())
            .map_err(|err| format!("could not read live KiCAD board over IPC: {err}"))?;
        reconcile_file_stackup(&path, &mut snapshot)?;
        Ok(snapshot)
    }
}

fn reconcile_file_stackup(
    path: &std::path::Path,
    snapshot: &mut IpcBoardSnapshot,
) -> std::result::Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read board layer table: {err}"))?;
    let layer_names = super::patch::board_copper_layer_names(&text)?;
    if snapshot.layer_names == layer_names {
        return Ok(());
    }

    let layer_count = layer_names.len() as u32;
    snapshot.layer_names = layer_names;
    snapshot.problem.layer_count = layer_count;
    snapshot.imported.layer_count = layer_count;

    for obstacle in &mut snapshot.problem.obstacles {
        obstacle
            .layers
            .retain(|layer| layer.index(layer_count).is_some());
    }
    snapshot
        .problem
        .obstacles
        .retain(|obstacle| !obstacle.layers.is_empty());
    for connection in &mut snapshot.problem.connections {
        connection
            .points_to_connect
            .retain(|point| point.layer.index(layer_count).is_some());
    }
    snapshot
        .problem
        .connections
        .retain(|connection| connection.points_to_connect.len() >= 2);
    snapshot
        .problem
        .plane_nets
        .retain(|_, layer| *layer > 0 && *layer + 1 < layer_count);
    snapshot
        .problem
        .escape_layers
        .retain(|_, layer| *layer > 0 && *layer + 1 < layer_count);

    // Preserve even invalid-layer copper here. route_board uses its presence to
    // clear every existing segment/via before solving. Hiding it would leave a
    // board containing only stale inner-layer copper uncleared.
    Ok(())
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
    use pcb_model::{LayerRef, Point2, Rect, RouteProblem, RouteSolution, Trace};
    use std::collections::BTreeMap;

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

    #[test]
    fn board_file_stackup_removes_stale_ipc_inner_layers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(
            &path,
            "(kicad_pcb (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal)))",
        )
        .unwrap();
        let bounds = Rect {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 10.0,
            max_y: 10.0,
        };
        let stale_inner = LayerRef("inner1".to_owned());
        let mut snapshot = IpcBoardSnapshot {
            problem: RouteProblem {
                layer_count: 4,
                min_trace_width: 0.2,
                obstacles: vec![pcb_model::Obstacle {
                    kind: "stale track".to_owned(),
                    layers: vec![stale_inner.clone()],
                    center: Point2::new(2.0, 2.0),
                    width: 1.0,
                    height: 0.2,
                    connected_to: vec!["GND".to_owned()],
                }],
                connections: vec![],
                bounds,
                clearance: 0.2,
                via_diameter: 0.6,
                via_drill: 0.3,
                net_widths: BTreeMap::new(),
                outline: None,
                escape_layers: BTreeMap::new(),
                plane_nets: BTreeMap::new(),
            },
            imported: ImportedBoard {
                layer_count: 4,
                bounds,
                parts: vec![],
                placement_keepouts: vec![],
                keepout_count: 0,
            },
            copper: RouteSolution {
                traces: vec![Trace {
                    connection: "GND".to_owned(),
                    layer: stale_inner,
                    width: 0.2,
                    path: vec![Point2::new(1.0, 1.0), Point2::new(2.0, 1.0)],
                }],
                vias: vec![],
            },
            net_codes: BTreeMap::new(),
            layer_names: vec![
                "F.Cu".to_owned(),
                "In1.Cu".to_owned(),
                "In2.Cu".to_owned(),
                "B.Cu".to_owned(),
            ],
        };

        reconcile_file_stackup(&path, &mut snapshot).unwrap();

        assert_eq!(snapshot.problem.layer_count, 2);
        assert_eq!(snapshot.layer_names, vec!["F.Cu", "B.Cu"]);
        assert!(snapshot.problem.obstacles.is_empty());
        assert_eq!(
            snapshot.copper.traces.len(),
            1,
            "stale copper must remain visible so route_board clears it"
        );
    }
}
