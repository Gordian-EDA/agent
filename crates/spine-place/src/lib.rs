//! `spine-place` — deterministic grammar-typesetting schematic placement.
//!
//! Parses the netlist into net classes ([`net`]), contracts series 2-pin runs
//! into chains over a reduced node graph ([`chain`]), then typesets modules and
//! spines with corpus-mined human conventions. No stochastic search.

pub mod chain;
pub mod net;
