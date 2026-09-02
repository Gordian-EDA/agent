//! Run one deterministic Gordian tool against an existing project.
//!
//! Usage: `cargo run --release -p gordian-core --example tool_once -- \
//!   <project-directory> <tool-name> [json-input]`

use std::path::PathBuf;

use anyhow::{Context, bail};
use gordian_core::AgentRuntime;
use gordian_core::tools::run_tool;
use kicad::KicadInstallation;
use serde_json::Value;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let project = PathBuf::from(args.next().context("missing project directory")?);
    let tool = args.next().context("missing tool name")?;
    let input = match args.next() {
        Some(json) => serde_json::from_str(&json).context("invalid JSON input")?,
        None => Value::Object(Default::default()),
    };
    if args.next().is_some() {
        bail!("usage: tool_once <project-directory> <tool-name> [json-input]");
    }

    let config = gordian_core::platform::load_config()?;
    let env = KicadInstallation::detect_with(
        config.kicad.symbol_dir.as_deref(),
        config.kicad.footprint_dir.as_deref(),
        config.kicad.cli_path.as_deref(),
    )
    .context("KiCad 10 environment not detected")?;
    let ctx = AgentRuntime::for_project_with_config(env, project, config)?;
    let result = run_tool(&tool, input, &ctx).with_context(|| format!("running {tool}"))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
