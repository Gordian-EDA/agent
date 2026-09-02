//! Shared scaffolding for the tests that drive a live KiCAD.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// One pcbnew at a time.
///
/// A second IPC session on the same machine hands out a different token for the
/// same socket, and the loser's board reads fail with a token mismatch — which
/// looks exactly like a board bug. Cargo runs test binaries concurrently, so the
/// lock has to live on the filesystem rather than in a process.
pub struct KicadLock(PathBuf);

/// Longer than any single board test; past this the holder has crashed.
const STALE_AFTER: Duration = Duration::from_secs(600);

impl KicadLock {
    pub fn acquire() -> Self {
        let path = std::env::temp_dir().join("gordian-kicad-ipc.lock");
        loop {
            if std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .is_ok()
            {
                return Self(path);
            }
            if std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .and_then(|at| SystemTime::now().duration_since(at).map_err(std::io::Error::other))
                .is_ok_and(|age| age > STALE_AFTER)
            {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for KicadLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
