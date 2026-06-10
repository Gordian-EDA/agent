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

    pub fn draft_path(&self) -> PathBuf {
        self.root.join("draft.circuit.yaml")
    }

    /// The current draft text, if a draft exists.
    pub fn read_draft(&self) -> Option<String> {
        std::fs::read_to_string(self.draft_path()).ok()
    }

    /// Write the draft and record which schematic text it was seeded from
    /// (`None` when no schematic exists yet).
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
            return false; // no meta -> nothing to compare against
        };
        let recorded: Option<u64> = serde_json::from_str::<serde_json::Value>(&meta)
            .ok()
            .and_then(|v| v.get("seeded_from_sch_hash").cloned())
            .and_then(|v| v.as_u64());
        recorded != current_sch_text.map(fnv1a64)
    }

    /// The next free `renders/render-NNN.png` path.
    pub fn next_render_path(&self) -> io::Result<PathBuf> {
        let dir = self.root.join("renders");
        for n in 1..10_000u32 {
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
