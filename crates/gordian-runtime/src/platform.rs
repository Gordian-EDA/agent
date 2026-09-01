//! Where Gordian's configuration lives on this platform.
//!
//! [`config`] is the data contract; this module is the single owner of the
//! on-disk location so every frontend (CLI, TUI, examples, the quality runner)
//! reads the same file.

use std::path::PathBuf;

use crate::config::GordianConfig;

const CONFIG_FILE: &str = "config.toml";

/// Platform config path, e.g. `~/.config/gordian/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "Gordian", "gordian")
        .map(|dirs| dirs.config_dir().join(CONFIG_FILE))
}

/// Read the platform config, falling back to defaults when it does not exist.
///
/// Frontends that must also create or secure the file own that themselves; this
/// is the read-only view for tools that merely need the same behavior as a
/// normal agent run.
pub fn load_config() -> anyhow::Result<GordianConfig> {
    let Some(path) = config_path() else {
        return Ok(GordianConfig::default());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GordianConfig::default());
        }
        Err(err) => return Err(anyhow::anyhow!("reading {}: {err}", path.display())),
    };
    let config: GordianConfig =
        toml::from_str(&text).map_err(|err| anyhow::anyhow!("parsing {}: {err}", path.display()))?;
    config
        .validate()
        .map_err(|err| anyhow::anyhow!("invalid config in {}: {err}", path.display()))?;
    Ok(config)
}
