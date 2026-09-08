//! KiCad installation and library path discovery.

use std::io;
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

/// A discovered KiCAD installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KicadInstallation {
    /// Directory containing `*.kicad_sym` libraries.
    symbol_dir: PathBuf,
    /// Directory containing `*.pretty` footprint libraries.
    footprint_dir: PathBuf,
    /// Path to the `kicad-cli` executable.
    cli_path: PathBuf,
    /// Output of `kicad-cli version`, e.g. `10.0.3`.
    cli_version: String,
}

impl KicadInstallation {
    /// Discover a complete, version-verified KiCad 10 installation.
    ///
    /// CLI candidates are tried in this order: `KICAD_CLI`, `PATH`,
    /// `~/.local/*/AppDir/usr/bin/kicad-cli`, workspace-adjacent AppDirs, and
    /// known macOS application paths. Every candidate must report version 10 or
    /// newer before its sibling libraries or Debian/Ubuntu and macOS fallback
    /// library paths are considered.
    pub fn detect() -> Option<Self> {
        Self::detect_with(None, None, None).ok()
    }

    /// Discover KiCAD using explicit overrides where provided, then known
    /// install paths / `PATH` for the missing pieces.
    pub fn detect_with(
        symbol_dir: Option<&Path>,
        footprint_dir: Option<&Path>,
        cli_path: Option<&Path>,
    ) -> io::Result<Self> {
        if let Some(cli_path) = cli_path {
            return Self::detect_at(symbol_dir, footprint_dir, cli_path);
        }

        let mut failures = Vec::new();
        for candidate in cli_candidates() {
            match Self::detect_at(symbol_dir, footprint_dir, &candidate) {
                Ok(installation) => return Ok(installation),
                Err(error) => failures.push(format!("{}: {error}", candidate.display())),
            }
        }
        let detail = if failures.is_empty() {
            String::new()
        } else {
            format!("; rejected candidates: {}", failures.join("; "))
        };
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "KiCad 10 is required; set kicad.cliPath, kicad.symbolDir, and kicad.footprintDir in config.toml{detail}"
            ),
        ))
    }

    fn detect_at(
        symbol_dir: Option<&Path>,
        footprint_dir: Option<&Path>,
        cli_path: &Path,
    ) -> io::Result<Self> {
        if !cli_path.is_file() {
            return Err(config_error("kicad.cliPath", cli_path, "is not a file"));
        }
        let cli_path = cli_path.to_path_buf();
        let cli_version = cli_version(&cli_path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "kicad.cliPath {} failed `kicad-cli version`: {error}",
                    cli_path.display()
                ),
            )
        })?;
        if !supported_version(&cli_version) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "KiCad 10 or newer is required, but kicad.cliPath {} reports version {cli_version}; set kicad.cliPath, kicad.symbolDir, and kicad.footprintDir to a KiCad 10 installation",
                    cli_path.display()
                ),
            ));
        }
        let symbol_dir = detect_symbol_dir(&cli_path, symbol_dir).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "KiCad symbol libraries were not found; set kicad.symbolDir to the KiCad 10 symbols directory",
            )
        })?;
        let footprint_dir = detect_footprint_dir(&cli_path, &symbol_dir, footprint_dir)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "KiCad footprint libraries were not found; set kicad.footprintDir to the KiCad 10 footprints directory",
                )
            })?;
        Ok(Self {
            symbol_dir,
            footprint_dir,
            cli_path,
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

    pub fn version(&self) -> &str {
        &self.cli_version
    }

    /// Build a KiCad 10 command with its configured standard-library roots.
    ///
    /// Project library tables use KiCad's portable `KICAD10_*_DIR` variables;
    /// binding them here makes every CLI operation honor the same installation
    /// selected by Gordian instead of whichever libraries the shell exposes.
    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(&self.cli_path);
        command
            .env("KICAD10_SYMBOL_DIR", &self.symbol_dir)
            .env("KICAD10_FOOTPRINT_DIR", &self.footprint_dir);
        command
    }
}

fn supported_version(version: &str) -> bool {
    version_major(version).is_some_and(|major| major >= 10)
}

