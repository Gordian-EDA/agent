//! Per-agent runtime state for KiCAD-backed tool execution.
//!
//! `AgentRuntime` is deliberately per instance: frontends build one from a
//! caller-supplied [`crate::GordianConfig`] and project directory, then hand it
//! to [`crate::Agent`]. The core crate does not own config persistence or any
//! process-global runtime.

use sch_io::write::escape_sexpr_string as sexpr_escape;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use kicad::KicadInstallation;
use kicad_footprint::FootprintCatalog;
use kicad_symbol::SymbolTable;
use kicad_symbol::search::SymbolIndex;

use crate::config::GordianConfig;

/// Per-agent runtime resources every KiCAD tool runs against.
///
/// This is the application state for one agent/project session: KiCAD
/// discovery, typed config, project-local paths/state, and tool service caches.
/// It is not process-global, so hosted or multi-tenant callers can construct a
/// fresh runtime for each request/session.
pub struct AgentRuntime {
    env: KicadInstallation,
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
    pub fn new(env: KicadInstallation, project_dir: PathBuf, sch_path: PathBuf) -> Result<Self> {
        Self::new_with_config(env, project_dir, sch_path, GordianConfig::default())
    }

    /// Build a runtime for an existing project directory with typed config.
    pub fn new_with_config(
        env: KicadInstallation,
        project_dir: PathBuf,
        sch_path: PathBuf,
        config: GordianConfig,
    ) -> Result<Self> {
        config
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid Gordian config: {e}"))?;
        validate_project_schematic_path(&project_dir, &sch_path)?;
        let project = ProjectContext::for_project(project_dir, sch_path)?;
        ensure_project_files(&env, &project.project_dir, &project.sch_path)?;
        let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
        let pcbnew_path = env.pcbnew_path().to_path_buf();
        let kicad_major = env.major_version();
        Ok(Self {
            env,
            services: ToolServices::new(
                provider,
                None,
                config.kicad.attach_running,
                pcbnew_path,
                kicad_major,
                config.kicad.enable_api_config,
            ),
            project,
            config,
            _tempdir: None,
        })
    }

    /// Build a runtime for a real project directory using the default schematic
    /// filename.
    pub fn for_project(env: KicadInstallation, project_dir: PathBuf) -> Result<Self> {
        Self::for_project_with_config(env, project_dir, GordianConfig::default())
    }

    /// Build a runtime for a real project directory using the configured
    /// schematic filename.
    pub fn for_project_with_config(
        env: KicadInstallation,
        project_dir: PathBuf,
        config: GordianConfig,
    ) -> Result<Self> {
        config
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid Gordian config: {e}"))?;
        std::fs::create_dir_all(&project_dir)
            .with_context(|| format!("creating project dir {}", project_dir.display()))?;
        let sch_path = project_dir.join(&config.project.schematic_filename);
        Self::new_with_config(env, project_dir, sch_path, config)
    }

    /// Detect a real KiCAD installation and build a runtime over a fresh
    /// temporary project. Returns `None` when no KiCAD is found.
    pub fn detect_for_test() -> Option<Self> {
        let env = KicadInstallation::detect()?;
        let tempdir = tempfile::tempdir().ok()?;
        let project_dir = tempdir.path().to_path_buf();
        let sch_path = project_dir.join("project.kicad_sch");
        let project = ProjectContext::for_project(project_dir, sch_path).ok()?;
        let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
        let pcbnew_path = env.pcbnew_path().to_path_buf();
        let kicad_major = env.major_version();
        Some(Self {
            env,
            project,
            services: ToolServices::new(provider, None, false, pcbnew_path, kicad_major, false),
            config: GordianConfig::default(),
            _tempdir: Some(tempdir),
        })
    }

