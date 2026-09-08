//! Report the ground pour's islands and what ties each of them to the plane.
//!
//! ```text
//! cargo run -p pcb-auto --example pour -- <board.kicad_pcb> [net]
//! ```

use std::path::PathBuf;

use kicad::KicadInstallation;
use pcb_auto::geom::{point_in_polygon, polygon_area};
use pcb_auto::model::Board;
use pcb_auto::stitch::filled_islands;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(!args.is_empty(), "usage: pour <board.kicad_pcb> [net]");
    let pcb = PathBuf::from(&args[0]);
    let net = args.get(1).cloned().unwrap_or_else(|| "GND".into());
    let kicad = KicadInstallation::detect().ok_or_else(|| anyhow::anyhow!("no KiCad 10 found"))?;
    let board = Board::load(&pcb)?;
    let gnd = board
        .net_by_name(&net)
        .ok_or_else(|| anyhow::anyhow!("no net {net}"))?;
    let islands = filled_islands(&kicad, &pcb, &net)?;
    let pads: Vec<(String, (f64, f64), bool)> = board
        .footprints()
        .iter()
        .flat_map(|f| {
            f.pads
                .iter()
                .filter(|p| p.net_id == gnd.id)
                .map(|p| (format!("{}.{}", f.ref_, p.number), p.pos, p.is_through()))
                .collect::<Vec<_>>()
        })
        .collect();
    let vias: Vec<(f64, f64)> = board
        .vias()
        .iter()
        .filter(|v| v.net_id == gnd.id)
        .map(|v| v.pos)
        .collect();
    for isl in &islands {
        let on: Vec<&String> = pads
            .iter()
            .filter(|(_, p, _)| point_in_polygon(*p, &isl.poly))
            .map(|(r, _, _)| r)
            .collect();
        let through = pads
            .iter()
            .filter(|(_, p, t)| *t && point_in_polygon(*p, &isl.poly))
            .count();
        let v = vias.iter().filter(|p| point_in_polygon(**p, &isl.poly)).count();
        println!(
            "{:6} area {:8.2}  pads {:2} (through {through}, vias {v})  {:?}",
            isl.layer,
            polygon_area(&isl.poly).abs(),
            on.len(),
            on
        );
    }
    Ok(())
}
