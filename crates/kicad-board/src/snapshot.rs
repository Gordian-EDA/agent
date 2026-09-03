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
