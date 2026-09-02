//! Project-wide revisions for schematic and board files.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::workspace::atomic_write;

/// Maximum number of project revisions retained on disk.
pub const RETENTION: usize = 50;

/// Monotonically increasing project revision identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevisionId(u64);

impl RevisionId {
    /// Creates a revision identifier from its numeric form.
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    /// Returns the numeric identifier stored in the revision directory name.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RevisionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for RevisionId {
    type Err = std::num::ParseIntError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

/// One path named by a mutator and whether it existed before the edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionFile {
    /// Project-relative path of the captured file.
    pub path: PathBuf,
    /// Whether the file existed when the revision was captured.
    pub existed: bool,
}

/// Durable metadata for one pre-edit project state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionManifest {
    /// Revision directory identifier.
    pub id: RevisionId,
    /// Mutating tool that captured the state.
    pub tool: String,
    /// Human-readable description of the pending edit.
    pub summary: String,
    /// Exact paths the tool declared it might change.
    pub files: Vec<RevisionFile>,
    /// UTC creation time in RFC 3339 form.
    pub created_at: String,
    /// Conversation identifier from the logging context, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Files restored by an undo operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// Revision whose contents were restored.
    pub id: RevisionId,
    /// Project-relative paths restored or removed.
    pub files: Vec<PathBuf>,
}

/// The first pre-write state captured for one project file in the active turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnBaseline {
    /// Revision containing the file's pre-write state.
    pub revision: RevisionId,
    /// Stored snapshot path, or `None` when the file did not exist yet.
    pub path: Option<PathBuf>,
}

/// The revision store rooted at `<project>/.gordian/revisions`.
pub struct Revisions {
    project: PathBuf,
    root: PathBuf,
    lock: Mutex<()>,
    turn_baselines: Mutex<Option<BTreeMap<PathBuf, TurnBaseline>>>,
}

impl Revisions {
    /// Opens a revision store without a live board session invalidator.
    pub fn for_project(project: PathBuf) -> Self {
        Self {
            root: project.join(".gordian/revisions"),
            project,
            lock: Mutex::new(()),
            turn_baselines: Mutex::new(None),
        }
    }

    /// Starts baseline tracking for a new user turn.
    pub fn begin_turn(&self) -> Result<()> {
        *self
            .turn_baselines
            .lock()
            .map_err(|_| anyhow!("turn baseline lock poisoned"))? = Some(BTreeMap::new());
        Ok(())
    }

    /// Returns the first pre-write snapshot captured for `path` this turn.
    pub fn turn_baseline(&self, path: &Path) -> Result<Option<TurnBaseline>> {
        let (_, relative) = self.resolve_path(path)?;
        Ok(self
            .turn_baselines
            .lock()
            .map_err(|_| anyhow!("turn baseline lock poisoned"))?
            .as_ref()
            .and_then(|baselines| baselines.get(&relative))
            .cloned())
    }

    /// Returns the captured state of `path` in one revision.
    pub fn file_at_revision(&self, id: RevisionId, path: &Path) -> Result<TurnBaseline> {
        let (_, relative) = self.resolve_path(path)?;
        let manifest = self.manifest(Some(id))?;
        let file = manifest
            .files
            .iter()
            .find(|file| file.path == relative)
            .ok_or_else(|| anyhow!("revision {id} did not capture {}", relative.display()))?;
        Ok(TurnBaseline {
            revision: id,
            path: file
                .existed
                .then(|| self.root.join(id.to_string()).join(relative)),
        })
    }

