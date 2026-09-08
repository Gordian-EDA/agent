//! The Gordian schematic + PCB agent.
//!
//! One run is: the design loop ([`agent`]) drives seven tools over the
//! deterministic schematic engine until a sheet passes its own checks, KiCad ERC
//! and an anchored visual critic ([`critic`]); the composition pass ([`compose`])
//! then polishes the layout trees while the board stage ([`board`]) routes the
//! same netlist; [`run`] bounds all of it by one wall clock and writes
//! `report.json`.

pub mod agent;
pub mod board;
pub mod compose;
pub mod critic;
mod engines;
pub mod prompt;
pub mod render;
pub mod run;
pub mod skills;
pub mod tools;

pub use agent::{Agent, Budget, Outcome, Usage};
pub use gordian_llm::{GenaiProvider, Provider};
pub use gordian_runtime::GordianConfig;
pub use gordian_runtime::platform;
pub use prompt::system_prompt;
pub use run::{Report, RunOptions};
