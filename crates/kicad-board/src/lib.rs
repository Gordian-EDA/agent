//! Persistence boundary for KiCad PCB documents.
//!
//! This crate owns saved-board parsing and atomic s-expression edits.

mod annotate;
mod edit;
mod netclass;
mod offline;
mod patch;
mod sexpr;
mod snapshot;

pub use annotate::{
    Annotation, GORDIAN_PREFIX, LOCKED_REASON, STAGED_DETAIL, STAGED_REASON, patch_annotations,
};
pub use edit::{BoardDoc, BoardFootprint};
pub use netclass::{
    NetClassUpdate, NetClassUpdateReport, board_net_widths, patch_board_net_class,
    patch_project_net_class, project_net_widths, write_net_class_update,
};
pub use offline::read_snapshot;
pub use patch::{
    FieldPosition, append_copper, append_copper_file, board_copper_layer_names,
    board_file_plane_nets, board_outline_bbox, field_position, footprint_placement,
    parse_net_codes, patch_field_hidden, patch_field_position, patch_field_text_size,
    patch_placements, silk_field_owners, strip_copper,
};
pub use sexpr::{sexpr_end, sexpr_point};
pub use snapshot::{
    BoardSide, BoardSnapshot, FootprintPlacement, ImportedBoard, ImportedPad, ImportedPart,
    SEED_ROW_PITCH, seed_row_references, seed_row_x, seed_row_y,
};
