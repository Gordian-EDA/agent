//! `sch-model` — the schematic layout MODEL: every type and contract the placement,
//! routing and text-solving algorithms speak, and nothing that implements one.
//!
//! Each algorithm is a LEAF with a defined input and a defined output, so an author can
//! work on it without the rest of the system:
//!
//! | leaf | trait | input → output |
//! |---|---|---|
//! | placement engine | [`engine::PlacementEngine`] | [`engine::SchematicPlaceProblem`] + [`engine::CandidateEvaluator`] → [`engine::PlacementOutput`] |
//! | wire router | [`route::SchRouter`] | [`route::RouteScene`] + terminals → paths |
//! | text solver | [`text::TextSolver`] | [`text::Obstacle`]s + [`text::Movable`]s → [`text::Pick`]s |
//!
//! The heavy collaborators (sheet realization, the symbol library, the `.kicad_sch`
//! writer) are INJECTED as trait objects by the composition root, `sch-floorplan`, which
//! also implements them. No leaf depends on it.
//!
//! Everything else here is the shared vocabulary the leaves compute over: the layout IR,
//! placeable items, placement geometry and its seeding passes. Pure geometry, grid
//! snapping, ids and disjoint-set helpers live in `geom`; net/part-name classification
//! lives in `circuit-graph::netclass`.

pub mod cells;
pub mod engine;
pub mod geometry;
pub mod idiom;
pub mod ir;
pub mod item;
pub mod place;
pub mod refine;
pub mod relation;
pub mod result;
pub mod route;
pub mod stub;
pub mod text;
pub mod topology;

/// Load a golden [`engine::ProblemFixture`] corpus for a leaf's tests and benches.
///
/// The fixtures are frozen `.problem.json` snapshots of the real validation designs, so a
/// leaf author needs neither KiCAD nor the realiser. Regenerate them with
/// `cargo run -p sch-floorplan --example freeze_problems`.
pub fn golden_problems() -> Vec<(String, engine::SchematicPlaceProblem)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("golden problems at {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".problem.json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let name = p
                .file_name()
                .unwrap()
                .to_string_lossy()
                .trim_end_matches(".problem.json")
                .to_owned();
            let fixture: engine::ProblemFixture =
                serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            (name, fixture.into())
        })
        .collect()
}
