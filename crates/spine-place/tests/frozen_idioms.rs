use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_check::model::Design;
use sch_floorplan::contract::{PlacementEngine, SchematicPlaceProblem};
use spine_place::SpinePlace;

fn compile_source(provider: &SymbolTable, json: &str) -> Design {
    let input: sch_check::PlacePartsInput = serde_json::from_str(json).unwrap();
    let (design, diagnostics) = sch_check::into_design(&input, provider);
    assert!(!diagnostics.has_errors(), "{diagnostics:#?}");
    design
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
        r#"{"parts":[
          {"ref":"R1","part":"Device:R","pins":{"1":"IN1","2":"A1"}},
          {"ref":"U1","part":"Isolator:PC817","pins":{"1":"A1","2":"FIELD_GND","3":"LOGIC_GND","4":"OUT1"}},
          {"ref":"R9","part":"Device:R","pins":{"1":"OUT1","2":"V5"}},
          {"ref":"R17","part":"Device:R","pins":{"1":"V5","2":"LED_A1"}},
          {"ref":"D1","part":"Device:LED","pins":{"1":"OUT1","2":"LED_A1"}},
          {"ref":"R2","part":"Device:R","pins":{"1":"IN2","2":"A2"}},
          {"ref":"U2","part":"Isolator:PC817","pins":{"1":"A2","2":"FIELD_GND","3":"LOGIC_GND","4":"OUT2"}},
          {"ref":"R10","part":"Device:R","pins":{"1":"OUT2","2":"V5"}},
          {"ref":"R18","part":"Device:R","pins":{"1":"V5","2":"LED_A2"}},
          {"ref":"D2","part":"Device:LED","pins":{"1":"OUT2","2":"LED_A2"}}
        ]}
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
    for refs in [
        ["R1", "U1", "R9", "R17", "D1"],
        ["R2", "U2", "R10", "R18", "D2"],
    ] {
        let xs = refs.map(|reference| item(reference).at.x);
        let ys = refs.map(|reference| item(reference).at.y);
        let span = xs.into_iter().fold(f64::MIN, f64::max)
            - xs.into_iter().fold(f64::MAX, f64::min)
            + ys.into_iter().fold(f64::MIN, f64::max)
            - ys.into_iter().fold(f64::MAX, f64::min);
        assert!(
            span <= 100.0,
            "scattered channel {refs:?}: {span:.2} mm, poses={:?}",
            refs.map(|reference| item(reference).at)
        );
    }
}
