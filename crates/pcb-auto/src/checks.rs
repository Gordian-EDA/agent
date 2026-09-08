//! What the pipeline needs to know about whether a board is done: KiCad DRC as the oracle,
//! plus the geometry facts DRC cannot see (courtyard overlaps, parts off the board).

use std::collections::BTreeMap;
use std::path::Path;

use kicad::KicadInstallation;

use crate::geom::box_in_polygon;
use crate::model::Board;

/// DRC plus the model-derived facts, for one board.
#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    pub ok: bool,
    /// `1 - unconnected / connections`.
    pub completion: f64,
    /// Pad-to-pad connections the netlist asks for.
    pub connections: usize,
    pub unconnected: usize,
    pub errors: usize,
    pub warnings: usize,
    pub tracks: usize,
    pub vias: usize,
    pub by_type: BTreeMap<String, usize>,
    /// Nets DRC reports a missing connection on, most-cited first.
    pub unrouted_nets: Vec<String>,
    pub courtyard_overlaps: Vec<(String, String)>,
    pub parts_outside_outline: Vec<String>,
    pub outline_mm: (f64, f64),
}

/// Connections the netlist asks for: one per pad past the first on every net.
pub fn total_connections(board: &Board) -> usize {
    board
        .pads_by_net()
        .values()
        .map(|pads| pads.len().saturating_sub(1))
        .sum()
}

/// Pairs of same-side footprints whose courtyards overlap: pure geometry, no DRC run.
pub fn courtyard_overlaps(board: &Board) -> Vec<(String, String)> {
    let fps: Vec<_> = board.footprints().into_iter().filter(|f| !f.is_dnp()).collect();
    let mut out = Vec::new();
    for (i, a) in fps.iter().enumerate() {
        for b in &fps[i + 1..] {
            if a.side() == b.side() && a.courtyard_bbox().overlaps(&b.courtyard_bbox()) {
                out.push((a.ref_.clone(), b.ref_.clone()));
            }
        }
    }
    out
}

pub fn parts_outside_outline(board: &Board) -> Vec<String> {
    let Some(poly) = board.outline_polygon() else {
        return vec![];
    };
    board
        .footprints()
        .into_iter()
        .filter(|f| !f.is_dnp() && !box_in_polygon(&f.courtyard_bbox(), &poly))
        .map(|f| f.ref_)
        .collect()
}

/// Types KiCad reports that say nothing about this board: the footprint on it differs from the
/// one in the library, or a text variable is unresolved. Neither is a layout fault.
const IGNORED: [&str; 3] = [
    "lib_footprint_issues",
    "lib_footprint_mismatch",
    "unresolved_variable",
];

/// The net a DRC item names. KiCad brackets it: `Track [GND] on F.Cu`, `Pad 1 [GND] of C1`.
fn net_in_description(desc: &str) -> Option<String> {
    let i = desc.find('[')?;
    let rest = &desc[i + 1..];
    let j = rest.find(']')?;
    (j > 0 && j <= 64).then(|| rest[..j].to_string())
}

/// DRC + completion, the way the pipeline judges a board. Zones are refilled first, so a pour
/// counts as copper for the unconnected test.
pub fn check(kicad: &KicadInstallation, pcb: &Path) -> anyhow::Result<CheckReport> {
    let board = Board::load(pcb)?;
    let report = kicad.drc(pcb)?;
    let connections = total_connections(&board);
    let unconnected = report.unconnected_items.len();
    let judged = || {
        report
            .violations
            .iter()
            .filter(|v| !IGNORED.contains(&v.kind.as_str()))
    };
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    for v in judged().filter(|v| v.severity == "error") {
        *by_type.entry(v.kind.clone()).or_default() += 1;
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for u in &report.unconnected_items {
        for item in &u.items {
            if let Some(n) = net_in_description(&item.description) {
                *counts.entry(n).or_default() += 1;
            }
        }
    }
    let mut unrouted_nets: Vec<(String, usize)> = counts.into_iter().collect();
    unrouted_nets.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let overlaps = courtyard_overlaps(&board);
    let outside = parts_outside_outline(&board);
    let bb = board.outline_bbox();
    let errors = judged().filter(|v| v.severity == "error").count();
    let warnings = judged().filter(|v| v.severity == "warning").count();
    Ok(CheckReport {
        ok: errors == 0 && unconnected == 0 && overlaps.is_empty() && outside.is_empty(),
        completion: if connections == 0 {
            1.0
        } else {
            (1.0 - unconnected as f64 / connections as f64).max(0.0)
        },
        connections,
        unconnected,
        errors,
        warnings,
        tracks: board.tracks().len(),
        vias: board.vias().len(),
        by_type,
        unrouted_nets: unrouted_nets.into_iter().map(|(n, _)| n).collect(),
        courtyard_overlaps: overlaps,
        parts_outside_outline: outside,
        outline_mm: bb.map(|b| (b.w(), b.h())).unwrap_or((0.0, 0.0)),
    })
}
