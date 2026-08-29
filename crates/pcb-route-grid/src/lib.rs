//! `pcb-route-grid` — the sequential grid-A* PCB router (the free-tier baseline).
//!
//! Rasterizes the board onto a per-layer occupancy grid and routes one net at a
//! time, shortest-net-first, with an A* per net. Connectivity-honest: drops
//! copper the `pcb-drc` oracle finds unconnected/shorted. Shared types come from
//! `pcb-model`; DRC from `pcb-drc`.

pub mod astar;
pub mod grid;
pub mod router;
