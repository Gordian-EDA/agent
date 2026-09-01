//! Persistence boundary for KiCad PCB documents.
//!
//! This crate owns conversion between KiCad IPC snapshots and the neutral PCB
//! domain model, plus the offline s-expression edits used when IPC is unavailable.

mod active;
mod netclass;
mod patch;
mod sexpr;

pub use active::{
    ImportedBoard, ImportedPad, ImportedPart, IpcBoardSnapshot, board_problem, bridge_route,
    from_bridge, is_seed_imported_board, save_live_board,
};
pub use netclass::{
    NetClassUpdate, NetClassUpdateReport, board_net_widths, patch_board_net_class,
    patch_project_net_class, project_net_widths, write_net_class_update,
};
pub use patch::{
    FieldPosition, append_copper, append_copper_file, board_copper_layer_names,
    board_file_plane_nets, board_outline_bbox, field_position, footprint_placement,
    parse_net_codes, patch_field_hidden, patch_field_position, patch_field_text_size,
    patch_placements, silk_field_owners, strip_copper,
};
pub use sexpr::{sexpr_end, sexpr_point};
