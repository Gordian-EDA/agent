//! Central Gordian configuration model.
//!
//! This module is deliberately only a data contract plus lightweight validation.
//! It does not read environment variables, open config files, write defaults, or
//! decide where a client stores settings. CLI, TUI, web, tests, and other
//! frontends can each create a [`GordianConfig`] from their own source and pass
//! the resulting values into core constructors.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Version of the config schema described by this module.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Default request token cap used by the production LLM provider today.
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;

/// Default number of symbol, footprint, and retrieval hits when a tool input
/// does not provide its own limit.
pub const DEFAULT_SEARCH_LIMIT: usize = 8;

/// Default number of similar reference designs to retrieve.
pub const DEFAULT_REFERENCE_COUNT: usize = 3;

/// Default long-edge cap for rendered schematic and board PNGs.
pub const DEFAULT_RENDER_MAX_PX: u32 = 1600;

/// Default schematic filename used when a frontend creates a fresh project.
pub const DEFAULT_SCHEMATIC_FILENAME: &str = "design.kicad_sch";

/// The full client-agnostic Gordian configuration.
///
/// This is intended to describe behavior, not persistence. Paths are optional
/// overrides only; when absent, callers may use discovery, command-line values,
/// profile settings, or any other frontend-specific source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GordianConfig {
    /// Config schema version. Bump only when the serialized shape changes in a
    /// way clients need to understand explicitly.
    pub schema_version: u32,
    /// LLM request behavior.
    pub llm: LlmConfig,
    /// KiCAD discovery and IPC behavior.
    pub kicad: KicadConfig,
    /// Project defaults that are config, not project state.
    pub project: ProjectConfig,
    /// Agent loop policy.
    pub agent: AgentConfig,
    /// Retrieval-augmented reference design behavior.
    pub retrieval: RetrievalConfig,
    /// Independent post-generation review behavior.
    pub review: ReviewConfig,
    /// Tool-level defaults shared across schematic and PCB tools.
    pub tools: ToolConfig,
}

impl Default for GordianConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            llm: LlmConfig::default(),
            kicad: KicadConfig::default(),
            project: ProjectConfig::default(),
            agent: AgentConfig::default(),
            retrieval: RetrievalConfig::default(),
            review: ReviewConfig::default(),
            tools: ToolConfig::default(),
        }
    }
}

impl GordianConfig {
    /// Validate invariants that cannot be represented in the Rust type system.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version == 0 {
            return Err(ConfigError::new(
                "schemaVersion",
                "schema version must be greater than zero",
            ));
        }
        self.llm.validate("llm")?;
        self.kicad.validate("kicad")?;
        self.project.validate("project")?;
        self.agent.validate("agent")?;
        self.retrieval.validate("retrieval")?;
        self.tools.validate("tools")?;
        Ok(())
    }
}

/// LLM request behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LlmConfig {
    /// Provider-routed model id, such as `gpt-4o`, `claude-sonnet-4-6`, or a
    /// namespaced genai id like `open_router::anthropic/claude-sonnet-4-5`.
    pub model: Option<String>,
    /// API key for the configured model provider, when the provider requires
    /// one. This is a local client setting; gordian-core does not decide where
    /// or how it is persisted.
    pub api_key: Option<String>,
    /// Optional provider endpoint override, for OpenAI-compatible gateways,
    /// local model servers, and test doubles.
    pub endpoint: Option<String>,
    /// Maximum completion tokens requested per agent call.
    pub max_tokens: u32,
    /// Whether the provider should ask for an ephemeral cache breakpoint over
    /// the static system/tool prefix when supported.
    pub ephemeral_cache: bool,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: None,
            api_key: None,
            endpoint: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            ephemeral_cache: true,
        }
    }
}

impl LlmConfig {
    fn validate(&self, path: &'static str) -> Result<(), ConfigError> {
        if let Some(model) = &self.model {
            if model.trim().is_empty() {
                return Err(ConfigError::new(
                    format!("{path}.model"),
                    "model must not be empty when set",
                ));
            }
        }
        if let Some(api_key) = &self.api_key {
            if api_key.trim().is_empty() {
                return Err(ConfigError::new(
                    format!("{path}.apiKey"),
                    "API key must not be empty when set",
                ));
            }
        }
        if let Some(endpoint) = &self.endpoint {
            let endpoint = endpoint.trim();
            if endpoint.is_empty() {
                return Err(ConfigError::new(
                    format!("{path}.endpoint"),
                    "endpoint must not be empty when set",
                ));
            }
            if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
                return Err(ConfigError::new(
                    format!("{path}.endpoint"),
                    "endpoint must start with http:// or https://",
                ));
            }
        }
        if self.max_tokens == 0 {
            return Err(ConfigError::new(
                format!("{path}.maxTokens"),
                "max tokens must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// KiCAD discovery and live board session behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KicadConfig {
    /// Optional symbol library directory override.
    pub symbol_dir: Option<PathBuf>,
    /// Optional footprint library directory override.
    pub footprint_dir: Option<PathBuf>,
    /// Optional `kicad-cli` path override.
    pub cli_path: Option<PathBuf>,
    /// Prefer attaching to an already-running KiCAD IPC server before launching
    /// a managed headless process.
    pub attach_running: bool,
}

impl KicadConfig {
    fn validate(&self, path: &'static str) -> Result<(), ConfigError> {
        validate_optional_path(path, "symbolDir", &self.symbol_dir)?;
        validate_optional_path(path, "footprintDir", &self.footprint_dir)?;
        validate_optional_path(path, "cliPath", &self.cli_path)?;
        Ok(())
    }
}

/// Defaults for project construction. The project directory itself is supplied
/// by the frontend and is not global config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectConfig {
    /// Conventional schematic filename inside a project directory.
    pub schematic_filename: String,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            schematic_filename: DEFAULT_SCHEMATIC_FILENAME.to_string(),
        }
    }
}

