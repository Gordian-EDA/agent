use pcb_engine::problem::{
    Bounds, Connection, LayerRef, Obstacle, Point2, RoutePoint, RouteProblem, RouteSolution, Trace,
    Via,
};

// ── (a) parse upstream-shaped JSON (no extension fields) ─────────────────────

#[test]
fn parse_upstream_json_applies_defaults() {
    // Verbatim SimpleRouteJson shape: no clearance / via_diameter / via_drill keys.
    let json = r#"{
        "layerCount": 2,
        "minTraceWidth": 0.1,
        "obstacles": [
            {
                "type": "rect",
                "layers": ["top"],
                "center": { "x": 5.0, "y": 3.0 },
                "width": 1.6,
                "height": 1.6,
                "connectedTo": ["GND"]
            }
        ],
        "connections": [
            {
                "name": "GND",
                "pointsToConnect": [
                    { "x": 5.0, "y": 3.0, "layer": "top" },
                    { "x": 10.0, "y": 3.0, "layer": "top" }
                ]
            }
        ],
        "bounds": { "minX": 0.0, "maxX": 20.0, "minY": 0.0, "maxY": 15.0 }
    }"#;

    let problem: RouteProblem = serde_json::from_str(json).expect("should parse");

    assert_eq!(problem.layer_count, 2);
    assert_eq!(problem.min_trace_width, 0.1);
    assert_eq!(problem.obstacles.len(), 1);
    assert_eq!(problem.connections.len(), 1);

    // Extension defaults must be applied.
    assert!(
        (problem.clearance - 0.2).abs() < 1e-9,
        "clearance default 0.2"
    );
    assert!(
        (problem.via_diameter - 0.6).abs() < 1e-9,
        "via_diameter default 0.6"
    );
    assert!(
        (problem.via_drill - 0.3).abs() < 1e-9,
        "via_drill default 0.3"
    );

    // Obstacle fields.
    let obs = &problem.obstacles[0];
    assert_eq!(obs.kind, "rect");
    assert_eq!(obs.layers, vec![LayerRef::top()]);
    assert_eq!(obs.connected_to, vec!["GND"]);

    // Bounds fields.
    assert_eq!(problem.bounds.min_x, 0.0);
    assert_eq!(problem.bounds.max_x, 20.0);
    assert_eq!(problem.bounds.min_y, 0.0);
    assert_eq!(problem.bounds.max_y, 15.0);
}

// ── (b) serialize → parse round-trip equality ─────────────────────────────────

#[test]
fn round_trip_problem() {
    let original = RouteProblem {
        layer_count: 4,
        min_trace_width: 0.15,
        clearance: 0.2,
        via_diameter: 0.6,
        via_drill: 0.3,
        net_widths: Default::default(),
        outline: None,
        obstacles: vec![Obstacle {
            kind: "oval".to_owned(),
            layers: vec![LayerRef::bottom()],
            center: Point2 { x: 1.0, y: 2.0 },
            width: 0.8,
            height: 1.2,
            connected_to: vec!["SIG".to_owned()],
        }],
        connections: vec![Connection {
            name: "SIG".to_owned(),
            points_to_connect: vec![
                RoutePoint {
                    x: 1.0,
                    y: 2.0,
                    layer: LayerRef::bottom(),
                },
                RoutePoint {
                    x: 5.0,
                    y: 2.0,
                    layer: LayerRef::top(),
                },
            ],
        }],
        bounds: Bounds {
            min_x: 0.0,
            max_x: 10.0,
            min_y: 0.0,
            max_y: 10.0,
        },
    };

    let json = serde_json::to_string(&original).expect("serialize");
    let parsed: RouteProblem = serde_json::from_str(&json).expect("parse");
    assert_eq!(original, parsed);
}

#[test]
fn round_trip_solution() {
    let original = RouteSolution {
        traces: vec![Trace {
            connection: "GND".to_owned(),
            layer: LayerRef::top(),
            width: 0.25,
            path: vec![Point2 { x: 0.0, y: 0.0 }, Point2 { x: 5.0, y: 0.0 }],
        }],
        vias: vec![Via {
            connection: "GND".to_owned(),
            at: Point2 { x: 5.0, y: 0.0 },
            diameter: 0.6,
            drill: 0.3,
        }],
    };

    let json = serde_json::to_string(&original).expect("serialize");
    let parsed: RouteSolution = serde_json::from_str(&json).expect("parse");
    assert_eq!(original, parsed);
}

// ── (c) unknown fields: tolerated on RouteProblem, rejected on RouteSolution ──

#[test]
fn unknown_fields_tolerated_on_route_problem() {
    let json = r#"{
        "layerCount": 2,
        "minTraceWidth": 0.1,
        "obstacles": [],
        "connections": [],
        "bounds": { "minX": 0.0, "maxX": 10.0, "minY": 0.0, "maxY": 10.0 },
        "futureExtension": "ignored",
        "anotherUnknown": 42
    }"#;

    // Must not error; unknown keys are silently ignored.
    let result: Result<RouteProblem, _> = serde_json::from_str(json);
    assert!(
        result.is_ok(),
        "RouteProblem should tolerate unknown fields"
    );
}

#[test]
fn unknown_fields_rejected_on_route_solution() {
    let json = r#"{
        "traces": [],
        "vias": [],
        "unknownField": "bad"
    }"#;

    let result: Result<RouteSolution, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "RouteSolution should reject unknown fields"
    );
}

// ── LayerRef::index mapping ───────────────────────────────────────────────────

#[test]
fn layer_ref_index_mapping() {
    assert_eq!(LayerRef::top().index(2), Some(0));
    assert_eq!(LayerRef::bottom().index(2), Some(1));
    assert_eq!(LayerRef::bottom().index(4), Some(3));

    // Inner layers on a 4-layer board.
    let inner1 = LayerRef("inner1".to_owned());
    let inner2 = LayerRef("inner2".to_owned());
    assert_eq!(inner1.index(4), Some(1));
    assert_eq!(inner2.index(4), Some(2));

    // Out of range: inner2 on a 2-layer board has no inner layers.
    assert_eq!(inner1.index(2), None);

    // Unknown name.
    assert_eq!(LayerRef("signal".to_owned()).index(4), None);
}
