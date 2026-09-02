//! Relational layout intent: the violation measure, the projection, and what the engines
//! actually ship when the author's relations fight connectivity gravity.
//!
//! The measure/projection tests are pure geometry (no KiCad). The engine tests compile a
//! tiny design and run a real placement, so they SKIP without a KiCad installation.

use geom::{Point2, Rect};
use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use kicad_symbol::geometry::{PinGeom, SymbolGeometry};
use sch_model::engine::{PlacementEngine, SchematicPlaceProblem};
use sch_model::place::PlaceOptions;
use sch_model::relation::{relation_group_spread, relation_viol, repair_relations};
use sch_model::ir::{Axis, GroupSide, LayoutIr, Relation, Side};
use sch_model::item::Item;

// ---------------------------------------------------------------------------
// Synthetic parts — a 2-pin passive's geometry is all the relation math reads.
// ---------------------------------------------------------------------------

fn passive(refdes: &str, at: [f64; 2]) -> Item {
    let pin = |number: &str, y: f64| PinGeom {
        number: number.to_string(),
        name: "~".to_string(),
        at: Point2::new(0.0, y),
        angle: 0.0,
        length: 2.54,
        unit: 1,
    };
    Item {
        refdes: refdes.to_string(),
        part: "Device:R".to_string(),
        value: "1k".to_string(),
        footprint: None,
        geom: SymbolGeometry {
            lib_id: "Device:R".to_string(),
            pins: vec![pin("1", 3.81), pin("2", -3.81)],
            raw_definition: String::new(),
        },
        pins: vec![
            ("1".into(), "~".into(), None),
            ("2".into(), "~".into(), None),
        ],
        at: at.into(),
        angle: 0.0,
        unit: 1,
        mirror: false,
        frozen: false,
        preseeded: false,
    }
}

fn ir(relations: Vec<Relation>) -> LayoutIr {
    LayoutIr {
        relations,
        ..Default::default()
    }
}

fn x_of(items: &[Item], refdes: &str) -> f64 {
    items.iter().find(|it| it.refdes == refdes).unwrap().at[0]
}
fn y_of(items: &[Item], refdes: &str) -> f64 {
    items.iter().find(|it| it.refdes == refdes).unwrap().at[1]
}

// ---------------------------------------------------------------------------
// The measure
// ---------------------------------------------------------------------------

#[test]
fn ordering_relations_count_exactly_the_pairs_out_of_order() {
    let items = vec![passive("R1", [0.0, 0.0]), passive("R2", [30.0, 0.0])];
    let (a, b) = ("R1".to_string(), "R2".to_string());
    assert_eq!(
        relation_viol(
            &items,
            &ir(vec![Relation::LeftOf {
                a: a.clone(),
                b: b.clone()
            }])
        ),
        0
    );
    assert_eq!(
        relation_viol(
            &items,
            &ir(vec![Relation::RightOf {
                a: a.clone(),
                b: b.clone()
            }])
        ),
        1
    );
    // Same `y` satisfies neither Above nor Below — both need a strict order.
    assert_eq!(
        relation_viol(
            &items,
            &ir(vec![Relation::Above {
                a: a.clone(),
                b: b.clone()
            }])
        ),
        1
    );
    assert_eq!(
        relation_viol(&items, &ir(vec![Relation::Below { a, b }])),
        1
    );
}

#[test]
fn a_relation_naming_an_absent_part_is_skipped_not_counted() {
    let items = vec![passive("R1", [0.0, 0.0])];
    let rel = Relation::LeftOf {
        a: "R1".into(),
        b: "R9".into(),
    };
    assert_eq!(relation_viol(&items, &ir(vec![rel])), 0);
}

