//! Realiser-level truthfulness oracle. Two halves, both invisible on the rendered sheet:
//! no point may be shared by two nets (a SHORT), and no authored net may come back in
//! more than one piece (an OPEN). This is the geometry behind the `place_parts` refusals
//! "the placed result does not match the requested connectivity (shorted A+B)" and
//! "(scattered GND)" — checked here directly on the emitted sheet, so a regression is a
//! failing unit test rather than a refused tool call a whole typeset later.
//!
//! Runs over every `place-parts` fixture in the validation corpus twice: once straight
//! through `emit_strategy` (`realised_corpus_sheets_are_truthful`, the same path
//! `floorplan_netlist` exercises), and once through the LIVE path the agent actually
//! places with (`live_place_parts_commits_every_corpus_fixture`, `place_parts` onto a
//! blank sheet), which `floorplan_netlist` does not cover. SKIPs without KiCAD.

use std::path::Path;

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;

fn corpus() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/validation")
}

/// Every `<name>.place-parts.json` in the corpus, in name order.
fn fixtures() -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(corpus())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".place-parts.json")
                .map(str::to_string)
        })
        .collect();
    out.sort();
    out
}

/// Realise `name` and report both halves of untruthfulness: every point two nets share,
/// and every authored net the sheet leaves in islands.
fn defects_of(env: &KicadInstallation, provider: &SymbolTable, name: &str) -> Vec<String> {
    let src = std::fs::read_to_string(corpus().join(format!("{name}.place-parts.json"))).unwrap();
    let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
    let (design, diags, _) = sch_check::into_design(&input, provider, &Default::default());
    assert!(!diags.has_errors(), "{name}: {diags:#?}");
    let ir = input
        .intent
        .clone()
        .map(sch_check::Intent::into_layout_ir)
        .unwrap_or_else(|| floorplan::baseline_ir(&design));
    let out =
        floorplan::emit_strategy(env, &design, Some(ir)).unwrap_or_else(|e| panic!("{name}: {e}"));
    out.net_shorts
        .into_iter()
        .chain(out.net_opens.into_iter().map(|net| format!("OPEN {net}")))
        .collect()
}

#[test]
fn realised_corpus_sheets_are_truthful() {
    if !corpus().is_dir() {
        eprintln!("SKIP: validation corpus not present");
        return;
    }
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    // `CLEARANCE_ONLY=bms-10s,esp32-multifunction` restricts the run for fast iteration
    // on one fixture; empty runs the whole corpus.
    let only = std::env::var("CLEARANCE_ONLY").unwrap_or_default();
    let only: Vec<&str> = only.split(',').filter(|s| !s.is_empty()).collect();
    let mut offenders: Vec<String> = Vec::new();
    for name in fixtures() {
        if !only.is_empty() && !only.contains(&name.as_str()) {
            continue;
        }
        let found = defects_of(&env, &provider, &name);
        if !found.is_empty() {
            offenders.push(format!("{name}: {found:#?}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "realised sheets are untruthful (shared points, or a net left in islands):\n{}",
        offenders.join("\n")
    );
}

/// The same corpus through the LIVE path the agent calls — `place_parts` onto a blank
/// sheet — asserting the gate it is refused by. A refusal here is the `place_parts`
/// failure a campaign pays a whole retry for, reproduced without one.
#[test]
fn live_place_parts_commits_every_corpus_fixture() {
    if !corpus().is_dir() {
        eprintln!("SKIP: validation corpus not present");
        return;
    }
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let only = std::env::var("CLEARANCE_ONLY").unwrap_or_default();
    let only: Vec<&str> = only.split(',').filter(|s| !s.is_empty()).collect();
    let mut refused: Vec<String> = Vec::new();
    for name in fixtures() {
        if !only.is_empty() && !only.contains(&name.as_str()) {
            continue;
        }
        let src =
            std::fs::read_to_string(corpus().join(format!("{name}.place-parts.json"))).unwrap();
        let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
        let (_, diags, _) = sch_check::into_design(&input, &provider, &Default::default());
        assert!(!diags.has_errors(), "{name}: {diags:#?}");
        let mut doc = sch_floorplan::live::blank_sheet().unwrap();
        match sch_floorplan::live::place_parts(&env, &mut doc, &input) {
            Ok(report) if !report.committed => {
                refused.push(format!("{name}: {:?}", report.mismatch))
            }
            Ok(_) => {}
            // A payload the live surface rejects outright says nothing about the
            // realiser: the corpus is written for the whole-sheet path, which accepts
            // forms (authored `power:` declarations, say) that live editing does not.
            Err(sch_floorplan::live::Error::InvalidPayload(_)) => {}
            Err(e) => panic!("{name}: {e}"),
        }
    }
    assert!(
        refused.is_empty(),
        "live place_parts refused fixtures the realiser should draw truthfully:\n{}",
        refused.join("\n")
    );
}
