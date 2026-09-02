//! Freeze each validation fixture's [`SchematicPlaceProblem`] to JSON, so every placement
//! LEAF can be worked on with no KiCAD installation and no symbol library.
//!
//! `cargo run -p sch-floorplan --example freeze_problems` rewrites
//! `crates/sch-model/tests/fixtures/*.problem.json`, the golden corpus the engine crates'
//! own benches and contract tests load.

use kicad::KicadInstallation;
use sch_model::engine::ProblemFixture;
use sch_model::place::PlaceOptions;

fn main() {
    let env = KicadInstallation::detect().expect("no KiCAD installation");
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/validation");
    let out = concat!(env!("CARGO_MANIFEST_DIR"), "/../sch-model/tests/fixtures");
    std::fs::create_dir_all(out).unwrap();

    let provider = kicad_symbol::SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let mut names: Vec<String> = std::fs::read_dir(src)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".place-parts.json")
                .map(str::to_owned)
        })
        .collect();
    names.sort();

    for name in names {
        let json = std::fs::read_to_string(format!("{src}/{name}.place-parts.json")).unwrap();
        let input: sch_check::PlacePartsInput = serde_json::from_str(&json).unwrap();
        let (design, diagnostics, _) =
            sch_check::into_design(&input, &provider, &Default::default());
        assert!(!diagnostics.has_errors(), "{name}: {diagnostics:#?}");
        let problem =
            sch_floorplan::floorplan::place_problem(&env, &design, None, PlaceOptions::default())
                .unwrap();
        let fixture = ProblemFixture::from(&problem);
        let path = format!("{out}/{name}.problem.json");
        std::fs::write(&path, serde_json::to_string(&fixture).unwrap()).unwrap();
        println!("{path}  {} items", problem.items.len());
    }
}
