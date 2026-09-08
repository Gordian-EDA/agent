//! The PCB stage: a board from the accepted schematic, laid out, checked and
//! rendered. Deterministic — no model call happens here, so it runs concurrently
//! with the schematic polish pass.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use kicad::KicadInstallation;
use serde_json::{Value, json};

use crate::agent::event;
use crate::engines::pcb;

/// What the board stage produced.
#[derive(Default)]
pub struct BoardOutcome {
    pub pcb: Option<PathBuf>,
    pub front_png: Option<PathBuf>,
    pub back_png: Option<PathBuf>,
    pub fab_files: Vec<PathBuf>,
    pub parts: usize,
    pub completion: f64,
    pub unrouted: usize,
    pub drc_errors: usize,
    pub drc_warnings: usize,
    pub outline_mm: (f64, f64),
    pub notes: Vec<String>,
    pub seconds: f64,
    pub error: Option<String>,
}

impl BoardOutcome {
    /// The board's half of `report.json`.
    pub fn as_json(&self) -> Value {
        json!({
            "pcb": self.pcb.as_ref().map(|p| p.display().to_string()),
            "render": self.front_png.as_ref().map(|p| p.display().to_string()),
            "render_back": self.back_png.as_ref().map(|p| p.display().to_string()),
            "fab_files": self.fab_files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "parts": self.parts,
            "completion": self.completion,
            "unrouted": self.unrouted,
            "drc_errors": self.drc_errors,
            "drc_warnings": self.drc_warnings,
            "outline_mm": [self.outline_mm.0, self.outline_mm.1],
            "notes": self.notes,
            "seconds": (self.seconds * 10.0).round() / 10.0,
            "error": self.error,
        })
    }
}

/// Everything about a sheet that the board depends on: every net with the pins
/// on it, and each part's footprint. Two sheets with the same identity route to
/// the same board, so a board built from one can be delivered for the other —
/// which is what lets a re-arranged layout keep a board that is already routed.
#[derive(Debug, PartialEq, Eq)]
pub struct Identity {
    nets: BTreeSet<(String, Vec<(String, String)>)>,
    footprints: BTreeMap<String, String>,
}

/// Read a sheet's board identity, or `None` when KiCad cannot export its netlist.
pub fn identity(kicad: &KicadInstallation, sch: &Path) -> Option<Identity> {
    let netlist = kicad.netlist(sch).ok()?;
    Some(Identity {
        nets: netlist
            .nets
            .into_iter()
            .map(|net| {
                let mut nodes = net.nodes;
                nodes.sort();
                (net.name, nodes)
            })
            .collect(),
        footprints: netlist
            .components
            .into_iter()
            .map(|c| {
                let footprint = c.properties.get("Footprint").cloned().unwrap_or_default();
                (c.reference, footprint)
            })
            .collect(),
    })
}

/// What the board stage is pointed at. `sch` is a snapshot of the delivered
/// sheet, so the polish pass may keep re-writing the project's own copy while
/// the board is being built from the same netlist.
pub struct Job {
    pub sch: PathBuf,
    pub pcb: PathBuf,
    pub render_png: PathBuf,
    pub timeout_s: u64,
}

/// Create the board from `job.sch`, auto-lay it out within its timeout, and
/// render the front. Never fails the run: a broken board stage is reported.
pub fn build(kicad: &KicadInstallation, job: &Job) -> BoardOutcome {
    let started = Instant::now();
    let mut outcome = BoardOutcome {
        pcb: Some(job.pcb.clone()),
        ..Default::default()
    };
    match run(kicad, job, &mut outcome) {
        Ok(()) => {}
        Err(error) => {
            outcome.error = Some(format!("{error:#}"));
            event(format!("board: failed: {error:#}"));
        }
    }
    outcome.seconds = started.elapsed().as_secs_f64();
    outcome
}

fn run(kicad: &KicadInstallation, job: &Job, outcome: &mut BoardOutcome) -> Result<()> {
    outcome.parts = pcb::board_from_schematic(kicad, &job.sch, &job.pcb)?;
    event(format!(
        "board: {} part(s) from the schematic",
        outcome.parts
    ));
    let options = pcb::AutoOptions {
        outline: pcb::Outline::Suggest,
        holes: 4,
        layers: 2,
        edge_for: Default::default(),
        gnd_zone: true,
        timeout_s: job.timeout_s,
    };
    let report = pcb::auto_layout(kicad, &job.pcb, &options)?;
    outcome.completion = report.completion;
    outcome.unrouted = report.unrouted;
    outcome.drc_errors = report.drc_errors;
    outcome.drc_warnings = report.drc_warnings;
    outcome.outline_mm = report.outline_mm;
    outcome.notes = report.notes;
    event(format!(
        "board: {:.1}% routed, {} unrouted, {} DRC error(s), {:.0}x{:.0} mm",
        report.completion * 100.0,
        report.unrouted,
        report.drc_errors,
        report.outline_mm.0,
        report.outline_mm.1
    ));

    if pcb::render(kicad, &job.pcb, &job.render_png, pcb::Side::Front).is_ok() {
        outcome.front_png = Some(job.render_png.clone());
    }
    Ok(())
}