fn detect_symbol_dir(cli_path: &Path, configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = configured {
        return dir.is_dir().then(|| dir.to_path_buf());
    }
    if let Some(sibling) = cli_share_dir(cli_path, "symbols")
        && sibling.is_dir()
    {
        return Some(sibling);
    }
    KNOWN_SYMBOL_DIRS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

fn detect_footprint_dir(
    cli_path: &Path,
    symbol_dir: &Path,
    configured: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = configured {
        return dir.is_dir().then(|| dir.to_path_buf());
    }
    let sibling = sibling_footprint_dir(symbol_dir);
    if sibling.is_dir() {
        return Some(sibling);
    }
    if let Some(sibling) = cli_share_dir(cli_path, "footprints")
        && sibling.is_dir()
    {
        return Some(sibling);
    }
    KNOWN_FOOTPRINT_DIRS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

fn cli_share_dir(cli_path: &Path, kind: &str) -> Option<PathBuf> {
    Some(
        cli_path
            .parent()?
            .parent()?
            .join("share")
            .join("kicad")
            .join(kind),
    )
}

fn config_error(key: &str, path: &Path, detail: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{key} {} {detail}", path.display()),
    )
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
    None
}

fn cli_candidates() -> Vec<PathBuf> {
    let environment_cli = std::env::var_os("KICAD_CLI").map(PathBuf::from);
    let path_cli = find_in_path("kicad-cli");
    let mut local_roots = std::env::var_os("HOME")
        .map(PathBuf::from)
        .into_iter()
        .map(|home| home.join(".local"))
        .collect::<Vec<_>>();
    if let Ok(current) = std::env::current_dir() {
        local_roots.extend(current.ancestors().map(|root| root.join(".local")));
    }
    local_roots.sort();
    local_roots.dedup();
    cli_candidates_from(environment_cli, path_cli, local_roots)
}

fn cli_candidates_from(
    environment_cli: Option<PathBuf>,
    path_cli: Option<PathBuf>,
    local_roots: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = environment_cli
        .into_iter()
        .chain(path_cli)
        .collect::<Vec<_>>();
    for local in local_roots {
        if let Ok(entries) = std::fs::read_dir(local) {
            let mut app_dirs = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path().join("AppDir/usr/bin/kicad-cli"))
                .filter(|path| path.is_file())
                .collect::<Vec<_>>();
            app_dirs.sort();
            app_dirs.reverse();
            candidates.extend(app_dirs);
        }
    }
    candidates.extend(
        KNOWN_CLI_PATHS
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.is_file()),
    );
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|path| seen.insert(path.clone()));
    candidates
}

fn cli_version(cli_path: &Path) -> io::Result<String> {
    let mut output = None;
    let mut last_error = None;
    for _ in 0..3 {
        match Command::new(cli_path).arg("version").output() {
            Ok(result) => {
                output = Some(result);
                break;
            }
            Err(error) => {
                last_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
    let output = output.ok_or_else(|| {
        last_error.unwrap_or_else(|| io::Error::other("version command did not run"))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "command exited {}{}",
            output.status,
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        )));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        Err(io::Error::other("command returned an empty version"))
    } else {
        Ok(version)
    }
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
    use super::{cli_candidates_from, supported_version};
    use std::path::PathBuf;

    #[test]
    fn supports_kicad_ten_and_newer() {
        assert!(!supported_version("9.0.2+dfsg-1"));
        assert!(!supported_version("9.0.3"));
        assert!(supported_version("10.0.5"));
        assert!(supported_version("11.0.0"));
    }

    #[test]
    fn cli_candidates_prefer_environment_then_path_then_local_appdirs() {
        let root = tempfile::tempdir().expect("tempdir");
        let local = root.path().join(".local");
        let app_cli = local.join("kicad-10/AppDir/usr/bin/kicad-cli");
        std::fs::create_dir_all(app_cli.parent().expect("app bin")).expect("app bin");
        std::fs::write(&app_cli, "").expect("app cli");
        let environment_cli = PathBuf::from("/configured/kicad-cli");
        let path_cli = PathBuf::from("/path/kicad-cli");

        let candidates = cli_candidates_from(
            Some(environment_cli.clone()),
            Some(path_cli.clone()),
            vec![local],
        );

        assert_eq!(candidates[0], environment_cli);
        assert_eq!(candidates[1], path_cli);
        assert_eq!(candidates[2], app_cli);
    }
}
