//! `sch-drag` — tidying a schematic the way a person does: by dragging.
//!
//! A human editing a sheet does not solve a placement problem and then route
//! it. They pick a part up, the editor keeps the connections attached, they
//! drop it where the drawing reads better, and they repeat until it looks
//! clean. This crate is that loop, made mechanical:
//!
//! - [`drag`] moves, rotates or mirrors a symbol and re-draws its connections,
//!   and *rolls itself back* if the netlist would change, a pin would be left
//!   with nothing drawn on it, or the sheet would gain a loose end;
//! - [`eval::measure`] says how clean a sheet is, in millimetres of equivalent
//!   wire, from the sheet's geometry alone — no KiCAD, no round trip;
//! - [`promote`] turns a pair of local labels back into the wire they stand
//!   for, wherever a person would have drawn one;
//! - [`tidy`] searches over both for the cleanest sheet it can reach in a
//!   budget, never accepting one that is not truthful.
//!
//! Nothing here plans a layout from scratch: the input is a placed sheet — from
//! an engine, from an agent's edits, or from a person — and the output is the
//! same sheet, drawn better.

pub mod drag;
pub mod eval;
pub mod promote;
pub mod route;
pub mod sheet;
pub mod tidy;

pub use drag::{
    DragError, DragReport, PinReSeat, Placement, TurnError, TurnReport, drag, drag_many,
    redraw_wire, reseat_many, turn_in_place,
};
pub use eval::{Metrics, Weights, measure};
pub use promote::{Substitute, promote, substitutes};
pub use sheet::Sheet;
pub use tidy::{TidyOptions, TidyReport, tidy};
