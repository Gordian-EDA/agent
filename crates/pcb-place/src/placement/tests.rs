use super::geometry::{
    EDGE_BAND, PLACE_GRID, courtyard_margin, rotated_copper_bbox, rotated_courtyard_half,
};
use super::hints::apply_grid_hints;
use super::legalize::is_legal;
use super::model::{Edge, GroupHint, LockedAt, Part, PartPad, PlaceProblem, PlacementHints, Rect};
use super::pairs::series_pairs;
use super::route::{PlaceOpts, place, place_board, place_variant, to_route_problem};
use crate::connectivity;
use crate::problem::{LayerRef, Point2, Polygon, RouteProblem};

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
        locked: None,
    }
}

fn place_at(p: &mut Part, x: f64, y: f64, rotation: f64) {
    p.locked = Some(LockedAt {
        at: Point2 { x, y },
        rotation,
    });
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
    let res = place(&problem, &PlacementHints::default());
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
            .map(|p| p.at.clone())
            .unwrap()
    };
    let d = |a: Point2, b: Point2| ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
    let connected = d(at("R1"), at("R9"));
    // Two parts the grid seeds at opposite ends and that no net pulls together.
    let unconnected = d(at("R3"), at("R7"));
    assert!(
        connected < unconnected,
        "connected R1-R9 ({connected:.2}) must be closer than unconnected R3-R7 ({unconnected:.2})"
    );
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
            region: Some(region.clone()),
            edge: None,
            grid: false,
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
    let at = |r: &str| {
        res.placements
            .iter()
            .find(|p| p.reference == r)
            .unwrap()
            .at
            .clone()
    };
    let d = |a: Point2, b: Point2| ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
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
            region: Some(region.clone()),
            edge: None,
            grid: true,
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
    let pos: Vec<Point2> = res.placements.iter().map(|p| p.at.clone()).collect();
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

// (place→route→lint integration is covered end-to-end by board_harness across
// all 77 circuits; a duplicate single-board smoke here would only pull the
// negotiated-mesh router into pcb-place's dev-deps.)

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
        locked: None,
    }
}

/// THE BYTE-BEHAVIOR GUARD for the engine-SDK refactor: a board that exercises the
/// FULL routability oracle — the baseline `LegalizingPlacer`, the `AnnealingPlacer`,
/// the `decouple` variant (two ICs + bypass caps), and the edge variant (an
/// edge-seeking connector). The pinned snapshot is the EXACT output of `place_board`
/// at the pre-refactor commit (verified byte-for-byte against a worktree at that
/// SHA), so any change to variant selection or final geometry trips this.
#[test]
fn oracle_placement_is_byte_identical_to_pre_refactor() {
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
            surround: None,
        }],
        edge_seek: vec!["J1".into()],
        corner_seek: vec![],
    };

    let res = place_board(&problem, &hints);
    let got = serde_json::to_string(&res).unwrap();
    const PINNED: &str = r#"{"placements":[{"reference":"U1","at":{"x":8.5,"y":13.5},"rotation":0.0},{"reference":"U2","at":{"x":14.0,"y":13.5},"rotation":0.0},{"reference":"Ca0","at":{"x":7.0,"y":9.5},"rotation":0.0},{"reference":"Ca1","at":{"x":10.5,"y":9.5},"rotation":0.0},{"reference":"Ca2","at":{"x":12.0,"y":7.5},"rotation":0.0},{"reference":"Cb0","at":{"x":14.0,"y":11.0},"rotation":0.0},{"reference":"Cb1","at":{"x":1.5,"y":8.5},"rotation":0.0},{"reference":"Cb2","at":{"x":8.0,"y":7.5},"rotation":0.0},{"reference":"J1","at":{"x":4.0,"y":13.5},"rotation":0.0},{"reference":"R1","at":{"x":15.5,"y":16.0},"rotation":0.0},{"reference":"R2","at":{"x":10.0,"y":16.0},"rotation":0.0}],"legal":true,"report":{"overlapsResolved":9,"outOfBoundsClamps":0,"hpwl":88.92999999999999,"layoutCost":518.962835441901}}"#;
    assert_eq!(
        got, PINNED,
        "oracle placement drifted from the pre-refactor byte-for-byte snapshot"
    );

    // And it is reproducible (the oracle's parallel evaluation is order-independent).
    let again = serde_json::to_string(&place_board(&problem, &hints)).unwrap();
    assert_eq!(got, again, "place_board must be deterministic across runs");
}
