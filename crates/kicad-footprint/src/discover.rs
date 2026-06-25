use std::io;
use std::path::{Path, PathBuf};

use kicad_cli::env::KicadEnv;

/// The `footprints` share directory for a [`KicadEnv`]: the sibling of its
/// `symbol_dir` (`.../share/kicad/symbols` -> `.../share/kicad/footprints`).
pub(crate) fn footprint_dir(env: &KicadEnv) -> PathBuf {
    env.symbol_dir
        .parent()
        .map(|p| p.join("footprints"))
        .unwrap_or_else(|| PathBuf::from("footprints"))
}

/// Enumerate the `Nickname -> .pretty path` of every installed footprint
/// library, sorted by nickname for determinism.
pub(crate) fn discover_libraries(footprint_dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut libs: Vec<(String, PathBuf)> = std::fs::read_dir(footprint_dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().is_some_and(|x| x == "pretty"))
        .filter_map(|p| {
            let nick = p.file_stem()?.to_str()?.to_string();
            Some((nick, p))
        })
        .collect();
    libs.sort();
    Ok(libs)
}
