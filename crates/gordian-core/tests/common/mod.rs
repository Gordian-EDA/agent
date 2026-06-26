use anyhow::{Context, Result, anyhow};
use gordian_core::{GenaiProvider, GordianConfig};

pub fn live_provider_from_config() -> Result<GenaiProvider> {
    let dirs = directories::ProjectDirs::from("", "Gordian", "gordian")
        .ok_or_else(|| anyhow!("could not determine platform config directory"))?;
    let path = dirs.config_dir().join("config.toml");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {}; run `gordian tui` once to create it, then set llm.model and llm.apiKey",
            path.display()
        )
    })?;
    let config: GordianConfig =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    config
        .validate()
        .map_err(|e| anyhow!("invalid config in {}: {e}", path.display()))?;
    GenaiProvider::from_config(&config.llm)
}
