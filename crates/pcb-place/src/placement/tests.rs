use super::anneal::{greedy_swap_polish_with_order, routing_aware_swap_order};
use super::cost::place_cost;
use super::cost::ratline_crossings;
use super::cost::ratline_obstruction_pressure;
use super::geometry::{
    EDGE_BAND, PLACE_GRID, PLACEMENT_GRID, courtyard_margin, rotated_copper_bbox,
    rotated_courtyard_half,
};
use super::hints::{apply_grid_hints, unified_fanout_place};
use super::legalize::is_legal;
use geom::Rect;
use pcb_place_api::{
    Edge, GroupHint, LockedAt, Part, PartPad, PlaceProblem, PlacementHints, derive_nets,
    series_pairs, to_route_problem,
};
use super::route::{
    EdgeLockedPlacer, GridAstarRanker, PlaceOpts, better_place_result,
    edge_seek_position_candidates, fanout_fast_path_accepts, net_centroid_position_candidates,
    obstructing_part_position_candidates, obstructing_part_position_candidates_from_edges, place,
    place_board, place_variant, placement_ranker_uses_bounded_pass,
    placement_ranker_uses_layout_only, polish_positions, polish_rotations, polish_swaps,
    position_polish_part_order, rank_key_with_full_grid_fallback,
    ratline_crossing_position_candidates, ratline_crossing_position_candidates_from_edges,
    ratline_obstruction_position_candidates, ratline_obstruction_position_candidates_from_edges,
    ratline_tree_edge_list, route_rank_key, route_rank_key_better, route_rank_key_clean_via_free,
    seat_corner_seek_parts, should_try_full_grid_ranker_fallback, swap_pair_order,
    unique_position_candidates,
};
use crate::connectivity;
use pcb_place_api::{Placer, RouteRanker, compute_hpwl, compute_hpwl_with_rotations};
use crate::problem::{Connection, LayerRef, Obstacle, Point2, Polygon, RoutePoint, RouteProblem};

fn board(w: f64, h: f64) -> Rect {
    Rect {
        min_x: 0.0,
        max_x: w,
        min_y: 0.0,
        max_y: h,
    }
}

fn top() -> Vec<LayerRef> {
    vec![LayerRef::top()]
}

fn square_outline() -> Polygon {
    Polygon::new(vec![
        Point2 { x: 5.0, y: 5.0 },
        Point2 { x: 15.0, y: 5.0 },
        Point2 { x: 15.0, y: 15.0 },
        Point2 { x: 5.0, y: 15.0 },
    ])
    .unwrap()
}

/// An R_0603-ish 2-pad part (crib numbers from the vendored footprint:
/// pads at ±0.825, 0.8×0.95). The courtyard here is the pad-enclosing one
/// (2.8×1.4): a courtyard MUST enclose its pads for courtyard-only
/// legalization to imply pad clearance — see the to_route_problem finding.
/// (The vendored R_0603 ships a tight body-hugging F.CrtYd of 1.6×0.825 that
/// does NOT enclose the ±1.225 pad span; using that here would let two
/// gap-legal courtyards still short foreign pads.)
fn r0603(reference: &str, pad1_net: Option<&str>, pad2_net: Option<&str>) -> Part {
    Part {
        reference: reference.to_owned(),
        courtyard_w: 2.8,
        courtyard_h: 1.4,
        pads: vec![
            PartPad {
                number: "1".to_owned(),
                offset: Point2 { x: -0.825, y: 0.0 },
                width: 0.8,
                height: 0.95,
                layers: top(),
                net: pad1_net.map(str::to_owned),
            },
            PartPad {
                number: "2".to_owned(),
                offset: Point2 { x: 0.825, y: 0.0 },
                width: 0.8,
                height: 0.95,
                layers: top(),
                net: pad2_net.map(str::to_owned),
            },
        ],
        edge_datum: None,
        locked: None,
    }
}

fn single_pad(reference: &str, net: &str, offset: Point2) -> Part {
    Part {
        reference: reference.to_owned(),
        courtyard_w: 20.0,
        courtyard_h: 20.0,
        pads: vec![PartPad {
            number: "1".to_owned(),
            offset,
            width: 0.8,
            height: 0.8,
            layers: top(),
            net: Some(net.to_owned()),
        }],
        edge_datum: None,
        locked: None,
    }
}

fn tiny_single_pad(reference: &str, net: &str, offset: Point2) -> Part {
    tiny_single_pad_on(reference, net, offset, top())
}

fn tiny_single_pad_on(reference: &str, net: &str, offset: Point2, layers: Vec<LayerRef>) -> Part {
    Part {
        reference: reference.to_owned(),
        courtyard_w: 1.0,
        courtyard_h: 1.0,
        pads: vec![PartPad {
            number: "1".to_owned(),
            offset,
            width: 0.4,
            height: 0.4,
            layers,
            net: Some(net.to_owned()),
        }],
        edge_datum: None,
        locked: None,
    }
}

fn place_at(p: &mut Part, x: f64, y: f64, rotation: f64) {
    p.locked = Some(LockedAt {
        at: Point2 { x, y },
        rotation,
    });
}

fn sorted_points(points: Vec<Point2>) -> Vec<(i64, i64)> {
    let mut out: Vec<_> = points
        .into_iter()
        .map(|p| ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64))
        .collect();
    out.sort_unstable();
    out
}

fn mechanical(reference: &str, size: f64) -> Part {
    Part {
        reference: reference.to_owned(),
        courtyard_w: size,
        courtyard_h: size,
        pads: Vec::new(),
        edge_datum: None,
        locked: None,
    }
}

fn placed_result(problem: &PlaceProblem, positions: &[Point2]) -> pcb_place_api::PlaceResult {
    pcb_place_api::PlaceResult {
        placements: problem
            .parts
            .iter()
            .zip(positions)
            .map(|(part, &at)| pcb_place_api::Placement {
                reference: part.reference.clone(),
                at,
                rotation: 0.0,
            })
            .collect(),
        legal: true,
        report: pcb_place_api::PlaceReport {
            overlaps_resolved: 0,
            out_of_bounds_clamps: 0,
            hpwl: 0.0,
            layout_cost: 0.0,
        },
    }
}

// ── empty hints: legal + deterministic ──────────────────────────────────

#[test]
fn empty_hints_small_board_is_legal_and_deterministic() {
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("R1", Some("A"), Some("B")),
            r0603("R2", Some("B"), Some("C")),
            r0603("R3", Some("C"), Some("A")),
        ],
        outline: None,
    };
    let hints = PlacementHints::default();
    let a = place(&problem, &hints);
    assert!(a.legal, "empty-hints placement must be legal: {a:?}");

    // Determinism: serialize twice, byte-equal (no RNG).
    let b = place(&problem, &hints);
    let ja = serde_json::to_string(&a).unwrap();
    let jb = serde_json::to_string(&b).unwrap();
    assert_eq!(ja, jb, "two place() runs must serialize byte-equal");
}

#[test]
fn placement_legalizer_moves_parts_out_of_keepouts() {
    let keepout = Rect {
        min_x: 0.0,
        max_x: 6.0,
        min_y: 0.0,
        max_y: 20.0,
    };
    let problem = PlaceProblem {
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![keepout],
        parts: vec![r0603("R1", Some("A"), Some("B"))],
        outline: None,
    };

    let result = place(&problem, &PlacementHints::default());
    assert!(result.legal, "free board space should be used: {result:?}");
    let courtyard = Rect::from_center_half(
        result.placements[0].at,
        rotated_courtyard_half(&problem.parts[0], result.placements[0].rotation),
    );
    let (ox, oy) = courtyard.axis_penetration(&keepout);
    assert!(ox <= geom::EPS || oy <= geom::EPS, "{courtyard:?}");
}

#[test]
fn placement_keepout_does_not_override_a_locked_footprint() {
    let locked_at = Point2 { x: 5.0, y: 5.0 };
    let mut locked = r0603("R1", Some("A"), Some("B"));
    locked.locked = Some(LockedAt {
        at: locked_at,
        rotation: 0.0,
    });
    let problem = PlaceProblem {
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![Rect::new(4.0, 4.0, 6.0, 6.0)],
        parts: vec![locked],
        outline: None,
    };

    let result = place(&problem, &PlacementHints::default());
    assert_eq!(result.placements[0].at, locked_at);
    assert!(
        !result.legal,
        "a conflicting user lock must be reported, not silently moved"
    );
}

#[test]
fn corner_seek_assigns_two_holes_together_instead_of_greedy_dead_end() {
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            mechanical("H1", 2.0),
            mechanical("H2", 6.0),
            mechanical("B1", 2.0),
            mechanical("B2", 2.0),
            mechanical("B3", 2.0),
        ],
        outline: None,
    };
    // H1 prefers top-left. H2 fits only top-left: the three blockers overlap
    // H2's other large corner seats but leave those same corners usable by H1.
    // A greedy H1-first pass strands H2; a joint assignment seats both.
    let mut result = placed_result(
        &problem,
        &[
            Point2 { x: 8.0, y: 4.0 },
            Point2 { x: 15.0, y: 10.0 },
            Point2 { x: 23.0, y: 3.0 },
            Point2 { x: 7.0, y: 17.0 },
            Point2 { x: 23.0, y: 17.0 },
        ],
    );
    let hints = PlacementHints {
        corner_seek: vec!["H1".into(), "H2".into()],
        ..PlacementHints::default()
    };

    seat_corner_seek_parts(&problem, &hints, &mut result);

    let h1 = result.placements[0].at;
    let h2 = result.placements[1].at;
    assert_eq!(h2, Point2 { x: 3.0, y: 3.0 });
    assert_ne!(h1, Point2 { x: 8.0, y: 4.0 });
    assert!(
        [
            Point2 { x: 1.0, y: 1.0 },
            Point2 { x: 29.0, y: 1.0 },
            Point2 { x: 1.0, y: 19.0 },
            Point2 { x: 29.0, y: 19.0 },
        ]
        .contains(&h1)
    );
    assert!(result.legal);
}

#[test]
fn corner_seek_places_four_holes_at_four_distinct_true_corners() {
    let problem = PlaceProblem {
        bounds: board(75.0, 55.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: (1..=4)
            .map(|number| mechanical(&format!("H{number}"), 7.4))
            .collect(),
        outline: None,
    };
    let mut result = placed_result(
        &problem,
        &[
            Point2 { x: 3.7, y: 11.0 },
            Point2 { x: 31.5, y: 3.7 },
            Point2 { x: 45.0, y: 30.0 },
            Point2 { x: 60.0, y: 40.0 },
        ],
    );
    let hints = PlacementHints {
        corner_seek: vec!["H4".into(), "H2".into(), "H1".into(), "H3".into()],
        ..PlacementHints::default()
    };

    seat_corner_seek_parts(&problem, &hints, &mut result);

    assert_eq!(
        sorted_points(result.placements.iter().map(|p| p.at).collect()),
        sorted_points(vec![
            Point2 { x: 3.7, y: 3.7 },
            Point2 { x: 71.3, y: 3.7 },
            Point2 { x: 3.7, y: 51.3 },
            Point2 { x: 71.3, y: 51.3 },
        ])
    );
}

#[test]
fn corner_seek_spreads_one_to_four_holes_stably() {
    let corners = [
        Point2 { x: 1.0, y: 1.0 },
        Point2 { x: 29.0, y: 1.0 },
        Point2 { x: 1.0, y: 19.0 },
        Point2 { x: 29.0, y: 19.0 },
    ];
    for count in 1..=4 {
        let problem = PlaceProblem {
            bounds: board(30.0, 20.0),
            clearance: 0.2,
            layer_count: 2,
            min_trace_width: 0.2,
            keepouts: vec![],
            parts: (1..=count)
                .map(|number| mechanical(&format!("H{number}"), 2.0))
                .collect(),
            outline: None,
        };
        let starts = (0..count)
            .map(|number| Point2 {
                x: 5.0 + number as f64 * 3.0,
                y: 15.0,
            })
            .collect::<Vec<_>>();
        let ascending = (1..=count)
            .map(|number| format!("H{number}"))
            .collect::<Vec<_>>();
        let mut descending = ascending.clone();
        descending.reverse();
        let mut forward = placed_result(&problem, &starts);
        let mut reverse = forward.clone();
        seat_corner_seek_parts(
            &problem,
            &PlacementHints {
                corner_seek: ascending,
                ..PlacementHints::default()
            },
            &mut forward,
        );
        seat_corner_seek_parts(
            &problem,
            &PlacementHints {
                corner_seek: descending,
                ..PlacementHints::default()
            },
            &mut reverse,
        );

        let positions = forward
            .placements
            .iter()
            .map(|placement| placement.at)
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            reverse.placements.iter().map(|p| p.at).collect::<Vec<_>>()
        );
        assert!(positions.iter().all(|position| corners.contains(position)));
        assert!(
            sorted_points(positions.clone())
                .windows(2)
                .all(|pair| pair[0] != pair[1])
        );
        if count == 2 {
            let indices = positions
                .iter()
                .map(|position| {
                    corners
                        .iter()
                        .position(|corner| corner == position)
                        .unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(indices[0] ^ indices[1], 3, "two holes must use a diagonal");
        }
    }
}

#[test]
fn corner_seek_preserves_authored_locks_and_regions() {
    let authored = Point2 { x: 8.0, y: 7.0 };
    let mut locked = mechanical("H1", 2.0);
    locked.locked = Some(LockedAt {
        at: authored,
        rotation: 0.0,
    });
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![locked, mechanical("H2", 2.0)],
        outline: None,
    };
    let mut result = placed_result(&problem, &[authored, Point2 { x: 15.0, y: 10.0 }]);
    let hints = PlacementHints {
        groups: vec![GroupHint {
            name: "authored hole region".into(),
            members: vec!["H2".into()],
            region: Some(Rect::new(12.0, 8.0, 18.0, 12.0)),
            edge: None,
            grid: false,
            rotation: None,
            surround: None,
        }],
        corner_seek: vec!["H1".into(), "H2".into()],
        ..PlacementHints::default()
    };

    seat_corner_seek_parts(&problem, &hints, &mut result);

    assert_eq!(result.placements[0].at, authored);
    assert_eq!(result.placements[1].at, Point2 { x: 15.0, y: 10.0 });
}

// ── series co-placement detection ───────────────────────────────────────

/// An anchor with `npads` pads, pad `Pi` on net `Si` (so each is a 1-pin net
/// until something else taps it). Courtyard sized to enclose the pad span.
fn dense_anchor(reference: &str, npads: usize) -> Part {
    let pads = (0..npads)
        .map(|i| PartPad {
            number: format!("P{i}"),
            offset: Point2 {
                x: i as f64 * 0.5,
                y: 0.0,
            },
            width: 0.3,
            height: 0.3,
            layers: top(),
            net: Some(format!("S{i}")),
        })
        .collect();
    Part {
        reference: reference.to_owned(),
        courtyard_w: npads as f64 * 0.5 + 1.0,
        courtyard_h: 2.0,
        pads,
        edge_datum: None,
        locked: None,
    }
}

#[test]
fn series_pairs_fires_only_for_a_2pin_tap_to_a_dense_anchor() {
    // R1.pad1 shares the 2-pin net S0 with a 16-pad anchor; pad2 ("OUT") dangles
    // to a header. This is a true series tap off a dense package → should pair.
    let dense = PlaceProblem {
        bounds: board(40.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![r0603("R1", Some("S0"), Some("OUT")), dense_anchor("U1", 16)],
        outline: None,
    };
    assert_eq!(
        series_pairs(&dense),
        vec![(0, 1)],
        "R1 should co-place with the 16-pad anchor U1"
    );

    // Same topology but the anchor has only 3 pads — below the escape-critical
    // threshold, so series co-placement must NOT fire (it perturbs clean boards).
    let small = PlaceProblem {
        parts: vec![r0603("R2", Some("S0"), Some("OUT")), dense_anchor("U2", 3)],
        ..dense
    };
    assert!(
        series_pairs(&small).is_empty(),
        "a 3-pad anchor is below SERIES_ANCHOR_MIN_PADS — no series pair"
    );
}

// ── locked parts never move ─────────────────────────────────────────────

#[test]
fn locked_part_does_not_move() {
    let mut locked = r0603("R1", Some("A"), Some("B"));
    place_at(&mut locked, 7.5, 12.0, 90.0);
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            locked,
            r0603("R2", Some("B"), Some("C")),
            r0603("R3", Some("C"), Some("A")),
        ],
        outline: None,
    };
    let res = place(&problem, &PlacementHints::default());
    let r1 = res.placements.iter().find(|p| p.reference == "R1").unwrap();
    assert_eq!(r1.at, Point2 { x: 7.5, y: 12.0 }, "locked R1 must stay put");
    assert_eq!(r1.rotation, 90.0, "locked rotation preserved");
    assert!(res.legal, "board with a locked part still legal: {res:?}");
}

