/// Load every .json file in crates/pcb-engine/fixtures/ and assert basic
/// sanity invariants for the RouteProblem model.
///
/// tscircuit-shape.json is a hand-authored file that uses only the upstream
/// SimpleRouteJson field set (layerCount, minTraceWidth, obstacles, connections,
/// bounds — no clearance/viaDiameter/viaDrill extension keys).  It mirrors the
/// shape of the tscircuit/autorouting benchmark dataset; the archived repo does
/// not contain a `datasets/` directory with SimpleRouteJson files, so a real
/// sample could not be retrieved.
use pcb_engine::problem::RouteProblem;
use std::path::Path;

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

#[test]
fn all_fixtures_parse_and_pass_sanity() {
    let dir = fixtures_dir();
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read fixtures dir {}: {}", dir.display(), e))
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .collect();

    assert!(
        !entries.is_empty(),
        "fixtures/ must contain at least one .json file"
    );

    for entry in entries {
        let path = entry.path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();

        // `place-*.json` are slice-4 PLACEMENT fixtures (`PlaceProblem`, not
        // `RouteProblem`) — a different model with no top-level obstacles /
        // connections. Their sanity is gated by `tests/placement_gate.rs`; this
        // RouteProblem sweep skips them rather than mis-parsing them.
        if name.starts_with("place-") {
            continue;
        }

        let json =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: read error: {e}"));

        let problem: RouteProblem = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("{name}: failed to parse as RouteProblem: {e}"));

        // 1. Bounds must be non-empty on both axes.
        let b = &problem.bounds;
        assert!(
            b.max_x > b.min_x,
            "{name}: bounds.maxX ({}) must be > bounds.minX ({})",
            b.max_x,
            b.min_x
        );
        assert!(
            b.max_y > b.min_y,
            "{name}: bounds.maxY ({}) must be > bounds.minY ({})",
            b.max_y,
            b.min_y
        );

        // 2. At least one connection.
        assert!(
            !problem.connections.is_empty(),
            "{name}: must have at least one connection"
        );

        // 3. Every connection point must lie inside bounds.
        for conn in &problem.connections {
            for (i, pt) in conn.points_to_connect.iter().enumerate() {
                assert!(
                    pt.x >= b.min_x && pt.x <= b.max_x,
                    "{name}: connection '{}' point {i} x={} is outside bounds [{}, {}]",
                    conn.name,
                    pt.x,
                    b.min_x,
                    b.max_x
                );
                assert!(
                    pt.y >= b.min_y && pt.y <= b.max_y,
                    "{name}: connection '{}' point {i} y={} is outside bounds [{}, {}]",
                    conn.name,
                    pt.y,
                    b.min_y,
                    b.max_y
                );
            }
        }

        // 4. Every connection point's layer must resolve to a valid index.
        for conn in &problem.connections {
            for (i, pt) in conn.points_to_connect.iter().enumerate() {
                assert!(
                    pt.layer.index(problem.layer_count).is_some(),
                    "{name}: connection '{}' point {i} has unresolvable layer '{}'",
                    conn.name,
                    pt.layer.0
                );
            }
        }

        // 5. Every obstacle's layers must all resolve to valid indices.
        for (oi, obs) in problem.obstacles.iter().enumerate() {
            for layer in &obs.layers {
                assert!(
                    layer.index(problem.layer_count).is_some(),
                    "{name}: obstacle {oi} has unresolvable layer '{}'",
                    layer.0
                );
            }
        }
    }
}
