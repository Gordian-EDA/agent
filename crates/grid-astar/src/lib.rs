//! `grid-astar` — the sequential grid-A* PCB router (the free-tier baseline).
//!
//! Rasterizes the board onto a per-layer occupancy grid and routes one net at a
//! time, shortest-net-first, with an A* per net. Connectivity-honest: drops
//! copper the `drc-lint` oracle finds unconnected/shorted. Shared types come from
//! `pcb-model` (re-exported as `problem`); DRC from `drc-lint`.

pub use drc_lint::{connectivity, lint};
pub use pcb_model as problem;

pub mod astar;
pub mod grid;
pub mod router;
