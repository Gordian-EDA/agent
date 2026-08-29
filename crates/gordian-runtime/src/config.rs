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

/// Default number of symbol and footprint hits when a tool input does not
/// provide its own limit.
pub const DEFAULT_SEARCH_LIMIT: usize = 5;

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
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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
    /// Compatibility sink for the retrieval settings shipped in schema v1
    /// before retrieval support was removed.  The values no longer affect
    /// behavior and are omitted when serializing new configs, but accepting
    /// them keeps existing schema-v1 files loadable.
    #[serde(default, rename = "retrieval", skip_serializing)]
    #[doc(hidden)]
    pub legacy_retrieval: Option<LegacyRetrievalConfig>,
    /// Independent post-generation review behavior.
    pub review: ReviewConfig,
    /// Tool-level defaults shared across schematic and PCB tools.
    pub tools: ToolConfig,
    /// Deterministic engine selection.
    pub engines: EngineConfig,
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
            review: ReviewConfig::default(),
            tools: ToolConfig::default(),
            engines: EngineConfig::default(),
        }
    }
}

/// Retired schema-v1 retrieval settings retained only for deserialization.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
#[doc(hidden)]
pub struct LegacyRetrievalConfig {
    pub enabled: bool,
    pub corpus_dir: Option<PathBuf>,
    pub references_per_query: usize,
}

impl Default for LegacyRetrievalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            corpus_dir: None,
            references_per_query: 3,
        }
    }
}

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
        self.tools.validate("tools")?;
        self.engines.validate("engines")?;
        Ok(())
    }
}

/// KiCAD discovery and live board session behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct KicadConfig {
    /// Optional symbol library directory override.
    pub symbol_dir: Option<PathBuf>,
    /// Optional footprint library directory override.
    pub footprint_dir: Option<PathBuf>,
    /// Optional `kicad-cli` path override.
    pub cli_path: Option<PathBuf>,
    /// Optional matching `pcbnew` path override for live IPC sessions.
    pub pcbnew_path: Option<PathBuf>,
    /// Prefer attaching to an already-running KiCAD IPC server before launching
    /// a managed headless process.
    pub attach_running: bool,
    /// Explicitly allow managed headless launch to enable the API server in the
    /// selected KiCAD major version's preferences. Disabled by default because
    /// library initialization must not silently rewrite user configuration.
    pub enable_api_config: bool,
}

impl KicadConfig {
    fn validate(&self, path: &'static str) -> Result<(), ConfigError> {
        validate_optional_path(path, "symbolDir", &self.symbol_dir)?;
        validate_optional_path(path, "footprintDir", &self.footprint_dir)?;
        validate_optional_path(path, "cliPath", &self.cli_path)?;
        validate_optional_path(path, "pcbnewPath", &self.pcbnew_path)?;
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

/// Agent loop policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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

/// Independent review behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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

/// Deterministic engine selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct EngineConfig {
    /// Schematic placement engine.
    pub schematic_placer: SchematicPlacementEngine,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            schematic_placer: SchematicPlacementEngine::Cluster,
        }
    }
}

impl EngineConfig {
    fn validate(&self, _path: &'static str) -> Result<(), ConfigError> {
        Ok(())
    }
}

/// Schematic placement engine selected at the application composition root.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SchematicPlacementEngine {
    /// Annealing followed by cluster-pose and compaction polish.
    #[default]
    Cluster,
    /// Simulated annealing only.
    #[serde(alias = "sa")]
    Anneal,
    /// Deterministic grammar placement only.
    Spine,
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
    fn kicad_pcbnew_path_round_trips_in_config() {
        let mut cfg = GordianConfig::default();
        cfg.kicad.pcbnew_path = Some(PathBuf::from("/opt/kicad10/bin/pcbnew"));

        let encoded = toml::to_string(&cfg).unwrap();
        let decoded: GordianConfig = toml::from_str(&encoded).unwrap();

        assert_eq!(decoded.kicad.pcbnew_path, cfg.kicad.pcbnew_path);
        decoded.validate().unwrap();
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
        assert_eq!(cfg.tools.default_search_limit, DEFAULT_SEARCH_LIMIT);
        assert_eq!(cfg.engines, EngineConfig::default());
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

    #[test]
    fn retired_schema_v1_retrieval_section_remains_loadable() {
        let cfg: GordianConfig = toml::from_str(
            r#"
            schemaVersion = 1

            [retrieval]
            enabled = true
            corpusDir = "/tmp/reference-corpus"
            referencesPerQuery = 3
            "#,
        )
        .unwrap();

        let legacy = cfg
            .legacy_retrieval
            .as_ref()
            .expect("legacy retrieval section is accepted");
        assert!(legacy.enabled);
        assert_eq!(
            legacy.corpus_dir.as_deref(),
            Some(std::path::Path::new("/tmp/reference-corpus"))
        );
        assert_eq!(legacy.references_per_query, 3);
        cfg.validate().unwrap();

        let serialized = toml::to_string(&cfg).unwrap();
        assert!(!serialized.contains("retrieval"), "{serialized}");
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
    fn engine_config_deserializes_aliases() {
        let cfg: GordianConfig = toml::from_str(
            r#"
            [engines]
            schematicPlacer = "sa"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.engines.schematic_placer,
            SchematicPlacementEngine::Anneal
        );
    }

    #[test]
    fn validation_reports_field_path() {
        let mut cfg = GordianConfig::default();
        cfg.tools.default_search_limit = 0;

        let err = cfg.validate().unwrap_err();
        assert_eq!(err.path, "tools.defaultSearchLimit");
    }

    #[test]
    fn validation_reports_empty_llm_adapter() {
        let mut cfg = GordianConfig::default();
        cfg.llm.adapter = Some(" ".to_string());

        let err = cfg.validate().unwrap_err();
        assert_eq!(err.path, "llm.adapter");
    }
}
