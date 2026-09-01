use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_check::model::Design;
use sch_floorplan::contract::{PlacementEngine, SchematicPlaceProblem};
use sch_floorplan::engine_support::{apply_cells, assign_cells};
use spine_place::SpinePlace;

fn compile_source(provider: &SymbolTable, yaml: &str) -> Design {
    circuit_lang::compile(yaml, provider)
        .design
        .expect("test design compiles")
}

#[test]
fn spine_preserves_inferred_pc817_channel_cells() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let design = compile_source(
        &provider,
        r#"
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, pins: {1: IN1, 2: A1}}
      U1: {part: Isolator:PC817, pins: {1: A1, 2: FIELD_GND, 3: LOGIC_GND, 4: OUT1}}
      R9: {part: Device:R, pins: {1: OUT1, 2: V5}}
      R17: {part: Device:R, pins: {1: V5, 2: LED_A1}}
      D1: {part: Device:LED, pins: {1: OUT1, 2: LED_A1}}
      R2: {part: Device:R, pins: {1: IN2, 2: A2}}
      U2: {part: Isolator:PC817, pins: {1: A2, 2: FIELD_GND, 3: LOGIC_GND, 4: OUT2}}
      R10: {part: Device:R, pins: {1: OUT2, 2: V5}}
      R18: {part: Device:R, pins: {1: V5, 2: LED_A2}}
      D2: {part: Device:LED, pins: {1: OUT2, 2: LED_A2}}
"#,
    );
    let mut problem = SchematicPlaceProblem::from_design(&env, &design).unwrap();

    let output = SpinePlace.place(&env, &design, &mut problem, None);

    assert_eq!(
        output
            .ir
            .idioms
            .iter()
            .filter(|idiom| idiom.kind == "pc817_channel")
            .count(),
        2
    );
    let item = |reference: &str| {
        problem
            .items
            .iter()
            .find(|item| item.refdes == reference)
            .unwrap()
    };
    assert!(
        (item("U1").at.x - item("U2").at.x).abs() < 1e-9,
        "engine={} U1={:?} U2={:?}",
        output.result.engine,
        item("U1").at,
        item("U2").at
    );
    assert!(
        item("U1").at.y < item("U2").at.y,
        "channel 1 must sit above channel 2: U1={:?} U2={:?}",
        item("U1").at,
        item("U2").at
    );
    // The contract of a frozen idiom: its members ship at the canonical cell poses
    // `apply_cells` gives them, up to the one rigid translation `normalize` applies to
    // the sheet. A collapse (everything on the margin origin) or a scatter both break it.
    let mut canonical = SchematicPlaceProblem::from_design(&env, &design).unwrap();
    let cells = assign_cells(&canonical.items, &output.ir);
    apply_cells(&mut canonical.items, &cells);
    let offset = [
        problem.items[0].at.x - canonical.items[0].at.x,
        problem.items[0].at.y - canonical.items[0].at.y,
    ];
    for (placed, seed) in problem.items.iter().zip(&canonical.items) {
        let d = [
            placed.at.x - seed.at.x - offset[0],
            placed.at.y - seed.at.y - offset[1],
        ];
        assert!(
            d[0].abs() < 1e-6 && d[1].abs() < 1e-6,
            "{} left its cell: {:?} vs canonical {:?} + {offset:?}",
            placed.refdes,
            placed.at,
            seed.at,
        );
    }
}
