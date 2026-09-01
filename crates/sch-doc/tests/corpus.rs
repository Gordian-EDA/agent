//! Locating the schematic corpus the round-trip and extractor gates run over:
//! KiCAD's own hand-drawn demos plus this workspace's placement snapshots.

#![allow(dead_code)] // each test binary uses a different slice of this helper

use std::path::{Path, PathBuf};

use kicad::KicadInstallation;

/// Walk up from the crate to the workspace root, then out to the checkout that
/// holds `.local` (a git worktree keeps it in the main checkout).
pub fn repo_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut dir: Option<&Path> = Some(Path::new(env!("CARGO_MANIFEST_DIR")));
    while let Some(current) = dir {
        roots.push(current.to_path_buf());
        dir = current.parent();
    }
    roots
}

fn first_existing(suffix: &str) -> Option<PathBuf> {
    std::env::var_os("KICAD_ROOT")
        .map(PathBuf::from)
        .map(|root| root.join(suffix))
        .filter(|p| p.exists())
        .or_else(|| {
            repo_roots()
                .into_iter()
                .map(|root| root.join(".local/kicad-10.0.4/AppDir").join(suffix))
                .find(|p| p.exists())
        })
}

/// The KiCAD 10 install the demos were authored with, if it is present.
pub fn kicad10() -> Option<KicadInstallation> {
    KicadInstallation::detect_with(
        Some(&first_existing("share/kicad/symbols")?),
        Some(&first_existing("share/kicad/footprints")?),
        Some(&first_existing("usr/bin/kicad-cli")?),
        Some(&first_existing("usr/bin/pcbnew")?),
    )
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "kicad_sch") {
            out.push(path);
        }
    }
}

/// Every corpus schematic, sorted.
pub fn files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(demos) = first_existing("share/kicad/demos") {
        collect(&demos, &mut out);
    }
    for root in repo_roots() {
        let snapshots = root.join("crates/sch-floorplan/tests/snapshots");
        if snapshots.is_dir() {
            collect(&snapshots, &mut out);
            break;
        }
    }
    out
}

/// Trim a corpus path to something readable in a failure message.
pub fn label(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    match path.parent().and_then(|p| p.file_name()) {
        Some(parent) => format!("{}/{name}", parent.to_string_lossy()),
        None => name.into_owned(),
    }
}
