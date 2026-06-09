//! circuit-lang: parse, desugar, lint, and canonically emit the
//! auto-pcb circuit markup language (spec §5). Pure — no I/O.

pub mod desugar;
pub mod diag;
pub mod lint;
pub mod model;
pub mod parse;
pub mod provider;
pub mod surface;
mod yaml;