impl ProjectConfig {
    fn validate(&self, path: &'static str) -> Result<(), ConfigError> {
        if self.schematic_filename.trim().is_empty() {
            return Err(ConfigError::new(
                format!("{path}.schematicFilename"),
                "schematic filename must not be empty",
            ));
        }
        if !self.schematic_filename.ends_with(".kicad_sch") {
            return Err(ConfigError::new(
                format!("{path}.schematicFilename"),
                "schematic filename must end with .kicad_sch",
            ));
        }
        Ok(())
    }
}

/// Agent loop policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentConfig {
    /// Run the independent post-commit review and bounded fix pass.
    pub post_commit_review: bool,
    /// Maximum number of review-driven follow-up fix turns after a commit.
    pub review_fix_rounds: u8,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            post_commit_review: true,
            review_fix_rounds: 1,
        }
    }
}

impl AgentConfig {
    fn validate(&self, _path: &'static str) -> Result<(), ConfigError> {
        Ok(())
    }
}

/// Retrieval-augmented reference design behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RetrievalConfig {
    /// Whether the agent may include similar real KiCAD designs as references.
    pub enabled: bool,
    /// Optional corpus directory override.
    pub corpus_dir: Option<PathBuf>,
    /// Default number of references returned per retrieval request.
    pub references_per_query: usize,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            corpus_dir: None,
            references_per_query: DEFAULT_REFERENCE_COUNT,
        }
    }
}

impl RetrievalConfig {
    fn validate(&self, path: &'static str) -> Result<(), ConfigError> {
        validate_optional_path(path, "corpusDir", &self.corpus_dir)?;
        if self.references_per_query == 0 {
            return Err(ConfigError::new(
                format!("{path}.referencesPerQuery"),
                "reference count must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// Independent review behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReviewConfig {
    /// Retry a malformed review JSON response once.
    pub retry_json: bool,
    /// Use the broader diverse-lens netlist review ensemble instead of the
    /// single quick lens.
    pub ensemble: bool,
    /// Run visual layout review when the calling workflow supports image input.
    pub layout: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            retry_json: false,
            ensemble: false,
            layout: true,
        }
    }
}

/// Tool-level defaults shared across schematic and PCB tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ToolConfig {
    /// Default max hits for symbol and footprint search tools.
    pub default_search_limit: usize,
    /// Long-edge cap for rendered schematic and board PNGs.
    pub render_max_px: u32,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            default_search_limit: DEFAULT_SEARCH_LIMIT,
            render_max_px: DEFAULT_RENDER_MAX_PX,
        }
    }
}

impl ToolConfig {
    fn validate(&self, path: &'static str) -> Result<(), ConfigError> {
        if self.default_search_limit == 0 {
            return Err(ConfigError::new(
                format!("{path}.defaultSearchLimit"),
                "search limit must be greater than zero",
            ));
        }
        if self.render_max_px == 0 {
            return Err(ConfigError::new(
                format!("{path}.renderMaxPx"),
                "render size must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// One config validation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    /// Dotted field path in serialized camelCase form.
    pub path: String,
    /// Human-readable failure.
    pub message: String,
}

impl ConfigError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for ConfigError {}

fn validate_optional_path(
    parent: &'static str,
    field: &'static str,
    value: &Option<PathBuf>,
) -> Result<(), ConfigError> {
    if let Some(path) = value {
        if path.as_os_str().is_empty() {
            return Err(ConfigError::new(
                format!("{parent}.{field}"),
                "path must not be empty when set",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = GordianConfig::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.schema_version, CONFIG_SCHEMA_VERSION);
        assert_eq!(cfg.llm.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(cfg.project.schematic_filename, DEFAULT_SCHEMATIC_FILENAME);
    }

    #[test]
    fn partial_deserialize_fills_defaults() {
        let cfg: GordianConfig = serde_json::from_value(serde_json::json!({
            "llm": { "model": "gpt-4o", "apiKey": "test-key" },
            "retrieval": { "enabled": false }
        }))
        .unwrap();

        assert_eq!(cfg.llm.model.as_deref(), Some("gpt-4o"));
        assert_eq!(cfg.llm.api_key.as_deref(), Some("test-key"));
        assert_eq!(cfg.llm.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(!cfg.retrieval.enabled);
        assert_eq!(cfg.tools.default_search_limit, DEFAULT_SEARCH_LIMIT);
        cfg.validate().unwrap();
    }

    #[test]
    fn validation_reports_field_path() {
        let mut cfg = GordianConfig::default();
        cfg.tools.default_search_limit = 0;

        let err = cfg.validate().unwrap_err();
        assert_eq!(err.path, "tools.defaultSearchLimit");
    }
}
