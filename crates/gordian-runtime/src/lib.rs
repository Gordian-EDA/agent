//! Shared agent plumbing below the turn loop and the tool crates: the central
//! [`GordianConfig`], the per-project [`AgentRuntime`] context, the tool result
//! contract, and durable workspace state files.

pub mod config;
pub mod footprint_compat;
pub mod render;
pub mod runtime;
pub mod tool;
pub mod workspace;

pub use config::GordianConfig;
pub use runtime::AgentRuntime;