#[test]
fn group_counts_foreign_intruders_and_wrong_side_members() {
    let cohesive = vec![
        passive("R1", [0.0, 0.0]),
        passive("R2", [10.0, 0.0]),
        passive("U1", [60.0, 0.0]),
    ];
    let left_of_u1 = Relation::Group {
        name: "input".into(),
        members: vec!["R1".into(), "R2".into()],
        side: Some(GroupSide::Anchored(Side::Left, "U1".into())),
        anchor: None,
    };
    assert_eq!(relation_viol(&cohesive, &ir(vec![left_of_u1.clone()])), 0);

    // A foreign part parked between the members breaks cohesion.
    let mut intruded = cohesive.clone();
    intruded.push(passive("C9", [5.0, 0.0]));
    assert_eq!(relation_viol(&intruded, &ir(vec![left_of_u1.clone()])), 1);

    // Both members on the wrong side of the anchor.
    let flipped = vec![
        passive("R1", [80.0, 0.0]),
        passive("R2", [90.0, 0.0]),
        passive("U1", [60.0, 0.0]),
    ];
    assert_eq!(relation_viol(&flipped, &ir(vec![left_of_u1])), 2);
}

#[test]
fn align_counts_members_off_the_shared_line() {
    let members = vec!["R1".into(), "R2".into(), "R3".into()];
    let row = vec![
        passive("R1", [0.0, 0.0]),
        passive("R2", [20.0, 0.0]),
        passive("R3", [40.0, 30.0]),
    ];
    assert_eq!(
        relation_viol(
            &row,
            &ir(vec![Relation::Align {
                members: members.clone(),
                axis: Axis::Horizontal,
            }])
        ),
        1
    );
    // The same parts are a column on neither axis: two of three share no `x`.
    assert_eq!(
        relation_viol(
            &row,
            &ir(vec![Relation::Align {
                members,
                axis: Axis::Vertical,
            }])
        ),
        2
    );
}

#[test]
fn group_spread_measures_only_group_members() {
    let items = vec![
        passive("R1", [0.0, 0.0]),
        passive("R2", [10.0, 0.0]),
        passive("R3", [500.0, 500.0]),
    ];
    let tight = relation_group_spread(
        &items,
        &ir(vec![Relation::Group {
            name: "g".into(),
            members: vec!["R1".into(), "R2".into()],
            side: None,
        anchor: None,
        }]),
    );
    let wide = relation_group_spread(
        &items,
        &ir(vec![Relation::Group {
            name: "g".into(),
            members: vec!["R1".into(), "R3".into()],
            side: None,
        anchor: None,
        }]),
    );
    assert!(tight < wide, "{tight} !< {wide}");
}

// ---------------------------------------------------------------------------
// The projection
// ---------------------------------------------------------------------------

#[test]
fn repair_orders_a_reversed_chain_and_leaves_it_satisfied() {
    // Authored order R1 | R2 | R3, seeded exactly backwards.
    let mut items = vec![
        passive("R1", [40.0, 0.0]),
        passive("R2", [20.0, 0.0]),
        passive("R3", [0.0, 0.0]),
    ];
    let intent = ir(vec![
        Relation::LeftOf {
            a: "R1".into(),
            b: "R2".into(),
        },
        Relation::LeftOf {
            a: "R2".into(),
            b: "R3".into(),
        },
    ]);
    assert_eq!(relation_viol(&items, &intent), 2);
    assert!(repair_relations(&mut items, &intent));
    assert_eq!(relation_viol(&items, &intent), 0);
    assert!(x_of(&items, "R1") < x_of(&items, "R2"));
    assert!(x_of(&items, "R2") < x_of(&items, "R3"));
}

#[test]
fn repair_separates_ordered_parts_by_at_least_their_own_bodies() {
    let mut items = vec![passive("R1", [10.0, 0.0]), passive("R2", [10.0, 0.0])];
    let intent = ir(vec![Relation::LeftOf {
        a: "R1".into(),
        b: "R2".into(),
    }]);
    repair_relations(&mut items, &intent);
    let (a, b) = (&items[0], &items[1]);
    let rects = (
        Rect::new(a.at[0] - 4.0, a.at[1] - 4.0, a.at[0] + 4.0, a.at[1] + 4.0),
        Rect::new(b.at[0] - 4.0, b.at[1] - 4.0, b.at[0] + 4.0, b.at[1] + 4.0),
    );
    assert!(!rects.0.overlaps(&rects.1), "repair packed two bodies");
}

