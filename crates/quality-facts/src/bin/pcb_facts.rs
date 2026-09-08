//! Deterministic board facts for the quality harness.
//!
//! ```text
//! pcb_facts <project-dir-or-pcb>
//! ```
//!
//! Prints JSON on stdout: where every footprint sits, what it is, what the
//! outline encloses, and how much copper is drawn — read straight from the
//! `.kicad_pcb`. Comparing two of these is how "the untouched parts did not
//! move" stops being a matter of opinion.

use std::path::{Path, PathBuf};

use quality_facts::pcb::Board;
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
    let board = match Board::read(&path) {
        Ok(board) => board,
        Err(error) => return json!({ "error": format!("{}: {error}", path.display()) }),
    };
    let parts: Vec<Value> = board
        .footprints
        .iter()
        .map(|fp| {
            json!({
                "reference": fp.reference,
                "lib_id": fp.lib_id,
                "value": fp.value,
                // Rounded to the micrometre: a board file re-saved by KiCad
                // must not read as a move.
                "pose": [round(fp.at.x), round(fp.at.y), round(fp.rotation)],
                "pad_nets": fp.pad_nets,
            })
        })
        .collect();
    let track_length: f64 = board
        .tracks
        .iter()
        .flat_map(|track| track.path.windows(2))
        .map(|pair| (pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y))
        .sum();
    json!({
        "board": true,
        "parts": parts,
        "outline": board.outline.map(|r| {
            json!([round(r.min_x), round(r.min_y), round(r.max_x), round(r.max_y)])
        }),
        "nets": board.nets,
        "via_count": board.via_count,
        "track_count": board.tracks.len(),
        "total_track_length": (track_length * 1000.0).round() / 1000.0,
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
