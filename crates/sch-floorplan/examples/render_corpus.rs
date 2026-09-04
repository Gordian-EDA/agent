//! Render every validation fixture through the production path into a directory.
//!
//! `cargo run -p sch-floorplan --example render_corpus -- <out-dir> [name…]`
//! writes one `<name>.kicad_sch` per fixture, so a metric can be measured on
//! real emitted sheets — and the same metric compared across two revisions of
//! the engine. It reports each sheet's net shorts and opens as it goes, so a
//! placement experiment cannot look good by breaking the netlist.

use std::path::{Path, PathBuf};

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/validation")
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().expect("usage: render_corpus <out-dir> [name…]"));
    let only: Vec<String> = args.collect();
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
    names.retain(|name| only.is_empty() || only.contains(name));
    for name in names {
        let src =
            std::fs::read_to_string(corpus().join(format!("{name}.place-parts.json"))).unwrap();
        let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
        let (design, diags, _) = sch_check::into_design(&input, &provider, &Default::default());
        if diags.has_errors() {
            eprintln!("{name}: {diags:#?}");
            continue;
        }
        // Compose the IR exactly as the live path and the netlist gate do: inference
        // supplies the rails, the ports and each block's TREE, and the caller's intent
        // lays over it. Building it from the intent alone drops every authored tree, so
        // every block came out as one bare row — a sheet the production path never draws.
        let mut ir = floorplan::infer_ir(&env, &design);
        if let Some(intent) = input.intent.clone() {
            floorplan::apply_intent(&mut ir, intent.into_layout_ir());
        }
        match floorplan::emit_strategy(&env, &design, Some(ir)) {
            Ok(result) => {
                std::fs::write(out.join(format!("{name}.kicad_sch")), &result.sch).unwrap();
                eprintln!(
                    "{name}: ok shorts={:?} opens={:?} warnings={:?}",
                    result.net_shorts, result.net_opens, result.layout_warnings
                );
            }
            Err(e) => eprintln!("{name}: {e}"),
        }
    }
}
