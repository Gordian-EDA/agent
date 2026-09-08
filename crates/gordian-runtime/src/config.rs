//! Central Gordian configuration model.
//!
//! This module is deliberately only a data contract plus lightweight validation.
//! It does not read environment variables, open config files, write defaults, or
//! decide where a client stores settings. CLI, TUI, web, tests, and other
//! frontends can each create a [`GordianConfig`] from their own source and pass
//! the resulting values into core constructors.

pub use gordian_llm::{DEFAULT_MAX_TOKENS, LlmConfig, LlmReasoningEffort};
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Version of the config schema described by this module.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Default wall clock for one run, schematic and board together.
pub const DEFAULT_BUDGET_SECONDS: u64 = 180;

/// Default ceiling on `build` calls in one run.
pub const DEFAULT_MAX_BUILDS: usize = 6;

/// Default schematic filename used when a frontend creates a fresh project.
pub const DEFAULT_SCHEMATIC_FILENAME: &str = "design.kicad_sch";

/// The full client-agnostic Gordian configuration.
///
/// This is intended to describe behavior, not persistence. Paths are optional
/// overrides only; when absent, callers may use discovery, command-line values,
/// profile settings, or any other frontend-specific source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct GordianConfig {
    /// Config schema version. Bump only when the serialized shape changes in a
    /// way clients need to understand explicitly.
    pub schema_version: u32,
    /// LLM request behavior.
    pub llm: LlmConfig,
    /// KiCAD command-line and library discovery.
    pub kicad: KicadConfig,
    /// Project defaults that are config, not project state.
    pub project: ProjectConfig,
    /// Agent loop policy.
    pub agent: AgentConfig,
    /// Sections the schema-v1 agent read and this one does not. See
    /// [`RetiredSection`].
    #[serde(default, rename = "retrieval", skip_serializing)]
    #[doc(hidden)]
    pub legacy_retrieval: Option<RetiredSection>,
    #[serde(default, rename = "review", skip_serializing)]
    #[doc(hidden)]
    pub legacy_review: Option<RetiredSection>,
    #[serde(default, rename = "tools", skip_serializing)]
    #[doc(hidden)]
    pub legacy_tools: Option<RetiredSection>,
}

impl Default for GordianConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            llm: LlmConfig::default(),
            kicad: KicadConfig::default(),
            project: ProjectConfig::default(),
            agent: AgentConfig::default(),
            legacy_retrieval: None,
            legacy_review: None,
            legacy_tools: None,
        }
    }
}

/// A config section the schema-v1 agent read and this one does not. It is
/// accepted so an existing `config.toml` still loads, ignored while running, and
/// never written back out.
pub type RetiredSection = serde_json::Value;

impl GordianConfig {
    /// Validate invariants that cannot be represented in the Rust type system.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::new(
                "schemaVersion",
                format!(
                    "unsupported schema version {}; expected {CONFIG_SCHEMA_VERSION}",
                    self.schema_version
                ),
            ));
        }
        self.llm
            .validate("llm")
            .map_err(|e| ConfigError::new(e.field, e.message))?;
        self.kicad.validate("kicad")?;
        self.project.validate("project")?;
        self.agent.validate("agent")?;
        Ok(())
    }
}

/// KiCAD 10 command-line and library discovery.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct KicadConfig {
    /// Optional symbol library directory override.
    pub symbol_dir: Option<PathBuf>,
    /// Optional footprint library directory override.
    pub footprint_dir: Option<PathBuf>,
    /// Optional `kicad-cli` path override.
    pub cli_path: Option<PathBuf>,
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
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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
        let field = format!("{path}.schematicFilename");
        let mut components = std::path::Path::new(&self.schematic_filename).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(ConfigError::new(
                field,
                "schematic filename must be exactly one relative filename component",
            ));
        }
        if !self.schematic_filename.ends_with(".kicad_sch") {
            return Err(ConfigError::new(
                field,
                "schematic filename must end with .kicad_sch",
            ));
        }
        Ok(())
    }
}

