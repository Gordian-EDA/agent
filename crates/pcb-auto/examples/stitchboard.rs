//! Run the ground-pour stitcher on a finished board and report what it closed.
//!
//! ```text
//! cargo run -p pcb-auto --example stitchboard -- <board.kicad_pcb> [rounds]
//! ```

use std::path::PathBuf;

use kicad::KicadInstallation;
use pcb_auto::checks::check;
use pcb_auto::model::Board;
use pcb_auto::stitch::stitch_pours;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(!args.is_empty(), "usage: stitchboard <board.kicad_pcb> [rounds]");
    let pcb = PathBuf::from(&args[0]);
    let rounds: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(4);
    let kicad = KicadInstallation::detect().ok_or_else(|| anyhow::anyhow!("no KiCad 10 found"))?;
    let clearance = Board::load(&pcb)?.design_rules().clearance;
    {
        // what the fill actually looks like, before anything is stitched
        let dir = tempfile::tempdir()?;
        let copy = dir.path().join("fill.kicad_pcb");
        std::fs::copy(&pcb, &copy)?;
        let pro = pcb.with_extension("kicad_pro");
        if pro.is_file() {
            std::fs::copy(&pro, copy.with_extension("kicad_pro"))?;
        }
        kicad.refill_zones(&copy, true)?;
        let filled = Board::load(&copy)?;
        for z in filled.zones() {
            if z.keepout.is_some() {
                continue;
            }
            println!(
                "zone {} on {:?}: {} filled island(s), areas {:?}",
                z.net_name,
                z.layers,
                z.filled.len(),
                z.filled
                    .iter()
                    .map(|p| pcb_auto::geom::polygon_area(p).abs().round())
                    .collect::<Vec<_>>()
            );
        }
    }
    for r in 0..rounds {
        let mut board = Board::load(&pcb)?;
        let n = stitch_pours(&kicad, &mut board, &pcb, "GND", clearance)?;
        board.strip_zone_fills();
        board.save(Some(&pcb))?;
        let c = check(&kicad, &pcb)?;
        println!(
            "round {r}: {n} via(s) -> unconnected {} completion {:.3} drc errors {}",
            c.unconnected, c.completion, c.errors
        );
        if n == 0 {
            break;
        }
    }
    Ok(())
}
