//! Chain-contraction invariants over the real validation fixtures: every
//! chainable 2-pin part lands in exactly one chain, chain nets bracket chain
//! parts, and terminals reference real nodes. Skips gracefully without KiCAD.

use std::collections::BTreeMap;

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::contract::SchematicPlaceProblem;
use sch_place::ir::LayoutIr;
use spine_place::chain::{NodeKind, contract};
use spine_place::net::classify_nets;

fn fixtures() -> Vec<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sch-floorplan/tests/fixtures/validation");
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .expect("fixtures dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".place-parts.json"))
        .collect();
    v.sort();
    v
}

#[test]
fn fixtures_contract_cleanly() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    for path in fixtures() {
        let json = std::fs::read_to_string(&path).unwrap();
        let input: sch_check::PlacePartsInput = serde_json::from_str(&json).unwrap();
        let (design, diagnostics, _) =
            sch_check::into_design(&input, &provider, &Default::default());
        assert!(!diagnostics.has_errors(), "{path:?}: {diagnostics:#?}");
        let problem = SchematicPlaceProblem::from_design(&env, &design).unwrap();
        let classes = classify_nets(&problem.inc, &LayoutIr::default());
        let g = contract(&problem.items, &problem.inc, &classes);

        // Every chain's nets bracket its parts.
        for c in &g.chains {
            assert_eq!(
                c.nets.len(),
                c.parts.len() + 1,
                "{path:?}: nets must bracket parts"
            );
            assert!(c.a.node < g.nodes.len() && c.b.node < g.nodes.len());
        }
        // Every 2-pin series part appears in exactly ONE chain.
        let mut seen: BTreeMap<usize, usize> = BTreeMap::new();
        for c in &g.chains {
            for &p in &c.parts {
                *seen.entry(p).or_default() += 1;
            }
        }
        for (&p, &n) in &seen {
            assert_eq!(n, 1, "{path:?}: item {p} in {n} chains");
        }
        // Part nodes reference real items.
        for n in &g.nodes {
            if let NodeKind::Part(i) = n {
                assert!(*i < problem.items.len());
            }
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        println!(
            "{name}: {} items -> {} nodes, {} chains ({} with parts)",
            problem.items.len(),
            g.nodes.len(),
            g.chains.len(),
            g.chains.iter().filter(|c| !c.parts.is_empty()).count()
        );
    }
}
