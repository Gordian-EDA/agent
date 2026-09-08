//! The `.kicad_pro` beside a board: the rules `kicad-cli pcb drc` checks that board against.
//!
//! Without one, KiCad falls back to built-in constraints a finely routed board violates. A value
//! written here is never stricter than the board's own geometry, nor — except clearance — than
//! KiCad's own default. Port of `pcbagent/kicad/project_file.py`.

use std::path::{Path, PathBuf};

use crate::model::{Board, Rules};

/// KiCad 10's built-in constraints, measured rather than guessed.
mod defaults {
    pub const MIN_CLEARANCE: f64 = 0.2;
    pub const MIN_TRACK_WIDTH: f64 = 0.2;
    pub const MIN_VIA_DIAMETER: f64 = 0.5;
    pub const MIN_VIA_ANNULAR_WIDTH: f64 = 0.1;
    pub const MIN_HOLE_CLEARANCE: f64 = 0.25;
    pub const MIN_HOLE_TO_HOLE: f64 = 0.25;
    pub const MIN_COPPER_EDGE_CLEARANCE: f64 = 0.5;
    pub const MIN_THROUGH_HOLE_DIAMETER: f64 = 0.3;
}

/// Round down to a 0.01 mm grid: a rule must sit at or below what it measures.
fn floor_mm(x: f64) -> f64 {
    ((x / 0.01 + 1e-9).floor() * 0.01 * 10000.0).round() / 10000.0
}

pub fn project_path(board_path: &Path) -> PathBuf {
    board_path.with_extension("kicad_pro")
}

/// The tightest geometry already on the board: the ceiling for every rule written.
struct Minimums {
    annular: Option<f64>,
    via_diameter: Option<f64>,
    hole: Option<f64>,
    track_width: Option<f64>,
}

fn board_minimums(board: &Board) -> Minimums {
    let mut ann = Vec::new();
    let mut via_d = Vec::new();
    let mut hole = Vec::new();
    let mut track = Vec::new();
    for v in board.vias() {
        via_d.push(v.size);
        hole.push(v.drill);
        ann.push((v.size - v.drill) / 2.0);
    }
    for t in board.tracks() {
        track.push(t.width);
    }
    for fp in board.footprints() {
        for p in &fp.pads {
            let Some(d) = p.drill else { continue };
            if p.kind != "thru_hole" || d <= 0.0 {
                continue;
            }
            hole.push(d);
            ann.push((p.size.0.min(p.size.1) - d) / 2.0);
        }
    }
    let min_of = |v: Vec<f64>| {
        v.into_iter()
            .filter(|x| *x > 0.0)
            .fold(f64::INFINITY, f64::min)
    };
    let opt = |x: f64| x.is_finite().then(|| floor_mm(x));
    Minimums {
        annular: opt(min_of(ann)),
        via_diameter: opt(min_of(via_d)),
        hole: opt(min_of(hole)),
        track_width: opt(min_of(track)),
    }
}

/// The smallest of what we derived, what the board contains and KiCad's default, so a rule can
/// only ever loosen the check. Clearance has no default ceiling: KiCad's real minimum is 0, and a
/// board routed at 0.3 mm checked at 0.2 mm hides the errors the widening was meant to avoid.
fn limit(default: Option<f64>, derived: Option<f64>, measured: Option<f64>) -> f64 {
    let mut v = f64::INFINITY;
    for x in [default, derived, measured].into_iter().flatten() {
        v = v.min(x);
    }
    floor_mm(if v.is_finite() { v } else { defaults::MIN_CLEARANCE })
}

/// Write (or update) `<board stem>.kicad_pro` so DRC checks the board against `rules`.
pub fn write_project_rules(board_path: &Path, rules: &Rules, board: &Board) -> anyhow::Result<PathBuf> {
    let m = board_minimums(board);
    let clearance = rules.clearance;
    let annular = ((rules.via_size - rules.via_drill) / 2.0).max(0.1);
    // Hole and edge rules relax only when the copper itself is finer than KiCad's default.
    let fine = (clearance < defaults::MIN_CLEARANCE).then_some(clearance);

    let min_clearance = limit(None, Some(clearance), None);
    let min_track_width = limit(
        Some(defaults::MIN_TRACK_WIDTH),
        Some(rules.track_width),
        m.track_width,
    );
    let min_via_diameter = limit(
        Some(defaults::MIN_VIA_DIAMETER),
        Some(rules.via_size),
        m.via_diameter,
    );
    let min_via_annular = limit(
        Some(defaults::MIN_VIA_ANNULAR_WIDTH),
        Some(annular),
        m.annular,
    );
    let min_hole_clearance = limit(Some(defaults::MIN_HOLE_CLEARANCE), fine, None);
    let min_hole_to_hole = limit(Some(defaults::MIN_HOLE_TO_HOLE), fine, None);
    let min_edge = limit(Some(defaults::MIN_COPPER_EDGE_CLEARANCE), fine, None);
    let min_hole = limit(Some(defaults::MIN_THROUGH_HOLE_DIAMETER), None, m.hole);

    // The Default netclass must satisfy the constraint block beside it, or every via drawn to it
    // is a `drill_out_of_range` violation of the project's own rules.
    let drill = rules.via_drill.max(min_hole);
    let diameter = rules
        .via_size
        .max(drill + 2.0 * min_via_annular)
        .max(min_via_diameter);
    let r4 = |v: f64| (v * 10000.0).round() / 10000.0;

    let pro = project_path(board_path);
    let json = format!(
        r#"{{
  "board": {{
    "design_settings": {{
      "defaults": {{}},
      "diff_pair_dimensions": [],
      "drc_exclusions": [],
      "rules": {{
        "min_clearance": {min_clearance},
        "min_track_width": {min_track_width},
        "min_via_diameter": {min_via_diameter},
        "min_via_annular_width": {min_via_annular},
        "min_hole_clearance": {min_hole_clearance},
        "min_hole_to_hole": {min_hole_to_hole},
        "min_copper_edge_clearance": {min_edge},
        "min_through_hole_diameter": {min_hole}
      }},
      "track_widths": [],
      "via_dimensions": []
    }}
  }},
  "boards": [],
  "libraries": {{ "pinned_footprint_libs": [], "pinned_symbol_libs": [] }},
  "meta": {{ "filename": "{name}", "version": 3 }},
  "net_settings": {{
    "classes": [
      {{
        "bus_width": 6,
        "clearance": {min_clearance},
        "diff_pair_gap": {min_clearance},
        "diff_pair_via_gap": 0.25,
        "diff_pair_width": {track},
        "line_style": 0,
        "microvia_diameter": 0.3,
        "microvia_drill": 0.1,
        "name": "Default",
        "pcb_color": "rgba(0, 0, 0, 0.000)",
        "priority": 2147483647,
        "schematic_color": "rgba(0, 0, 0, 0.000)",
        "track_width": {track},
        "tuning_profile": "",
        "via_diameter": {via_d},
        "via_drill": {via_k},
        "wire_width": 6
      }}
    ],
    "meta": {{ "version": 5 }}
  }},
  "pcbnew": {{ "page_layout_descr_file": "" }},
  "sheets": [],
  "text_variables": {{}}
}}
"#,
        name = pro
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        track = r4(rules.track_width),
        via_d = r4(diameter),
        via_k = r4(drill),
    );
    std::fs::write(&pro, json)?;
    Ok(pro)
}
