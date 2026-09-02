//! `pcb-route-grid` — the sequential grid-A* PCB router (the free-tier baseline).
//!
//! Rasterizes the board onto the shared [`pcb_grid`] occupancy grid and routes
//! one net at a time, shortest-net-first, with an A* per net. Connectivity-honest:
//! drops copper the injected [`pcb_model::Drc`] oracle finds unconnected or
//! shorted. It also supplies the [`probe::GridRouteProbe`] a placer ranks
//! candidate placements with.

pub mod probe;
pub mod router;
