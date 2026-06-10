//! Snapshot / undo store backing the TUI's `:undo` and the per-write safety net.
//!
//! Snapshots live under `<project>/.auto-pcb/history/` as `<NNNN>-<filename>`,
//! where `NNNN` is a monotonically increasing 4-digit counter. There are no
//! timestamps in filenames: the numbering provides ordering and the store is
//! deterministic. `undo` has *pop* semantics — it restores the latest snapshot
//! over the live file and then removes that snapshot from history.

use std::io;
use std::path::{Path, PathBuf};

/// Width of the zero-padded numeric prefix on snapshot filenames.
const SEQ_WIDTH: usize = 4;

/// A history store rooted at a project's `.auto-pcb/history` directory.
pub struct SnapshotStore {
    history_dir: PathBuf,
}

impl SnapshotStore {
    /// Open (creating if necessary) the snapshot store for `project_dir`.
    pub fn for_project(project_dir: impl AsRef<Path>) -> io::Result<Self> {
        let history_dir = project_dir.as_ref().join(".auto-pcb").join("history");
        std::fs::create_dir_all(&history_dir)?;
        Ok(Self { history_dir })
    }

    /// Copy the current contents of `file` into history as the next snapshot.
    ///
    /// Errors (e.g. `NotFound`) if `file` does not exist.
    pub fn snapshot(&self, file: impl AsRef<Path>) -> io::Result<()> {
        let file = file.as_ref();
        let name = file.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "snapshot source has no file name",
            )
        })?;
        let seq = self.next_seq()?;
        let dest = self
            .history_dir
            .join(format!("{seq:0SEQ_WIDTH$}-{}", name.to_string_lossy()));
        // `copy` propagates a clean `NotFound` if the source is missing.
        std::fs::copy(file, &dest)?;
        Ok(())
    }

    /// Restore the latest snapshot over `file` and remove it from history.
    ///
    /// Returns a `NotFound` error (leaving `file` untouched) when history is
    /// empty.
    pub fn undo(&self, file: impl AsRef<Path>) -> io::Result<()> {
        let latest = self
            .list()?
            .pop()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no snapshots to undo"))?;
        std::fs::copy(&latest, file.as_ref())?;
        std::fs::remove_file(&latest)?;
        Ok(())
    }

    /// List snapshot files in history, sorted by their numeric prefix.
    pub fn list(&self) -> io::Result<Vec<PathBuf>> {
        let mut snapshots: Vec<(u32, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&self.history_dir)? {
            let path = entry?.path();
            if let Some(seq) = seq_of(&path) {
                snapshots.push((seq, path));
            }
        }
        snapshots.sort_by_key(|(seq, _)| *seq);
        Ok(snapshots.into_iter().map(|(_, path)| path).collect())
    }

    /// The next sequence number: one past the highest existing prefix, so
    /// numbering is monotonic even across gaps from deleted snapshots.
    fn next_seq(&self) -> io::Result<u32> {
        let mut max = 0;
        for entry in std::fs::read_dir(&self.history_dir)? {
            let path = entry?.path();
            if let Some(seq) = seq_of(&path) {
                max = max.max(seq);
            }
        }
        Ok(max + 1)
    }
}

/// Parse the leading `NNNN-` numeric prefix from a snapshot file name.
fn seq_of(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let (prefix, _) = name.split_once('-')?;
    prefix.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_and_undo_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let sch = tmp.path().join("x.kicad_sch");
        std::fs::write(&sch, "v1").unwrap();
        let store = SnapshotStore::for_project(tmp.path()).unwrap();
        store.snapshot(&sch).unwrap(); // history/0001-*.kicad_sch
        std::fs::write(&sch, "v2").unwrap();
        store.snapshot(&sch).unwrap();
        std::fs::write(&sch, "v3").unwrap();
        store.undo(&sch).unwrap(); // restores v2 (latest snapshot)
        assert_eq!(std::fs::read_to_string(&sch).unwrap(), "v2");
        // Pop semantics: the restored snapshot is removed, leaving only 0001.
        // (The plan's literal `== 2` contradicts its own "removes it (pop
        // semantics)" prose; pop is the explicit contract, so 1 is correct.)
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn undo_on_empty_history_is_a_clean_error() {
        let tmp = tempfile::tempdir().unwrap();
        let sch = tmp.path().join("x.kicad_sch");
        std::fs::write(&sch, "v1").unwrap();
        let store = SnapshotStore::for_project(tmp.path()).unwrap();
        let err = store.undo(&sch).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        // File is untouched.
        assert_eq!(std::fs::read_to_string(&sch).unwrap(), "v1");
    }

    #[test]
    fn numbering_survives_gaps() {
        let tmp = tempfile::tempdir().unwrap();
        let sch = tmp.path().join("x.kicad_sch");
        std::fs::write(&sch, "v1").unwrap();
        let store = SnapshotStore::for_project(tmp.path()).unwrap();
        store.snapshot(&sch).unwrap(); // 0001
        store.snapshot(&sch).unwrap(); // 0002
        store.snapshot(&sch).unwrap(); // 0003

        // Simulate a gap: delete the middle snapshot.
        let history = tmp.path().join(".auto-pcb/history");
        std::fs::remove_file(history.join("0002-x.kicad_sch")).unwrap();

        // Next snapshot must be 0004, never reusing 0002.
        store.snapshot(&sch).unwrap();
        assert!(history.join("0004-x.kicad_sch").exists());

        let names: Vec<String> = store
            .list()
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["0001-x.kicad_sch", "0003-x.kicad_sch", "0004-x.kicad_sch"]
        );
    }

    #[test]
    fn snapshot_of_nonexistent_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SnapshotStore::for_project(tmp.path()).unwrap();
        let missing = tmp.path().join("nope.kicad_sch");
        let err = store.snapshot(&missing).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
