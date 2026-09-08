//! A faithful Rust port of the `schagent` deterministic schematic engine: a netlist + flexbox-style
//! layout tree design JSON is laid out, compiled to a KiCad 10 `.kicad_sch` and checked.

pub mod check;
pub mod compile;
pub mod engine;
pub mod extract;
pub mod flexlayout;
pub mod geo;
pub mod model;
pub mod sexp;
pub mod symlib;

pub use geo::Geo;
pub use model::{Design, Label, Part, Power, Rect, Text};
pub use symlib::{Library, Pin, SymbolHit, SymbolInfo};

use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What one `build` produced: the laid-out design, its problems and its netlist.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BuildReport {
    /// Hard problems; the build is "not clean" while this is non-empty.
    pub issues: Vec<String>,
    pub warnings: Vec<String>,
    /// e.g. "paper changed to A3", "block X folded into Y".
    pub notes: Vec<String>,
    /// net -> {"U1.12", ...} pin NUMBERS.
    pub netlist: BTreeMap<String, BTreeSet<String>>,
    pub paper: String,
    /// The laid-out raw design (absolute grid units) — Python's `d_raw`.
    pub raw: serde_json::Value,
    /// The checker's `net: pins` listing, exactly as Python's `Checker.netlist_text` renders it.
    pub netlist_lines: String,
}

impl BuildReport {
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }

    /// `net: pins` listing with the auto-named `N$n` nets last, as Python's `Checker.netlist_text` renders it.
    pub fn netlist_text(&self, limit: usize) -> String {
        let mut nets: Vec<(&String, &BTreeSet<String>)> = self.netlist.iter().collect();
        nets.sort_by(|a, b| (a.0.starts_with("N$"), a.0).cmp(&(b.0.starts_with("N$"), b.0)));
        let mut lines: Vec<String> = nets
            .iter()
            .map(|(net, pins)| {
                format!(
                    "{net}: {}",
                    pins.iter().cloned().collect::<Vec<_>>().join(" ")
                )
            })
            .collect();
        if lines.len() > limit {
            lines.truncate(limit);
            lines.push(format!("... ({} more nets)", self.netlist.len() - limit));
        }
        lines.join("\n")
    }
}

/// Netlist+layout-tree design JSON (the `build` tool argument) -> laid out, compiled to `out_sch`, checked.
pub fn build(_lib: &Library, design: &serde_json::Value, out_sch: &Path) -> Result<BuildReport> {
    let (raw, errors) = flexlayout::build_flex_design(design);
    let mut report = finish(raw, errors, None, out_sch)?;
    // a netlist design also has to come out as one connected net per intended net
    let has_pins = design
        .get("parts")
        .and_then(|v| v.as_array())
        .is_some_and(|a| a.iter().any(|p| p.get("pins").is_some()));
    if has_pins && !report.netlist.is_empty() {
        report
            .issues
            .extend(check::netlist_mismatch(design, &report.netlist));
    }
    Ok(report)
}

/// Edit mode: the model's patch against a base sheet, packed into the sheet's free space.
pub fn build_patch(
    _lib: &Library,
    base: &Design,
    patch: &serde_json::Value,
    out_sch: &Path,
) -> Result<BuildReport> {
    let (mut raw, mut errors) = model::apply_patch(&base.to_json(true), patch);
    if let Some(circuit) = patch.get("add").and_then(|a| a.get("circuit")) {
        let paper = raw
            .get("paper")
            .and_then(|p| p.as_str())
            .unwrap_or("A4")
            .to_string();
        let (next, errs) = engine::add_circuit_to_raw(&raw, circuit, &paper);
        raw = next;
        errors.extend(errs);
    }
    finish(raw, errors, Some(base), out_sch)
}

/// Compile + check a laid-out raw design and fold the layout messages into the report.
fn finish(
    raw: serde_json::Value,
    errors: Vec<String>,
    base: Option<&Design>,
    out_sch: &Path,
) -> Result<BuildReport> {
    let (notes, hard): (Vec<String>, Vec<String>) =
        errors.into_iter().partition(|e| e.starts_with("note:"));
    let paper = raw
        .get("paper")
        .and_then(|p| p.as_str())
        .unwrap_or("A4")
        .to_string();
    if !hard.is_empty() {
        return Ok(BuildReport {
            issues: hard,
            notes,
            paper,
            raw,
            ..Default::default()
        });
    }
    let built = check::build(&raw, out_sch, base)?;
    Ok(BuildReport {
        issues: built.issues,
        warnings: built.warnings,
        notes,
        netlist: built.nets,
        paper,
        raw,
        netlist_lines: built.netlist_text,
    })
}

/// Lossless load of an existing sheet.
pub fn extract(sch: &Path) -> Result<Design> {
    extract::load(sch)
}

/// KiCad ERC violation lines, errors first.
pub fn run_erc(kicad_cli: &Path, sch: &Path) -> Result<Vec<String>> {
    check::run_erc(kicad_cli, sch)
}