#[test]
fn locked_anchor_with_unlocked_caps_does_not_move() {
    // A LOCKED IC (≥3-pad decoupling anchor) carrying UNLOCKED bypass caps must
    // not be dragged by the annealer's block-move (which rigidly shifts an anchor
    // + its caps). The cap-anchor cohesion still clusters the caps around the
    // fixed IC; only unlocked anchors may be block-shifted.
    let mut ic = Part {
        reference: "U1".to_owned(),
        courtyard_w: 3.0,
        courtyard_h: 3.0,
        pads: vec![
            PartPad {
                number: "1".to_owned(),
                offset: Point2 { x: -1.0, y: 0.0 },
                width: 0.6,
                height: 0.6,
                layers: top(),
                net: Some("VCC".to_owned()),
            },
            PartPad {
                number: "2".to_owned(),
                offset: Point2 { x: 1.0, y: 0.0 },
                width: 0.6,
                height: 0.6,
                layers: top(),
                net: Some("GND".to_owned()),
            },
            PartPad {
                number: "3".to_owned(),
                offset: Point2 { x: 0.0, y: 1.0 },
                width: 0.6,
                height: 0.6,
                layers: top(),
                net: Some("OUT".to_owned()),
            },
        ],
        edge_datum: None,
        locked: None,
    };
    place_at(&mut ic, 4.0, 10.0, 0.0);
    // A LOCKED sink on U1's OUT net, pinned far to the right: the only way the
    // annealer can shorten the OUT net is to block-shift the (locked) U1 cluster
    // rightward — which it must NOT do. (Both ends locked → the net length is
    // fixed and the lock wins.)
    let mut sink = r0603("R3", Some("OUT"), Some("GND"));
    place_at(&mut sink, 26.0, 10.0, 0.0);
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            ic,
            r0603("C1", Some("VCC"), Some("GND")),
            r0603("C2", Some("VCC"), Some("GND")),
            sink,
        ],
        outline: None,
    };
    let res = place_variant(
        &problem,
        &PlacementHints::default(),
        PlaceOpts {
            anneal: true,
            aspect_edge: false,
            decouple: false,
        },
    );
    let u1 = res.placements.iter().find(|p| p.reference == "U1").unwrap();
    assert_eq!(
        u1.at,
        Point2 { x: 4.0, y: 10.0 },
        "locked anchor U1 must stay put despite carrying unlocked caps + a far net sink: {res:?}"
    );
    assert!(res.legal, "{res:?}");
}

// ── connected parts end closer than unconnected ─────────────────────────

#[test]
fn connected_parts_end_closer_than_unconnected() {
    // R1 and R9 share net "L" but sort to opposite ends of the deterministic
    // initial grid (refs are placed in sorted order across a near-square
    // grid). The net spring must pull them together so the connected pair
    // ends MUCH closer than the unconnected pair (R3, R7) that the grid keeps
    // apart. This proves the spring overcomes the seed, not that any two
    // adjacent grid cells differ.
    let problem = PlaceProblem {
        bounds: board(60.0, 50.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("R1", Some("L"), Some("P1")),
            r0603("R2", None, None),
            r0603("R3", None, None),
            r0603("R4", None, None),
            r0603("R5", None, None),
            r0603("R6", None, None),
            r0603("R7", None, None),
            r0603("R8", None, None),
            r0603("R9", Some("L"), Some("P2")),
        ],
        outline: None,
    };
    let res = place(&problem, &PlacementHints::default());
    assert!(res.legal, "{res:?}");
    let at = |r: &str| {
        res.placements
            .iter()
            .find(|p| p.reference == r)
            .map(|p| p.at)
            .unwrap()
    };
    let d = |a: Point2, b: Point2| a.dist(b);
    let connected = d(at("R1"), at("R9"));
    // Two parts the grid seeds at opposite ends and that no net pulls together.
    let unconnected = d(at("R3"), at("R7"));
    assert!(
        connected < unconnected,
        "connected R1-R9 ({connected:.2}) must be closer than unconnected R3-R7 ({unconnected:.2})"
    );
}

