//! Per-agent runtime state for KiCAD-backed tool execution.
//!
//! `AgentRuntime` is deliberately per instance: frontends build one from a
//! caller-supplied [`crate::GordianConfig`] and project directory, then hand it
//! to [`crate::Agent`]. The core crate does not own config persistence or any
//! process-global runtime.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use circuit_lang::model::Design;
use kicad_env::KicadEnv;
use kicad_footprint::FootprintCatalog;
use kicad_symbol::SymbolTable;
use kicad_symbol::search::SymbolIndex;
use sch_floorplan::floorplan::{LayoutIr, infer_ir};

use crate::config::{DEFAULT_SCHEMATIC_FILENAME, GordianConfig};

/// Per-agent runtime resources every KiCAD tool runs against.
///
/// This is the application state for one agent/project session: KiCAD
/// discovery, typed config, project-local paths/state, and tool service caches.
/// It is not process-global, so hosted or multi-tenant callers can construct a
/// fresh runtime for each request/session.
pub struct AgentRuntime {
    env: KicadEnv,
    /// Client-supplied typed config. Persistence belongs to the frontend.
    config: GordianConfig,
    /// Project-local files and persistent state.
    project: ProjectContext,
    /// Shared services and caches used by tools.
    services: ToolServices,
    /// Keeps a test tempdir alive for the runtime's lifetime; `None` for real runtimes.
    _tempdir: Option<tempfile::TempDir>,
}

/// Project-local identity and persistent state.
pub struct ProjectContext {
    /// Project directory holding the schematic and `.gordian/`.
    project_dir: PathBuf,
    /// Path to the project's `.kicad_sch` (may not exist yet).
    sch_path: PathBuf,
    /// Project-local persistent state directory `.gordian/`.
    workspace: crate::workspace::Workspace,
}

/// Shared services and caches for tool execution.
struct ToolServices {
    /// Symbol provider over the installed libraries (memoizes lookups).
    provider: SymbolTable,
    /// Cross-library name index, built on first `search_symbols` and reused.
    index: OnceLock<SymbolIndex>,
    /// Cross-library footprint catalog, built on first `search_footprints` /
    /// `get_footprint_info` / `regenerate_board` and reused.
    footprint_catalog: OnceLock<FootprintCatalog>,
    /// Test override: when set, build the footprint catalog from this directory
    /// of `.pretty` libraries instead of the installed KiCAD footprint share dir.
    footprint_dir_override: Option<PathBuf>,
    /// KiCAD IPC session manager for live board editing.
    kicad: kicad_ipc::SessionManager,
}

/// Tool execution happens on blocking threads; the context must cross them.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AgentRuntime>();
};

impl AgentRuntime {
    /// Build a runtime for an existing project directory.
    ///
    /// `project_dir` must exist; `sch_path` is the schematic the tools read and
    /// write. The schematic itself need not exist yet.
    pub fn new(env: KicadEnv, project_dir: PathBuf, sch_path: PathBuf) -> Result<Self> {
        Self::new_with_config(env, project_dir, sch_path, GordianConfig::default())
    }

    /// Build a runtime for an existing project directory with typed config.
    pub fn new_with_config(
        env: KicadEnv,
        project_dir: PathBuf,
        sch_path: PathBuf,
        config: GordianConfig,
    ) -> Result<Self> {
        let project = ProjectContext::for_project(project_dir, sch_path)?;
        let provider = SymbolTable::from_env(&env);
        Ok(Self {
            env,
            services: ToolServices::new(provider, None, config.kicad.attach_running),
            project,
            config,
            _tempdir: None,
        })
    }

    /// Build a runtime for a real project directory using the default schematic
    /// filename.
    pub fn for_project(env: KicadEnv, project_dir: PathBuf) -> Result<Self> {
        Self::for_project_with_config(env, project_dir, GordianConfig::default())
    }

    /// Build a runtime for a real project directory using the configured
    /// schematic filename.
    pub fn for_project_with_config(
        env: KicadEnv,
        project_dir: PathBuf,
        config: GordianConfig,
    ) -> Result<Self> {
        std::fs::create_dir_all(&project_dir)
            .with_context(|| format!("creating project dir {}", project_dir.display()))?;
        let schematic_filename = if config.project.schematic_filename.trim().is_empty() {
            DEFAULT_SCHEMATIC_FILENAME
        } else {
            config.project.schematic_filename.as_str()
        };
        let sch_path = project_dir.join(schematic_filename);
        Self::new_with_config(env, project_dir, sch_path, config)
    }

