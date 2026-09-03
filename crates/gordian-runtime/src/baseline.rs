//! In-memory project snapshot captured at the start of a user turn.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Bare filenames a KiCad design needs beyond its `kicad_*` files.
const DESIGN_FILENAMES: [&str; 2] = ["fp-lib-table", "sym-lib-table"];

/// Directory names never worth walking: runtime state, VCS metadata and build
/// output, any of which can dwarf the design by orders of magnitude when the
/// project directory is also a source repository.
const SKIPPED_DIRS: [&str; 2] = ["target", "node_modules"];

/// Project files as they existed when the active user turn started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnBaseline {
    /// Project-relative paths and their exact contents.
    pub files: BTreeMap<PathBuf, Vec<u8>>,
}

impl TurnBaseline {
    /// Captures the design files — everything KiCad needs to reopen the project
    /// from a scratch copy of the snapshot — and nothing else.
    pub fn capture(project: &Path) -> Result<Self> {
        let mut files = BTreeMap::new();
        capture_dir(project, project, &mut files)?;
        Ok(Self { files })
    }

    /// Returns the turn-start bytes for a project path that existed then.
    pub fn file<'a>(&'a self, project: &Path, path: &Path) -> Option<&'a [u8]> {
        let relative = path.strip_prefix(project).unwrap_or(path);
        self.files.get(relative).map(Vec::as_slice)
    }
}

fn capture_dir(
    project: &Path,
    directory: &Path,
    files: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("reading project directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(project)
            .expect("captured paths start inside the project");
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if !is_skipped_dir(&entry.file_name()) {
                capture_dir(project, &path, files)?;
            }
        } else if file_type.is_file() && is_design_file(&path) {
            files.insert(
                relative.to_path_buf(),
                fs::read(&path).with_context(|| format!("capturing {}", path.display()))?,
            );
        }
    }
    Ok(())
}

/// Whether a directory is skipped wholesale — hidden directories (`.git`, and
/// Gordian's own `.gordian` state) plus known build-output trees.
fn is_skipped_dir(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(b".")
        || name
            .to_str()
            .is_some_and(|name| SKIPPED_DIRS.contains(&name))
}

/// Whether a file belongs to the KiCad design: any `kicad_*` file (sheets,
/// boards, project settings, local libraries) or a library table.
fn is_design_file(path: &Path) -> bool {
    let has_kicad_extension = path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.starts_with("kicad_"));
    has_kicad_extension
        || path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| DESIGN_FILENAMES.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_design_files_in_memory_and_skips_everything_else() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("design.kicad_sch"), b"sheet").unwrap();
        fs::write(project.path().join("sym-lib-table"), b"libs").unwrap();
        fs::create_dir(project.path().join("subdir")).unwrap();
        fs::write(project.path().join("subdir/child.kicad_sch"), b"child").unwrap();
        fs::write(project.path().join("README.md"), b"prose").unwrap();
        fs::create_dir(project.path().join(".gordian")).unwrap();
        fs::write(project.path().join(".gordian/state"), b"ephemeral").unwrap();
        fs::create_dir(project.path().join("target")).unwrap();
        fs::write(project.path().join("target/stale.kicad_sch"), b"build").unwrap();

        let baseline = TurnBaseline::capture(project.path()).unwrap();

        assert_eq!(
            baseline.file(project.path(), &project.path().join("design.kicad_sch")),
            Some(b"sheet".as_slice())
        );
        assert_eq!(
            baseline.file(
                project.path(),
                &project.path().join("subdir/child.kicad_sch")
            ),
            Some(b"child".as_slice())
        );
        assert_eq!(
            baseline.file(project.path(), &project.path().join("sym-lib-table")),
            Some(b"libs".as_slice())
        );
        assert!(!baseline.files.contains_key(Path::new("README.md")));
        assert!(!baseline.files.contains_key(Path::new(".gordian/state")));
        assert!(!baseline.files.contains_key(Path::new("target/stale.kicad_sch")));
    }
}
