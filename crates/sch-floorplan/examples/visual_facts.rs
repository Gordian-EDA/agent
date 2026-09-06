//! Count the visual defects the quality harness counts, on sheets already emitted.
//!
//! ```text
//! cargo run --release -p sch-floorplan --example visual_facts -- <dir-or-sheet>…
//! ```
//!
//! [`sch_floorplan::visual::measure`] is what the harness reports as
//! `body_overlaps` / `text_collisions` / `wires_through_bodies`, so running it over
//! the corpus [`render_corpus`](../examples/render_corpus.rs) and
//! [`replay_blocks`](../examples/replay_blocks.rs) write gives that metric on inputs
//! that do not move between revisions — the engine's own `layout_warnings` model a
//! label differently and cannot stand in for it.

use std::path::PathBuf;

fn main() {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    assert!(!args.is_empty(), "usage: visual_facts <dir-or-sheet>…");
    let mut sheets: Vec<PathBuf> = Vec::new();
    for arg in args {
        match arg.is_dir() {
            true => sheets.extend(std::fs::read_dir(&arg).unwrap().flatten().map(|e| e.path())),
            false => sheets.push(arg),
        }
    }
    sheets.retain(|path| path.extension().is_some_and(|ext| ext == "kicad_sch"));
    sheets.sort();

    let (mut texts, mut bodies, mut wires) = (0, 0, 0);
    for path in sheets {
        let doc = sch_doc::SchDoc::read(&path).unwrap();
        let facts = sch_floorplan::visual::measure(&doc);
        println!(
            "{:44} text_collisions={:4} body_overlaps={:3} wires_through_bodies={:3}",
            path.file_stem().unwrap().to_string_lossy(),
            facts.text_collisions.len(),
            facts.body_overlaps.len(),
            facts.wires_through_bodies.len()
        );
        if std::env::var_os("VERBOSE").is_some() {
            for c in &facts.text_collisions {
                println!("    text: {c:?}");
            }
        }
        texts += facts.text_collisions.len();
        bodies += facts.body_overlaps.len();
        wires += facts.wires_through_bodies.len();
    }
    println!("TOTAL text_collisions={texts} body_overlaps={bodies} wires_through_bodies={wires}");
}
