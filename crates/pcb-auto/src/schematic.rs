//! Build a staging board from a schematic: the parts' `Footprint` fields become footprints, the
//! schematic netlist nets the pads, and everything is parked in a grid for the placer to work from.

use std::collections::BTreeMap;
use std::path::Path;

use kicad::KicadInstallation;
use kicad_footprint::{FootprintCatalog, FootprintId};

use crate::model::{parse_footprint_module, Board};
use crate::project;

/// Where the staging grid starts and how far apart it parks parts.
const STAGE_ORIGIN: (f64, f64) = (20.0, 20.0);
const STAGE_STEP: (f64, f64) = (22.0, 26.0);
const STAGE_COLS: usize = 6;

/// Create `out_pcb` (plus a `.kicad_pro` with sane rules) from a schematic.
///
/// Footprints come from each part's `Footprint` field; pad nets come from
/// `kicad-cli sch export netlist`. Returns the number of parts placed.
pub fn board_from_schematic(
    kicad: &KicadInstallation,
    sch: &Path,
    out_pcb: &Path,
) -> anyhow::Result<usize> {
    let netlist = kicad.netlist(sch)?;
    let catalog = FootprintCatalog::from_root(kicad.footprint_dir())?;

    // ref -> pad number -> net name
    let mut pad_nets: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for net in &netlist.nets {
        if net.name.trim().is_empty() || net.name.starts_with("unconnected-") {
            continue;
        }
        for (reference, pad) in &net.nodes {
            pad_nets
                .entry(reference.clone())
                .or_default()
                .insert(pad.clone(), net.name.clone());
        }
    }

    let mut board = Board::empty(2);
    let mut placed = 0usize;
    let mut missing: Vec<String> = Vec::new();
    let (mut x, mut y) = STAGE_ORIGIN;
    for comp in &netlist.components {
        let Some(lib_id) = comp
            .properties
            .get("Footprint")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        else {
            missing.push(format!("{}: no Footprint field", comp.reference));
            continue;
        };
        let id = match FootprintId::parse(lib_id) {
            Ok(id) => id,
            Err(_) => {
                missing.push(format!("{}: unreadable footprint {lib_id:?}", comp.reference));
                continue;
            }
        };
        let source = match catalog.source(&id) {
            Ok(s) => s,
            Err(_) => {
                missing.push(format!("{}: {lib_id} is not in the library", comp.reference));
                continue;
            }
        };
        let module = parse_footprint_module(&source)?;
        board.add_footprint(
            &module,
            &comp.reference,
            &comp.value,
            (x, y),
            0.0,
            "front",
            lib_id,
        );
        for (pad, net) in pad_nets.get(&comp.reference).into_iter().flatten() {
            board.set_pad_net(&comp.reference, pad, net);
        }
        placed += 1;
        x += STAGE_STEP.0;
        if placed % STAGE_COLS == 0 {
            x = STAGE_ORIGIN.0;
            y += STAGE_STEP.1;
        }
    }
    anyhow::ensure!(
        placed > 0,
        "no part of {} carries a usable Footprint field ({})",
        sch.display(),
        missing.join("; ")
    );
    if let Some(parent) = out_pcb.parent() {
        std::fs::create_dir_all(parent)?;
    }
    board.save(Some(out_pcb))?;
    let rules = crate::rules::infer_rules(&board);
    project::write_project_rules(out_pcb, &rules, &board)?;
    if !missing.is_empty() {
        tracing::warn!(skipped = missing.len(), "{}", missing.join("; "));
    }
    Ok(placed)
}