#[test]
fn repair_is_idempotent() {
    let mut items = vec![
        passive("R1", [40.0, 0.0]),
        passive("R2", [0.0, 0.0]),
        passive("R3", [0.0, 30.0]),
    ];
    let intent = ir(vec![
        Relation::LeftOf {
            a: "R1".into(),
            b: "R2".into(),
        },
        Relation::Above {
            a: "R1".into(),
            b: "R3".into(),
        },
    ]);
    repair_relations(&mut items, &intent);
    let once: Vec<Point2> = items.iter().map(|it| it.at).collect();
    repair_relations(&mut items, &intent);
    assert_eq!(items.iter().map(|it| it.at).collect::<Vec<_>>(), once);
}

#[test]
fn repair_never_moves_a_frozen_part() {
    let mut items = vec![passive("R1", [40.0, 0.0]), passive("R2", [0.0, 0.0])];
    items[1].frozen = true;
    let held = items[1].at;
    let intent = ir(vec![Relation::LeftOf {
        a: "R1".into(),
        b: "R2".into(),
    }]);
    repair_relations(&mut items, &intent);
    assert_eq!(items[1].at, held);
    assert!(x_of(&items, "R1") < x_of(&items, "R2"));
}

#[test]
fn contradictory_intent_leaves_the_axis_untouched_instead_of_guessing() {
    let mut items = vec![passive("R1", [0.0, 0.0]), passive("R2", [20.0, 0.0])];
    let before: Vec<Point2> = items.iter().map(|it| it.at).collect();
    let intent = ir(vec![
        Relation::LeftOf {
            a: "R1".into(),
            b: "R2".into(),
        },
        Relation::LeftOf {
            a: "R2".into(),
            b: "R1".into(),
        },
    ]);
    repair_relations(&mut items, &intent);
    assert_eq!(items.iter().map(|it| it.at).collect::<Vec<_>>(), before);
}

#[test]
fn repair_snaps_an_aligned_row_onto_one_line() {
    let mut items = vec![
        passive("R1", [0.0, 0.0]),
        passive("R2", [20.0, 12.0]),
        passive("R3", [40.0, 0.0]),
    ];
    let intent = ir(vec![Relation::Align {
        members: vec!["R1".into(), "R2".into(), "R3".into()],
        axis: Axis::Horizontal,
    }]);
    repair_relations(&mut items, &intent);
    assert_eq!(relation_viol(&items, &intent), 0);
    assert_eq!(y_of(&items, "R1"), y_of(&items, "R2"));
    assert_eq!(y_of(&items, "R2"), y_of(&items, "R3"));
}

