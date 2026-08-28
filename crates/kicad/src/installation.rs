//! KiCad installation and library path discovery.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Known symbol-library locations, checked in order. The macOS installer's
/// disk image is dragged straight to `/Applications`, so `KiCad.app` lands
/// there directly; the nested `KiCad/KiCad.app` form only occurs when a user
/// drags the enclosing folder instead, which some older releases shipped.
const KNOWN_SYMBOL_DIRS: &[&str] = &[
    "/usr/share/kicad/symbols",
    "/usr/local/share/kicad/symbols",
    "/Applications/KiCad.app/Contents/SharedSupport/symbols",
    "/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols",
];

/// Known footprint-library locations, checked in order.
const KNOWN_FOOTPRINT_DIRS: &[&str] = &[
    "/usr/share/kicad/footprints",
    "/usr/local/share/kicad/footprints",
    "/Applications/KiCad.app/Contents/SharedSupport/footprints",
    "/Applications/KiCad/KiCad.app/Contents/SharedSupport/footprints",
];

/// Known `kicad-cli` locations, checked when it is not on `PATH`. The macOS
/// app bundle does not add its `MacOS/` directory to `PATH` on install.
const KNOWN_CLI_PATHS: &[&str] = &[
    "/Applications/KiCad.app/Contents/MacOS/kicad-cli",
    "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli",
];

/// Known `pcbnew` locations, checked when it is not beside the selected CLI or
/// on `PATH`.
const KNOWN_PCBNEW_PATHS: &[&str] = &[
    "/Applications/KiCad.app/Contents/MacOS/pcbnew",
    "/Applications/KiCad/KiCad.app/Contents/MacOS/pcbnew",
];

/// A discovered KiCAD installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KicadInstallation {
    /// Directory containing `*.kicad_sym` libraries.
    symbol_dir: PathBuf,
    /// Directory containing `*.pretty` footprint libraries.
    footprint_dir: PathBuf,
    /// Path to the `kicad-cli` executable.
    cli_path: PathBuf,
    /// Path to the matching PCB editor used for live IPC sessions.
    pcbnew_path: PathBuf,
    /// Output of `kicad-cli version`, e.g. `10.0.3`.
    cli_version: String,
}

impl KicadInstallation {
    /// Discover an installed KiCAD. Returns `None` if required resources cannot
    /// be found.
    ///
    /// Checks known install paths for libraries and `PATH` for `kicad-cli`.
    pub fn detect() -> Option<Self> {
        Self::detect_with(None, None, None, None)
    }

    /// Discover KiCAD using explicit overrides where provided, then known
    /// install paths / `PATH` for the missing pieces.
    pub fn detect_with(
        symbol_dir: Option<&Path>,
        footprint_dir: Option<&Path>,
        cli_path: Option<&Path>,
        pcbnew_path: Option<&Path>,
    ) -> Option<Self> {
        let symbol_dir = detect_symbol_dir(symbol_dir)?;
        let footprint_dir = detect_footprint_dir(&symbol_dir, footprint_dir)?;
        let cli_path = match cli_path {
            Some(path) if path.is_file() => path.to_path_buf(),
            Some(_) => return None,
            None => find_in_path("kicad-cli")?,
        };
        let cli_version = cli_version(&cli_path)?;
        if !supported_version(&cli_version) {
            return None;
        }
        let pcbnew_path = detect_pcbnew_path(&cli_path, pcbnew_path)?;
        Some(Self {
            symbol_dir,
            footprint_dir,
            cli_path,
            pcbnew_path,
            cli_version,
        })
    }

    /// Build a library-only fixture with no executable installation.
    #[doc(hidden)]
    pub fn for_library_tests(symbol_dir: PathBuf, footprint_dir: PathBuf) -> Self {
        Self {
            symbol_dir,
            footprint_dir,
            cli_path: PathBuf::new(),
            pcbnew_path: PathBuf::new(),
            cli_version: "0".to_string(),
        }
    }

    pub fn major_version(&self) -> Option<u32> {
        version_major(&self.cli_version)
    }

    pub fn symbol_dir(&self) -> &Path {
        &self.symbol_dir
    }

    pub fn footprint_dir(&self) -> &Path {
        &self.footprint_dir
    }

    pub fn cli_path(&self) -> &Path {
        &self.cli_path
    }

    pub fn pcbnew_path(&self) -> &Path {
        &self.pcbnew_path
    }

    pub fn version(&self) -> &str {
        &self.cli_version
    }
}

fn supported_version(version: &str) -> bool {
    matches!(version_major(version), Some(9 | 10))
}

fn detect_symbol_dir(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = configured {
        return dir.is_dir().then(|| dir.to_path_buf());
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
    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    {
        return Some(found);
    }
    if name == "kicad-cli" {
        return KNOWN_CLI_PATHS
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file());
    }
    None
}

fn detect_pcbnew_path(cli_path: &Path, configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return path.is_file().then(|| path.to_path_buf());
    }
    if let Some(sibling) = cli_path.parent().map(|parent| parent.join("pcbnew"))
        && sibling.is_file()
    {
        return Some(sibling);
    }
    if let Some(found) = find_in_path("pcbnew") {
        return Some(found);
    }
    KNOWN_PCBNEW_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn cli_version(cli_path: &Path) -> Option<String> {
    let output = Command::new(cli_path).arg("version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

fn version_major(version: &str) -> Option<u32> {
    version
        .trim()
        .split('.')
        .next()?
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::supported_version;

    #[test]
    fn supports_maintained_kicad_versions() {
        assert!(supported_version("9.0.2+dfsg-1"));
        assert!(supported_version("9.0.3"));
        assert!(supported_version("10.0.5"));
        assert!(!supported_version("11.0.0"));
    }
}
