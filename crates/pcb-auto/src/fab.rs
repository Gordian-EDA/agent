//! Fabrication outputs: Gerbers, the drill file, the pick-and-place table and the BOM.

use std::path::{Path, PathBuf};

use kicad::KicadInstallation;

/// Export the fabrication bundle for `pcb` into `out_dir`, returning every file written.
///
/// The BOM comes from `sch` when one is given; a board on its own still yields Gerbers, drill
/// and placement files.
pub fn export_fab(
    kicad: &KicadInstallation,
    pcb: &Path,
    sch: Option<&Path>,
    out_dir: &Path,
) -> anyhow::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(out_dir)?;
    let mut files = kicad.export_gerbers(pcb, out_dir)?;
    files.extend(kicad.export_drill(pcb, out_dir)?);
    let stem = pcb
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "board".into());
    files.push(kicad.export_pos(pcb, &out_dir.join(format!("{stem}-pos.csv")))?);
    if let Some(sch) = sch {
        files.push(kicad.export_bom(sch, &out_dir.join(format!("{stem}-bom.csv")))?);
    }
    Ok(files)
}