/// How far one run may go. Retired schema-v1 keys in this section are ignored
/// rather than rejected, so an older `config.toml` still loads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentConfig {
    /// Wall clock for one run, schematic and board together. `--budget` overrides it.
    pub budget_seconds: u64,
    /// Ceiling on `build` calls. `--max-builds` overrides it.
    pub max_builds: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            budget_seconds: DEFAULT_BUDGET_SECONDS,
            max_builds: DEFAULT_MAX_BUILDS,
        }
    }
}

impl AgentConfig {
    fn validate(&self, path: &'static str) -> Result<(), ConfigError> {
        if self.budget_seconds == 0 {
            return Err(ConfigError::new(
                format!("{path}.budgetSeconds"),
                "the wall clock must be at least one second",
            ));
        }
        if self.max_builds == 0 {
            return Err(ConfigError::new(
                format!("{path}.maxBuilds"),
                "a run needs at least one build",
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
    if let Some(path) = value
        && path.as_os_str().is_empty()
    {
        return Err(ConfigError::new(
            format!("{parent}.{field}"),
            "path must not be empty when set",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The run's bounds have defaults, are settable, and refuse a zero.
    #[test]
    fn the_agent_budget_and_build_ceiling_are_validated() {
        let mut config = GordianConfig::default();
        assert_eq!(config.agent.budget_seconds, DEFAULT_BUDGET_SECONDS);
        assert_eq!(config.agent.max_builds, DEFAULT_MAX_BUILDS);
        config.validate().unwrap();

        config.agent.budget_seconds = 0;
        assert_eq!(config.validate().unwrap_err().path, "agent.budgetSeconds");

        config.agent.budget_seconds = 90;
        config.agent.max_builds = 0;
        assert_eq!(config.validate().unwrap_err().path, "agent.maxBuilds");

        let parsed: GordianConfig = toml::from_str("[agent]\nbudgetSeconds = 120\nmaxBuilds = 3\n")
            .expect("camelCase keys");
        assert_eq!(parsed.agent.budget_seconds, 120);
        assert_eq!(parsed.agent.max_builds, 3);
    }

    #[test]
    fn default_config_is_valid() {
        let cfg = GordianConfig::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.schema_version, CONFIG_SCHEMA_VERSION);
        assert_eq!(cfg.llm.adapter, None);
        assert_eq!(cfg.llm.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(cfg.llm.reasoning_effort, None);
        assert!(!cfg.llm.capture_reasoning);
        assert_eq!(cfg.project.schematic_filename, DEFAULT_SCHEMATIC_FILENAME);
    }

    #[test]
    fn unsupported_schema_versions_are_rejected() {
        for version in [0, CONFIG_SCHEMA_VERSION + 1, u32::MAX] {
            let cfg = GordianConfig {
                schema_version: version,
                ..GordianConfig::default()
            };

            let err = cfg.validate().unwrap_err();
            assert_eq!(err.path, "schemaVersion");
            assert!(err.message.contains("unsupported schema version"));
            assert!(err.message.contains(&CONFIG_SCHEMA_VERSION.to_string()));
        }
    }

    #[test]
    fn partial_deserialize_fills_defaults() {
        let cfg: GordianConfig = serde_json::from_value(serde_json::json!({
            "llm": { "adapter": "openai", "model": "gpt-4o", "apiKey": "test-key" }
        }))
        .unwrap();

        assert_eq!(cfg.llm.adapter.as_deref(), Some("openai"));
        assert_eq!(cfg.llm.model.as_deref(), Some("gpt-4o"));
        assert_eq!(cfg.llm.api_key.as_deref(), Some("test-key"));
        assert_eq!(cfg.llm.max_tokens, DEFAULT_MAX_TOKENS);
        cfg.validate().unwrap();
    }

    #[test]
    fn unknown_config_fields_are_rejected_instead_of_silently_defaulted() {
        let top_level = serde_json::from_value::<GordianConfig>(serde_json::json!({
            "schemaVerzion": CONFIG_SCHEMA_VERSION
        }))
        .unwrap_err();
        assert!(top_level.to_string().contains("schemaVerzion"));

        let nested = serde_json::from_value::<GordianConfig>(serde_json::json!({
            "llm": { "apiKEy": "secret" }
        }))
        .unwrap_err();
        assert!(nested.to_string().contains("apiKEy"));
    }

    /// The sections the previous agent read are accepted and dropped, so a
    /// config written for it still loads and is never written back out.
    #[test]
    fn retired_schema_v1_sections_remain_loadable() {
        let cfg: GordianConfig = toml::from_str(
            r#"
            schemaVersion = 1

            [retrieval]
            enabled = true
            referencesPerQuery = 3

            [review]
            retryJson = false

            [tools]
            defaultSearchLimit = 8

            [agent]
            postCommitReview = false
            reviewFixRounds = 1
            "#,
        )
        .unwrap();

        assert!(cfg.legacy_retrieval.is_some());
        assert!(cfg.legacy_review.is_some());
        assert!(cfg.legacy_tools.is_some());
        assert_eq!(cfg.agent.budget_seconds, DEFAULT_BUDGET_SECONDS);
        cfg.validate().unwrap();

        let serialized = toml::to_string(&cfg).unwrap();
        for retired in ["retrieval", "review", "postCommitReview"] {
            assert!(!serialized.contains(retired), "{serialized}");
        }
    }

    #[test]
    fn llm_config_debug_redacts_the_api_key() {
        let cfg = GordianConfig {
            llm: LlmConfig {
                api_key: Some("super-secret-token".to_owned()),
                ..LlmConfig::default()
            },
            ..GordianConfig::default()
        };

        let debug = format!("{cfg:?}");

        assert!(!debug.contains("super-secret-token"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
        assert!(debug.contains("vision_capable"), "{debug}");
    }

    #[test]
    fn schematic_filename_must_be_a_relative_filename() {
        for filename in [
            "/tmp/outside.kicad_sch",
            "../outside.kicad_sch",
            "subdir/design.kicad_sch",
            "",
        ] {
            let mut cfg = GordianConfig::default();
            cfg.project.schematic_filename = filename.to_string();

            let err = cfg.validate().unwrap_err();
            assert_eq!(err.path, "project.schematicFilename");
            assert_eq!(
                err.message,
                "schematic filename must be exactly one relative filename component"
            );
        }
    }

    #[test]
    fn llm_reasoning_effort_deserializes_keywords_and_budgets() {
        let cfg: GordianConfig = serde_json::from_value(serde_json::json!({
            "llm": {
                "reasoningEffort": "medium",
                "captureReasoning": true
            }
        }))
        .unwrap();
        assert_eq!(cfg.llm.reasoning_effort, Some(LlmReasoningEffort::Medium));
        assert!(cfg.llm.capture_reasoning);
        cfg.validate().unwrap();

        let cfg: GordianConfig = serde_json::from_value(serde_json::json!({
            "llm": { "reasoningEffort": "8000" }
        }))
        .unwrap();
        assert_eq!(
            cfg.llm.reasoning_effort,
            Some(LlmReasoningEffort::Budget(8000))
        );
        cfg.validate().unwrap();

        let cfg: GordianConfig = serde_json::from_value(serde_json::json!({
            "llm": { "reasoningEffort": 12000 }
        }))
        .unwrap();
        assert_eq!(
            cfg.llm.reasoning_effort,
            Some(LlmReasoningEffort::Budget(12000))
        );
        cfg.validate().unwrap();
    }

    #[test]
    fn llm_reasoning_effort_rejects_invalid_values() {
        let err = serde_json::from_value::<GordianConfig>(serde_json::json!({
            "llm": { "reasoningEffort": "deeper" }
        }))
        .unwrap_err();

        assert!(
            err.to_string().contains("reasoning effort must be"),
            "{err}"
        );

        let err = serde_json::from_value::<GordianConfig>(serde_json::json!({
            "llm": { "reasoningEffort": 0 }
        }))
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("reasoning effort budget must be greater than zero"),
            "{err}"
        );
    }

    #[test]
    fn validation_reports_empty_llm_adapter() {
        let mut cfg = GordianConfig::default();
        cfg.llm.adapter = Some(" ".to_string());

        let err = cfg.validate().unwrap_err();
        assert_eq!(err.path, "llm.adapter");
    }
}
