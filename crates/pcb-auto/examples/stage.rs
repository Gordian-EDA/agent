//! Stage a board from a schematic and print what landed on it.
//!
//! ```text
//! cargo run -p pcb-auto --example stage -- <file.kicad_sch> <out.kicad_pcb>
//! ```

use std::path::PathBuf;

use kicad::KicadInstallation;
use pcb_auto::model::Board;
use pcb_auto::schematic::board_from_schematic;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() == 2, "usage: stage <schematic> <out.kicad_pcb>");
    let kicad = KicadInstallation::detect().ok_or_else(|| anyhow::anyhow!("no KiCad 10 found"))?;
    let out = PathBuf::from(&args[1]);
    let n = board_from_schematic(&kicad, &PathBuf::from(&args[0]), &out)?;
    let b = Board::load(&out)?;
    println!("{n} parts, {} nets, {} pads netted", b.nets().len() - 1,
             b.footprints().iter().flat_map(|f| f.pads.iter()).filter(|p| p.net_id != 0).count());
    Ok(())
}
