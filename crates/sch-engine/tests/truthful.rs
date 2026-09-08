//! Correctness gate: a delivered sheet must carry the netlist it was given.
//!
//! The Python reference is reproduced elsewhere by bit-for-bit parity; here the
//! fixtures are judged against the design instead, because the reference itself
//! lays some nets on top of each other (see `PYTHON_SHORTS` in `engine.rs`).

use std::path::{Path, PathBuf};

use sch_engine::Library;
use serde_json::Value;

const SYMBOL_DIR: &str = "/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/share/kicad/symbols";

fn library() -> Option<Library> {
    let dir = std::env::var("KICAD_SYMBOL_DIR").unwrap_or_else(|_| SYMBOL_DIR.to_string());
    let dir = PathBuf::from(dir);
    dir.is_dir().then(|| Library::load(&dir).expect("symbol index"))
}

#[test]
fn no_fixture_design_splits_or_shorts_a_net() {
    let Some(lib) = library() else {
        eprintln!("no KiCad symbol dir; skipping");
        return;
    };
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let out = tempfile::tempdir().unwrap();
    let mut names: Vec<PathBuf> = std::fs::read_dir(&fixtures)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("design.json").is_file())
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no fixtures under {}", fixtures.display());

    let mut failures: Vec<String> = Vec::new();
    for dir in &names {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let design: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("design.json")).unwrap())
                .unwrap();
        let report = sch_engine::build(&lib, &design, &out.path().join(format!("{name}.kicad_sch")))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        // several fixtures are deliberately malformed designs (duplicate parts, unknown
        // pins); only the connectivity faults are this test's business
        for issue in report
            .issues
            .iter()
            .filter(|i| i.contains("shorted together") || i.contains("is split into"))
        {
            failures.push(format!("{name}: {issue}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} netlist faults over {} fixtures:\n  {}",
        failures.len(),
        names.len(),
        failures.join("\n  ")
    );
}