#[test]
fn repair_moves_a_group_onto_the_named_side_of_its_anchor() {
    let mut items = vec![
        passive("R1", [80.0, 0.0]),
        passive("R2", [92.7, 0.0]),
        passive("U1", [60.0, 0.0]),
    ];
    items[2].frozen = true;
    let intent = ir(vec![Relation::Group {
        name: "input".into(),
        members: vec!["R1".into(), "R2".into()],
        side: Some(GroupSide::Anchored(Side::Left, "U1".into())),
        anchor: None,
    }]);
    repair_relations(&mut items, &intent);
    assert_eq!(relation_viol(&items, &intent), 0);
    // Moved RIGIDLY: the members keep their relative offset.
    assert!((x_of(&items, "R2") - x_of(&items, "R1") - 12.7).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// The engines
// ---------------------------------------------------------------------------

const CHAIN: &str = r#"
{"parts":[
  {"ref":"PWR1","part":"power:VCC","pins":{"1":"VCC"}},
  {"ref":"PWR2","part":"power:GND","pins":{"1":"GND"}},
  {"ref":"R1","part":"Device:R","value":"1k","pins":{"1":"VCC","2":"N1"}},
  {"ref":"R2","part":"Device:R","value":"2k","pins":{"1":"N1","2":"N2"}},
  {"ref":"R3","part":"Device:R","value":"3k","pins":{"1":"N2","2":"GND"}}
]}
"#;

fn chain_problem(
    env: &KicadInstallation,
    intent: LayoutIr,
) -> (sch_check::Design, SchematicPlaceProblem) {
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let input: sch_check::PlacePartsInput = serde_json::from_str(CHAIN).unwrap();
    let (design, diagnostics, _) = sch_check::into_design(&input, &provider, &Default::default());
    assert!(!diagnostics.has_errors(), "{:#?}", diagnostics);
    let problem =
        sch_floorplan::floorplan::place_problem(env, &design, Some(intent), PlaceOptions::default())
            .unwrap();
    (design, problem)
}

/// Run `engine` over `problem` against the real routed evaluator.
fn run(
    env: &KicadInstallation,
    design: &sch_check::Design,
    problem: &mut SchematicPlaceProblem,
    engine: &dyn PlacementEngine,
) -> sch_model::engine::PlacementOutput {
    use sch_floorplan::floorplan::place::{RoutedEvaluator, RoutedSheetRealizer};
    let (inc, ir) = (problem.inc.clone(), problem.ir.clone());
    let realizer = RoutedSheetRealizer::new(env, &inc, &ir);
    engine.place(problem, &RoutedEvaluator::new(realizer, design))
}

/// The series chain's connectivity pulls R1→R2→R3 left-to-right; the author asks for the
/// reverse. The engine must ship the AUTHOR's order.
fn engine_honours_reversed_order(engine: &dyn PlacementEngine) {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let (design, mut problem) = chain_problem(&env, LayoutIr::default());
    let mut intent = sch_floorplan::floorplan::infer_ir(&env, &design);
    intent.relations = vec![
        Relation::LeftOf {
            a: "R3".into(),
            b: "R2".into(),
        },
        Relation::LeftOf {
            a: "R2".into(),
            b: "R1".into(),
        },
    ];
    problem.ir = intent.clone();
    let out = run(&env, &design, &mut problem, engine);
    assert_eq!(
        relation_viol(&problem.items, &intent),
        0,
        "{}: shipped {:?}",
        engine.name(),
        problem
            .items
            .iter()
            .map(|it| (it.refdes.clone(), it.at[0]))
            .collect::<Vec<_>>()
    );
    assert!(x_of(&problem.items, "R3") < x_of(&problem.items, "R2"));
    assert!(x_of(&problem.items, "R2") < x_of(&problem.items, "R1"));
    assert_eq!(out.result.truthfulness_breaks, 0, "{}", engine.name());
}

#[test]
fn anneal_ships_the_authored_order_against_connectivity_gravity() {
    engine_honours_reversed_order(&anneal_place::Anneal);
}

#[test]
fn spine_ships_the_authored_order_against_connectivity_gravity() {
    engine_honours_reversed_order(&spine_place::SpinePlace);
}

#[test]
fn anneal_stacks_an_authored_column() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let (design, mut problem) = chain_problem(&env, LayoutIr::default());
    let mut intent = sch_floorplan::floorplan::infer_ir(&env, &design);
    intent.relations = vec![Relation::Align {
        members: vec!["R1".into(), "R2".into(), "R3".into()],
        axis: Axis::Vertical,
    }];
    problem.ir = intent.clone();
    let out = run(&env, &design, &mut problem, &anneal_place::Anneal);
    assert_eq!(relation_viol(&problem.items, &intent), 0);
    assert_eq!(out.result.truthfulness_breaks, 0);
}
