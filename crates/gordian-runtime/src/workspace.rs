//! Project-local persistent state: `<project>/.gordian/`.
//!
//! Holds generated `renders/` and schematic undo snapshots. The directory ships its own
//! `.gitignore` containing `*` so it never pollutes the user's repo. The
//! `session/` subdirectory is reserved for a future resume feature.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Open (creating if needed) the `.gordian/` directory under `project_dir`.
    pub fn for_project(project_dir: &Path) -> io::Result<Self> {
        let root = project_dir.join(".gordian");
        ensure_real_dir(&root)?;
        ensure_real_dir(&root.join("renders"))?;
        let gi = root.join(".gitignore");
        match std::fs::symlink_metadata(&gi) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "workspace file must not be a symbolic link: {}",
                        gi.display()
                    ),
                ));
            }
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("workspace path is not a file: {}", gi.display()),
                ));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => atomic_write(&gi, b"*\n")?,
            Err(err) => return Err(err),
        }
        Ok(Self { root })
    }

    /// Atomically persist a PNG under the next free `renders/render-NNN.png`.
    /// Existing leaves of every kind, including dangling symlinks, are never
    /// followed or replaced.
    pub fn write_render(&self, png: &[u8]) -> io::Result<PathBuf> {
        let dir = self.root.join("renders");
        for n in 1..=999u32 {
            let p = dir.join(format!("render-{n:03}.png"));
            match std::fs::symlink_metadata(&p) {
                Ok(_) => continue,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
            temp.write_all(png)?;
            temp.as_file().sync_all()?;
            match temp.persist_noclobber(&p) {
                Ok(_) => {
                    sync_dir(&dir)?;
                    return Ok(p);
                }
                Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err.error),
            }
        }
        Err(io::Error::other("renders/ directory is full"))
    }
}

fn ensure_real_dir(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "workspace directory must not be a symbolic link: {}",
                path.display()
            ),
        )),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("workspace path is not a directory: {}", path.display()),
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => std::fs::create_dir(path),
        Err(err) => Err(err),
    }
}

/// Replace one state file atomically from a temporary file in the same
/// directory. Readers see either the complete old value or the complete new
/// value; an existing leaf symlink is replaced rather than followed.
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("workspace path has no parent: {}", path.display()),
        )
    })?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(contents)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|err| err.error)?;
    // The file sync above makes its contents durable; syncing the directory
    // makes the rename itself durable across a sudden power loss.
    sync_dir(parent)?;
    Ok(())
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_self_ignoring_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let _workspace = Workspace::for_project(dir.path()).unwrap();
        assert!(dir.path().join(".gordian/renders").is_dir());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gordian/.gitignore")).unwrap(),
            "*\\n"
        );
    }

    #[test]
    fn render_paths_increment() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::for_project(dir.path()).unwrap();
        let first = workspace.write_render(b"first").unwrap();
        let second = workspace.write_render(b"second").unwrap();
        assert!(first.ends_with("render-001.png"));
        assert!(second.ends_with("render-002.png"));
        assert_eq!(std::fs::read(first).unwrap(), b"first");
        assert_eq!(std::fs::read(second).unwrap(), b"second");
    }

    #[cfg(unix)]
    #[test]
    fn workspace_directory_symlink_cannot_escape_the_project() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), project.path().join(".gordian")).unwrap();
        let error = Workspace::for_project(project.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!outside.path().join("renders").exists());
        assert!(!outside.path().join(".gitignore").exists());
    }
}
