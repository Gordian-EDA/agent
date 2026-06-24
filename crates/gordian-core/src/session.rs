//! Session persistence — reserved for a future resume feature.
//!
//! The agent's conversation lives in [`crate::Agent`]'s in-memory history today.
//! A persisted session would let the CLI / web backend resume a conversation
//! across process restarts. The on-disk location is the platform state dir
//! ([`session_dir`]); the serialization format is intentionally left open.

use std::path::PathBuf;

/// The directory sessions would persist to: the platform state/data dir under a
/// `gordian/sessions` namespace (e.g. `~/.local/state/gordian/sessions` on
/// Linux), falling back to a relative `.gordian/sessions` when no home is found.
pub fn session_dir() -> PathBuf {
    directories::ProjectDirs::from("", "Gordian", "gordian")
        .map(|d| d.data_local_dir().join("sessions"))
        .unwrap_or_else(|| PathBuf::from(".gordian/sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_dir_is_namespaced() {
        let dir = session_dir();
        assert!(
            dir.ends_with("sessions"),
            "sessions live under a sessions/ leaf: {}",
            dir.display()
        );
    }
}
