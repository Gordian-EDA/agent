//! Interface-owned config loading and persistence.
//!
//! gordian-core defines the typed model; this binary crate decides where TOML
//! lives and when to create it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use gordian_core::GordianConfig;
use kicad_env::KicadEnv;

const CONFIG_FILE: &str = "config.toml";

pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: GordianConfig,
}

pub fn load_or_create() -> Result<LoadedConfig> {
    let path = default_config_path()?;
    if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let config: GordianConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config
            .validate()
            .map_err(|e| anyhow!("invalid config in {}: {e}", path.display()))?;
        return Ok(LoadedConfig { path, config });
    }

    let config = GordianConfig::default();
    write_default_config(&path, &config)?;
    Ok(LoadedConfig { path, config })
}

pub fn detect_kicad(config: &GordianConfig) -> Option<KicadEnv> {
    KicadEnv::detect_with(
        config.kicad.symbol_dir.as_deref(),
        config.kicad.footprint_dir.as_deref(),
        config.kicad.cli_path.as_deref(),
    )
}

fn default_config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "Gordian", "gordian")
        .ok_or_else(|| anyhow!("could not determine platform config directory"))?;
    Ok(dirs.config_dir().join(CONFIG_FILE))
}

fn write_default_config(path: &Path, config: &GordianConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }

    let body = format!(
        "# Gordian config. Persistence is owned by the CLI/TUI; gordian-core only consumes this typed model.\n\
         # Fill in llm.adapter, llm.model, and llm.apiKey for hosted providers. Local/no-auth endpoints may omit apiKey.\n\
         # Example:\n\
         # [llm]\n\
         # adapter = \"openai\"\n\
         # model = \"gpt-4o\"\n\
         # apiKey = \"sk-...\"\n\
         # endpoint = \"https://your-openai-compatible-gateway/v1\"\n\n{}",
        toml::to_string_pretty(config).context("serializing default config")?
    );
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    set_owner_only_permissions(path).ok();
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_toml_roundtrips() {
        let cfg = GordianConfig::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: GordianConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed, cfg);
    }
}
