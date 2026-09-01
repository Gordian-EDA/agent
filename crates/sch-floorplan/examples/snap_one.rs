//! Dump the Design + LayoutIr a `place-parts` validation fixture builds, or
//! (with `--sch`) render it exactly as `placement_snapshot` does.
use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;

fn main() {
    let name = std::env::args().nth(1).unwrap();
    let sch = std::env::args().any(|a| a == "--sch");
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/validation");
    let env = KicadInstallation::detect().unwrap();
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let src = std::fs::read_to_string(format!("{dir}/{name}.place-parts.json")).unwrap();
    let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
    let (design, diags) = sch_check::into_design(&input, &provider);
    assert!(!diags.has_errors(), "{diags:#?}");
    let ir = input
        .intent
        .clone()
        .map(sch_check::Intent::into_layout_ir)
        .unwrap_or_else(|| floorplan::infer_ir(&env, &design));
    if sch {
        print!(
            "{}",
            floorplan::emit_strategy(&env, &design, Box::new(anneal_place::Anneal), Some(ir))
                .unwrap()
                .sch
        );
    } else {
        println!("== DESIGN ==\n{design:#?}");
        println!("== IR ==\n{ir:#?}");
    }
}
