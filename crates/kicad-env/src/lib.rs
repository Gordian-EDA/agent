//! KiCAD installation and library path discovery.
//!
//! This crate is the shared compatibility layer for locating KiCAD resources.
//! Higher-level crates should depend on it rather than deriving install paths
//! from each other.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Known symbol-library locations, checked in order.
const KNOWN_SYMBOL_DIRS: &[&str] = &[
    "/usr/share/kicad/symbols",
    "/usr/local/share/kicad/symbols",
    "/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols",
];

/// Known footprint-library locations, checked in order.
const KNOWN_FOOTPRINT_DIRS: &[&str] = &[
    "/usr/share/kicad/footprints",
    "/usr/local/share/kicad/footprints",
    "/Applications/KiCad/KiCad.app/Contents/SharedSupport/footprints",
];

/// A discovered KiCAD installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KicadEnv {
    /// Directory containing `*.kicad_sym` libraries.
    pub symbol_dir: PathBuf,
    /// Directory containing `*.pretty` footprint libraries.
    pub footprint_dir: PathBuf,
    /// Path to the `kicad-cli` executable.
    pub cli_path: PathBuf,
    /// Output of `kicad-cli version`, e.g. `10.0.3`.
    pub cli_version: String,
}

impl KicadEnv {
    /// Discover an installed KiCAD. Returns `None` if required resources cannot
    /// be found.
    ///
    /// Checks known install paths for libraries and `PATH` for `kicad-cli`.
    pub fn detect() -> Option<Self> {
        Self::detect_with(None, None, None)
    }

    /// Discover KiCAD using explicit overrides where provided, then known
    /// install paths / `PATH` for the missing pieces.
    pub fn detect_with(
        symbol_dir: Option<&Path>,
        footprint_dir: Option<&Path>,
        cli_path: Option<&Path>,
    ) -> Option<Self> {
        let symbol_dir = detect_symbol_dir(symbol_dir)?;
        let footprint_dir = detect_footprint_dir(&symbol_dir, footprint_dir)?;
        let cli_path = match cli_path {
            Some(path) if path.is_file() => path.to_path_buf(),
            Some(_) => return None,
            None => find_in_path("kicad-cli")?,
        };
        let cli_version = cli_version(&cli_path)?;
        Some(Self {
            symbol_dir,
            footprint_dir,
            cli_path,
            cli_version,
        })
    }

    /// Build an environment pointing at an arbitrary symbol directory
    /// (for tests and unsupported distros). CLI fields are placeholders.
    ///
    /// The footprint directory is derived as the sibling `footprints` directory.
    pub fn with_symbol_dir(symbol_dir: PathBuf) -> Self {
        let footprint_dir = sibling_footprint_dir(&symbol_dir);
        Self::with_library_dirs(symbol_dir, footprint_dir)
    }

    /// Build an environment with explicit symbol and footprint directories.
    pub fn with_library_dirs(symbol_dir: PathBuf, footprint_dir: PathBuf) -> Self {
        Self {
            symbol_dir,
            footprint_dir,
            cli_path: PathBuf::new(),
            cli_version: "0".to_string(),
        }
    }
}

fn detect_symbol_dir(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = configured {
        return dir.is_dir().then(|| dir.to_path_buf());
    }
    if let Some(dir) = env_dir("KICAD_SYMBOL_DIR") {
        return Some(dir);
    }
    KNOWN_SYMBOL_DIRS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

fn detect_footprint_dir(symbol_dir: &Path, configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = configured {
        return dir.is_dir().then(|| dir.to_path_buf());
    }
    if let Some(dir) = env_dir("KICAD_FOOTPRINT_DIR") {
        return Some(dir);
    }
    let sibling = sibling_footprint_dir(symbol_dir);
    if sibling.is_dir() {
        return Some(sibling);
    }
    KNOWN_FOOTPRINT_DIRS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

fn sibling_footprint_dir(symbol_dir: &Path) -> PathBuf {
    symbol_dir
        .parent()
        .map(|p| p.join("footprints"))
        .unwrap_or_else(|| PathBuf::from("footprints"))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    if name == "kicad-cli"
        && let Some(path) = env_file("KICAD_CLI_PATH")
    {
        return Some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

fn env_file(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

fn cli_version(cli_path: &Path) -> Option<String> {
    let output = Command::new(cli_path).arg("version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}
