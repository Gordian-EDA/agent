//! Parity of `model.rs` against Python `schagent.model`: `design_from_json` -> `to_json`,
//! `normalize_json` and `apply_patch`.

use sch_engine::model::{apply_patch, design_from_json, normalize_json};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn design_json_round_trip_matches_python() {
    let mut checked = 0;
    for entry in std::fs::read_dir(fixtures()).unwrap().flatten() {
        let raw_path = entry.path().join("raw.json");
        let want_path = entry.path().join("model.json");
        if !raw_path.is_file() || !want_path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let raw = read(&raw_path);
        let want = read(&want_path);
        assert_eq!(
            normalize_json(&raw),
            want["normalized"],
            "{name}: normalize_json"
        );
        let des = design_from_json(&raw, None);
        assert_eq!(des.to_json(false), want["plain"], "{name}: to_json()");
        assert_eq!(
            des.to_json(true),
            want["with_ids"],
            "{name}: to_json(with_ids)"
        );
        checked += 1;
    }
    assert!(checked >= 15, "only {checked} fixtures had model goldens");
}

#[test]
fn apply_patch_matches_python() {
    let g = read(&fixtures().join("patch.json"));
    let (out, errs) = apply_patch(&g["base"], &g["patch"]);
    assert_eq!(out, g["result"], "patched design");
    let want: Vec<String> = g["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap().into())
        .collect();
    assert_eq!(errs, want, "patch errors");
}

/// `round_py` / `round_py_dp` against 4000 Python `round()` results.
#[test]
fn python_round_matches_on_4000_values() {
    let cases: Vec<Vec<f64>> =
        serde_json::from_value(read(&fixtures().join("roundfuzz.json"))).unwrap();
    for c in &cases {
        assert_eq!(sch_engine::model::round_py(c[0]), c[1], "round({})", c[0]);
        assert_eq!(
            sch_engine::model::round_py_dp(c[0], 3),
            c[2],
            "round({},3)",
            c[0]
        );
        assert_eq!(
            sch_engine::model::round_py_dp(c[0], 4),
            c[3],
            "round({},4)",
            c[0]
        );
    }
    assert!(cases.len() > 3000);
}
