//! Shared agent plumbing: the [`GordianConfig`] data contract, where it lives on
//! this platform, and process-wide tracing for the CLI.

pub mod config;
pub mod logging;
pub mod platform;

pub use config::GordianConfig;