    /// Captures the exact named files before a mutating tool writes them.
    pub fn capture(&self, tool: &str, summary: &str, files: &[PathBuf]) -> Result<RevisionId> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("revision store lock poisoned"))?;
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating revision store {}", self.root.display()))?;
        let id = RevisionId(self.next_id()?);
        let staging = tempfile::Builder::new()
            .prefix(".capture-")
            .tempdir_in(&self.root)
            .context("creating revision staging directory")?;
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        for requested in files {
            let (source, relative) = self.resolve_path(requested)?;
            if !seen.insert(relative.clone()) {
                continue;
            }
            let existed = match fs::symlink_metadata(&source) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!(
                        "revision file must not be a symbolic link: {}",
                        source.display()
                    )
                }
                Ok(metadata) if metadata.is_file() => true,
                Ok(_) => bail!("revision path is not a file: {}", source.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(error).with_context(|| format!("reading {}", source.display()));
                }
            };
            if existed {
                let destination = staging.path().join(&relative);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&source, &destination)
                    .with_context(|| format!("capturing {} in revision {id}", source.display()))?;
            }
            entries.push(RevisionFile {
                path: relative,
                existed,
            });
        }
        let manifest = RevisionManifest {
            id,
            tool: tool.to_owned(),
            summary: summary.to_owned(),
            files: entries,
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            thread_id: crate::logging::thread_id().map(str::to_owned),
        };
        let manifest_json = serde_json::to_vec_pretty(&manifest)?;
        atomic_write(&staging.path().join("manifest.json"), &manifest_json)?;
        fs::rename(staging.keep(), self.root.join(id.to_string()))
            .with_context(|| format!("committing revision {id}"))?;
        if let Some(baselines) = self
            .turn_baselines
            .lock()
            .map_err(|_| anyhow!("turn baseline lock poisoned"))?
            .as_mut()
        {
            for entry in &manifest.files {
                baselines
                    .entry(entry.path.clone())
                    .or_insert_with(|| TurnBaseline {
                        revision: id,
                        path: entry
                            .existed
                            .then(|| self.root.join(id.to_string()).join(&entry.path)),
                    });
            }
        }
        self.prune()?;
        Ok(id)
    }

    /// Restores all paths in a revision, or the latest revision when `id` is absent.
    pub fn restore(&self, id: Option<RevisionId>) -> Result<Restored> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("revision store lock poisoned"))?;
        let id = match id {
            Some(id) => id,
            None => self
                .revision_ids()?
                .into_iter()
                .next_back()
                .ok_or_else(|| anyhow!("no revisions to restore"))?,
        };
        let manifest = self.read_manifest(id)?;
        let mut staged = Vec::new();
        for entry in &manifest.files {
            let (_, relative) = self.resolve_path(&entry.path)?;
            let target = self.project.join(&relative);
            if entry.existed {
                let source = self.root.join(id.to_string()).join(&relative);
                let bytes = fs::read(&source).with_context(|| {
                    format!("reading {} from revision {id}", relative.display())
                })?;
                staged.push((target, Some(bytes)));
            } else {
                staged.push((target, None));
            }
        }
        for (target, contents) in &staged {
            match contents {
                Some(contents) => atomic_write(target, contents).with_context(|| {
                    format!("restoring {} from revision {id}", target.display())
                })?,
                None => match fs::remove_file(target) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("removing {}", target.display()));
                    }
                },
            }
        }
        Ok(Restored {
            id,
            files: manifest.files.into_iter().map(|entry| entry.path).collect(),
        })
    }

    /// Returns revision manifests newest first, limited to `limit` entries.
    pub fn history(&self, limit: usize) -> Result<Vec<RevisionManifest>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("revision store lock poisoned"))?;
        self.revision_ids()?
            .into_iter()
            .rev()
            .take(limit)
            .map(|id| self.read_manifest(id))
            .collect()
    }

    /// Returns one manifest, or the latest manifest when `id` is absent.
    pub fn manifest(&self, id: Option<RevisionId>) -> Result<RevisionManifest> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("revision store lock poisoned"))?;
        let id = match id {
            Some(id) => id,
            None => self
                .revision_ids()?
                .into_iter()
                .next_back()
                .ok_or_else(|| anyhow!("no revisions available"))?,
        };
        self.read_manifest(id)
    }

    fn resolve_path(&self, requested: &Path) -> Result<(PathBuf, PathBuf)> {
        let relative = if requested.is_absolute() {
            requested.strip_prefix(&self.project).map_err(|_| {
                anyhow!(
                    "revision path {} is outside project {}",
                    requested.display(),
                    self.project.display()
                )
            })?
        } else {
            requested
        };
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            bail!(
                "revision path must stay inside the project: {}",
                requested.display()
            );
        }
        Ok((self.project.join(relative), relative.to_path_buf()))
    }

    fn next_id(&self) -> Result<u64> {
        Ok(self
            .revision_ids()?
            .into_iter()
            .next_back()
            .map_or(1, |id| id.0 + 1))
    }

    fn revision_ids(&self) -> Result<BTreeSet<RevisionId>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeSet::new());
            }
            Err(error) => return Err(error.into()),
        };
        Ok(entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect())
    }

    fn read_manifest(&self, id: RevisionId) -> Result<RevisionManifest> {
        let path = self.root.join(id.to_string()).join("manifest.json");
        let bytes =
            fs::read(&path).with_context(|| format!("no revision {id} at {}", path.display()))?;
        let manifest: RevisionManifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("reading revision {id} manifest"))?;
        if manifest.id != id {
            bail!("revision {id} manifest identifies revision {}", manifest.id);
        }
        Ok(manifest)
    }

    fn prune(&self) -> Result<()> {
        let ids = self.revision_ids()?;
        let excess = ids.len().saturating_sub(RETENTION);
        let protected = self
            .turn_baselines
            .lock()
            .map_err(|_| anyhow!("turn baseline lock poisoned"))?
            .as_ref()
            .into_iter()
            .flat_map(|baselines| baselines.values().map(|baseline| baseline.revision))
            .collect::<BTreeSet<_>>();
        for id in ids
            .into_iter()
            .filter(|id| !protected.contains(id))
            .take(excess)
        {
            fs::remove_dir_all(self.root.join(id.to_string()))
                .with_context(|| format!("pruning revision {id}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_manifest_and_restore_existing_and_created_files() {
        let project = tempfile::tempdir().unwrap();
        let schematic = project.path().join("design.kicad_sch");
        let board = project.path().join("design.kicad_pcb");
        fs::write(&schematic, b"before schematic").unwrap();
        let revisions = Revisions::for_project(project.path().to_path_buf());

        let id = revisions
            .capture(
                "sync_board",
                "Create the project board",
                &[schematic.clone(), board.clone()],
            )
            .unwrap();
        fs::write(&schematic, b"after schematic").unwrap();
        fs::write(&board, b"created board").unwrap();

        let manifest = revisions.history(1).unwrap().pop().unwrap();
        assert_eq!(manifest.id, id);
        assert_eq!(manifest.tool, "sync_board");
        assert_eq!(manifest.summary, "Create the project board");
        assert_eq!(manifest.files.len(), 2);
        assert_eq!(manifest.files[0].path, Path::new("design.kicad_sch"));
        assert!(manifest.files[0].existed);
        assert_eq!(manifest.files[1].path, Path::new("design.kicad_pcb"));
        assert!(!manifest.files[1].existed);
        chrono::DateTime::parse_from_rfc3339(&manifest.created_at).unwrap();
        let on_disk: RevisionManifest = serde_json::from_slice(
            &fs::read(
                project
                    .path()
                    .join(format!(".gordian/revisions/{id}/manifest.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(on_disk, manifest);

        let restored = revisions.restore(Some(id)).unwrap();
        assert_eq!(restored.id, id);
        assert_eq!(fs::read(&schematic).unwrap(), b"before schematic");
        assert!(!board.exists(), "a file created after capture is removed");
    }

    #[test]
    fn restore_defaults_to_latest_revision() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join("design.kicad_sch");
        let revisions = Revisions::for_project(project.path().to_path_buf());
        fs::write(&path, b"one").unwrap();
        revisions
            .capture("first", "first", std::slice::from_ref(&path))
            .unwrap();
        fs::write(&path, b"two").unwrap();
        let latest = revisions
            .capture("second", "second", std::slice::from_ref(&path))
            .unwrap();
        fs::write(&path, b"three").unwrap();

        assert_eq!(revisions.restore(None).unwrap().id, latest);
        assert_eq!(fs::read(path).unwrap(), b"two");
    }

    #[test]
    fn turn_baseline_is_first_capture_per_file() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join("design.kicad_sch");
        let revisions = Revisions::for_project(project.path().to_path_buf());
        fs::write(&path, b"turn start").unwrap();
        revisions.begin_turn().unwrap();

        let first = revisions
            .capture("edit", "first", std::slice::from_ref(&path))
            .unwrap();
        fs::write(&path, b"after first").unwrap();
        revisions
            .capture("edit", "second", std::slice::from_ref(&path))
            .unwrap();

        let baseline = revisions.turn_baseline(&path).unwrap().unwrap();
        assert_eq!(baseline.revision, first);
        assert_eq!(fs::read(baseline.path.unwrap()).unwrap(), b"turn start");

        revisions.begin_turn().unwrap();
        let next = revisions
            .capture("edit", "next turn", std::slice::from_ref(&path))
            .unwrap();
        assert_eq!(
            revisions.turn_baseline(&path).unwrap().unwrap().revision,
            next
        );
    }

    #[test]
    fn capture_prunes_everything_beyond_retention() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join("design.kicad_sch");
        let revisions = Revisions::for_project(project.path().to_path_buf());
        fs::write(&path, b"state").unwrap();
        for index in 0..=RETENTION {
            fs::write(&path, index.to_string()).unwrap();
            revisions
                .capture("edit", "edit", std::slice::from_ref(&path))
                .unwrap();
        }

        let history = revisions.history(RETENTION + 10).unwrap();
        assert_eq!(history.len(), RETENTION);
        assert_eq!(history.first().unwrap().id.get(), (RETENTION + 1) as u64);
        assert_eq!(history.last().unwrap().id.get(), 2);
        assert!(!project.path().join(".gordian/revisions/1").exists());
    }
}
