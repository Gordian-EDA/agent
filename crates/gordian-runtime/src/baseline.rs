//! In-memory project snapshot captured at the start of a user turn.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Project files as they existed when the active user turn started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnBaseline {
    /// Project-relative paths and their exact contents.
    pub files: BTreeMap<PathBuf, Vec<u8>>,
}

impl TurnBaseline {
    /// Captures every regular project file except ephemeral `.gordian` state.
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
        if relative
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == std::ffi::OsStr::new(".gordian"))
        {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            capture_dir(project, &path, files)?;
        } else if file_type.is_file() {
            files.insert(
                relative.to_path_buf(),
                fs::read(&path).with_context(|| format!("capturing {}", path.display()))?,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_project_files_in_memory_and_skips_runtime_state() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("design.kicad_sch"), b"sheet").unwrap();
        fs::create_dir(project.path().join("subdir")).unwrap();
        fs::write(project.path().join("subdir/child.kicad_sch"), b"child").unwrap();
        fs::create_dir(project.path().join(".gordian")).unwrap();
        fs::write(project.path().join(".gordian/state"), b"ephemeral").unwrap();

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
        assert!(!baseline.files.contains_key(Path::new(".gordian/state")));
    }
}
