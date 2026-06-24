//! The KiCAD domain for the Gordian agent.
//!
//! This crate is the DOMAIN half of the split: it turns the generic
//! [`gordian_core::Agent`] loop into a KiCAD schematic + PCB design agent. It
//! owns the tool registry ([`tools`] + [`tools_pcb`]), the composed-sheet emit
//! ([`multisheet`]), the SVG→PNG render ([`render`]), the netlist [`review`]
//! prompt/lenses, the [`prompts`] system prompt, and — the seam to the core —
//! [`PcbTools`], a [`gordian_core::ToolProvider`].
//!
//! [`PcbTools`] owns the non-`Send` [`tools::PcbToolCtx`] (symbol caches, the live
//! KiCAD IPC session) and runs the synchronous tools on the blocking pool, so the
//! core's `async` trait method stays clean. To build a working agent:
//!
//! ```ignore
//! let ctx = PcbToolCtx::for_project(env, project_dir)?;
//! let agent = gordian_core::Agent::new(
//!     llm_client::from_env()?,
//!     Box::new(PcbTools::new(ctx)),
//!     gordian_kicad::prompts::system_prompt(),
//! );
//! ```

pub mod multisheet;
pub mod prompts;
mod provider;
pub mod render;
pub mod review;
pub mod tools;
pub mod tools_pcb;
pub mod workspace;

pub use provider::PcbTools;
