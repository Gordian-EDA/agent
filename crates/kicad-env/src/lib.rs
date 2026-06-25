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
    /// `AUTO_PCB_SYMBOL_DIR` and `AUTO_PCB_FOOTPRINT_DIR` override library
    /// discovery independently before falling back to known install paths.
    pub fn detect() -> Option<Self> {
        let symbol_dir = detect_symbol_dir()?;
        let footprint_dir = detect_footprint_dir(&symbol_dir)?;
        let cli_path = find_in_path("kicad-cli")?;
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

fn detect_symbol_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AUTO_PCB_SYMBOL_DIR") {
        let dir = PathBuf::from(dir);
        return dir.is_dir().then_some(dir);
    }
    KNOWN_SYMBOL_DIRS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

fn detect_footprint_dir(symbol_dir: &Path) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AUTO_PCB_FOOTPRINT_DIR") {
        let dir = PathBuf::from(dir);
        return dir.is_dir().then_some(dir);
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
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn cli_version(cli_path: &Path) -> Option<String> {
    let output = Command::new(cli_path).arg("version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}
