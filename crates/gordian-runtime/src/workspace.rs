//! Project-local persistent state: `<project>/.gordian/`.
//!
//! Holds the working draft (`draft.circuit.yaml` — the document `create_design`
//! patches and `apply_design` applies), `draft.meta.json` (the content hash of
//! the `.kicad_sch` the draft was seeded from, for staleness detection), and
//! `renders/` (PNGs from `render_schematic`). The directory ships its own
//! `.gitignore` containing `*` so it never pollutes the user's repo. The
//! `session/` subdirectory is reserved for a future resume feature.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

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

    /// Path of the working draft (`draft.circuit.yaml`) inside `.gordian/`.
    pub fn draft_path(&self) -> PathBuf {
        self.root.join("draft.circuit.yaml")
    }

    /// The current draft text, if a draft exists.
    ///
    /// Missing drafts are distinct from drafts that cannot be read. Callers
    /// must not treat corruption, permission errors, or an unsafe file type as
    /// permission to seed or overwrite the user's existing work.
    pub fn read_draft(&self) -> io::Result<Option<String>> {
        let path = self.draft_path();
        match read_regular_to_string(&path) {
            Ok(draft) => Ok(Some(draft)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // File::open reports NotFound for a dangling symlink too. Only
                // translate the error to `None` when the leaf truly is absent.
                match std::fs::symlink_metadata(&path) {
                    Err(leaf_err) if leaf_err.kind() == io::ErrorKind::NotFound => Ok(None),
                    Ok(_) => Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("workspace path is not a regular file: {}", path.display()),
                    )),
                    Err(leaf_err) => Err(leaf_err),
                }
            }
            Err(err) => Err(err),
        }
    }

    /// Write the draft and record which schematic text it was seeded from
    /// (`None` when no schematic exists yet). Passing `sch_text = None` records
    /// a null hash, so a later `draft_is_stale(Some(_))` returns `true`.
    pub fn write_draft(&self, yaml: &str, sch_text: Option<&str>) -> io::Result<()> {
        atomic_write(&self.draft_path(), yaml.as_bytes())?;
        let meta = serde_json::json!({
            "seeded_from_sch_hash": sch_text.map(fnv1a64),
        });
        atomic_write(
            &self.root.join("draft.meta.json"),
            meta.to_string().as_bytes(),
        )
    }

    /// True when the on-disk schematic no longer matches what the draft was
    /// seeded from (the user edited it in KiCAD out-of-band).
    pub fn draft_is_stale(&self, current_sch_text: Option<&str>) -> bool {
        let Ok(meta) = read_regular_to_string(&self.root.join("draft.meta.json")) else {
            // No meta: a draft without a recorded seed hash can't be trusted
            // (e.g. a partial write), so treat it as stale; a fresh workspace
            // with no draft at all is simply not stale.
            return !matches!(
                std::fs::symlink_metadata(self.draft_path()),
                Err(err) if err.kind() == io::ErrorKind::NotFound
            );
        };
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta) else {
            return true;
        };
        let recorded = match meta.get("seeded_from_sch_hash") {
            Some(serde_json::Value::Null) => None,
            Some(value) => match value.as_u64() {
                Some(hash) => Some(hash),
                None => return true,
            },
            None => return true,
        };
        recorded != current_sch_text.map(fnv1a64)
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

#[cfg(unix)]
fn read_regular_to_string(path: &Path) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;

    // Reject special files before opening so an existing FIFO cannot block the
    // agent indefinitely. Identity is checked again below to cover leaf swaps.
    let before = std::fs::symlink_metadata(path)?;
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("workspace path is not a regular file: {}", path.display()),
        ));
    }
    // Compare the opened descriptor's identity to a second non-following leaf
    // lookup. This rejects a symlink swapped into place during the open without
    // ever reading from its target.
    let mut file = std::fs::File::open(path)?;
    let opened = file.metadata()?;
    let leaf = std::fs::symlink_metadata(path)?;
    if leaf.file_type().is_symlink()
        || !opened.is_file()
        || opened.dev() != leaf.dev()
        || opened.ino() != leaf.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("workspace path is not a regular file: {}", path.display()),
        ));
    }
    let mut out = String::new();
    file.read_to_string(&mut out)?;
    Ok(out)
}

