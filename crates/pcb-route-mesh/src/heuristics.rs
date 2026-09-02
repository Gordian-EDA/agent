use pcb_model::{Connection, LayerRef, Point2, RoutingView};

pub(crate) fn connection_span_um(conn: &Connection) -> u64 {
    let Some((min_x, max_x, min_y, max_y)) = connection_bbox(conn) else {
        return 0;
    };
    (((max_x - min_x) + (max_y - min_y)) * 1000.0).round() as u64
}

pub(crate) fn connection_obstacle_pressure_um(problem: &RoutingView, conn: &Connection) -> u64 {
    let Some((mut min_x, mut max_x, mut min_y, mut max_y)) = connection_bbox(conn) else {
        return 0;
    };
    let expand = problem.clearance + problem.net_width(&conn.name) / 2.0;
    min_x -= expand;
    max_x += expand;
    min_y -= expand;
    max_y += expand;

    let mut pressure = 0;
    for obstacle in &problem.obstacles {
        if obstacle.connected_to.iter().any(|net| net == &conn.name) {
            continue;
        }
        if !conn
            .points_to_connect
            .iter()
            .any(|pt| obstacle.layers.iter().any(|layer| layer == &pt.layer))
        {
            continue;
        }
        let ob_min_x = obstacle.center.x - obstacle.width / 2.0 - expand;
        let ob_max_x = obstacle.center.x + obstacle.width / 2.0 + expand;
        let ob_min_y = obstacle.center.y - obstacle.height / 2.0 - expand;
        let ob_max_y = obstacle.center.y + obstacle.height / 2.0 + expand;
        let overlap_x = max_x.min(ob_max_x) - min_x.max(ob_min_x);
        let overlap_y = max_y.min(ob_max_y) - min_y.max(ob_min_y);
        if overlap_x > 0.0 && overlap_y > 0.0 {
            let overlap_um = ((overlap_x + overlap_y) * 1000.0).round().max(0.0) as u64;
            pressure += 1_000_000 + overlap_um;
        }
    }
    pressure
}

pub(crate) fn connection_segment_obstacle_pressure_um(
    problem: &RoutingView,
    conn: &Connection,
) -> u64 {
    let expand = problem.clearance + problem.net_width(&conn.name) / 2.0;
    let mut pressure = 0;
    for segment in connection_tree_segments_for_pressure(conn) {
        let line = geom::Segment::new(segment.a, segment.b);
        for obstacle in &problem.obstacles {
            if obstacle.connected_to.iter().any(|net| net == &conn.name) {
                continue;
            }
            if !segment
                .layers
                .iter()
                .any(|layer| obstacle.layers.iter().any(|ob_layer| ob_layer == layer))
            {
                continue;
            }
            let obstacle_rect = geom::Rect::from_center_half(
                obstacle.center,
                (obstacle.width / 2.0, obstacle.height / 2.0),
            )
            .inflate(expand);
            if obstacle_rect.dist_to_segment(line) <= geom::EPS {
                let len_um = (segment.a.dist(segment.b) * 1000.0).round().max(0.0) as u64;
                pressure += 1_000_000 + len_um;
            }
        }
    }
    pressure
}

pub(crate) fn connection_crossing_pressures(problem: &RoutingView) -> Vec<usize> {
    let segments: Vec<Vec<TreeSegment>> = problem
        .connections
        .iter()
        .map(connection_tree_segments_for_pressure)
        .collect();
    let mut pressure = vec![0; problem.connections.len()];

    for i in 0..segments.len() {
        for j in i + 1..segments.len() {
            let mut crossings = 0usize;
            for a in &segments[i] {
                for b in &segments[j] {
                    if segment_layers_overlap(a, b) && segments_cross(a.a, a.b, b.a, b.b) {
                        crossings += 1;
                    }
                }
            }
            pressure[i] += crossings;
            pressure[j] += crossings;
        }
    }

    pressure
}

