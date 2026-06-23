//! Project-local persistent state: `<project>/.autopcb/`.
//!
//! Holds the working draft (`draft.circuit.yaml` — the document `edit_design`
//! patches and `apply_design` applies), `draft.meta.json` (the content hash of
//! the `.kicad_sch` the draft was seeded from, for staleness detection), and
//! `renders/` (PNGs from `render_schematic`). The directory ships its own
//! `.gitignore` containing `*` so it never pollutes the user's repo. The
//! `session/` subdirectory is reserved for a future resume feature.

use std::io;
use std::path::{Path, PathBuf};

pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Open (creating if needed) the `.autopcb/` directory under `project_dir`.
    pub fn for_project(project_dir: &Path) -> io::Result<Self> {
        let root = project_dir.join(".autopcb");
        std::fs::create_dir_all(root.join("renders"))?;
        let gi = root.join(".gitignore");
        if !gi.exists() {
            std::fs::write(&gi, "*\n")?;
        }
        Ok(Self { root })
    }

    /// Path of the working draft (`draft.circuit.yaml`) inside `.autopcb/`.
    pub fn draft_path(&self) -> PathBuf {
        self.root.join("draft.circuit.yaml")
    }

    /// The current draft text, if a draft exists.
    pub fn read_draft(&self) -> Option<String> {
        std::fs::read_to_string(self.draft_path()).ok()
    }

    /// Write the draft and record which schematic text it was seeded from
    /// (`None` when no schematic exists yet). Passing `sch_text = None` records
    /// a null hash, so a later `draft_is_stale(Some(_))` returns `true`.
    pub fn write_draft(&self, yaml: &str, sch_text: Option<&str>) -> io::Result<()> {
        std::fs::write(self.draft_path(), yaml)?;
        let meta = serde_json::json!({
            "seeded_from_sch_hash": sch_text.map(fnv1a64),
        });
        std::fs::write(self.root.join("draft.meta.json"), meta.to_string())
    }

    /// True when the on-disk schematic no longer matches what the draft was
    /// seeded from (the user edited it in KiCAD out-of-band).
    pub fn draft_is_stale(&self, current_sch_text: Option<&str>) -> bool {
        let Ok(meta) = std::fs::read_to_string(self.root.join("draft.meta.json")) else {
            // No meta: a draft without a recorded seed hash can't be trusted
            // (e.g. a partial write), so treat it as stale; a fresh workspace
            // with no draft at all is simply not stale.
            return self.draft_path().exists();
        };
        let recorded: Option<u64> = serde_json::from_str::<serde_json::Value>(&meta)
            .ok()
            .and_then(|v| v.get("seeded_from_sch_hash").cloned())
            .and_then(|v| v.as_u64());
        recorded != current_sch_text.map(fnv1a64)
    }

    /// Path of the persisted board draft (`board.json`) inside `.autopcb/`.
    pub fn board_path(&self) -> PathBuf {
        self.root.join("board.json")
    }

    /// The raw board-draft JSON text, if a board draft exists. Callers
    /// deserialize into their own `BoardDraft` (mirrors [`Self::read_draft`]).
    pub fn read_board(&self) -> Option<String> {
        std::fs::read_to_string(self.board_path()).ok()
    }

    /// Persist the board-draft JSON text (mirrors [`Self::write_draft`]).
    pub fn write_board(&self, json: &str) -> io::Result<()> {
        std::fs::write(self.board_path(), json)
    }

    /// Path of the persisted route solution (`route.json`) inside `.autopcb/`.
    /// Written by `route_board` (Task 2); read here so `get_board` can report
    /// the routed flag.
    pub fn route_path(&self) -> PathBuf {
        self.root.join("route.json")
    }

    /// The raw route-solution JSON text, if a routed solution exists.
    pub fn read_route(&self) -> Option<String> {
        std::fs::read_to_string(self.route_path()).ok()
    }

    /// Persist the route-solution JSON text (used by `route_board` in Task 2).
    pub fn write_route(&self, json: &str) -> io::Result<()> {
        std::fs::write(self.route_path(), json)
    }

    /// The next free `renders/render-NNN.png` path.
    pub fn next_render_path(&self) -> io::Result<PathBuf> {
        let dir = self.root.join("renders");
        // TOCTOU note: the returned path is not reserved. This is a
        // single-process UI tool, so we accept the tiny window between this
        // existence check and the caller's write rather than locking.
        for n in 1..=999u32 {
            let p = dir.join(format!("render-{n:03}.png"));
            if !p.exists() {
                return Ok(p);
            }
        }
        Err(io::Error::other("renders/ directory is full"))
    }
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
        assert!(dir.path().join(".autopcb/renders").is_dir());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".autopcb/.gitignore")).unwrap(),
            "*\n"
        );
        assert!(ws.read_draft().is_none());
    }

    #[test]
    fn draft_roundtrip_and_staleness_hash() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::for_project(dir.path()).unwrap();

        ws.write_draft("version: 1\n", Some("sch contents v1")).unwrap();
        assert_eq!(ws.read_draft().as_deref(), Some("version: 1\n"));
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
        let a = ws.next_render_path().unwrap();
        std::fs::write(&a, b"x").unwrap();
        let b = ws.next_render_path().unwrap();
        assert!(a.ends_with("render-001.png"), "{}", a.display());
        assert!(b.ends_with("render-002.png"), "{}", b.display());
    }
}
