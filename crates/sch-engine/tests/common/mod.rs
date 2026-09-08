#![allow(dead_code)] // each test binary uses a different subset

//! Shared fixture loading for the S4 parity tests.

use sch_engine::Library;
use serde_json::Value;
use std::path::{Path, PathBuf};

const SYMBOL_DIR: &str = "/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/share/kicad/symbols";
const KICAD_CLI: &str = "/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/bin/kicad-cli";

pub struct Case {
    pub name: String,
    /// The laid-out design (Python `flexlayout.build_flex_design` output).
    pub raw: Value,
    /// The Python-compiled sheet, when the fixture has one.
    pub sheet: Option<String>,
    pub report: Value,
    pub extract: Value,
}

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read(p: &Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())),
    )
    .unwrap()
}

pub fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(fixtures()).unwrap().flatten() {
        let dir = entry.path();
        if !dir.join("raw.json").is_file() || !dir.join("report.json").is_file() {
            continue;
        }
        out.push(Case {
            name: entry.file_name().to_string_lossy().to_string(),
            raw: read(&dir.join("raw.json")),
            sheet: std::fs::read_to_string(dir.join("sheet.kicad_sch")).ok(),
            report: read(&dir.join("report.json")),
            extract: if dir.join("extract.json").is_file() {
                read(&dir.join("extract.json"))
            } else {
                Value::Null
            },
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    assert!(
        !out.is_empty(),
        "no fixtures under {}",
        fixtures().display()
    );
    out
}

/// The library index, or `None` when this machine has no KiCad symbols (the test then passes).
pub fn library() -> Option<Library> {
    let dir =
        PathBuf::from(std::env::var("KICAD_SYMBOL_DIR").unwrap_or_else(|_| SYMBOL_DIR.to_string()));
    dir.is_dir().then(|| Library::load(&dir).expect("index"))
}

pub fn kicad_cli() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var("KICAD_CLI").unwrap_or_else(|_| KICAD_CLI.to_string()));
    p.is_file().then_some(p)
}

/// Replace every uuid with a placeholder so two sheets compare on everything else.
pub fn normalise_uuids(text: &str) -> String {
    let b = text.as_bytes();
    let is_hex = |c: u8| c.is_ascii_hexdigit();
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        let uuid_here = i + 36 <= b.len()
            && (0..36).all(|k| match k {
                8 | 13 | 18 | 23 => b[i + k] == b'-',
                _ => is_hex(b[i + k]),
            });
        if uuid_here {
            out.extend_from_slice(b"<uuid>");
            i += 36;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).expect("sheet text stays valid utf-8")
}

/// One top-level `(wire ...)`: its two endpoints.
pub type WireSeg = ([f64; 2], [f64; 2]);

fn nums(line: &str) -> Vec<f64> {
    line.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .filter_map(|t| t.parse().ok())
        .collect()
}

/// Split a sheet into everything that is not a conductor, its wire segments and its junctions.
///
/// The engine fuses overlapping wire fragments and so writes fewer wires and fewer junction dots
/// than the reference; the parity tests compare those two lists on their own terms.
pub fn conductors(text: &str) -> (String, Vec<WireSeg>, Vec<[f64; 2]>) {
    let (mut rest, mut wires, mut junctions) = (String::new(), Vec::new(), Vec::new());
    let mut block: Vec<&str> = Vec::new();
    let mut kind = "";
    for line in text.lines() {
        if kind.is_empty() {
            if line == "\t(wire" || line == "\t(junction" {
                kind = line.trim_start_matches("\t(");
                block.clear();
                continue;
            }
            rest.push_str(line);
            rest.push('\n');
            continue;
        }
        if line == "\t)" {
            let pts: Vec<Vec<f64>> = block
                .iter()
                .filter(|l| l.contains("(xy ") || l.contains("(at "))
                .map(|l| nums(l))
                .collect();
            match kind {
                "wire" => wires.push((
                    [pts[0][0], pts[0][1]],
                    [pts[1][0], pts[1][1]],
                )),
                _ => junctions.push([pts[0][0], pts[0][1]]),
            }
            kind = "";
            continue;
        }
        block.push(line);
    }
    (rest, wires, junctions)
}

/// Segments as an order-and-direction-independent set of rounded endpoints.
pub fn seg_set(segs: &[WireSeg]) -> std::collections::BTreeSet<[i64; 4]> {
    let h = |v: f64| (v * 100.0).round() as i64;
    segs.iter()
        .map(|(a, b)| {
            let (p, q) = ([h(a[0]), h(a[1])], [h(b[0]), h(b[1])]);
            let (p, q) = if p <= q { (p, q) } else { (q, p) };
            [p[0], p[1], q[0], q[1]]
        })
        .collect()
}

pub fn point_set(pts: &[[f64; 2]]) -> std::collections::BTreeSet<[i64; 2]> {
    let h = |v: f64| (v * 100.0).round() as i64;
    pts.iter().map(|p| [h(p[0]), h(p[1])]).collect()
}