#[cfg(test)]
pub(crate) fn connection_crossing_pressure(problem: &RoutingView, idx: usize) -> usize {
    connection_crossing_pressures(problem)
        .get(idx)
        .copied()
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn connection_tree_segments(conn: &Connection) -> Vec<(Point2, Point2)> {
    connection_tree_segment_indices(conn)
        .into_iter()
        .map(|(a, b)| {
            (
                conn.points_to_connect[a].point(),
                conn.points_to_connect[b].point(),
            )
        })
        .collect()
}

#[derive(Clone)]
struct TreeSegment {
    a: Point2,
    b: Point2,
    layers: Vec<LayerRef>,
}

fn connection_tree_segments_for_pressure(conn: &Connection) -> Vec<TreeSegment> {
    connection_tree_segment_indices(conn)
        .into_iter()
        .map(|(a, b)| {
            let mut layers = vec![
                conn.points_to_connect[a].layer.clone(),
                conn.points_to_connect[b].layer.clone(),
            ];
            if layers[0] == layers[1] {
                layers.pop();
            }
            TreeSegment {
                a: conn.points_to_connect[a].point(),
                b: conn.points_to_connect[b].point(),
                layers,
            }
        })
        .collect()
}

fn segment_layers_overlap(a: &TreeSegment, b: &TreeSegment) -> bool {
    a.layers
        .iter()
        .any(|layer| b.layers.iter().any(|other| other == layer))
}

fn connection_tree_segment_indices(conn: &Connection) -> Vec<(usize, usize)> {
    match conn.points_to_connect.as_slice() {
        [] | [_] => Vec::new(),
        [_, _] => vec![(0, 1)],
        points => {
            let positions: Vec<Point2> = points.iter().map(|pt| pt.point()).collect();
            let mut segments = Vec::with_capacity(points.len().saturating_sub(1));
            let mut in_tree = vec![false; points.len()];
            in_tree[0] = true;
            for _ in 1..points.len() {
                let mut best: Option<(usize, usize)> = None;
                for (ai, &ai_in_tree) in in_tree.iter().enumerate() {
                    if !ai_in_tree {
                        continue;
                    }
                    for (bi, &bi_in_tree) in in_tree.iter().enumerate() {
                        if bi_in_tree {
                            continue;
                        }
                        let replace = best.is_none_or(|(old_a, old_b)| {
                            connection_tree_segment_pair_better(
                                conn, &positions, ai, bi, old_a, old_b,
                            )
                        });
                        if replace {
                            best = Some((ai, bi));
                        }
                    }
                }
                let Some((ai, bi)) = best else {
                    break;
                };
                in_tree[bi] = true;
                segments.push((ai, bi));
            }
            segments
        }
    }
}

fn connection_tree_segment_pair_better(
    conn: &Connection,
    positions: &[Point2],
    a: usize,
    b: usize,
    old_a: usize,
    old_b: usize,
) -> bool {
    let dist = positions[a].dist(positions[b]);
    let old_dist = positions[old_a].dist(positions[old_b]);
    if dist < old_dist - 1e-9 {
        return true;
    }
    if (dist - old_dist).abs() > 1e-9 {
        return false;
    }

    let layer_change = conn.points_to_connect[a].layer != conn.points_to_connect[b].layer;
    let old_layer_change =
        conn.points_to_connect[old_a].layer != conn.points_to_connect[old_b].layer;
    if layer_change != old_layer_change {
        return !layer_change;
    }

    (a, b) < (old_a, old_b)
}

pub(crate) fn segments_cross(a: Point2, b: Point2, c: Point2, d: Point2) -> bool {
    let orient = |p: Point2, q: Point2, r: Point2| {
        let v = (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x);
        if v.abs() < geom::EPS {
            0
        } else if v > 0.0 {
            1
        } else {
            -1
        }
    };
    orient(a, b, c) * orient(a, b, d) < 0 && orient(c, d, a) * orient(c, d, b) < 0
}

fn connection_bbox(conn: &Connection) -> Option<(f64, f64, f64, f64)> {
    let first = conn.points_to_connect.first()?;
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.x, first.x, first.y, first.y);
    for pt in &conn.points_to_connect {
        min_x = min_x.min(pt.x);
        max_x = max_x.max(pt.x);
        min_y = min_y.min(pt.y);
        max_y = max_y.max(pt.y);
    }
    Some((min_x, max_x, min_y, max_y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{LayerRef, Obstacle, Rect, RoutePoint};

    fn conn(name: &str, pts: &[(f64, f64, &str)]) -> Connection {
        Connection {
            name: name.to_owned(),
            points_to_connect: pts
                .iter()
                .map(|&(x, y, layer)| RoutePoint {
                    x,
                    y,
                    layer: LayerRef(layer.to_owned()),
                })
                .collect(),
        }
    }

    fn problem(connections: Vec<Connection>, obstacles: Vec<Obstacle>) -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections,
            bounds: Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 20.0,
                max_y: 20.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
        }
    }

    fn obstacle(center: (f64, f64), layer: &str, connected_to: &[&str]) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef(layer.to_owned())],
            center: Point2 {
                x: center.0,
                y: center.1,
            },
            width: 0.5,
            height: 0.5,
            connected_to: connected_to.iter().map(|net| (*net).to_owned()).collect(),
        }
    }

    #[test]
    fn connection_span_matches_bbox_half_perimeter_um() {
        let c = conn(
            "N",
            &[(1.0, 3.0, "top"), (7.0, 11.0, "top"), (5.0, 4.0, "top")],
        );

        assert_eq!(connection_span_um(&c), 14_000);
    }

    #[test]
    fn obstacle_pressure_ignores_own_pads_and_wrong_layers() {
        let p = problem(
            vec![conn("SIG", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")])],
            vec![
                obstacle((2.0, 1.0), "top", &["SIG"]),
                obstacle((2.0, 1.0), "bottom", &[]),
                obstacle((3.0, 1.0), "top", &[]),
            ],
        );

        assert!(
            connection_obstacle_pressure_um(&p, &p.connections[0]) > 1_000_000,
            "foreign obstacle on an active terminal layer should add pressure"
        );
    }

    #[test]
    fn segment_obstacle_pressure_tracks_tree_corridors_not_whole_bbox() {
        let p = problem(
            vec![conn(
                "BUS",
                &[(1.0, 1.0, "top"), (1.0, 9.0, "top"), (9.0, 9.0, "top")],
            )],
            vec![obstacle((8.0, 2.0), "top", &[])],
        );

        assert!(
            connection_obstacle_pressure_um(&p, &p.connections[0]) > 0,
            "bbox pressure should see the obstacle inside the broad net envelope"
        );
        assert_eq!(
            connection_segment_obstacle_pressure_um(&p, &p.connections[0]),
            0,
            "segment pressure should ignore obstacles away from the actual nearest-neighbour tree"
        );
    }

    #[test]
    fn tree_segment_indices_prefer_same_layer_edge_on_distance_tie() {
        let p = problem(
            vec![conn(
                "BUS",
                &[(2.0, 2.0, "top"), (12.0, 2.0, "bottom"), (2.0, 12.0, "top")],
            )],
            vec![],
        );

        let segments = connection_tree_segment_indices(&p.connections[0]);

        assert_eq!(
            segments.first().copied(),
            Some((0, 2)),
            "equal-length heuristic tree edges should prefer same-layer endpoints: {segments:?}"
        );
    }

    #[test]
    fn segment_obstacle_pressure_uses_layers_and_net_width() {
        let mut p = problem(
            vec![conn("SIG", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")])],
            vec![
                obstacle((5.0, 1.0), "bottom", &[]),
                obstacle((5.0, 1.6), "top", &[]),
            ],
        );

        assert_eq!(
            connection_segment_obstacle_pressure_um(&p, &p.connections[0]),
            0,
            "wrong-layer and thin-net-clear obstacles should not add segment pressure"
        );
        p.net_widths.insert("SIG".to_owned(), 1.0);
        assert!(
            connection_segment_obstacle_pressure_um(&p, &p.connections[0]) > 1_000_000,
            "wider active net should expand the segment corridor into the top-layer obstacle"
        );
    }

    #[test]
    fn crossing_pressure_counts_strict_two_pin_crossings() {
        let p = problem(
            vec![
                conn("H", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
                conn("V", &[(10.0, 2.0, "top"), (10.0, 18.0, "top")]),
                conn("OPEN", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")]),
            ],
            vec![],
        );

        assert_eq!(connection_crossing_pressure(&p, 0), 1);
        assert_eq!(connection_crossing_pressure(&p, 2), 0);
    }

    #[test]
    fn crossing_pressure_ignores_disjoint_layer_crossings() {
        let p = problem(
            vec![
                conn("TOP_H", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
                conn("BOT_V", &[(10.0, 2.0, "bottom"), (10.0, 18.0, "bottom")]),
                conn("MIXED_D", &[(2.0, 2.0, "top"), (18.0, 18.0, "bottom")]),
            ],
            vec![],
        );

        assert_eq!(
            connection_crossing_pressures(&p),
            vec![1, 1, 2],
            "same-layer and mixed-layer crossings should add pressure, but disjoint top/bottom crossings should not"
        );
    }

    #[test]
    fn crossing_pressure_counts_multi_pin_tree_crossings() {
        let p = problem(
            vec![
                conn(
                    "BUS",
                    &[(2.0, 10.0, "top"), (18.0, 10.0, "top"), (18.0, 14.0, "top")],
                ),
                conn("SIG", &[(10.0, 2.0, "top"), (10.0, 18.0, "top")]),
                conn("OPEN", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")]),
            ],
            vec![],
        );

        assert_eq!(connection_tree_segments(&p.connections[0]).len(), 2);
        assert_eq!(connection_crossing_pressures(&p), vec![1, 1, 0]);
        assert_eq!(connection_crossing_pressure(&p, 0), 1);
    }
}
