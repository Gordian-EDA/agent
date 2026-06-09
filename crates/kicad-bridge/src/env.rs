//! KiCAD environment discovery: where the symbol libraries and `kicad-cli` live.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Known symbol-library locations, checked in order.
const KNOWN_SYMBOL_DIRS: &[&str] = &[
    "/usr/share/kicad/symbols",
    "/usr/local/share/kicad/symbols",
    "/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols",
];

/// A discovered KiCAD installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KicadEnv {
    /// Directory containing `*.kicad_sym` libraries.
    pub symbol_dir: PathBuf,
    /// Path to the `kicad-cli` executable.
    pub cli_path: PathBuf,
    /// Output of `kicad-cli version`, e.g. `10.0.3`.
    pub cli_version: String,
}

impl KicadEnv {
    /// Discover an installed KiCAD. Returns `None` if the symbol directory
    /// or `kicad-cli` cannot be found.
    ///
    /// The symbol directory honors the `AUTO_PCB_SYMBOL_DIR` environment
    /// variable as an override before falling back to known install paths.
    pub fn detect() -> Option<Self> {
        let symbol_dir = detect_symbol_dir()?;
        let cli_path = find_in_path("kicad-cli")?;
        let cli_version = cli_version(&cli_path)?;
        Some(Self {
            symbol_dir,
            cli_path,
            cli_version,
        })
    }

    /// Build an environment pointing at an arbitrary symbol directory
    /// (for tests and unsupported distros). CLI fields are placeholders.
    pub fn with_symbol_dir(symbol_dir: PathBuf) -> Self {
        Self {
            symbol_dir,
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