    /// Build a runtime over a fresh temporary project whose footprint index is
    /// sourced from `footprint_dir` instead of an installed KiCAD share dir.
    pub fn with_footprint_dir_for_test(footprint_dir: PathBuf) -> Option<Self> {
        let env = KicadInstallation::detect().unwrap_or_else(|| {
            KicadInstallation::for_library_tests(
                PathBuf::from("/nonexistent"),
                PathBuf::from("/nonexistent"),
            )
        });
        let tempdir = tempfile::tempdir().ok()?;
        let project_dir = tempdir.path().to_path_buf();
        let sch_path = project_dir.join("project.kicad_sch");
        let project = ProjectContext::for_project(project_dir, sch_path).ok()?;
        let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
        let pcbnew_path = env.pcbnew_path().to_path_buf();
        let kicad_major = env.major_version();
        Some(Self {
            env,
            project,
            services: ToolServices::new(
                provider,
                Some(footprint_dir),
                false,
                pcbnew_path,
                kicad_major,
                false,
            ),
            config: GordianConfig::default(),
            _tempdir: Some(tempdir),
        })
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
    pub fn env(&self) -> &KicadInstallation {
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
    pub fn kicad(&self) -> &kicad_ipc::SessionManager {
        &self.services.kicad
    }

    /// The typed Gordian config this runtime was built with.
    pub fn config(&self) -> &GordianConfig {
        &self.config
    }

    /// The cross-library symbol index, built once and cached.
    pub fn index(&self) -> Result<&SymbolIndex> {
        if let Some(idx) = self.services.index.get() {
            return Ok(idx);
        }
        let idx = SymbolIndex::build(self.env.symbol_dir()).context("building symbol index")?;
        let _ = self.services.index.set(idx);
        Ok(self.services.index.get().expect("index just set"))
    }

    /// The cross-library footprint catalog, built once and cached.
    pub fn footprint_catalog(&self) -> Result<&FootprintCatalog> {
        if let Some(catalog) = self.services.footprint_catalog.get() {
            return Ok(catalog);
        }
        let catalog = match &self.services.footprint_dir_override {
            Some(dir) => FootprintCatalog::from_root(dir)
                .with_context(|| format!("building footprint catalog from {}", dir.display()))?,
            None => FootprintCatalog::from_root(self.env.footprint_dir())
                .context("building footprint catalog")?,
        };
        let _ = self.services.footprint_catalog.set(catalog);
        Ok(self
            .services
            .footprint_catalog
            .get()
            .expect("footprint catalog just set"))
    }
}

fn validate_project_schematic_path(project_dir: &Path, sch_path: &Path) -> Result<()> {
    let relative = sch_path.strip_prefix(project_dir).map_err(|_| {
        anyhow::anyhow!(
            "schematic path {} must be a direct child of project directory {}",
            sch_path.display(),
            project_dir.display()
        )
    })?;
    let mut components = relative.components();
    let is_direct_schematic = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
        && relative.extension().is_some_and(|ext| ext == "kicad_sch");
    if !is_direct_schematic {
        anyhow::bail!(
            "schematic path {} must be exactly one direct .kicad_sch child of project directory {}",
            sch_path.display(),
            project_dir.display()
        );
    }
    Ok(())
}

fn ensure_project_files(
    env: &KicadInstallation,
    project_dir: &Path,
    sch_path: &Path,
) -> Result<()> {
    write_project_file(sch_path)?;
    write_sym_lib_table(env, project_dir)?;
    write_fp_lib_table(env, project_dir)?;
    Ok(())
}

fn write_project_file(sch_path: &Path) -> Result<()> {
    let stem = sch_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let path = sch_path.with_file_name(format!("{stem}.kicad_pro"));
    if path.exists() {
        return Ok(());
    }
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project.kicad_pro");
    let project = serde_json::json!({
        "board": {
            "design_settings": {
                "rules": {
                    "min_clearance": 0.0,
                    "min_track_width": 0.0,
                    "min_via_diameter": 0.0,
                    "min_hole_clearance": 0.0,
                    "min_hole_to_hole": 0.0
                }
            }
        },
        "net_settings": {
            "classes": [
                {
                    "name": "Default",
                    "clearance": 0.2,
                    "track_width": 0.25,
                    "via_diameter": 0.8,
                    "via_drill": 0.4,
                    "microvia_diameter": 0.3,
                    "microvia_drill": 0.1,
                    "diff_pair_gap": 0.25,
                    "diff_pair_width": 0.2,
                    "priority": 2147483647
                }
            ],
            "meta": { "version": 3 }
        },
        "meta": { "filename": filename, "version": 1 }
    });
    let out = serde_json::to_string_pretty(&project)?;
    std::fs::write(&path, format!("{out}\n")).with_context(|| format!("writing {}", path.display()))
}

fn write_sym_lib_table(env: &KicadInstallation, project_dir: &Path) -> Result<()> {
    let path = project_dir.join("sym-lib-table");
    if path.exists() {
        return Ok(());
    }
    let mut libs = Vec::new();
    for entry in std::fs::read_dir(env.symbol_dir())
        .with_context(|| format!("reading symbol dir {}", env.symbol_dir().display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("kicad_sym") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        libs.push((name.to_string(), path));
    }
    libs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("(sym_lib_table\n");
    for (name, path) in libs {
        out.push_str(&format!(
            "  (lib (name \"{}\") (type \"KiCad\") (uri \"{}\") (options \"\") (descr \"\"))\n",
            sexpr_escape(&name),
            sexpr_escape(&path.display().to_string())
        ));
    }
    out.push_str(")\n");
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))
}

fn write_fp_lib_table(env: &KicadInstallation, project_dir: &Path) -> Result<()> {
    let path = project_dir.join("fp-lib-table");
    if path.exists() {
        return Ok(());
    }
    let mut libs = Vec::new();
    for entry in std::fs::read_dir(env.footprint_dir())
        .with_context(|| format!("reading footprint dir {}", env.footprint_dir().display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("pretty") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        libs.push((name.to_string(), path));
    }
    libs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("(fp_lib_table\n");
    for (name, path) in libs {
        out.push_str(&format!(
            "  (lib (name \"{}\") (type \"KiCad\") (uri \"{}\") (options \"\") (descr \"\"))\n",
            sexpr_escape(&name),
            sexpr_escape(&path.display().to_string())
        ));
    }
    out.push_str(")\n");
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))
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
        pcbnew_path: PathBuf,
        expected_kicad_major: Option<u32>,
        enable_api_config: bool,
    ) -> Self {
        Self {
            provider,
            index: OnceLock::new(),
            footprint_catalog: OnceLock::new(),
            footprint_dir_override,
            kicad: kicad_ipc::SessionManager::with_installation(
                pcbnew_path,
                expected_kicad_major,
                attach_running_kicad,
                enable_api_config,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_env(root: &Path) -> KicadInstallation {
        let symbols = root.join("symbols");
        let footprints = root.join("footprints");
        std::fs::create_dir_all(&symbols).expect("symbols dir");
        std::fs::create_dir_all(&footprints).expect("footprints dir");
        KicadInstallation::for_library_tests(symbols, footprints)
    }

    fn assert_explicit_path_rejected_without_writes(
        env: &KicadInstallation,
        project_dir: PathBuf,
        sch_path: PathBuf,
        case: &str,
    ) {
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::create_dir_all(sch_path.parent().expect("schematic parent"))
            .expect("schematic parent dir");
        let escaped_pro = sch_path.with_extension("kicad_pro");

        let err = AgentRuntime::new_with_config(
            env.clone(),
            project_dir.clone(),
            sch_path,
            GordianConfig::default(),
        )
        .err()
        .expect("invalid schematic path must fail");

        assert!(
            err.to_string().contains("schematic path"),
            "{case}: {err:#}"
        );
        assert!(!escaped_pro.exists(), "{case}: wrote outside project");
        assert!(
            !project_dir.join(".gordian").exists(),
            "{case}: workspace written"
        );
        assert!(
            !project_dir.join("sym-lib-table").exists(),
            "{case}: scaffold written"
        );
        assert!(
            !project_dir.join("fp-lib-table").exists(),
            "{case}: scaffold written"
        );
    }

    #[test]
    fn project_scaffold_writes_project_and_library_tables() {
        let temp = tempfile::tempdir().expect("tempdir");
        let libs = temp.path().join("libs");
        let symbols = libs.join("symbols");
        let footprints = libs.join("footprints");
        std::fs::create_dir_all(&symbols).expect("symbols dir");
        std::fs::create_dir_all(&footprints).expect("footprints dir");
        std::fs::write(symbols.join("Device.kicad_sym"), "").expect("symbol lib");
        std::fs::create_dir_all(footprints.join("Resistor_SMD.pretty")).expect("fp lib");

        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let env = KicadInstallation::for_library_tests(symbols, footprints);

        ensure_project_files(&env, &project, &project.join("design.kicad_sch"))
            .expect("project files");

        let pro = std::fs::read_to_string(project.join("design.kicad_pro")).expect("project file");
        assert!(pro.contains("\"filename\": \"design.kicad_pro\""));
        let sym = std::fs::read_to_string(project.join("sym-lib-table")).expect("sym table");
        assert!(sym.contains("(name \"Device\")"));
        let fp = std::fs::read_to_string(project.join("fp-lib-table")).expect("fp table");
        assert!(fp.contains("(name \"Resistor_SMD\")"));
    }

    #[test]
    fn invalid_configured_schematic_paths_are_rejected_before_writes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = fixture_env(temp.path());

        for (case, filename) in [
            (
                "absolute",
                temp.path()
                    .join("outside.kicad_sch")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("parent", "../outside.kicad_sch".to_string()),
            ("subdir", "nested/design.kicad_sch".to_string()),
        ] {
            let project_dir = temp.path().join(format!("project-{case}"));
            let mut config = GordianConfig::default();
            config.project.schematic_filename = filename;

            let err =
                AgentRuntime::for_project_with_config(env.clone(), project_dir.clone(), config)
                    .err()
                    .expect("invalid schematic path must fail");

            assert!(
                err.to_string().contains("project.schematicFilename"),
                "{case}: {err:#}"
            );
            assert!(
                !project_dir.exists(),
                "{case}: project directory was written"
            );
            assert!(
                !temp.path().join("outside.kicad_sch").exists(),
                "{case}: path escaped the project directory"
            );
        }
    }

    #[test]
    fn explicit_schematic_path_must_be_a_direct_project_child() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = fixture_env(temp.path());
        assert_explicit_path_rejected_without_writes(
            &env,
            temp.path().join("project-absolute"),
            temp.path().join("absolute-outside.kicad_sch"),
            "absolute",
        );

        let parent_project = temp.path().join("project-parent");
        assert_explicit_path_rejected_without_writes(
            &env,
            parent_project.clone(),
            parent_project.join("../parent-outside.kicad_sch"),
            "parent",
        );

        let subdir_project = temp.path().join("project-subdir");
        assert_explicit_path_rejected_without_writes(
            &env,
            subdir_project.clone(),
            subdir_project.join("nested/design.kicad_sch"),
            "subdir",
        );

        assert_explicit_path_rejected_without_writes(
            &env,
            temp.path().join("project-mismatched-parent"),
            temp.path().join("other-project/design.kicad_sch"),
            "mismatched parent",
        );

        let extension_project = temp.path().join("project-wrong-extension");
        assert_explicit_path_rejected_without_writes(
            &env,
            extension_project.clone(),
            extension_project.join("design.sch"),
            "wrong extension",
        );
    }

    #[test]
    fn explicit_direct_schematic_child_builds_runtime() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = fixture_env(temp.path());
        let project_dir = temp.path().join("valid-project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let sch_path = project_dir.join("custom.kicad_sch");

        let runtime = AgentRuntime::new_with_config(
            env,
            project_dir.clone(),
            sch_path.clone(),
            GordianConfig::default(),
        )
        .expect("valid direct child");

        assert_eq!(runtime.sch_path(), sch_path);
        assert!(project_dir.join("custom.kicad_pro").is_file());
        assert!(project_dir.join("sym-lib-table").is_file());
        assert!(project_dir.join("fp-lib-table").is_file());
        assert!(project_dir.join(".gordian").is_dir());
    }
}
