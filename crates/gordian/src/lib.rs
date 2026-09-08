//! The Gordian agent's user-facing surfaces: the config file both entry points
//! read, and the interactive cockpit.
//!
//! The design logic lives in `gordian-core`; this crate is the shell around it.
//! `main.rs` is the CLI over the same two pieces.

pub mod config;
pub mod tui;

/// A request that asks only for the schematic ("render the schematic") skips the board;
/// any mention of the board, layout, routing or fabrication — or no mention of the
/// schematic at all — gets both.
pub fn wants_board(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    let board_words = ["pcb", "board", "layout", "rout", "fabricat", "gerber"];
    if board_words.iter().any(|w| lower.contains(w)) {
        return true;
    }
    !lower.contains("schematic")
}
