//! The occupancy-grid and A* kernel both PCB routing engines build on.
//!
//! [`grid::RouteGrid`] discretises a [`pcb_model::RoutingView`] into per-layer
//! cells; [`astar`] searches that grid. Neither knows anything about design
//! rules, engines, or files — a router crate owns those policies and injects
//! them.

pub mod astar;
pub mod grid;
pub mod tidy;
