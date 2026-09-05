//! Audit the junction dots on finished sheets against KiCAD's rule.
//!
//! `cargo run -p sch-floorplan --example junction_audit -- <dir>` reads every
//! `.kicad_sch` in `<dir>` and reports, per sheet, how many dots sit where three
//! conductors meet versus fewer, and the joins that need a dot and lack one — the
//! before/after measurement for any change to the junction pass.

use std::path::PathBuf;

use sch_doc::SchDoc;

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: junction_audit <dir>"),
    );
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "kicad_sch"))
        .collect();
    files.sort();
    let (mut t_dots, mut t_spurious, mut t_missing) = (0, 0, 0);
    for path in &files {
        let doc = SchDoc::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        let meets = sch_doc::meets(&doc);
        let dots = sch_doc::drawn_dots(&doc);
        let spurious = dots
            .keys()
            .filter(|k| !meets.get(*k).copied().unwrap_or_default().needs_dot())
            .count();
        let missing: Vec<&sch_doc::MeetKey> = meets
            .iter()
            .filter(|(k, m)| m.needs_dot() && !dots.contains_key(*k))
            .map(|(k, _)| k)
            .collect();
        println!(
            "{}: dots={} spurious={spurious} joined={} missing={}",
            path.file_stem().unwrap().to_string_lossy(),
            dots.len(),
            dots.len() - spurious,
            missing.len()
        );
        if std::env::var("JUNCTION_AUDIT_VERBOSE").is_ok() {
            for k in missing.iter() {
                let m = meets[*k];
                println!(
                    "  undotted ({:.2},{:.2}) ends={} passes={} pins={}",
                    k.0 as f64 / 1000.0,
                    k.1 as f64 / 1000.0,
                    m.ends,
                    m.passes,
                    m.pins
                );
            }
        }
        t_dots += dots.len();
        t_spurious += spurious;
        t_missing += missing.len();
    }
    println!("TOTAL: dots={t_dots} spurious={t_spurious} missing={t_missing}");
}
