//! Render every validation fixture through the production path into a directory.
//!
//! `cargo run -p sch-floorplan --example render_corpus -- <out-dir>` writes one
//! `<name>.kicad_sch` per fixture, so a metric can be measured on real emitted
//! sheets — and the same metric compared across two revisions of the engine.

use std::path::{Path, PathBuf};

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;
use sch_model::engine::PlacementEngine;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/validation")
}

fn main() {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: render_corpus <out-dir>"),
    );
    std::fs::create_dir_all(&out).unwrap();
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let mut names: Vec<String> = std::fs::read_dir(corpus())
        .unwrap()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".place-parts.json")
                .map(str::to_string)
        })
        .collect();
    names.sort();
    for name in names {
        let src =
            std::fs::read_to_string(corpus().join(format!("{name}.place-parts.json"))).unwrap();
        let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
        let (design, diags, _) = sch_check::into_design(&input, &provider, &Default::default());
        if diags.has_errors() {
            eprintln!("{name}: {diags:#?}");
            continue;
        }
        let ir = input
            .intent
            .clone()
            .map(sch_check::Intent::into_layout_ir)
            .unwrap_or_else(|| floorplan::baseline_ir(&design));
        let engine: Box<dyn PlacementEngine> = Box::new(spine_place::SpinePlace);
        match floorplan::emit_strategy(&env, &design, engine, Some(ir)) {
            Ok(result) => {
                std::fs::write(out.join(format!("{name}.kicad_sch")), result.sch).unwrap();
                eprintln!("{name}: ok");
            }
            Err(e) => eprintln!("{name}: {e}"),
        }
    }
}
