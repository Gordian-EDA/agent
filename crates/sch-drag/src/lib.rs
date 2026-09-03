//! `sch-drag` — moving a symbol on a drawn sheet the way a person does.
//!
//! A human editing a sheet picks a part up, the editor keeps the connections
//! attached, and they drop it where the drawing reads better. This crate is that
//! move, made mechanical:
//!
//! - [`drag`] moves, rotates or mirrors a symbol and re-draws its connections,
//!   and *rolls itself back* if the netlist would change, a pin would be left
//!   with nothing drawn on it, or the sheet would gain a loose end;
//! - [`route`] draws one connection between two points around what is in the way;
//! - [`eval::measure`] says how clean a sheet is, in millimetres of equivalent
//!   wire, from the sheet's geometry alone — no KiCAD, no round trip.
//!
//! Nothing here plans a layout: the input is a placed sheet — from the typesetter,
//! from an agent's edits, or from a person — and the output is the same sheet with
//! one symbol moved.

pub mod drag;
pub mod eval;
pub mod route;
pub mod sheet;

pub use drag::{
    DragError, DragReport, PinReSeat, Placement, RetiredPin, TurnError, TurnReport, drag,
    drag_many, redraw_wire, reseat_many, turn_in_place,
};
pub use eval::{Metrics, Weights, measure};
pub use sheet::Sheet;