    /// Detect a real KiCAD installation and build a runtime over a fresh
    /// temporary project. Returns `None` when no KiCAD is found.
    pub fn detect_for_test() -> Option<Self> {
        let env = KicadEnv::detect()?;
        let tempdir = tempfile::tempdir().ok()?;
        let project_dir = tempdir.path().to_path_buf();
        let sch_path = project_dir.join("project.kicad_sch");
        let project = ProjectContext::for_project(project_dir, sch_path).ok()?;
        let provider = SymbolTable::from_env(&env);
        Some(Self {
            env,
            project,
            services: ToolServices::new(provider, None, false),
            config: GordianConfig::default(),
            _tempdir: Some(tempdir),
        })
    }

    /// Build a runtime over a fresh temporary project whose footprint index is
    /// sourced from `footprint_dir` instead of an installed KiCAD share dir.
    pub fn with_footprint_dir_for_test(footprint_dir: PathBuf) -> Option<Self> {
        let env = KicadEnv::detect()
            .unwrap_or_else(|| KicadEnv::with_symbol_dir(PathBuf::from("/nonexistent")));
        let tempdir = tempfile::tempdir().ok()?;
        let project_dir = tempdir.path().to_path_buf();
        let sch_path = project_dir.join("project.kicad_sch");
        let project = ProjectContext::for_project(project_dir, sch_path).ok()?;
        let provider = SymbolTable::from_env(&env);
        Some(Self {
            env,
            project,
            services: ToolServices::new(provider, Some(footprint_dir), false),
            config: GordianConfig::default(),
            _tempdir: Some(tempdir),
        })
    }

    /// The Layout IR for `design`.
    pub(crate) fn layout_for(&self, design: &Design) -> LayoutIr {
        infer_ir(&self.env, design)
    }

    /// The project's `.kicad_sch` path.
    pub fn sch_path(&self) -> &Path {
        &self.project.sch_path
    }

    /// The project's `.kicad_pcb` path.
    pub fn pcb_path(&self) -> PathBuf {
        self.project.sch_path.with_extension("kicad_pcb")
    }

    /// The project directory.
    pub fn project_dir(&self) -> &Path {
        &self.project.project_dir
    }

    /// Close the cached live KiCAD session, if one is open.
    pub fn close_kicad_session(&self) {
        self.services.kicad.close();
    }

    /// The detected KiCAD environment.
    pub fn env(&self) -> &KicadEnv {
        &self.env
    }

    /// The symbol provider over the installed libraries.
    pub fn provider(&self) -> &SymbolTable {
        &self.services.provider
    }

    /// The project's `.gordian/` persistent state.
    pub fn workspace(&self) -> &crate::workspace::Workspace {
        &self.project.workspace
    }

    /// The live KiCAD IPC session manager.
    pub(crate) fn kicad(&self) -> &kicad_ipc::SessionManager {
        &self.services.kicad
    }

    /// The typed Gordian config this runtime was built with.
    pub fn config(&self) -> &GordianConfig {
        &self.config
    }

    /// The cross-library symbol index, built once and cached.
    pub(crate) fn index(&self) -> Result<&SymbolIndex> {
        if let Some(idx) = self.services.index.get() {
            return Ok(idx);
        }
        let idx = SymbolIndex::build(&self.env).context("building symbol index")?;
        let _ = self.services.index.set(idx);
        Ok(self.services.index.get().expect("index just set"))
    }

    /// The cross-library footprint catalog, built once and cached.
    pub(crate) fn footprint_catalog(&self) -> Result<&FootprintCatalog> {
        if let Some(catalog) = self.services.footprint_catalog.get() {
            return Ok(catalog);
        }
        let catalog = match &self.services.footprint_dir_override {
            Some(dir) => FootprintCatalog::from_root(dir)
                .with_context(|| format!("building footprint catalog from {}", dir.display()))?,
            None => FootprintCatalog::from_env(&self.env).context("building footprint catalog")?,
        };
        let _ = self.services.footprint_catalog.set(catalog);
        Ok(self
            .services
            .footprint_catalog
            .get()
            .expect("footprint catalog just set"))
    }
}

impl ProjectContext {
    fn for_project(project_dir: PathBuf, sch_path: PathBuf) -> Result<Self> {
        let workspace = crate::workspace::Workspace::for_project(&project_dir)
            .with_context(|| format!("opening .gordian workspace in {}", project_dir.display()))?;
        Ok(Self {
            project_dir,
            sch_path,
            workspace,
        })
    }
}

impl ToolServices {
    fn new(
        provider: SymbolTable,
        footprint_dir_override: Option<PathBuf>,
        attach_running_kicad: bool,
    ) -> Self {
        Self {
            provider,
            index: OnceLock::new(),
            footprint_catalog: OnceLock::new(),
            footprint_dir_override,
            kicad: kicad_ipc::SessionManager::with_attach_running(attach_running_kicad),
        }
    }
}
