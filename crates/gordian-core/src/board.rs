//! The PCB stage: a board from the accepted schematic, laid out, checked and
//! rendered. Deterministic — no model call happens here, so it runs concurrently
//! with the schematic polish pass.

use std::path::PathBuf;
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
