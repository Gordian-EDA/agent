//! Deterministic board facts for the quality harness.
//!
//! ```text
//! pcb_facts <project-dir-or-pcb>
//! ```
//!
//! Prints JSON on stdout: where every footprint sits, what it is, and what the
//! outline encloses — read straight from the `.kicad_pcb`, with no live KiCAD
//! session. Comparing two of these is how "the untouched parts did not move"
//! stops being a matter of opinion. The quality runner is the only consumer, so
//! the shape is flat and stable rather than general.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path] = args.as_slice() else {
        eprintln!("usage: pcb_facts <project-dir-or-pcb>");
        std::process::exit(2);
    };
    println!("{}", facts(Path::new(path)));
}

fn facts(root: &Path) -> Value {
    let Some(path) = board_path(root) else {
        return json!({ "board": false, "parts": [] });
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return json!({ "error": format!("could not read {}: {e}", path.display()) }),
    };
    let doc = match kicad_board::BoardDoc::parse(text.clone()) {
        Ok(doc) => doc,
        Err(e) => return json!({ "error": e }),
    };
    let mut parts: Vec<Value> = doc
        .footprints()
        .into_iter()
        .map(|fp| {
            json!({
                "reference": fp.reference,
                "lib_id": fp.lib_id,
                "value": fp.value,
                // Rounded to the micrometre: a board file re-saved by KiCAD
                // must not read as a move.
                "pose": [round(fp.at.x), round(fp.at.y), round(fp.rotation)],
                "pad_nets": fp.pad_nets,
            })
        })
        .collect();
    parts.sort_by(|a, b| a["reference"].as_str().cmp(&b["reference"].as_str()));
    json!({
        "board": true,
        "parts": parts,
        "outline": kicad_board::board_outline_bbox(&text)
            .map(|(min_x, min_y, max_x, max_y)| json!([
                round(min_x),
                round(min_y),
                round(max_x),
                round(max_y)
            ])),
        "nets": doc.net_codes().keys().collect::<Vec<_>>(),
    })
}

fn round(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// The project's board, chosen deterministically.
fn board_path(root: &Path) -> Option<PathBuf> {
    if root.is_file() {
        return Some(root.to_path_buf());
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "kicad_pcb"))
        .collect();
    found.sort();
    found.into_iter().next()
}
