//! Live KiCAD-board views and the explicit bridge-to-domain conversion seam.

use std::path::PathBuf;

use geom::{Point2, Rect};
use pcb_model::{
    Connection, LayerRef, Obstacle, RoutePoint, RouteProblem, RouteSolution, Trace, Via, ViaSpan,
};
use pcb_place_api::Placement;

use gordian_runtime::AgentRuntime;

/// Domain view consumed by placement and routing tools.
#[derive(Debug, Clone)]
pub struct IpcBoardSnapshot {
    pub problem: RouteProblem,
    pub imported: ImportedBoard,
    pub copper: RouteSolution,
    pub layer_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ImportedBoard {
    pub layer_count: u32,
    pub bounds: Rect,
    pub parts: Vec<ImportedPart>,
    pub placement_keepouts: Vec<Rect>,
    pub keepout_count: usize,
}

#[derive(Debug, Clone)]
pub struct ImportedPart {
    pub reference: String,
    pub lib_id: String,
    pub at: Point2,
    pub rotation: i32,
    pub locked: bool,
    pub pads: Vec<ImportedPad>,
}

#[derive(Debug, Clone)]
pub struct ImportedPad {
    pub number: String,
    pub net: Option<String>,
    pub at: Point2,
    pub layers: Vec<LayerRef>,
}

pub(super) fn from_bridge(snapshot: kicad_ipc::snapshot::IpcBoardSnapshot) -> IpcBoardSnapshot {
    let problem = snapshot.problem;
    IpcBoardSnapshot {
        problem: RouteProblem {
            layer_count: problem.layer_count,
            min_trace_width: problem.min_trace_width,
            obstacles: problem
                .obstacles
                .into_iter()
                .map(|obstacle| Obstacle {
                    kind: obstacle.kind,
                    layers: obstacle.layers.into_iter().map(domain_layer).collect(),
                    center: obstacle.center,
                    width: obstacle.width,
                    height: obstacle.height,
                    connected_to: obstacle.connected_to,
                })
                .collect(),
            connections: problem
                .connections
                .into_iter()
                .map(|connection| Connection {
                    name: connection.name,
                    points_to_connect: connection
                        .points_to_connect
                        .into_iter()
                        .map(|point| RoutePoint {
                            x: point.x,
                            y: point.y,
                            layer: domain_layer(point.layer),
                        })
                        .collect(),
                })
                .collect(),
            bounds: problem.bounds,
            clearance: problem.clearance,
            via_diameter: problem.via_diameter,
            via_drill: problem.via_drill,
            net_widths: problem.net_widths,
            outline: problem.outline,
            escape_layers: Default::default(),
            plane_nets: problem.plane_nets,
        },
        imported: ImportedBoard {
            layer_count: snapshot.imported.layer_count,
            bounds: snapshot.imported.bounds,
            parts: snapshot
                .imported
                .parts
                .into_iter()
                .map(|part| ImportedPart {
                    reference: part.reference,
                    lib_id: part.lib_id,
                    at: part.at,
                    rotation: part.rotation,
                    locked: part.locked,
                    pads: part
                        .pads
                        .into_iter()
                        .map(|pad| ImportedPad {
                            number: pad.number,
                            net: pad.net,
                            at: pad.at,
                            layers: pad.layers.into_iter().map(domain_layer).collect(),
                        })
                        .collect(),
                })
                .collect(),
            placement_keepouts: snapshot.imported.placement_keepouts,
            keepout_count: snapshot.imported.keepout_count,
        },
        copper: RouteSolution {
            traces: snapshot
                .copper
                .traces
                .into_iter()
                .map(|trace| Trace {
                    connection: trace.connection,
                    layer: domain_layer(trace.layer),
                    width: trace.width,
                    path: trace.path,
                })
                .collect(),
            vias: snapshot
                .copper
                .vias
                .into_iter()
                .map(|via| Via {
                    connection: via.connection,
                    at: via.at,
                    diameter: via.diameter,
                    drill: via.drill,
                    span: domain_via_span(via.span),
                })
                .collect(),
        },
        layer_names: snapshot.layer_names,
    }
}

pub(super) fn bridge_route(
    problem: &RouteProblem,
    solution: &RouteSolution,
) -> kicad_ipc::RouteWrite {
    let bridge_solution = kicad_ipc::snapshot::BoardCopper {
        traces: solution
            .traces
            .iter()
            .map(|trace| kicad_ipc::snapshot::BoardTrace {
                connection: trace.connection.clone(),
                layer: bridge_layer(&trace.layer),
                width: trace.width,
                path: trace.path.clone(),
            })
            .collect(),
        vias: solution
            .vias
            .iter()
            .map(|via| kicad_ipc::snapshot::BoardVia {
                connection: via.connection.clone(),
                at: via.at,
                diameter: via.diameter,
                drill: via.drill,
                span: bridge_via_span(&via.span),
            })
            .collect(),
    };
    kicad_ipc::RouteWrite {
        layer_count: problem.layer_count,
        solution: bridge_solution,
    }
}

fn domain_layer(layer: kicad_ipc::snapshot::CopperLayer) -> LayerRef {
    LayerRef(layer.0)
}

fn bridge_layer(layer: &LayerRef) -> kicad_ipc::snapshot::CopperLayer {
    kicad_ipc::snapshot::CopperLayer(layer.0.clone())
}

fn domain_via_span(span: kicad_ipc::snapshot::BoardViaSpan) -> ViaSpan {
    match span {
        kicad_ipc::snapshot::BoardViaSpan::Through => ViaSpan::Through,
        kicad_ipc::snapshot::BoardViaSpan::Partial { from, to, micro } => {
            ViaSpan::Partial { from, to, micro }
        }
    }
}

fn bridge_via_span(span: &ViaSpan) -> kicad_ipc::snapshot::BoardViaSpan {
    match *span {
        ViaSpan::Through => kicad_ipc::snapshot::BoardViaSpan::Through,
        ViaSpan::Partial { from, to, micro } => {
            kicad_ipc::snapshot::BoardViaSpan::Partial { from, to, micro }
        }
    }
}

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
            Ok(snapshot) => {
                let mut snapshot = from_bridge(snapshot);
                reconcile_file_stackup(&path, &mut snapshot)?;
                return Ok(snapshot);
            }
            Err(err) if err.is_transient_api_ready_error() => {
                last_ready_err = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(750));
            }
            Err(err) if err.is_transport_timeout() || err.is_type_mismatch() => {
                let initial = err.to_string();
                ctx.close_kicad_session();
                let snapshot = ctx
                    .kicad()
                    .with_session(&path, |session| session.kicad().board_snapshot())
                    .map_err(|retry| {
                        format!(
                            "could not read live KiCAD board over IPC after reconnect; initial error: {initial}; retry error: {retry}"
                        )
                    })?;
                let mut snapshot = from_bridge(snapshot);
                reconcile_file_stackup(&path, &mut snapshot)?;
                return Ok(snapshot);
            }
            Err(err) => return Err(format!("could not read live KiCAD board over IPC: {err}")),
        }
    }
    if let Some(err) = last_ready_err {
        Err(format!("could not read live KiCAD board over IPC: {err}"))
    } else {
        let snapshot = ctx
            .kicad()
            .with_session(&path, |session| session.kicad().board_snapshot())
            .map_err(|err| format!("could not read live KiCAD board over IPC: {err}"))?;
        let mut snapshot = from_bridge(snapshot);
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
    let ipc_layer_names = snapshot.layer_names.clone();
    let layer_count = layer_names.len() as u32;
    if ipc_layer_names != layer_names {
        snapshot.problem.plane_nets.retain(|_, layer| {
            let Some(name) = ipc_layer_names.get(*layer as usize) else {
                return false;
            };
            let Some(mapped) = layer_names.iter().position(|candidate| candidate == name) else {
                return false;
            };
            *layer = mapped as u32;
            true
        });
    }
    for (net, layer) in super::patch::board_file_plane_nets(&text)? {
        snapshot.problem.plane_nets.insert(net, layer);
    }
    snapshot.problem.escape_layers.retain(|_, layer| {
        let Some(name) = ipc_layer_names.get(*layer as usize) else {
            return false;
        };
        let Some(mapped) = layer_names.iter().position(|candidate| candidate == name) else {
            return false;
        };
        *layer = mapped as u32;
        mapped > 0 && mapped + 1 < layer_names.len()
    });
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
    fn board_file_stackup_remaps_ipc_bottom_plane_and_removes_inner_layers() {
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
                layer_count: 32,
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
                plane_nets: BTreeMap::from([("GND".to_owned(), 31)]),
            },
            imported: ImportedBoard {
                layer_count: 32,
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
            layer_names: std::iter::once("F.Cu".to_owned())
                .chain((1..=30).map(|index| format!("In{index}.Cu")))
                .chain(std::iter::once("B.Cu".to_owned()))
                .collect(),
        };

        reconcile_file_stackup(&path, &mut snapshot).unwrap();

        assert_eq!(snapshot.problem.layer_count, 2);
        assert_eq!(snapshot.layer_names, vec!["F.Cu", "B.Cu"]);
        assert_eq!(snapshot.problem.plane_nets["GND"], 1);
        assert!(snapshot.problem.obstacles.is_empty());
        assert_eq!(
            snapshot.copper.traces.len(),
            1,
            "stale copper must remain visible so route_board clears it"
        );
    }

    #[test]
    fn bridge_snapshot_conversion_preserves_layers_and_via_spans() {
        use kicad_ipc::snapshot as bridge;

        let bounds = Rect::new(0.0, 0.0, 10.0, 10.0);
        let snapshot = from_bridge(bridge::IpcBoardSnapshot {
            problem: bridge::BoardRouting {
                layer_count: 4,
                min_trace_width: 0.2,
                obstacles: vec![],
                connections: vec![],
                bounds,
                clearance: 0.2,
                via_diameter: 0.6,
                via_drill: 0.3,
                net_widths: BTreeMap::new(),
                outline: None,
                plane_nets: BTreeMap::new(),
            },
            imported: bridge::ImportedBoard {
                layer_count: 4,
                bounds,
                parts: vec![],
                placement_keepouts: vec![],
                keepout_count: 0,
            },
            copper: bridge::BoardCopper {
                traces: vec![bridge::BoardTrace {
                    connection: "SIG".to_owned(),
                    layer: bridge::CopperLayer("inner1".to_owned()),
                    width: 0.2,
                    path: vec![Point2::new(1.0, 1.0), Point2::new(2.0, 2.0)],
                }],
                vias: vec![bridge::BoardVia {
                    connection: "SIG".to_owned(),
                    at: Point2::new(2.0, 2.0),
                    diameter: 0.5,
                    drill: 0.2,
                    span: bridge::BoardViaSpan::Partial {
                        from: 0,
                        to: 1,
                        micro: true,
                    },
                }],
            },
            layer_names: vec![
                "F.Cu".into(),
                "In1.Cu".into(),
                "In2.Cu".into(),
                "B.Cu".into(),
            ],
        });

        assert_eq!(snapshot.copper.traces[0].layer, LayerRef("inner1".into()));
        assert_eq!(
            snapshot.copper.vias[0].span,
            ViaSpan::Partial {
                from: 0,
                to: 1,
                micro: true
            }
        );
    }
}
