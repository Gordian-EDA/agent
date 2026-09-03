//! Saved-board domain types shared by placement and routing workflows.

use geom::{Point2, Rect};
use pcb_model::{RouteSolution, RoutingView};

/// Whether a KiCad net name denotes electrical design connectivity.
///
/// KiCad assigns `unconnected-(REF-PadN)` names to isolated pads. Those names
/// are file-local bookkeeping, not schematic nets that may be routed or synced.
pub fn is_design_net_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with("unconnected-")
}

/// Whether KiCad derived a net name from one of its member pads.
pub fn is_derived_net_name(name: &str) -> bool {
    name.starts_with("Net-(")
}

/// Domain view parsed from a saved KiCad board.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardSnapshot {
    pub problem: RoutingView,
    pub imported: ImportedBoard,
    pub copper: RouteSolution,
    pub layer_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedBoard {
    pub layer_count: u32,
    pub bounds: Rect,
    pub parts: Vec<ImportedPart>,
    pub placement_keepouts: Vec<Rect>,
    pub keepout_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedPart {
    pub reference: String,
    pub lib_id: String,
    pub at: Point2,
    pub rotation: i32,
    pub side: BoardSide,
    pub locked: bool,
    /// The `gordian:` properties this footprint carries — why it is staged, who
    /// locked it. The board file is the only place this state lives.
    pub properties: std::collections::BTreeMap<String, String>,
    pub courtyard: Option<Rect>,
    pub pads: Vec<ImportedPad>,
}

impl ImportedPart {
    /// The value of one `gordian:` property.
    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties.get(name).map(String::as_str)
    }
}

/// Side of the board carrying a footprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardSide {
    Front,
    Back,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedPad {
    pub number: String,
    pub net: Option<String>,
    pub at: Point2,
    pub layers: Vec<pcb_model::LayerRef>,
    pub shape: String,
    pub size: Point2,
    pub drill: Option<Point2>,
}

/// One footprint placement to apply to a saved board.
#[derive(Debug, Clone, PartialEq)]
pub struct FootprintPlacement {
    pub reference: String,
    pub at: Point2,
    pub rotation_deg: Option<f64>,
}

/// The seed row's pitch: one part every 2.54 mm, running right from the inset.
pub const SEED_ROW_PITCH: f64 = 2.54;

const SEED_ROW_OFF_LATTICE: f64 = 0.25;

/// Returns the vertical position of the seed row for a board top edge.
pub fn seed_row_y(min_y: f64) -> f64 {
    let lattice = 2.0 * SEED_ROW_OFF_LATTICE;
    ((min_y + 2.0) / lattice).round() * lattice + SEED_ROW_OFF_LATTICE
}

/// Returns the horizontal position of one footprint in the seed row.
pub fn seed_row_x(min_x: f64, index: usize) -> f64 {
    min_x + 2.0 + index as f64 * SEED_ROW_PITCH
}

/// Returns references that have not moved from the board's seed row.
pub fn seed_row_references(board: &ImportedBoard) -> Vec<String> {
    let row_y = seed_row_y(board.bounds.min_y);
    let mut references: Vec<String> = board
        .parts
        .iter()
        .filter(|part| {
            let lattice = (part.at.x - board.bounds.min_x - 2.0) / SEED_ROW_PITCH;
            (part.at.y - row_y).abs() < geom::EPS
                && part.rotation == 0
                && lattice >= -geom::EPS
                && (lattice - lattice.round()).abs() < geom::EPS
        })
        .map(|part| part.reference.clone())
        .collect();
    references.sort();
    references
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(reference: &str, x: f64, y: f64, rotation: i32) -> ImportedPart {
        ImportedPart {
            reference: reference.to_owned(),
            lib_id: "Resistor_SMD:R_0603_1608Metric".to_owned(),
            at: Point2 { x, y },
            rotation,
            side: BoardSide::Front,
            locked: false,
            properties: std::collections::BTreeMap::new(),
            courtyard: None,
            pads: vec![],
        }
    }

    fn board_of(parts: Vec<ImportedPart>) -> ImportedBoard {
        ImportedBoard {
            layer_count: 2,
            bounds: Rect {
                min_x: 10.0,
                max_x: 30.0,
                min_y: 5.0,
                max_y: 20.0,
            },
            parts,
            placement_keepouts: vec![],
            keepout_count: 0,
        }
    }

    #[test]
    fn seed_row_excludes_placed_parts() {
        let row = seed_row_y(5.0);
        let board = board_of(vec![
            part("C1", seed_row_x(10.0, 1), row, 0),
            part("R1", 16.0, 12.0, 0),
        ]);
        assert_eq!(seed_row_references(&board), ["C1"]);
    }

    #[test]
    fn seed_row_is_off_the_placement_lattice() {
        for min_y in [0.0, 5.0, 5.25, -3.5, 12.7] {
            let steps = seed_row_y(min_y) / 0.5;
            assert!((steps - steps.round()).abs() > 0.1);
        }
    }
}
