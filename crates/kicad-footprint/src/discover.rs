//! Low-level filesystem discovery of `.pretty` libraries and their entries.
//!
//! Private to the crate: [`crate::FootprintCatalog`] is the public entry point.

use std::path::{Path, PathBuf};

use crate::catalog::{FootprintEntry, FootprintLibrary};
use crate::error::{Error, Result};
use crate::id::{FootprintId, LibraryId};

/// Enumerate `.pretty` libraries and their `.kicad_mod` entries under `root`,
/// sorted by id for determinism.
///
/// A failure to read `root` itself is reported as [`Error::Io`]; an individual
/// library directory that cannot be read is skipped so one bad library does not
/// sink the whole scan.
pub(crate) fn discover(root: &Path) -> Result<(Vec<FootprintLibrary>, Vec<FootprintEntry>)> {
    let read = std::fs::read_dir(root).map_err(|e| Error::Io {
        path: root.to_path_buf(),
        source: e,
    })?;

    let mut pretty: Vec<(LibraryId, PathBuf)> = read
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().is_some_and(|x| x == "pretty"))
        .filter_map(|p| {
            let nick = p.file_stem()?.to_str()?;
            LibraryId::new(nick).ok().map(|id| (id, p))
        })
        .collect();
    pretty.sort_by(|a, b| a.0.cmp(&b.0));

    let mut libraries = Vec::with_capacity(pretty.len());
    let mut entries = Vec::new();
    for (lib_id, dir) in pretty {
        libraries.push(FootprintLibrary::new(lib_id.clone(), dir.clone()));
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<(String, PathBuf)> = rd
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "kicad_mod"))
            .filter_map(|p| {
                let name = p.file_stem()?.to_str()?.to_string();
                Some((name, p))
            })
            .collect();
        files.sort();
        for (name, path) in files {
            if let Ok(id) = FootprintId::new(lib_id.clone(), name) {
                entries.push(FootprintEntry::new(id, path));
            }
        }
    }
    Ok((libraries, entries))
}