#[cfg(not(unix))]
fn read_regular_to_string(path: &Path) -> io::Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("workspace path is not a regular file: {}", path.display()),
        ));
    }
    std::fs::read_to_string(path)
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

/// FNV-1a 64-bit — tiny, dependency-free content hash for staleness checks.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_self_ignoring_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(dir.path()).unwrap();
        assert!(dir.path().join(".gordian/renders").is_dir());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gordian/.gitignore")).unwrap(),
            "*\n"
        );
        assert!(ws.read_draft().unwrap().is_none());
    }

    #[test]
    fn draft_roundtrip_and_staleness_hash() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(dir.path()).unwrap();

        ws.write_draft("version: 1\n", Some("sch contents v1"))
            .unwrap();
        assert_eq!(ws.read_draft().unwrap().as_deref(), Some("version: 1\n"));
        // Same sch text -> not stale; different -> stale.
        assert!(!ws.draft_is_stale(Some("sch contents v1")));
        assert!(ws.draft_is_stale(Some("sch contents v2")));
        // Draft seeded with no schematic on disk: stale only once a sch appears.
        ws.write_draft("version: 1\n", None).unwrap();
        assert!(!ws.draft_is_stale(None));
        assert!(ws.draft_is_stale(Some("anything")));
    }

    #[test]
    fn render_paths_increment() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(dir.path()).unwrap();
        let a = ws.write_render(b"first").unwrap();
        let b = ws.write_render(b"second").unwrap();
        assert!(a.ends_with("render-001.png"), "{}", a.display());
        assert!(b.ends_with("render-002.png"), "{}", b.display());
        assert_eq!(std::fs::read(a).unwrap(), b"first");
        assert_eq!(std::fs::read(b).unwrap(), b"second");
    }

    #[cfg(unix)]
    #[test]
    fn workspace_directory_symlink_cannot_escape_the_project() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), project.path().join(".gordian")).unwrap();

        let err = Workspace::for_project(project.path())
            .err()
            .expect("workspace symlink must be rejected");

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!outside.path().join("renders").exists());
        assert!(!outside.path().join(".gitignore").exists());
    }

    #[cfg(unix)]
    #[test]
    fn draft_symlink_is_never_read_or_followed_by_atomic_write() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(project.path()).unwrap();
        let target = outside.path().join("secret.kicad_sch");
        std::fs::write(&target, "outside secret").unwrap();
        symlink(&target, ws.draft_path()).unwrap();

        assert_eq!(
            ws.read_draft().unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "draft reads must reject symlinks"
        );
        ws.write_draft("version: 1\n", None).unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "outside secret");
        assert_eq!(ws.read_draft().unwrap().as_deref(), Some("version: 1\n"));
        assert!(
            !std::fs::symlink_metadata(ws.draft_path())
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn unreadable_draft_is_not_reported_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(dir.path()).unwrap();
        std::fs::write(ws.draft_path(), [0xff, 0xfe]).unwrap();

        let err = ws.read_draft().unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(ws.draft_path()).unwrap(), [0xff, 0xfe]);
    }

    #[test]
    fn malformed_draft_metadata_is_always_stale() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(dir.path()).unwrap();
        ws.write_draft("version: 1\n", None).unwrap();
        std::fs::write(dir.path().join(".gordian/draft.meta.json"), "not json").unwrap();

        assert!(ws.draft_is_stale(None));
    }

    #[cfg(unix)]
    #[test]
    fn render_dangling_symlink_is_not_followed() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(project.path()).unwrap();
        let target = outside.path().join("created-outside.png");
        symlink(
            &target,
            project.path().join(".gordian/renders/render-001.png"),
        )
        .unwrap();

        let written = ws.write_render(b"png").unwrap();

        assert!(written.ends_with("render-002.png"), "{}", written.display());
        assert_eq!(std::fs::read(written).unwrap(), b"png");
        assert!(!target.exists());
    }
}
