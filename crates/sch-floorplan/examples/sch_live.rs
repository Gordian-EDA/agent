//! `sch_live` — drive the live-edit API from a shell, one `.kicad_sch` at a time.
//!
//! ```text
//! sch_live place-parts <sch> <input.json>
//! sch_live arrange     <sch> R1,R2,… | @block
//! sch_live rewire      <sch> R1,R2,… | @block
//! ```
//!
//! The schematic is edited in place — created blank if it does not exist — and the
//! operation's report is printed to stdout as JSON. A report with a non-empty
//! `mismatch` means the document was rolled back and the process exits non-zero.

use std::path::Path;

use kicad::KicadInstallation;
use sch_check::PlacePartsInput;
use sch_doc::SchDoc;
use sch_floorplan::live::{self, Selection};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let positional: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let [command, sheet, rest @ ..] = positional.as_slice() else {
        eprintln!("usage: sch_live <place-parts|arrange|rewire> <sch> <arg>");
        std::process::exit(2);
    };

    let env = KicadInstallation::detect().ok_or("no KiCad installation detected")?;
    let path = Path::new(sheet);
    let mut doc = match path.exists() {
        true => SchDoc::read(path)?,
        false => live::blank_sheet()?,
    };

    let (json, committed) = match *command {
        "place-parts" => {
            let source = std::fs::read_to_string(rest.first().ok_or("missing input.json")?)?;
            let input: PlacePartsInput = serde_json::from_str(&source)?;
            let report = live::place_parts(&env, &mut doc, &input)?;
            (serde_json::to_string_pretty(&report)?, report.committed)
        }
        "arrange" => {
            let report = live::arrange(&env, &mut doc, &selection(rest)?, None, None)?;
            (serde_json::to_string_pretty(&report)?, report.committed)
        }
        "rewire" => {
            let report = live::rewire(&env, &mut doc, &selection(rest)?)?;
            (serde_json::to_string_pretty(&report)?, report.committed)
        }
        other => return Err(format!("unknown command `{other}`").into()),
    };

    doc.write(path)?;
    println!("{json}");
    if !committed {
        std::process::exit(1);
    }
    Ok(())
}

/// `R1,R2,R3`, four numbers `x1,y1,x2,y2` for a bounding box, or `@name` for a block.
fn selection(rest: &[&str]) -> Result<Selection, Box<dyn std::error::Error>> {
    let list = rest.first().ok_or("missing selection")?;
    if let Some(block) = list.strip_prefix('@') {
        return Ok(Selection::Block(block.to_string()));
    }
    let parts: Vec<&str> = list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let numbers: Option<Vec<f64>> = parts.iter().map(|p| p.parse().ok()).collect();
    match numbers {
        Some(n) if n.len() == 4 => Ok(Selection::Bbox([n[0], n[1], n[2], n[3]])),
        _ => Ok(Selection::Refs(
            parts.into_iter().map(String::from).collect(),
        )),
    }
}