#[test]
fn ratline_crossing_proxy_counts_only_true_two_pin_crossings() {
    let problem = PlaceProblem {
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("A", Some("N1"), None),
            r0603("B", Some("N2"), None),
            r0603("C", Some("N2"), None),
            r0603("D", Some("N1"), None),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let crossed = vec![
        Point2 { x: 0.0, y: 0.0 },
        Point2 { x: 10.0, y: 0.0 },
        Point2 { x: 0.0, y: 10.0 },
        Point2 { x: 10.0, y: 10.0 },
    ];
    let untangled = vec![
        Point2 { x: 0.0, y: 0.0 },
        Point2 { x: 10.0, y: 0.0 },
        Point2 { x: 10.0, y: 10.0 },
        Point2 { x: 0.0, y: 10.0 },
    ];

    let rotations = vec![0.0; problem.parts.len()];
    assert_eq!(ratline_crossings(&problem, &rotations, &nets, &crossed), 1);
    assert_eq!(
        ratline_crossings(&problem, &rotations, &nets, &untangled),
        0
    );
}

#[test]
fn ratline_crossing_proxy_uses_physical_pad_positions() {
    let problem = PlaceProblem {
        bounds: board(40.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            single_pad("A", "N1", Point2 { x: 8.0, y: 0.0 }),
            single_pad("B", "N2", Point2 { x: -8.0, y: 0.0 }),
            single_pad("C", "N2", Point2 { x: 8.0, y: 0.0 }),
            single_pad("D", "N1", Point2 { x: -8.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let pos = vec![
        Point2 { x: 8.0, y: 5.0 },
        Point2 { x: 24.0, y: 5.0 },
        Point2 { x: 24.0, y: 25.0 },
        Point2 { x: 8.0, y: 25.0 },
    ];

    // Centre ratlines A-D and B-C are parallel verticals, but the actual pad
    // endpoints form an X. The routeability proxy must see the physical pads.
    assert!(!geom::Segment::new(pos[0], pos[3]).intersects(geom::Segment::new(pos[1], pos[2])));
    assert_eq!(ratline_crossings(&problem, &rotations, &nets, &pos), 1);
}

#[test]
fn ratline_crossing_proxy_counts_multi_pin_tree_crossings() {
    let problem = PlaceProblem {
        bounds: board(40.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad("A", "BUS", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("B", "BUS", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("C", "BUS", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("D", "SIG", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("E", "SIG", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let crossed = vec![
        Point2 { x: 0.0, y: 0.0 },
        Point2 { x: 10.0, y: 0.0 },
        Point2 { x: 10.0, y: 10.0 },
        Point2 { x: 5.0, y: -5.0 },
        Point2 { x: 5.0, y: 5.0 },
    ];
    let untangled = vec![
        Point2 { x: 0.0, y: 0.0 },
        Point2 { x: 10.0, y: 0.0 },
        Point2 { x: 10.0, y: 10.0 },
        Point2 { x: 15.0, y: -5.0 },
        Point2 { x: 15.0, y: 5.0 },
    ];

    assert_eq!(
        ratline_crossings(&problem, &rotations, &nets, &crossed),
        1,
        "BUS should contribute a deterministic tree edge that crosses SIG"
    );
    assert_eq!(
        ratline_crossings(&problem, &rotations, &nets, &untangled),
        0
    );
}

#[test]
fn ratline_obstruction_pressure_counts_foreign_parts_and_keepouts() {
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![Rect {
            min_x: 20.0,
            max_x: 22.0,
            min_y: 4.0,
            max_y: 6.0,
        }],
        parts: vec![
            tiny_single_pad("A", "N", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("B", "N", Point2 { x: 0.0, y: 0.0 }),
            r0603("X", None, None),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let blocked = vec![
        Point2 { x: 2.0, y: 5.0 },
        Point2 { x: 28.0, y: 5.0 },
        Point2 { x: 12.0, y: 5.0 },
    ];
    let clear = vec![
        Point2 { x: 2.0, y: 5.0 },
        Point2 { x: 28.0, y: 5.0 },
        Point2 { x: 12.0, y: 12.0 },
    ];

    assert_eq!(
        ratline_obstruction_pressure(&problem, &rotations, &nets, &half, margin, &blocked),
        2,
        "foreign component plus keepout should both add routing pressure"
    );
    assert_eq!(
        ratline_obstruction_pressure(&problem, &rotations, &nets, &half, margin, &clear),
        1,
        "moving the unrelated part off the corridor leaves only keepout pressure"
    );
}

#[test]
fn greedy_swap_polish_untangles_crossed_two_pin_ratlines() {
    let problem = PlaceProblem {
        bounds: board(30.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("A", Some("N1"), None),
            r0603("B", Some("N2"), None),
            r0603("C", Some("N2"), None),
            r0603("D", Some("N1"), None),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pairs = Vec::new();
    let edge_idx = Vec::new();
    let mut pos = vec![
        Point2 { x: 5.0, y: 5.0 },
        Point2 { x: 25.0, y: 5.0 },
        Point2 { x: 5.0, y: 25.0 },
        Point2 { x: 25.0, y: 25.0 },
    ];
    let cost_of = |p: &[Point2]| {
        place_cost(
            &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, p,
        )
    };
    let mut cost = cost_of(&pos);
    assert_eq!(ratline_crossings(&problem, &rotations, &nets, &pos), 1);

    let all_pairs: Vec<(usize, usize)> = (0..4).flat_map(|a| (a + 1..4).map(move |b| (a, b))).collect();
    greedy_swap_polish_with_order(&problem, &half, &all_pairs, &mut pos, &mut cost, cost_of);

    assert_eq!(ratline_crossings(&problem, &rotations, &nets, &pos), 0);
}

#[test]
fn anneal_swap_order_prioritizes_connected_and_crossing_pairs() {
    let mut locked = r0603("LOCK", Some("N3"), None);
    locked.locked = Some(LockedAt {
        at: Point2 { x: 15.0, y: 15.0 },
        rotation: 0.0,
    });
    let problem = PlaceProblem {
        bounds: board(40.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("A", Some("N1"), None),
            r0603("B", Some("N2"), None),
            r0603("C", Some("N2"), None),
            r0603("D", Some("N1"), None),
            r0603("E", None, None),
            r0603("F", None, None),
            locked,
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let pos = vec![
        Point2 { x: 5.0, y: 5.0 },
        Point2 { x: 35.0, y: 5.0 },
        Point2 { x: 5.0, y: 35.0 },
        Point2 { x: 35.0, y: 35.0 },
        Point2 { x: 20.0, y: 20.0 },
        Point2 { x: 8.0, y: 20.0 },
        Point2 { x: 15.0, y: 15.0 },
    ];
    let order = routing_aware_swap_order(&problem, &nets, &rotations, &pos, &[0, 1, 2, 3, 4, 5]);

    assert_eq!(
        &order[..2],
        &[(0, 3), (1, 2)],
        "same-net pairs should be polished first"
    );
    let crossing = order
        .iter()
        .position(|pair| *pair == (0, 1))
        .expect("crossing-related pair should be present");
    let obstructing = order
        .iter()
        .position(|pair| *pair == (0, 4))
        .expect("obstructing unrelated part should still be present");
    assert!(
        crossing < obstructing,
        "crossed ratline endpoints should be tried before obstruction fallback pairs: {order:?}"
    );
    let fallback = order
        .iter()
        .position(|pair| *pair == (0, 5))
        .expect("non-obstructing unrelated part should still be present");
    assert!(
        obstructing < fallback,
        "foreign parts sitting in a ratline corridor should be swapped before unrelated fallback pairs: {order:?}"
    );
    assert!(
        order.iter().all(|(a, b)| *a != 6 && *b != 6),
        "locked parts excluded from movable set must not appear in anneal polish"
    );
}

#[test]
fn anneal_swap_order_ignores_disjoint_layer_crossings() {
    let problem = PlaceProblem {
        bounds: board(40.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad_on("A", "TOP", Point2 { x: 0.0, y: 0.0 }, vec![LayerRef::top()]),
            tiny_single_pad_on("B", "TOP", Point2 { x: 0.0, y: 0.0 }, vec![LayerRef::top()]),
            tiny_single_pad_on(
                "C",
                "BOTTOM",
                Point2 { x: 0.0, y: 0.0 },
                vec![LayerRef::bottom()],
            ),
            tiny_single_pad_on(
                "D",
                "BOTTOM",
                Point2 { x: 0.0, y: 0.0 },
                vec![LayerRef::bottom()],
            ),
            tiny_single_pad("E", "FLOAT", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let pos = vec![
        Point2 { x: 5.0, y: 20.0 },
        Point2 { x: 35.0, y: 20.0 },
        Point2 { x: 20.0, y: 5.0 },
        Point2 { x: 20.0, y: 35.0 },
        Point2 { x: 20.0, y: 20.0 },
    ];

    let order = routing_aware_swap_order(&problem, &nets, &rotations, &pos, &[0, 1, 2, 3, 4]);

    let disjoint_crossing = order
        .iter()
        .position(|pair| *pair == (0, 2))
        .expect("geometrically crossing but layer-disjoint endpoint pair should be present");
    let obstruction = order
        .iter()
        .position(|pair| *pair == (0, 4))
        .expect("obstructing unrelated part should be present");
    assert!(
        obstruction < disjoint_crossing,
        "layer-disjoint ratline crossings should not outrank true obstruction relief: {order:?}"
    );
}

#[test]
fn position_polish_order_prioritizes_crossing_parts_before_obstructors() {
    let problem = PlaceProblem {
        bounds: board(40.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad("A", "N1", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("B", "N2", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("C", "N2", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("D", "N1", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("X", "FLOAT", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pos = vec![
        Point2 { x: 5.0, y: 5.0 },
        Point2 { x: 35.0, y: 5.0 },
        Point2 { x: 5.0, y: 35.0 },
        Point2 { x: 35.0, y: 35.0 },
        Point2 { x: 20.0, y: 20.0 },
    ];

    let order = position_polish_part_order(&problem, &nets, &rotations, &half, margin, &pos);

    assert_eq!(
        &order[..4],
        &[0, 1, 2, 3],
        "crossing ratline endpoints should be polished before a passive obstructor: {order:?}"
    );
    assert_eq!(order[4], 4);
}

#[test]
fn position_polish_order_ignores_disjoint_layer_crossings() {
    let problem = PlaceProblem {
        bounds: board(40.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad_on("A", "TOP", Point2 { x: 0.0, y: 0.0 }, vec![LayerRef::top()]),
            tiny_single_pad_on("B", "TOP", Point2 { x: 0.0, y: 0.0 }, vec![LayerRef::top()]),
            tiny_single_pad_on(
                "C",
                "BOTTOM",
                Point2 { x: 0.0, y: 0.0 },
                vec![LayerRef::bottom()],
            ),
            tiny_single_pad_on(
                "D",
                "BOTTOM",
                Point2 { x: 0.0, y: 0.0 },
                vec![LayerRef::bottom()],
            ),
            tiny_single_pad("X", "FLOAT", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pos = vec![
        Point2 { x: 5.0, y: 20.0 },
        Point2 { x: 35.0, y: 20.0 },
        Point2 { x: 20.0, y: 5.0 },
        Point2 { x: 20.0, y: 35.0 },
        Point2 { x: 20.0, y: 20.0 },
    ];

    let order = position_polish_part_order(&problem, &nets, &rotations, &half, margin, &pos);

    assert_eq!(
        order[0], 4,
        "real obstruction pressure should outrank layer-disjoint geometric crossings: {order:?}"
    );
}

#[test]
fn position_polish_takes_legal_grid_step_that_lowers_cost() {
    let problem = PlaceProblem {
        bounds: board(30.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad("A", "N", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("B", "N", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<crate::problem::Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pairs = Vec::new();
    let edge_idx = Vec::new();
    let mut pos = vec![Point2 { x: 5.0, y: 5.0 }, Point2 { x: 10.0, y: 5.0 }];
    let before = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    let before_dist = pos[0].dist(pos[1]);

    polish_positions(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );

    let after = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    assert!(
        after + 1e-9 < before,
        "position polish should lower cost: {before} -> {after}"
    );
    assert!(
        pos[0].dist(pos[1]) < before_dist,
        "connected parts should move closer: {before_dist} -> {}",
        pos[0].dist(pos[1])
    );
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn position_polish_can_jump_over_narrow_illegal_band() {
    let mut b = tiny_single_pad("B", "N", Point2 { x: 0.0, y: 0.0 });
    place_at(&mut b, 12.0, 5.0, 0.0);
    let problem = PlaceProblem {
        bounds: board(20.0, 10.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![Rect {
            min_x: 5.55,
            max_x: 5.95,
            min_y: 0.0,
            max_y: 10.0,
        }],
        parts: vec![tiny_single_pad("A", "N", Point2 { x: 0.0, y: 0.0 }), b],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pairs = Vec::new();
    let edge_idx = Vec::new();
    let mut pos = vec![Point2 { x: 5.0, y: 5.0 }, Point2 { x: 12.0, y: 5.0 }];
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
    let before = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    let jumped = vec![Point2 { x: 7.0, y: 5.0 }, Point2 { x: 12.0, y: 5.0 }];
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &jumped));
    let jumped_cost = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &jumped,
    );
    assert!(
        jumped_cost + 1e-9 < before,
        "fixture should make the legal jump cheaper: {before} -> {jumped_cost}"
    );

    polish_positions(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );

    let after = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    assert!(
        pos[0].x > 5.95,
        "A should jump across the narrow illegal band, got {:?}",
        pos[0]
    );
    assert!(
        after + 1e-9 < before,
        "coarse position polish should lower cost: {before} -> {after}"
    );
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn position_polish_can_take_ratline_crossing_relief_move() {
    let mut a = tiny_single_pad("A", "N1", Point2 { x: 0.0, y: 0.0 });
    let mut c = tiny_single_pad("C", "N2", Point2 { x: 0.0, y: 0.0 });
    let mut d = tiny_single_pad("D", "N2", Point2 { x: 0.0, y: 0.0 });
    place_at(&mut a, 5.0, 5.0, 0.0);
    place_at(&mut c, 5.0, 25.0, 0.0);
    place_at(&mut d, 25.0, 5.0, 0.0);
    let problem = PlaceProblem {
        bounds: board(30.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            a,
            tiny_single_pad("B", "N1", Point2 { x: 0.0, y: 0.0 }),
            c,
            d,
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pairs = Vec::new();
    let edge_idx = Vec::new();
    let mut pos = vec![
        Point2 { x: 5.0, y: 5.0 },
        Point2 { x: 25.0, y: 25.0 },
        Point2 { x: 5.0, y: 25.0 },
        Point2 { x: 25.0, y: 5.0 },
    ];
    assert_eq!(ratline_crossings(&problem, &rotations, &nets, &pos), 1);

    let relief = ratline_crossing_position_candidates(&problem, &nets, &rotations, &pos, 1);
    assert!(
        relief.iter().any(|candidate| {
            let mut candidate_pos = pos.clone();
            candidate_pos[1] = *candidate;
            is_legal(&problem, &half, &copper_bbox, margin, &candidate_pos)
                && ratline_crossings(&problem, &rotations, &nets, &candidate_pos) == 0
        }),
        "crossing-relief candidates should include a legal uncrossing move: {relief:?}"
    );

    let before = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    polish_positions(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );
    let after = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );

    assert_eq!(ratline_crossings(&problem, &rotations, &nets, &pos), 0);
    assert!(
        pos[1].x < 16.0 && pos[1].y < 16.0,
        "B should take a long crossing-relief move, got {:?}",
        pos[1]
    );
    assert!(
        after + 1e-9 < before,
        "crossing-relief move should lower placement cost: {before} -> {after}"
    );
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn crossing_relief_candidates_include_multi_pin_tree_edges() {
    let problem = PlaceProblem {
        bounds: board(40.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad("A", "BUS", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("B", "BUS", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("C", "BUS", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("D", "SIG", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("E", "SIG", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pos = vec![
        Point2 { x: 5.0, y: 15.0 },
        Point2 { x: 15.0, y: 15.0 },
        Point2 { x: 5.0, y: 25.0 },
        Point2 { x: 10.0, y: 10.0 },
        Point2 { x: 10.0, y: 20.0 },
    ];
    assert_eq!(
        ratline_crossings(&problem, &rotations, &nets, &pos),
        1,
        "BUS tree edge A-B should cross SIG"
    );

    let relief = ratline_crossing_position_candidates(&problem, &nets, &rotations, &pos, 1);

    assert!(
        relief.iter().any(|candidate| {
            let mut candidate_pos = pos.clone();
            candidate_pos[1] = *candidate;
            is_legal(&problem, &half, &copper_bbox, margin, &candidate_pos)
                && ratline_crossings(&problem, &rotations, &nets, &candidate_pos) == 0
        }),
        "multi-pin BUS tree crossing should produce a legal uncrossing candidate: {relief:?}"
    );
}

#[test]
fn crossing_relief_candidates_ignore_disjoint_pad_layers() {
    let problem = PlaceProblem {
        bounds: board(30.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad_on("A", "TOP", Point2 { x: 0.0, y: 0.0 }, vec![LayerRef::top()]),
            tiny_single_pad_on("B", "TOP", Point2 { x: 0.0, y: 0.0 }, vec![LayerRef::top()]),
            tiny_single_pad_on(
                "C",
                "BOTTOM",
                Point2 { x: 0.0, y: 0.0 },
                vec![LayerRef::bottom()],
            ),
            tiny_single_pad_on(
                "D",
                "BOTTOM",
                Point2 { x: 0.0, y: 0.0 },
                vec![LayerRef::bottom()],
            ),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let pos = vec![
        Point2 { x: 5.0, y: 15.0 },
        Point2 { x: 25.0, y: 15.0 },
        Point2 { x: 15.0, y: 5.0 },
        Point2 { x: 15.0, y: 25.0 },
    ];
    assert_eq!(
        ratline_crossings(&problem, &rotations, &nets, &pos),
        0,
        "placement cost should not count top-only versus bottom-only crossings"
    );

    let relief = ratline_crossing_position_candidates(&problem, &nets, &rotations, &pos, 1);

    assert!(
        relief.is_empty(),
        "crossing-relief candidates should ignore false conflicts on disjoint pad layers: {relief:?}"
    );
}

#[test]
fn crossing_relief_candidates_keep_mixed_layer_conflicts() {
    let problem = PlaceProblem {
        bounds: board(30.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad_on("A", "TOP", Point2 { x: 0.0, y: 0.0 }, vec![LayerRef::top()]),
            tiny_single_pad_on("B", "TOP", Point2 { x: 0.0, y: 0.0 }, vec![LayerRef::top()]),
            tiny_single_pad_on("C", "MIX", Point2 { x: 0.0, y: 0.0 }, vec![LayerRef::top()]),
            tiny_single_pad_on(
                "D",
                "MIX",
                Point2 { x: 0.0, y: 0.0 },
                vec![LayerRef::bottom()],
            ),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pos = vec![
        Point2 { x: 5.0, y: 15.0 },
        Point2 { x: 25.0, y: 15.0 },
        Point2 { x: 5.0, y: 5.0 },
        Point2 { x: 25.0, y: 25.0 },
    ];
    assert_eq!(ratline_crossings(&problem, &rotations, &nets, &pos), 1);

    let relief = ratline_crossing_position_candidates(&problem, &nets, &rotations, &pos, 1);

    assert!(
        relief.iter().any(|candidate| {
            let mut candidate_pos = pos.clone();
            candidate_pos[1] = *candidate;
            is_legal(&problem, &half, &copper_bbox, margin, &candidate_pos)
                && ratline_crossings(&problem, &rotations, &nets, &candidate_pos) == 0
        }),
        "mixed-layer ratline crossings should still produce a legal relief candidate: {relief:?}"
    );
}

#[test]
fn position_polish_can_take_ratline_obstruction_relief_move() {
    let mut a = tiny_single_pad("A", "N", Point2 { x: 0.0, y: 0.0 });
    let mut blocker = tiny_single_pad("X", "FLOAT", Point2 { x: 0.0, y: 0.0 });
    place_at(&mut a, 5.0, 10.0, 0.0);
    place_at(&mut blocker, 15.0, 10.0, 0.0);
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            a,
            tiny_single_pad("B", "N", Point2 { x: 0.0, y: 0.0 }),
            blocker,
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pairs = Vec::new();
    let edge_idx = Vec::new();
    let mut pos = vec![
        Point2 { x: 5.0, y: 10.0 },
        Point2 { x: 25.0, y: 10.0 },
        Point2 { x: 15.0, y: 10.0 },
    ];
    assert_eq!(
        ratline_obstruction_pressure(&problem, &rotations, &nets, &half, margin, &pos),
        1
    );

    let relief = ratline_obstruction_position_candidates(
        &problem, &nets, &rotations, &half, margin, &pos, 1,
    );
    assert!(
        relief.iter().any(|candidate| {
            let mut candidate_pos = pos.clone();
            candidate_pos[1] = *candidate;
            is_legal(&problem, &half, &copper_bbox, margin, &candidate_pos)
                && ratline_obstruction_pressure(
                    &problem,
                    &rotations,
                    &nets,
                    &half,
                    margin,
                    &candidate_pos,
                ) == 0
        }),
        "obstruction-relief candidates should include a legal unobstructed move: {relief:?}"
    );

    let before = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    polish_positions(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );
    let after = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );

    assert_eq!(
        ratline_obstruction_pressure(&problem, &rotations, &nets, &half, margin, &pos),
        0
    );
    assert!(
        pos[1].dist(Point2 { x: 25.0, y: 10.0 }) > 1.0,
        "B should move out of the obstructed straight corridor, got {:?}",
        pos[1]
    );
    assert!(
        after + 1e-9 < before,
        "obstruction-relief move should lower placement cost: {before} -> {after}"
    );
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn position_polish_can_move_foreign_obstructor_off_ratline() {
    let mut a = tiny_single_pad("A", "N", Point2 { x: 0.0, y: 0.0 });
    let mut b = tiny_single_pad("B", "N", Point2 { x: 0.0, y: 0.0 });
    place_at(&mut a, 5.0, 10.0, 0.0);
    place_at(&mut b, 25.0, 10.0, 0.0);
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            a,
            b,
            tiny_single_pad("X", "FLOAT", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pairs = Vec::new();
    let edge_idx = Vec::new();
    let mut pos = vec![
        Point2 { x: 5.0, y: 10.0 },
        Point2 { x: 25.0, y: 10.0 },
        Point2 { x: 15.0, y: 10.0 },
    ];
    assert_eq!(
        ratline_obstruction_pressure(&problem, &rotations, &nets, &half, margin, &pos),
        1
    );

    let relief =
        obstructing_part_position_candidates(&problem, &nets, &rotations, &half, margin, &pos, 2);
    assert!(
        relief.iter().any(|candidate| {
            let mut candidate_pos = pos.clone();
            candidate_pos[2] = *candidate;
            is_legal(&problem, &half, &copper_bbox, margin, &candidate_pos)
                && ratline_obstruction_pressure(
                    &problem,
                    &rotations,
                    &nets,
                    &half,
                    margin,
                    &candidate_pos,
                ) == 0
        }),
        "foreign-obstructor candidates should include a legal corridor-clearing move: {relief:?}"
    );

    let before = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    polish_positions(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );
    let after = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );

    assert_eq!(
        ratline_obstruction_pressure(&problem, &rotations, &nets, &half, margin, &pos),
        0
    );
    assert!(
        (pos[2].y - 10.0).abs() >= PLACEMENT_GRID.pitch() - 1e-9,
        "movable foreign blocker should leave the locked ratline corridor, got {:?}",
        pos[2]
    );
    assert!(
        after + 1e-9 < before,
        "foreign-obstructor move should lower placement cost: {before} -> {after}"
    );
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn cached_ratline_edges_match_public_position_candidate_helpers() {
    let problem = PlaceProblem {
        bounds: board(30.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad("A", "N1", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("B", "N1", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("C", "N2", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("D", "N2", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("X", "FLOAT", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pos = vec![
        Point2 { x: 5.0, y: 15.0 },
        Point2 { x: 25.0, y: 15.0 },
        Point2 { x: 15.0, y: 5.0 },
        Point2 { x: 15.0, y: 25.0 },
        Point2 { x: 15.0, y: 15.0 },
    ];
    let edges = ratline_tree_edge_list(&problem, &nets, &rotations, &pos);

    assert_eq!(
        sorted_points(ratline_crossing_position_candidates(
            &problem, &nets, &rotations, &pos, 1
        )),
        sorted_points(ratline_crossing_position_candidates_from_edges(
            &problem, &rotations, 1, &edges
        )),
        "cached ratline edges must preserve crossing-relief candidates"
    );
    assert_eq!(
        sorted_points(ratline_obstruction_position_candidates(
            &problem, &nets, &rotations, &half, margin, &pos, 1
        )),
        sorted_points(ratline_obstruction_position_candidates_from_edges(
            &problem, &rotations, &half, margin, &pos, 1, &edges
        )),
        "cached ratline edges must preserve obstruction-relief candidates"
    );
    assert_eq!(
        sorted_points(obstructing_part_position_candidates(
            &problem, &nets, &rotations, &half, margin, &pos, 4
        )),
        sorted_points(obstructing_part_position_candidates_from_edges(
            &problem, &half, margin, &pos, 4, &edges
        )),
        "cached ratline edges must preserve foreign-obstructor candidates"
    );
}

#[test]
fn position_polish_candidates_are_snapped_clamped_and_deduped() {
    let problem = PlaceProblem {
        bounds: board(10.0, 10.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![r0603("R1", Some("A"), Some("B"))],
        outline: None,
    };
    let old = Point2 { x: 5.0, y: 5.0 };

    let candidates = unique_position_candidates(
        &problem,
        0,
        0.0,
        (1.0, 1.0),
        rotated_copper_bbox(&problem.parts[0], 0.0),
        old,
        vec![
            old,
            Point2 { x: 5.01, y: 5.01 },
            Point2 { x: 6.01, y: 5.99 },
            Point2 { x: 6.0, y: 6.0 },
            Point2 { x: -10.0, y: -10.0 },
            Point2 { x: -9.9, y: -9.9 },
        ],
    );

    assert_eq!(
        candidates,
        vec![Point2 { x: 6.0, y: 6.0 }, Point2 { x: 1.0, y: 1.0 }],
        "position polish should evaluate each snapped/clamped target once"
    );
}

#[test]
fn edge_seek_position_candidates_include_all_edge_band_targets() {
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![tiny_single_pad("J1", "N", Point2 { x: 0.0, y: 0.0 })],
        outline: None,
    };
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let pos = vec![Point2 { x: 15.0, y: 10.0 }];

    let candidates = edge_seek_position_candidates(&problem, &half, 0.0, &pos, 0, &[0]);

    assert_eq!(
        candidates,
        vec![
            Point2 {
                x: 15.0,
                y: 0.5 + EDGE_BAND,
            },
            Point2 {
                x: 15.0,
                y: 20.0 - 0.5 - EDGE_BAND,
            },
            Point2 {
                x: 0.5 + EDGE_BAND,
                y: 10.0,
            },
            Point2 {
                x: 30.0 - 0.5 - EDGE_BAND,
                y: 10.0,
            },
        ],
        "edge polish should offer direct non-local targets for every board edge"
    );
    assert!(
        edge_seek_position_candidates(&problem, &half, 0.0, &pos, 0, &[]).is_empty(),
        "non-edge-seeking parts should not pay extra edge candidates"
    );
}

#[test]
fn position_polish_can_take_long_edge_seek_move() {
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![tiny_single_pad("J1", "N", Point2 { x: 0.0, y: 0.0 })],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pairs = Vec::new();
    let edge_idx = vec![0];
    let mut pos = vec![Point2 { x: 15.0, y: 10.0 }];
    let before = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );

    polish_positions(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );

    let after = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    let edge_gap = [
        pos[0].x - half[0].0 - problem.bounds.min_x,
        problem.bounds.max_x - pos[0].x - half[0].0,
        pos[0].y - half[0].1 - problem.bounds.min_y,
        problem.bounds.max_y - pos[0].y - half[0].1,
    ]
    .into_iter()
    .fold(f64::MAX, f64::min);

    assert!(
        edge_gap <= PLACEMENT_GRID.pitch() + 1e-9,
        "edge-seeking part should jump from the board middle to an edge, got {pos:?}"
    );
    assert!(
        after + 1e-9 < before,
        "edge-seek polish should lower placement cost: {before} -> {after}"
    );
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn position_polish_can_take_long_pad_centroid_move() {
    let mut b = tiny_single_pad("B", "N", Point2 { x: -5.0, y: 0.0 });
    place_at(&mut b, 45.0, 5.0, 0.0);
    let problem = PlaceProblem {
        bounds: board(60.0, 10.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![tiny_single_pad("A", "N", Point2 { x: 0.0, y: 0.0 }), b],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pairs = Vec::new();
    let edge_idx = Vec::new();
    let mut pos = vec![Point2 { x: 5.0, y: 5.0 }, Point2 { x: 45.0, y: 5.0 }];
    let before = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    let centroid = vec![Point2 { x: 40.0, y: 5.0 }, Point2 { x: 45.0, y: 5.0 }];
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &centroid));
    let centroid_cost = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &centroid,
    );
    assert!(
        centroid_cost + 1e-9 < before,
        "fixture should make the pad-centroid move cheaper: {before} -> {centroid_cost}"
    );

    polish_positions(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );

    let after = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    assert!(
        pos[0].x > 30.0,
        "A should make a long move toward B's physical pad, got {:?}",
        pos[0]
    );
    assert!(
        after + 1e-9 < before,
        "centroid polish should lower cost: {before} -> {after}"
    );
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn position_candidates_include_pad_median_to_ignore_far_outlier_net() {
    let multi = Part {
        reference: "U1".to_owned(),
        courtyard_w: 1.0,
        courtyard_h: 1.0,
        pads: ["N1", "N2", "N3", "N4"]
            .iter()
            .map(|net| PartPad {
                number: (*net).to_owned(),
                offset: Point2 { x: 0.0, y: 0.0 },
                width: 0.4,
                height: 0.4,
                layers: top(),
                net: Some((*net).to_owned()),
            })
            .collect(),
        edge_datum: None,
        locked: None,
    };
    let problem = PlaceProblem {
        bounds: board(120.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            multi,
            tiny_single_pad("A", "N1", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("B", "N2", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("C", "N3", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("D", "N4", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let pos = vec![
        Point2 { x: 80.0, y: 10.0 },
        Point2 { x: 10.0, y: 10.0 },
        Point2 { x: 12.0, y: 10.0 },
        Point2 { x: 14.0, y: 10.0 },
        Point2 { x: 100.0, y: 10.0 },
    ];

    let candidates = net_centroid_position_candidates(&problem, &nets, &rotations, &pos, 0);

    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 34.0).abs() < 1e-9 && (p.y - 10.0).abs() < 1e-9),
        "existing mean candidate should still be present: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 14.0).abs() < 1e-9 && (p.y - 10.0).abs() < 1e-9),
        "median candidate should target the dense pad cluster despite the outlier: {candidates:?}"
    );
}

#[test]
fn position_candidates_include_pad_median_axis_targets() {
    let multi = Part {
        reference: "U1".to_owned(),
        courtyard_w: 1.0,
        courtyard_h: 1.0,
        pads: ["N1", "N2", "N3", "N4"]
            .iter()
            .map(|net| PartPad {
                number: (*net).to_owned(),
                offset: Point2 { x: 0.0, y: 0.0 },
                width: 0.4,
                height: 0.4,
                layers: top(),
                net: Some((*net).to_owned()),
            })
            .collect(),
        edge_datum: None,
        locked: None,
    };
    let problem = PlaceProblem {
        bounds: board(120.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            multi,
            tiny_single_pad("A", "N1", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("B", "N2", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("C", "N3", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("D", "N4", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let pos = vec![
        Point2 { x: 80.0, y: 20.0 },
        Point2 { x: 10.0, y: 10.0 },
        Point2 { x: 12.0, y: 12.0 },
        Point2 { x: 14.0, y: 14.0 },
        Point2 { x: 100.0, y: 30.0 },
    ];

    let candidates = net_centroid_position_candidates(&problem, &nets, &rotations, &pos, 0);

    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 34.0).abs() < 1e-9 && (p.y - 16.5).abs() < 1e-9),
        "full mean target should still be present: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 34.0).abs() < 1e-9 && (p.y - 20.0).abs() < 1e-9),
        "mean x-axis alignment should preserve current y while targeting the net-average pad location: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 80.0).abs() < 1e-9 && (p.y - 16.5).abs() < 1e-9),
        "mean y-axis alignment should preserve current x while targeting the net-average pad location: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 14.0).abs() < 1e-9 && (p.y - 14.0).abs() < 1e-9),
        "full median target should still be present: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 14.0).abs() < 1e-9 && (p.y - 20.0).abs() < 1e-9),
        "median x-axis alignment should preserve current y while targeting the dense pad cluster: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 80.0).abs() < 1e-9 && (p.y - 14.0).abs() < 1e-9),
        "median y-axis alignment should preserve current x while targeting the dense pad cluster: {candidates:?}"
    );
}

#[test]
fn position_candidates_include_nearest_same_net_pad_target() {
    let movable = Part {
        reference: "U1".to_owned(),
        courtyard_w: 2.0,
        courtyard_h: 2.0,
        pads: vec![PartPad {
            number: "1".to_owned(),
            offset: Point2 { x: 2.0, y: 0.0 },
            width: 0.4,
            height: 0.4,
            layers: top(),
            net: Some("N".to_owned()),
        }],
        edge_datum: None,
        locked: None,
    };
    let problem = PlaceProblem {
        bounds: board(120.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            movable,
            tiny_single_pad("NEAR", "N", Point2 { x: 0.0, y: 0.0 }),
            tiny_single_pad("FAR", "N", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let pos = vec![
        Point2 { x: 20.0, y: 10.0 },
        Point2 { x: 10.0, y: 10.0 },
        Point2 { x: 100.0, y: 10.0 },
    ];

    let candidates = net_centroid_position_candidates(&problem, &nets, &rotations, &pos, 0);

    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 53.0).abs() < 1e-9 && (p.y - 10.0).abs() < 1e-9),
        "mean target should still be present: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 8.0).abs() < 1e-9 && (p.y - 10.0).abs() < 1e-9),
        "nearest-pad target should align U1.1 with the near same-net pad: {candidates:?}"
    );
}

#[test]
fn position_candidates_include_nearest_same_net_pad_axis_targets() {
    let problem = PlaceProblem {
        bounds: board(120.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad("U1", "N", Point2 { x: 2.0, y: 1.0 }),
            tiny_single_pad("J1", "N", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let pos = vec![Point2 { x: 20.0, y: 10.0 }, Point2 { x: 10.0, y: 14.0 }];

    let candidates = net_centroid_position_candidates(&problem, &nets, &rotations, &pos, 0);

    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 8.0).abs() < 1e-9 && (p.y - 13.0).abs() < 1e-9),
        "full nearest-pad target should still be present: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 8.0).abs() < 1e-9 && (p.y - 10.0).abs() < 1e-9),
        "x-axis pad alignment should preserve current y while lining up the pad: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|p| (p.x - 20.0).abs() < 1e-9 && (p.y - 13.0).abs() < 1e-9),
        "y-axis pad alignment should preserve current x while lining up the pad: {candidates:?}"
    );
}

#[test]
fn position_polish_can_take_axis_alignment_when_full_pad_target_is_illegal() {
    let mut locked = tiny_single_pad("J1", "N", Point2 { x: 0.0, y: 0.0 });
    place_at(&mut locked, 10.0, 14.0, 0.0);
    let problem = PlaceProblem {
        bounds: board(40.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad("U1", "N", Point2 { x: 0.0, y: 0.0 }),
            locked,
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<crate::problem::Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pairs = Vec::new();
    let edge_idx = Vec::new();
    let mut pos = vec![Point2 { x: 20.0, y: 10.0 }, Point2 { x: 10.0, y: 14.0 }];
    let before = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );

    polish_positions(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );

    let after = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    assert!(
        after + 1e-9 < before,
        "axis pad alignment should lower placement cost when the full pad target overlaps: {before} -> {after}"
    );
    assert!(
        (pos[0].x - 10.0).abs() < 1e-9 || (pos[0].y - 14.0).abs() < 1e-9,
        "movable pad should align on one routing axis with the locked pad, got {:?}",
        pos[0]
    );
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn swap_polish_untangles_post_legalized_assignment() {
    let problem = PlaceProblem {
        bounds: board(30.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("A", Some("N1"), None),
            r0603("B", Some("N2"), None),
            r0603("C", Some("N2"), None),
            r0603("D", Some("N1"), None),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let copper_bbox: Vec<crate::problem::Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let margin = courtyard_margin(problem.clearance);
    let pairs = Vec::new();
    let edge_idx = Vec::new();
    let mut pos = vec![
        Point2 { x: 5.0, y: 5.0 },
        Point2 { x: 25.0, y: 5.0 },
        Point2 { x: 5.0, y: 25.0 },
        Point2 { x: 25.0, y: 25.0 },
    ];
    let before = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    assert_eq!(ratline_crossings(&problem, &rotations, &nets, &pos), 1);

    polish_swaps(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &rotations,
        &half,
        &copper_bbox,
        &mut pos,
    );

    let after = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    assert!(
        after + 1e-9 < before,
        "swap polish should lower cost: {before} -> {after}"
    );
    assert_eq!(ratline_crossings(&problem, &rotations, &nets, &pos), 0);
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn swap_pair_order_prioritizes_connected_and_crossing_pairs() {
    let mut locked = r0603("LOCK", Some("N3"), None);
    locked.locked = Some(LockedAt {
        at: Point2 { x: 15.0, y: 15.0 },
        rotation: 0.0,
    });
    let problem = PlaceProblem {
        bounds: board(40.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("A", Some("N1"), None),
            r0603("B", Some("N2"), None),
            r0603("C", Some("N2"), None),
            r0603("D", Some("N1"), None),
            r0603("E", None, None),
            r0603("F", None, None),
            locked,
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let rotations = vec![0.0; problem.parts.len()];
    let pos = vec![
        Point2 { x: 5.0, y: 5.0 },
        Point2 { x: 35.0, y: 5.0 },
        Point2 { x: 5.0, y: 35.0 },
        Point2 { x: 35.0, y: 35.0 },
        Point2 { x: 20.0, y: 20.0 },
        Point2 { x: 8.0, y: 20.0 },
        Point2 { x: 15.0, y: 15.0 },
    ];

    let order = swap_pair_order(&problem, &nets, &rotations, &pos);

    assert_eq!(
        &order[..2],
        &[(0, 3), (1, 2)],
        "same-net parts should be tried before unrelated swap pairs"
    );
    let obstructing = order
        .iter()
        .position(|pair| *pair == (0, 4))
        .expect("obstructing unrelated part should still be covered by the full sweep");
    let crossing = order
        .iter()
        .position(|pair| *pair == (0, 1))
        .expect("crossing-related pair should be present");
    assert!(
        crossing < obstructing,
        "crossing-related swaps should be tried before obstruction fallback pairs: {order:?}"
    );
    let fallback = order
        .iter()
        .position(|pair| *pair == (0, 5))
        .expect("non-obstructing unrelated part should still be paired");
    assert!(
        obstructing < fallback,
        "foreign parts sitting in a ratline corridor should be swapped before unrelated fallback pairs: {order:?}"
    );
    assert!(
        order.iter().all(|(a, b)| *a != 6 && *b != 6),
        "locked parts must not be considered for swap polish"
    );
}

#[test]
fn rotation_polish_lowers_pad_level_wirelength_for_unlocked_part() {
    let problem = PlaceProblem {
        bounds: board(100.0, 100.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            single_pad("A", "N", Point2 { x: 0.0, y: 4.0 }),
            single_pad("B", "N", Point2 { x: 0.0, y: -4.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let margin = courtyard_margin(problem.clearance);
    let pos = vec![Point2 { x: 20.0, y: 50.0 }, Point2 { x: 80.0, y: 50.0 }];
    let mut rotations = vec![0.0; problem.parts.len()];
    let mut half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let mut copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let pairs = Vec::new();
    let edge_idx = Vec::new();
    let before = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );

    polish_rotations(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &pos,
        &mut rotations,
        &mut half,
        &mut copper_bbox,
    );

    let after = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    assert!(
        after < before,
        "rotation polish should lower pad-level cost: {before} -> {after}"
    );
    assert!(
        rotations.iter().any(|&r| r != 0.0),
        "at least one unlocked part should rotate"
    );
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn rotation_polish_revisits_parts_after_later_rotations_change_the_cost() {
    let mut c = tiny_single_pad("C", "N", Point2 { x: 0.0, y: 3.0 });
    place_at(&mut c, 10.0, 4.0, 0.0);
    let problem = PlaceProblem {
        bounds: board(20.0, 10.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad("A", "N", Point2 { x: 2.0, y: 4.0 }),
            tiny_single_pad("B", "N", Point2 { x: 0.0, y: -3.0 }),
            c,
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let margin = courtyard_margin(problem.clearance);
    let pos = vec![
        Point2 { x: 2.0, y: 4.0 },
        Point2 { x: 6.0, y: 4.0 },
        Point2 { x: 10.0, y: 4.0 },
    ];
    let mut rotations = vec![0.0; problem.parts.len()];
    let mut half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let mut copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let pairs = Vec::new();
    let edge_idx = Vec::new();

    polish_rotations(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &pos,
        &mut rotations,
        &mut half,
        &mut copper_bbox,
    );

    assert_eq!(
        rotations,
        vec![0.0, 180.0, 0.0],
        "A must be revisited after B rotates; a single pass leaves A at 90°"
    );
    let cost = place_cost(
        &problem, &nets, &half, margin, &rotations, &pairs, &edge_idx, &pos,
    );
    assert!(
        cost < 5.0,
        "multi-pass rotation polish should reach the lower total cost, got {cost}"
    );
    assert!(is_legal(&problem, &half, &copper_bbox, margin, &pos));
}

#[test]
fn rotation_polish_preserves_locked_rotation() {
    let mut a = single_pad("A", "N", Point2 { x: 0.0, y: 4.0 });
    let mut b = single_pad("B", "N", Point2 { x: 0.0, y: -4.0 });
    place_at(&mut a, 20.0, 50.0, 0.0);
    place_at(&mut b, 80.0, 50.0, 0.0);
    let problem = PlaceProblem {
        bounds: board(100.0, 100.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![a, b],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let margin = courtyard_margin(problem.clearance);
    let pos = vec![Point2 { x: 20.0, y: 50.0 }, Point2 { x: 80.0, y: 50.0 }];
    let mut rotations = vec![0.0; problem.parts.len()];
    let mut half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_courtyard_half(p, r))
        .collect();
    let mut copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .zip(&rotations)
        .map(|(p, &r)| rotated_copper_bbox(p, r))
        .collect();
    let pairs = Vec::new();
    let edge_idx = Vec::new();

    polish_rotations(
        &problem,
        &nets,
        margin,
        &pairs,
        &edge_idx,
        &pos,
        &mut rotations,
        &mut half,
        &mut copper_bbox,
    );

    assert_eq!(rotations, vec![0.0, 0.0]);
}

// ── region containment ──────────────────────────────────────────────────

#[test]
fn group_with_region_lands_members_inside() {
    let region = Rect {
        min_x: 40.0,
        max_x: 58.0,
        min_y: 22.0,
        max_y: 38.0,
    };
    let problem = PlaceProblem {
        bounds: board(60.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("R1", Some("A"), Some("B")),
            r0603("R2", Some("B"), Some("C")),
            r0603("R3", None, None),
        ],
        outline: None,
    };
    let hints = PlacementHints {
        groups: vec![GroupHint {
            name: "corner".to_owned(),
            members: vec!["R1".to_owned(), "R2".to_owned()],
            region: Some(region),
            edge: None,
            grid: false,
            rotation: None,
            surround: None,
        }],
        ..Default::default()
    };
    let res = place(&problem, &hints);
    assert!(res.legal, "{res:?}");
    for r in ["R1", "R2"] {
        let p = res.placements.iter().find(|p| p.reference == r).unwrap();
        assert!(
            region.contains(p.at),
            "{r} at {:?} must land inside region {region:?}",
            p.at
        );
    }
}

// ── decoupling caps seed beside their anchor IC ─────────────────────────

/// An IC-like anchor: `npads` pads, the first two on `pwr`/GND (so a 2-pad cap on
/// those nets pairs with it via `decoupling_pairs`), the rest dangling unique nets.
fn ic_anchor(reference: &str, npads: usize, pwr: &str) -> Part {
    let pads = (0..npads)
        .map(|i| {
            let net = match i {
                0 => pwr.to_owned(),
                1 => "GND".to_owned(),
                _ => format!("{reference}_S{i}"),
            };
            PartPad {
                number: format!("{}", i + 1),
                offset: Point2 {
                    x: (i as f64 - npads as f64 / 2.0) * 0.5,
                    y: 0.0,
                },
                width: 0.3,
                height: 0.3,
                layers: top(),
                net: Some(net),
            }
        })
        .collect();
    Part {
        reference: reference.to_owned(),
        courtyard_w: npads as f64 * 0.5 + 1.0,
        courtyard_h: 3.0,
        pads,
        edge_datum: None,
        locked: None,
    }
}

#[test]
fn decoupling_caps_seed_beside_their_anchor_ic() {
    // Two ICs on a wide board, each with its own VCC rail (VCC1/VCC2) sharing GND,
    // and three bypass caps apiece. The force seed splits each cap between its IC
    // and the (far) shared-GND centroid, stranding it mid-board. The DECOUPLE
    // variant's seed-snap must pull every cap to within a tight radius of ITS anchor
    // — verified on the final legal placement. (The baseline is UNSNAPPED so
    // `place_best` can fall back to it when the snap hurts routability; the snap now
    // lives behind `opts.decouple`.)
    let mut parts = vec![ic_anchor("U1", 8, "VCC1"), ic_anchor("U2", 8, "VCC2")];
    for c in ["Ca0", "Ca1", "Ca2"] {
        parts.push(r0603(c, Some("VCC1"), Some("GND")));
    }
    for c in ["Cb0", "Cb1", "Cb2"] {
        parts.push(r0603(c, Some("VCC2"), Some("GND")));
    }
    let problem = PlaceProblem {
        bounds: board(80.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts,
        outline: None,
    };
    let res = place_variant(
        &problem,
        &PlacementHints::default(),
        PlaceOpts {
            decouple: true,
            aspect_edge: false,
            anneal: false,
        },
    );
    assert!(res.legal, "{res:?}");
    let at = |r: &str| res.placements.iter().find(|p| p.reference == r).unwrap().at;
    let d = |a: Point2, b: Point2| a.dist(b);
    // Each cap must hug its OWN anchor (not the other IC). A generous bound: well
    // under the inter-IC span, proving the cap is clustered, not stranded.
    for c in ["Ca0", "Ca1", "Ca2"] {
        assert!(
            d(at(c), at("U1")) < 12.0,
            "{c} must hug U1, dist {:.1}",
            d(at(c), at("U1"))
        );
        assert!(
            d(at(c), at("U1")) < d(at(c), at("U2")),
            "{c} must be nearer U1 than U2"
        );
    }
    for c in ["Cb0", "Cb1", "Cb2"] {
        assert!(
            d(at(c), at("U2")) < 12.0,
            "{c} must hug U2, dist {:.1}",
            d(at(c), at("U2"))
        );
        assert!(
            d(at(c), at("U2")) < d(at(c), at("U1")),
            "{c} must be nearer U2 than U1"
        );
    }
}

#[test]
fn fanout_keeps_crystal_cluster_near_dense_ic() {
    let mut ic = dense_anchor("U1", 24);
    ic.pads[0].net = Some("V3V3".to_string());
    ic.pads[1].net = Some("GND".to_string());
    ic.pads[10].net = Some("XTAL_IN".to_string());
    ic.pads[11].net = Some("XTAL_OUT".to_string());
    let problem = PlaceProblem {
        bounds: board(80.0, 60.0),
        clearance: 0.15,
        layer_count: 6,
        min_trace_width: 0.15,
        keepouts: vec![],
        parts: vec![
            ic,
            r0603("C1", Some("XTAL_IN"), Some("GND")),
            r0603("C2", Some("XTAL_OUT"), Some("GND")),
            r0603("Y1", Some("XTAL_IN"), Some("XTAL_OUT")),
            r0603("C3", Some("V3V3"), Some("GND")),
        ],
        outline: None,
    };

    let res = place_board(&problem, &PlacementHints::default());
    assert!(res.legal, "fanout crystal placement must be legal: {res:?}");
    let at = |r: &str| res.placements.iter().find(|p| p.reference == r).unwrap().at;
    let u1 = at("U1");
    for reference in ["C1", "C2", "Y1"] {
        let dist = at(reference).dist(u1);
        assert!(
            dist < 10.0,
            "{reference} should stay in the inner oscillator cluster near U1, dist {dist:.1}"
        );
    }
}

#[test]
fn fanout_does_not_promote_high_pad_edge_connector_to_central_ic() {
    let mut parts = vec![dense_anchor("U1", 24), ic_anchor("U2", 4, "V3V3")];
    for i in 0..3 {
        parts.push(r0603(
            &format!("R{}", i + 1),
            Some(&format!("S{i}")),
            Some(&format!("USB_OUT{i}")),
        ));
    }
    let mut problem = PlaceProblem {
        bounds: board(60.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts,
        outline: None,
    };
    let hints = PlacementHints {
        edge_seek: vec!["U1".to_owned()],
        ..PlacementHints::default()
    };

    assert!(
        !unified_fanout_place(&mut problem, &hints),
        "a USB receptacle marked edge-seeking must not become the central IC"
    );
    assert!(problem.parts.iter().all(|part| part.locked.is_none()));
}

#[test]
fn fanout_chooses_dense_ic_over_larger_edge_connector() {
    let mut parts = vec![dense_anchor("U1", 24), dense_anchor("U2", 20)];
    for i in 0..3 {
        parts.push(r0603(
            &format!("R{}", i + 1),
            Some(&format!("S{i}")),
            Some(&format!("IC_OUT{i}")),
        ));
    }
    // The two dense parts use the same synthetic S* net names; keep the USB
    // receptacle electrically distinct so only U2 owns the three series parts.
    for pad in &mut parts[0].pads {
        pad.net = pad.net.as_ref().map(|net| format!("USB_{net}"));
    }
    let mut problem = PlaceProblem {
        bounds: board(80.0, 60.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts,
        outline: None,
    };
    let hints = PlacementHints {
        edge_seek: vec!["U1".to_owned()],
        ..PlacementHints::default()
    };

    assert!(unified_fanout_place(&mut problem, &hints));
    let ic_lock = problem.parts[1].locked.as_ref().expect("U2 central lock");
    let connector_lock = problem.parts[0].locked.as_ref().expect("U1 edge lock");
    let board_center = Point2 { x: 40.0, y: 30.0 };
    assert!(
        ic_lock.at.dist(board_center) < 8.0,
        "U2 should own the central hub position, got {:?}",
        ic_lock.at
    );
    assert!(
        connector_lock.at.dist(board_center) > ic_lock.at.dist(board_center) + 3.0,
        "U1 should remain outside central U2, got U1={:?}, U2={:?}",
        connector_lock.at,
        ic_lock.at
    );
}

// ── grid hint: tight footprint-sized pitch, centred ─────────────────────

#[test]
fn grid_hint_spreads_members_within_region() {
    // A grid hint tiles members evenly across the region (cell centres), which
    // keeps the array's routing channels open. The pitch is the region divided by
    // the column/row count, so every member lands inside the region.
    let mut problem = PlaceProblem {
        bounds: board(60.0, 60.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("D1", Some("A"), Some("B")),
            r0603("D2", Some("B"), Some("C")),
            r0603("D3", Some("C"), Some("D")),
            r0603("D4", Some("D"), Some("A")),
        ],
        outline: None,
    };
    let region = Rect {
        min_x: 10.0,
        max_x: 50.0,
        min_y: 10.0,
        max_y: 50.0,
    };
    let hints = PlacementHints {
        groups: vec![GroupHint {
            name: "array".to_owned(),
            members: vec!["D1".into(), "D2".into(), "D3".into(), "D4".into()],
            region: Some(region),
            edge: None,
            grid: true,
            rotation: None,
            surround: None,
        }],
        ..Default::default()
    };
    apply_grid_hints(&mut problem, &hints);
    for p in &problem.parts {
        let at = &p.locked.as_ref().expect("grid member is locked").at;
        assert!(
            at.x >= region.min_x - 1e-9
                && at.x <= region.max_x + 1e-9
                && at.y >= region.min_y - 1e-9
                && at.y <= region.max_y + 1e-9,
            "{} must land inside the region, got {at:?}",
            p.reference
        );
    }
}

#[test]
fn grid_hint_clamps_oversize_array_into_bounds_at_board_corner() {
    // A region tucked at the board corner: a cell centred near the region edge
    // would push a member's courtyard off-board, and because the cells are LOCKED
    // the legalizer cannot pull them back. The per-cell clamp keeps every member's
    // courtyard on-board. (An over-constrained region — smaller than its array —
    // is an authoring error; the clamp guarantees on-board, not non-overlap.)
    let region = Rect {
        min_x: 0.0,
        max_x: 3.0,
        min_y: 0.0,
        max_y: 3.0,
    };
    let problem = PlaceProblem {
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("D1", Some("A"), Some("B")),
            r0603("D2", Some("B"), Some("C")),
            r0603("D3", Some("C"), Some("D")),
            r0603("D4", Some("D"), Some("A")),
        ],
        outline: None,
    };
    let hints = PlacementHints {
        groups: vec![GroupHint {
            name: "array".to_owned(),
            members: vec!["D1".into(), "D2".into(), "D3".into(), "D4".into()],
            region: Some(region),
            edge: None,
            grid: true,
            rotation: Some(90.0),
            surround: None,
        }],
        ..Default::default()
    };
    // apply_grid_hints alone must keep every locked cell's courtyard on-board.
    let mut hinted = problem.clone();
    apply_grid_hints(&mut hinted, &hints);
    let b = &hinted.bounds;
    for p in &hinted.parts {
        let l = p.locked.as_ref().expect("grid member must be locked");
        assert_eq!(l.rotation, 90.0, "grid rotation must lock every member");
        let (hw, hh) = rotated_courtyard_half(p, l.rotation);
        assert!(
            l.at.x - hw >= b.min_x - 1e-9 && l.at.x + hw <= b.max_x + 1e-9,
            "{} overflows x: {:?}",
            p.reference,
            l.at
        );
        assert!(
            l.at.y - hh >= b.min_y - 1e-9 && l.at.y + hh <= b.max_y + 1e-9,
            "{} overflows y: {:?}",
            p.reference,
            l.at
        );
    }
}

// ── edge affinity ───────────────────────────────────────────────────────

#[test]
fn edge_affinity_part_touches_edge_band() {
    let problem = PlaceProblem {
        bounds: board(60.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            // A connector-ish 2-pin part.
            Part {
                reference: "J1".to_owned(),
                courtyard_w: 2.54,
                courtyard_h: 3.81,
                pads: vec![
                    PartPad {
                        number: "1".to_owned(),
                        offset: Point2 { x: 0.0, y: -1.27 },
                        width: 1.7,
                        height: 1.7,
                        layers: vec![LayerRef::top(), LayerRef::bottom()],
                        net: Some("NET1".to_owned()),
                    },
                    PartPad {
                        number: "2".to_owned(),
                        offset: Point2 { x: 0.0, y: 1.27 },
                        width: 1.7,
                        height: 1.7,
                        layers: vec![LayerRef::top(), LayerRef::bottom()],
                        net: Some("NET2".to_owned()),
                    },
                ],
                edge_datum: None,
                locked: None,
            },
            r0603("R1", Some("NET1"), Some("X")),
            r0603("R2", Some("NET2"), Some("Y")),
        ],
        outline: None,
    };
    let hints = PlacementHints {
        groups: vec![GroupHint {
            name: "connector".to_owned(),
            members: vec!["J1".to_owned()],
            region: None,
            edge: Some(Edge::W),
            grid: false,
            rotation: None,
            surround: None,
        }],
        ..Default::default()
    };
    let res = place(&problem, &hints);
    assert!(res.legal, "{res:?}");
    let j1 = res.placements.iter().find(|p| p.reference == "J1").unwrap();
    // West edge band: the courtyard's left edge within EDGE_BAND of min_x.
    let left_edge = j1.at.x - 2.54 / 2.0;
    assert!(
        left_edge <= problem.bounds.min_x + EDGE_BAND + PLACE_GRID,
        "J1 left edge {left_edge:.2} must sit in the west band (<= {:.2})",
        problem.bounds.min_x + EDGE_BAND + PLACE_GRID
    );
}

#[test]
fn edge_locked_placer_pins_edge_seek_connector_to_frame() {
    let problem = PlaceProblem {
        bounds: board(60.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            Part {
                reference: "J1".to_owned(),
                courtyard_w: 2.54,
                courtyard_h: 8.0,
                pads: vec![
                    PartPad {
                        number: "1".to_owned(),
                        offset: Point2 { x: 0.0, y: -2.54 },
                        width: 1.2,
                        height: 1.2,
                        layers: vec![LayerRef::top(), LayerRef::bottom()],
                        net: Some("SIG1".to_owned()),
                    },
                    PartPad {
                        number: "2".to_owned(),
                        offset: Point2 { x: 0.0, y: 2.54 },
                        width: 1.2,
                        height: 1.2,
                        layers: vec![LayerRef::top(), LayerRef::bottom()],
                        net: Some("SIG2".to_owned()),
                    },
                ],
                edge_datum: None,
                locked: None,
            },
            r0603("R1", Some("SIG1"), Some("X")),
            r0603("R2", Some("SIG2"), Some("Y")),
        ],
        outline: None,
    };
    let hints = PlacementHints {
        edge_seek: vec!["J1".to_owned()],
        ..Default::default()
    };

    let res = EdgeLockedPlacer.place(&problem, &hints);

    assert!(res.legal, "{res:?}");
    let j1 = res.placements.iter().find(|p| p.reference == "J1").unwrap();
    let (hw, hh) = rotated_courtyard_half(&problem.parts[0], j1.rotation);
    let edge_gap = [
        j1.at.x - hw - problem.bounds.min_x,
        problem.bounds.max_x - (j1.at.x + hw),
        j1.at.y - hh - problem.bounds.min_y,
        problem.bounds.max_y - (j1.at.y + hh),
    ]
    .into_iter()
    .fold(f64::INFINITY, f64::min);
    assert!(
        edge_gap <= PLACE_GRID,
        "edge-locked connector should sit on the board frame, gap={edge_gap:.3}, placement={j1:?}"
    );
}

// ── overlap resolution: everything starts at one point ──────────────────

#[test]
fn all_at_one_point_resolves_to_no_overlap() {
    // Six parts all LOCKED-free but seeded by the engine; then we additionally
    // stress the legalizer by forcing a degenerate seed via tiny board cell:
    // simplest expression — many parts, small-ish board, no nets (pure repulsion
    // + legalizer must still separate them).
    let parts: Vec<Part> = (0..8)
        .map(|i| r0603(&format!("R{i}"), None, None))
        .collect();
    let problem = PlaceProblem {
        bounds: board(40.0, 40.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts,
        outline: None,
    };
    let res = place(&problem, &PlacementHints::default());
    assert!(
        res.legal,
        "8 parts must legalize to zero overlap on a 40x40 board: {res:?}"
    );

    // Stronger: directly verify exact geometry has no courtyard overlap.
    let half: Vec<(f64, f64)> = problem
        .parts
        .iter()
        .map(|p| (p.courtyard_w / 2.0, p.courtyard_h / 2.0))
        .collect();
    let copper_bbox: Vec<Rect> = problem
        .parts
        .iter()
        .map(|p| rotated_copper_bbox(p, 0.0))
        .collect();
    let pos: Vec<Point2> = res.placements.iter().map(|p| p.at).collect();
    assert!(is_legal(
        &problem,
        &half,
        &copper_bbox,
        courtyard_margin(0.2),
        &pos
    ));
}

#[test]
fn is_legal_rejects_pad_overhang_on_custom_outline() {
    // The connector-pad-overhang fidelity guard: a part whose CENTRE is inside the outline
    // but whose PAD copper overhangs the edge is illegal (it would ship copper_edge_clearance),
    // even though the old centre-only check passed it. r0603 copper reaches ~1.225mm in x.
    let problem = PlaceProblem {
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![r0603("R1", Some("A"), Some("B"))],
        // A 5..15 square outline.
        outline: Some(square_outline()),
    };
    let half = vec![rotated_courtyard_half(&problem.parts[0], 0.0)];
    let copper_bbox = vec![rotated_copper_bbox(&problem.parts[0], 0.0)];
    let margin = courtyard_margin(0.2);
    // Centred: copper (±1.225) + 0.5 clearance sits well inside the square → legal.
    assert!(is_legal(
        &problem,
        &half,
        &copper_bbox,
        margin,
        &[Point2 { x: 10.0, y: 10.0 }]
    ));
    // Near the right edge: centre x=14.4 is inside the polygon, but copper reaches
    // 14.4 + 1.225 = 15.6 > 15 → overhangs → illegal (the centre-only check missed this).
    assert!(!is_legal(
        &problem,
        &half,
        &copper_bbox,
        margin,
        &[Point2 { x: 14.4, y: 10.0 }]
    ));
}

#[test]
fn is_legal_rejects_pad_inside_rectangular_edge_clearance() {
    let problem = PlaceProblem {
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![r0603("R1", Some("A"), Some("B"))],
        outline: None,
    };
    let half = vec![rotated_courtyard_half(&problem.parts[0], 0.0)];
    let copper_bbox = vec![rotated_copper_bbox(&problem.parts[0], 0.0)];
    let margin = courtyard_margin(0.2);

    assert!(is_legal(
        &problem,
        &half,
        &copper_bbox,
        margin,
        &[Point2 { x: 10.0, y: 10.0 }]
    ));
    assert!(!is_legal(
        &problem,
        &half,
        &copper_bbox,
        margin,
        &[Point2 { x: 1.25, y: 10.0 }]
    ));
}

#[test]
fn is_legal_uses_asymmetric_copper_bbox_for_off_centre_pads() {
    // A connector's pads are OFF-CENTRE from the origin (origin at pin 1). A symmetric
    // centre±max|offset| box would be ~2× too large on the empty side and FALSE-REJECT a part
    // whose copper actually clears the edge — this guards the true-bbox fix. Two pads both at
    // +x (offsets 2.0 and 4.0, 1×1mm): real copper bbox x = 1.5..4.5 (no copper on the −x side).
    let off_centre = Part {
        reference: "J1".to_owned(),
        courtyard_w: 6.0,
        courtyard_h: 2.0,
        pads: vec![
            PartPad {
                number: "1".to_owned(),
                offset: Point2 { x: 2.0, y: 0.0 },
                width: 1.0,
                height: 1.0,
                layers: top(),
                net: Some("A".to_owned()),
            },
            PartPad {
                number: "2".to_owned(),
                offset: Point2 { x: 4.0, y: 0.0 },
                width: 1.0,
                height: 1.0,
                layers: top(),
                net: Some("B".to_owned()),
            },
        ],
        edge_datum: None,
        locked: None,
    };
    // Asymmetric bbox: +x only, nothing on −x.
    let bb = rotated_copper_bbox(&off_centre, 0.0);
    assert!(
        (bb.min_x - 1.5).abs() < 1e-9 && (bb.max_x - 4.5).abs() < 1e-9,
        "x bbox 1.5..4.5, got {bb:?}"
    );
    let problem = PlaceProblem {
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![off_centre],
        outline: Some(square_outline()),
    };
    let half = vec![rotated_courtyard_half(&problem.parts[0], 0.0)];
    let copper_bbox = vec![bb];
    let margin = courtyard_margin(0.2);
    // At x=8 the real copper is 9.5..12.5 (+0.5 → 9..13, inside the 5..15 square) → LEGAL.
    // A symmetric ±4.5 box would reach x=3 (<5) and wrongly reject. This is the regression guard.
    assert!(is_legal(
        &problem,
        &half,
        &copper_bbox,
        margin,
        &[Point2 { x: 8.0, y: 10.0 }]
    ));
}

// ── to_route_problem: parseable + connectivity oracle accepts pads/points ─

#[test]
fn to_route_problem_round_trips_and_oracle_accepts_geometry() {
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.25,
        keepouts: vec![],
        parts: vec![
            r0603("R1", Some("SIG"), Some("GND")),
            r0603("R2", Some("SIG"), Some("GND")),
        ],
        outline: None,
    };
    let res = place(&problem, &PlacementHints::default());
    assert!(res.legal);
    let rp = to_route_problem(&problem, &res.placements);

    // Round-trips serde.
    let json = serde_json::to_string(&rp).unwrap();
    let rp2: RouteProblem = serde_json::from_str(&json).unwrap();
    assert_eq!(rp, rp2, "emitted RouteProblem must round-trip serde");

    // Multi-pin nets became connections (SIG and GND each have 2 pins).
    let names: Vec<&str> = rp.connections.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.contains(&"SIG") && names.contains(&"GND"),
        "{names:?}"
    );

    // Every connection point must sit on a pad of its net: feed an EMPTY
    // solution to the connectivity oracle. With no copper, multi-pin nets are
    // reported Unconnected (their points are not yet joined) but there must be
    // NO CrossNetMerge — the points-on-pads geometry is sound. (A clean route
    // below proves the points are actually reachable.)
    let empty = crate::problem::RouteSolution {
        traces: vec![],
        vias: vec![],
    };
    let v = connectivity::check(&rp, &empty);
    assert!(
        v.iter()
            .all(|x| matches!(x, connectivity::Violation::Unconnected { .. })),
        "no copper: only Unconnected expected, got {v:?}"
    );
}

// Keep place→route→lint integration outside this crate; a duplicate single-board
// smoke here would pull the pcb-route-mesh router into pcb-place's dev-deps.

#[test]
fn grid_ranker_weights_failed_net_by_pin_count() {
    let rp = RouteProblem {
        layer_count: 2,
        min_trace_width: 0.2,
        obstacles: vec![Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef::top(), LayerRef::bottom()],
            center: Point2 { x: 10.0, y: 10.0 },
            width: 2.0,
            height: 20.0,
            connected_to: vec![],
        }],
        connections: vec![Connection {
            name: "BUS".to_owned(),
            points_to_connect: vec![
                RoutePoint {
                    x: 2.0,
                    y: 5.0,
                    layer: LayerRef::top(),
                },
                RoutePoint {
                    x: 2.0,
                    y: 15.0,
                    layer: LayerRef::top(),
                },
                RoutePoint {
                    x: 18.0,
                    y: 10.0,
                    layer: LayerRef::top(),
                },
            ],
        }],
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        via_diameter: 0.6,
        via_drill: 0.3,
        net_widths: Default::default(),
        outline: None,
        escape_layers: Default::default(),
        plane_nets: Default::default(),
    };

    let faults = GridAstarRanker.faults(&rp);
    assert!(
        faults >= 3,
        "one failed three-pin net should be weighted by pins, got {faults}"
    );
}

#[test]
fn grid_ranker_uses_best_orthogonal_strictness_key() {
    let rp = RouteProblem {
        layer_count: 2,
        min_trace_width: 0.2,
        obstacles: vec![Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef::top()],
            center: Point2 { x: 10.0, y: 10.0 },
            width: 1.0,
            height: 20.0,
            connected_to: vec![],
        }],
        connections: vec![Connection {
            name: "SIG".to_owned(),
            points_to_connect: vec![
                RoutePoint {
                    x: 2.0,
                    y: 10.0,
                    layer: LayerRef::top(),
                },
                RoutePoint {
                    x: 18.0,
                    y: 10.0,
                    layer: LayerRef::top(),
                },
            ],
        }],
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        via_diameter: 0.6,
        via_drill: 0.3,
        net_widths: Default::default(),
        outline: None,
        escape_layers: Default::default(),
        plane_nets: Default::default(),
    };
    let strict_key = route_rank_key(&rp, &crate::router::route_orthogonal(&rp));
    let lenient_key = route_rank_key(&rp, &crate::router::route_orthogonal_lenient(&rp));
    let expected = if route_rank_key_better(lenient_key, strict_key) {
        lenient_key
    } else {
        strict_key
    };

    assert_eq!(
        GridAstarRanker.rank_key(&rp),
        expected,
        "placement ranker should mirror the grid router's orthogonal strict/lenient arbitration"
    );
}

#[test]
fn grid_ranker_bounds_high_terminal_candidate_work() {
    let with_terminals = |count: usize| RouteProblem {
        layer_count: 2,
        min_trace_width: 0.2,
        obstacles: vec![],
        connections: vec![Connection {
            name: "BUS".to_owned(),
            points_to_connect: (0..count)
                .map(|i| RoutePoint {
                    x: i as f64,
                    y: 1.0,
                    layer: LayerRef::top(),
                })
                .collect(),
        }],
        bounds: board(50.0, 10.0),
        clearance: 0.2,
        via_diameter: 0.6,
        via_drill: 0.3,
        net_widths: Default::default(),
        outline: None,
        escape_layers: Default::default(),
        plane_nets: Default::default(),
    };

    assert!(!placement_ranker_uses_bounded_pass(&with_terminals(40)));
    assert!(placement_ranker_uses_bounded_pass(&with_terminals(41)));
    assert!(placement_ranker_uses_bounded_pass(&with_terminals(48)));
    assert!(!placement_ranker_uses_bounded_pass(&with_terminals(49)));
    assert!(!placement_ranker_uses_layout_only(&with_terminals(48)));
    assert!(placement_ranker_uses_layout_only(&with_terminals(49)));
    assert_eq!(
        GridAstarRanker.rank_key(&with_terminals(49)),
        (0, 0, 0, 0, 0),
        "layout-only candidates must share a neutral route key so the oracle uses geometry"
    );
}

#[test]
fn grid_ranker_skips_full_grid_fallback_when_orthogonal_is_clean_and_via_free() {
    let called = std::cell::Cell::new(false);
    let clean_orthogonal = (0, 0, 0, 0, 30_000);

    assert!(route_rank_key_clean_via_free(clean_orthogonal));

    let key = rank_key_with_full_grid_fallback(clean_orthogonal, true, || {
        called.set(true);
        (0, 0, 0, 1, 20_000)
    });

    assert_eq!(key, clean_orthogonal);
    assert!(
        !called.get(),
        "clean via-free orthogonal rank should avoid the expensive full-grid fallback"
    );
}

#[test]
fn grid_ranker_uses_full_grid_fallback_when_clean_orthogonal_spends_vias() {
    let clean_via_orthogonal = (0, 0, 0, 2, 30_000);
    let cleaner_full_grid = (0, 0, 0, 1, 20_000);

    assert!(
        !route_rank_key_clean_via_free(clean_via_orthogonal),
        "clean but via-heavy orthogonal routes must continue into fallback comparison"
    );

    let key = rank_key_with_full_grid_fallback(clean_via_orthogonal, true, || cleaner_full_grid);

    assert_eq!(
        key, cleaner_full_grid,
        "small clean placements should still compare full-grid quality when the proxy route spends vias"
    );
}

#[test]
fn grid_ranker_uses_full_grid_fallback_when_orthogonal_still_faults() {
    let faulty_orthogonal = (2, 0, 1, 0, 0);
    let routed_full_grid = (0, 0, 0, 3, 40_000);

    let key = rank_key_with_full_grid_fallback(faulty_orthogonal, true, || routed_full_grid);

    assert_eq!(
        key, routed_full_grid,
        "full grid route quality should rescue a placement the orthogonal proxy ranks faulty"
    );
}

#[test]
fn grid_ranker_size_gate_can_skip_full_grid_fallback() {
    let called = std::cell::Cell::new(false);
    let faulty_orthogonal = (2, 0, 1, 0, 0);

    let key = rank_key_with_full_grid_fallback(faulty_orthogonal, false, || {
        called.set(true);
        (0, 0, 0, 0, 0)
    });

    assert_eq!(key, faulty_orthogonal);
    assert!(
        !called.get(),
        "large-board gate should preserve placement-ranker runtime by skipping full grid"
    );
}

fn ranker_gate_problem(connection_count: usize) -> RouteProblem {
    RouteProblem {
        layer_count: 2,
        min_trace_width: 0.2,
        obstacles: Vec::new(),
        connections: (0..connection_count)
            .map(|idx| Connection {
                name: format!("N{idx}"),
                points_to_connect: vec![
                    RoutePoint {
                        x: 1.0,
                        y: 1.0 + idx as f64,
                        layer: LayerRef::top(),
                    },
                    RoutePoint {
                        x: 9.0,
                        y: 1.0 + idx as f64,
                        layer: LayerRef::top(),
                    },
                ],
            })
            .collect(),
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        via_diameter: 0.6,
        via_drill: 0.3,
        net_widths: Default::default(),
        outline: None,
        escape_layers: Default::default(),
        plane_nets: Default::default(),
    }
}

#[test]
fn grid_ranker_faulty_proxy_gets_larger_full_grid_fallback_gate() {
    let medium = ranker_gate_problem(6);
    let faulty_orthogonal = (1, 0, 1, 0, 0);
    let clean_via_orthogonal = (0, 0, 0, 1, 10_000);

    assert!(
        should_try_full_grid_ranker_fallback(&medium, faulty_orthogonal),
        "faulty medium-size placement candidates should get a full-grid rescue check"
    );
    assert!(
        !should_try_full_grid_ranker_fallback(&medium, clean_via_orthogonal),
        "clean via-heavy medium candidates should keep the old tighter runtime gate"
    );
}

#[test]
fn grid_ranker_faulty_proxy_still_skips_large_full_grid_fallback() {
    let large = ranker_gate_problem(7);
    let faulty_orthogonal = (1, 0, 1, 0, 0);

    assert!(
        !should_try_full_grid_ranker_fallback(&large, faulty_orthogonal),
        "faulty fallback expansion should remain bounded for placement-oracle runtime"
    );
}

#[test]
fn fanout_fast_path_requires_bounded_clean_via_free_route_evidence() {
    let bounded = ranker_gate_problem(20); // 40 terminals: bounded routing still applies.
    assert!(fanout_fast_path_accepts(&bounded, (0, 0, 0, 0, 1)));
    assert!(
        !fanout_fast_path_accepts(&bounded, (1, 0, 1, 0, 1)),
        "an unrouted fanout must retain the fallback portfolio"
    );
    assert!(
        !fanout_fast_path_accepts(&bounded, (0, 0, 0, 1, 1)),
        "a via-dependent fanout must retain the fallback portfolio"
    );

    let layout_only = ranker_gate_problem(25); // 50 terminals: no bounded route proof.
    assert!(placement_ranker_uses_layout_only(&layout_only));
    assert!(
        !fanout_fast_path_accepts(&layout_only, (0, 0, 0, 0, 0)),
        "the layout-only sentinel must never masquerade as a clean route"
    );
}

#[test]
fn placement_route_rank_prefers_fewer_geometry_violations_before_failed_net_count() {
    let failed_one_small_net = (2, 0, 1, 0, 10_000);
    let emitted_two_geometry_errors = (2, 2, 0, 0, 1_000);

    assert!(
        route_rank_key_better(failed_one_small_net, emitted_two_geometry_errors),
        "at equal total faults, placement ranking should prefer an honest failed net over emitted DRC geometry"
    );
    assert!(
        !route_rank_key_better(emitted_two_geometry_errors, failed_one_small_net),
        "geometry DRC must not win merely because fewer nets are reported failed"
    );
}

#[test]
fn place_result_selector_keeps_routable_layout_over_lower_cost_unroutable_one() {
    let sig = |reference: &str| Part {
        reference: reference.to_owned(),
        courtyard_w: 1.0,
        courtyard_h: 1.0,
        pads: vec![PartPad {
            number: "1".to_owned(),
            offset: Point2 { x: 0.0, y: 0.0 },
            width: 0.5,
            height: 0.5,
            layers: vec![LayerRef::top(), LayerRef::bottom()],
            net: Some("SIG".to_owned()),
        }],
        edge_datum: None,
        locked: None,
    };
    let blocker = Part {
        reference: "W".to_owned(),
        courtyard_w: 1.0,
        courtyard_h: 20.0,
        pads: vec![PartPad {
            number: "1".to_owned(),
            offset: Point2 { x: 0.0, y: 0.0 },
            width: 1.0,
            height: 20.0,
            layers: vec![LayerRef::top(), LayerRef::bottom()],
            net: None,
        }],
        edge_datum: None,
        locked: None,
    };
    let problem = PlaceProblem {
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![sig("A"), sig("B"), blocker],
        outline: None,
    };
    let result = |ax, ay, bx, by, layout_cost| pcb_place_api::PlaceResult {
        placements: vec![
            pcb_place_api::Placement {
                reference: "A".to_owned(),
                at: Point2 { x: ax, y: ay },
                rotation: 0.0,
            },
            pcb_place_api::Placement {
                reference: "B".to_owned(),
                at: Point2 { x: bx, y: by },
                rotation: 0.0,
            },
            pcb_place_api::Placement {
                reference: "W".to_owned(),
                at: Point2 { x: 10.0, y: 10.0 },
                rotation: 0.0,
            },
        ],
        legal: true,
        report: pcb_place_api::PlaceReport {
            overlaps_resolved: 0,
            out_of_bounds_clamps: 0,
            hpwl: layout_cost,
            layout_cost,
        },
    };
    let routable = result(2.0, 5.0, 2.0, 15.0, 1000.0);
    let unroutable = result(2.0, 10.0, 18.0, 10.0, 1.0);

    let winner = better_place_result(&problem, routable, unroutable);

    assert_eq!(winner.placements[1].at.x, 2.0);
    assert_eq!(
        winner.report.layout_cost, 1000.0,
        "routing faults must dominate layout cost"
    );
}

#[test]
fn place_result_selector_prefers_lower_via_route_before_layout_cost() {
    let sig = |reference: &str| Part {
        reference: reference.to_owned(),
        courtyard_w: 1.0,
        courtyard_h: 1.0,
        pads: vec![PartPad {
            number: "1".to_owned(),
            offset: Point2 { x: 0.0, y: 0.0 },
            width: 0.5,
            height: 0.5,
            layers: vec![LayerRef::top(), LayerRef::bottom()],
            net: Some("SIG".to_owned()),
        }],
        edge_datum: None,
        locked: None,
    };
    let top_wall = Part {
        reference: "W".to_owned(),
        courtyard_w: 1.0,
        courtyard_h: 1.0,
        pads: vec![PartPad {
            number: "1".to_owned(),
            offset: Point2 { x: 0.0, y: 0.0 },
            width: 1.0,
            height: 20.0,
            layers: vec![LayerRef::top()],
            net: None,
        }],
        edge_datum: None,
        locked: None,
    };
    let problem = PlaceProblem {
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![sig("A"), sig("B"), top_wall],
        outline: None,
    };
    let result = |ax, ay, bx, by, layout_cost| pcb_place_api::PlaceResult {
        placements: vec![
            pcb_place_api::Placement {
                reference: "A".to_owned(),
                at: Point2 { x: ax, y: ay },
                rotation: 0.0,
            },
            pcb_place_api::Placement {
                reference: "B".to_owned(),
                at: Point2 { x: bx, y: by },
                rotation: 0.0,
            },
            pcb_place_api::Placement {
                reference: "W".to_owned(),
                at: Point2 { x: 10.0, y: 10.0 },
                rotation: 0.0,
            },
        ],
        legal: true,
        report: pcb_place_api::PlaceReport {
            overlaps_resolved: 0,
            out_of_bounds_clamps: 0,
            hpwl: layout_cost,
            layout_cost,
        },
    };
    let same_side_no_via = result(2.0, 5.0, 2.0, 15.0, 1000.0);
    let cross_wall_with_vias = result(2.0, 10.0, 18.0, 10.0, 1.0);

    let no_via_key =
        GridAstarRanker.rank_key(&to_route_problem(&problem, &same_side_no_via.placements));
    let via_key = GridAstarRanker.rank_key(&to_route_problem(
        &problem,
        &cross_wall_with_vias.placements,
    ));
    assert_eq!(no_via_key.0, 0, "same-side placement should route cleanly");
    assert_eq!(
        via_key.0, 0,
        "cross-wall placement should also route cleanly"
    );
    assert!(
        no_via_key.3 < via_key.3,
        "same-side route should need fewer vias: {no_via_key:?} vs {via_key:?}"
    );

    let winner = better_place_result(&problem, same_side_no_via, cross_wall_with_vias);

    assert_eq!(
        winner.report.layout_cost, 1000.0,
        "lower-via routed placement must beat lower layout cost at equal faults"
    );
}

#[test]
fn place_result_selector_keeps_incumbent_on_exact_rank_tie() {
    let problem = PlaceProblem {
        bounds: board(20.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![r0603("R1", None, None)],
        outline: None,
    };
    let mk = |x| pcb_place_api::PlaceResult {
        placements: vec![pcb_place_api::Placement {
            reference: "R1".to_owned(),
            at: Point2 { x, y: 10.0 },
            rotation: 0.0,
        }],
        legal: true,
        report: pcb_place_api::PlaceReport {
            overlaps_resolved: 0,
            out_of_bounds_clamps: 0,
            hpwl: 1.0,
            layout_cost: 1.0,
        },
    };

    let winner = better_place_result(&problem, mk(3.0), mk(15.0));

    assert_eq!(winner.placements[0].at.x, 3.0);
}

// ── HPWL is reported and sane ────────────────────────────────────────────

#[test]
fn hpwl_is_reported_and_nonnegative() {
    let problem = PlaceProblem {
        bounds: board(30.0, 20.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("R1", Some("A"), Some("B")),
            r0603("R2", Some("B"), Some("C")),
        ],
        outline: None,
    };
    let res = place(&problem, &PlacementHints::default());
    assert!(res.report.hpwl >= 0.0, "HPWL must be non-negative");
    // Net "B" is the only 2-pin net; its HPWL is the pad-center bbox half-perim,
    // strictly positive once the two parts are apart.
    assert!(
        res.report.hpwl > 0.0,
        "two connected parts give positive HPWL"
    );
    let nets = derive_nets(&problem);
    let pos: Vec<Point2> = res.placements.iter().map(|p| p.at).collect();
    let rotations: Vec<f64> = res.placements.iter().map(|p| p.rotation).collect();
    let recomputed = compute_hpwl_with_rotations(&problem, &nets, &pos, &rotations);
    assert!(
        (res.report.hpwl - recomputed).abs() < 1e-9,
        "reported HPWL must match final placement rotations"
    );
}

#[test]
fn hpwl_with_rotations_uses_final_unlocked_rotation() {
    let problem = PlaceProblem {
        bounds: board(30.0, 30.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            tiny_single_pad("U1", "N", Point2 { x: 4.0, y: 0.0 }),
            tiny_single_pad("J1", "N", Point2 { x: 0.0, y: 0.0 }),
        ],
        outline: None,
    };
    let nets = derive_nets(&problem);
    let pos = vec![Point2 { x: 10.0, y: 10.0 }, Point2 { x: 10.0, y: 20.0 }];

    let unrotated = compute_hpwl(&problem, &nets, &pos);
    let rotated = compute_hpwl_with_rotations(&problem, &nets, &pos, &[270.0, 0.0]);

    assert_eq!(unrotated, 14.0);
    assert_eq!(rotated, 6.0);
}

// ── never panics on an impossible board ─────────────────────────────────

#[test]
fn impossible_board_returns_not_legal_without_panic() {
    // A board far too small for its parts: 3 R_0603 courtyards (1.6mm wide)
    // cannot fit with margin on a 1x1 board. The engine must return
    // legal:false, never panic.
    let problem = PlaceProblem {
        bounds: board(1.0, 1.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![
            r0603("R1", Some("A"), Some("B")),
            r0603("R2", Some("B"), Some("C")),
            r0603("R3", Some("C"), Some("A")),
        ],
        outline: None,
    };
    let res = place(&problem, &PlacementHints::default());
    assert!(!res.legal, "an impossible board must report legal:false");
    assert_eq!(
        res.placements.len(),
        3,
        "still returns a placement per part"
    );
}

// ── empty problem is trivially legal ────────────────────────────────────

#[test]
fn rotate_offset_matches_kicad_convention() {
    // Verified against kicad-cli: a SOIC-8 pad at local (-2.475, 1.905) under a
    // footprint rotated 270° lands at world offset (-1.905, -2.475). The two
    // 90/270 directions must not be swapped, or routing targets the wrong pad.
    let p = Point2::new(-2.475, 1.905).rotate(270.0);
    assert!(
        (p.x - -1.905).abs() < 1e-9 && (p.y - -2.475).abs() < 1e-9,
        "{p:?}"
    );
    // 90 is the inverse; 180 negates; 0 is identity.
    let q = Point2::new(-2.475, 1.905).rotate(90.0);
    assert!(
        (q.x - 1.905).abs() < 1e-9 && (q.y - 2.475).abs() < 1e-9,
        "{q:?}"
    );
    let r = Point2::new(1.0, 2.0).rotate(180.0);
    assert!(
        (r.x - -1.0).abs() < 1e-9 && (r.y - -2.0).abs() < 1e-9,
        "{r:?}"
    );
}

#[test]
fn empty_problem_is_legal() {
    let problem = PlaceProblem {
        bounds: board(10.0, 10.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts: vec![],
        outline: None,
    };
    let res = place(&problem, &PlacementHints::default());
    assert!(res.legal);
    assert!(res.placements.is_empty());
    assert_eq!(res.report.hpwl, 0.0);
}

// ── oracle determinism: variant selection + final placement are byte-stable ──

/// An IC-like anchor: `npads` pads, the first two on `pwr`/GND so a 2-pad cap on
/// those nets pairs with it, the rest on this IC's own signal nets.
fn ic8(reference: &str, pwr: &str) -> Part {
    let pads = (0..8)
        .map(|i| {
            let net = match i {
                0 => pwr.to_owned(),
                1 => "GND".to_owned(),
                _ => format!("{reference}_S{i}"),
            };
            PartPad {
                number: format!("{}", i + 1),
                offset: Point2 {
                    x: (i as f64 - 4.0) * 0.5,
                    y: 0.0,
                },
                width: 0.3,
                height: 0.3,
                layers: top(),
                net: Some(net),
            }
        })
        .collect();
    Part {
        reference: reference.into(),
        courtyard_w: 5.0,
        courtyard_h: 3.0,
        pads,
        edge_datum: None,
        locked: None,
    }
}

/// THE BYTE-BEHAVIOR GUARD for the placement oracle: a board that exercises the FULL
/// routability oracle — the baseline `LegalizingPlacer`, the `AnnealingPlacer`, the
/// `decouple` variant (two ICs + bypass caps), and the edge variant (an edge-seeking
/// connector). The pinned snapshot is the exact current output of `place_board`, so any
/// change to variant selection, final geometry, or reported layout metric trips this.
#[test]
fn oracle_placement_is_byte_identical_to_pinned_snapshot() {
    let mut parts = vec![ic8("U1", "VCC1"), ic8("U2", "VCC2")];
    for c in ["Ca0", "Ca1", "Ca2"] {
        parts.push(r0603(c, Some("VCC1"), Some("GND")));
    }
    for c in ["Cb0", "Cb1", "Cb2"] {
        parts.push(r0603(c, Some("VCC2"), Some("GND")));
    }
    parts.push(Part {
        reference: "J1".into(),
        courtyard_w: 2.54,
        courtyard_h: 7.62,
        pads: vec![
            PartPad {
                number: "1".into(),
                offset: Point2 { x: 0.0, y: -2.54 },
                width: 1.7,
                height: 1.7,
                layers: vec![LayerRef::top(), LayerRef::bottom()],
                net: Some("U1_S2".into()),
            },
            PartPad {
                number: "2".into(),
                offset: Point2 { x: 0.0, y: 0.0 },
                width: 1.7,
                height: 1.7,
                layers: vec![LayerRef::top(), LayerRef::bottom()],
                net: Some("U2_S2".into()),
            },
            PartPad {
                number: "3".into(),
                offset: Point2 { x: 0.0, y: 2.54 },
                width: 1.7,
                height: 1.7,
                layers: vec![LayerRef::top(), LayerRef::bottom()],
                net: Some("GND".into()),
            },
        ],
        edge_datum: None,
        locked: None,
    });
    parts.push(r0603("R1", Some("U1_S3"), Some("U2_S3")));
    parts.push(r0603("R2", Some("U1_S4"), Some("U2_S4")));

    let problem = PlaceProblem {
        bounds: board(80.0, 50.0),
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.2,
        keepouts: vec![],
        parts,
        outline: None,
    };
    let hints = PlacementHints {
        groups: vec![GroupHint {
            name: "conn".into(),
            members: vec!["J1".into()],
            region: None,
            edge: Some(Edge::W),
            grid: false,
            rotation: None,
            surround: None,
        }],
        edge_seek: vec!["J1".into()],
        corner_seek: vec![],
    };

    let res = place_board(&problem, &hints);
    let got = serde_json::to_string(&res).unwrap();
    const PINNED: &str = r#"{"placements":[{"reference":"U1","at":{"x":9.0,"y":11.5},"rotation":270.0},{"reference":"U2","at":{"x":15.0,"y":10.0},"rotation":0.0},{"reference":"Ca0","at":{"x":4.5,"y":5.0},"rotation":90.0},{"reference":"Ca1","at":{"x":5.5,"y":12.0},"rotation":90.0},{"reference":"Ca2","at":{"x":9.0,"y":8.0},"rotation":0.0},{"reference":"Cb0","at":{"x":14.5,"y":7.5},"rotation":0.0},{"reference":"Cb1","at":{"x":9.5,"y":5.5},"rotation":180.0},{"reference":"Cb2","at":{"x":14.5,"y":5.5},"rotation":0.0},{"reference":"J1","at":{"x":2.0,"y":7.5},"rotation":180.0},{"reference":"R1","at":{"x":20.5,"y":12.5},"rotation":90.0},{"reference":"R2","at":{"x":14.5,"y":14.5},"rotation":0.0}],"legal":true,"report":{"overlapsResolved":7,"outOfBoundsClamps":0,"hpwl":95.63499999999999,"layoutCost":326.90557330030066}}"#;
    assert_eq!(
        got, PINNED,
        "oracle placement drifted from the pinned byte-for-byte snapshot"
    );
}
