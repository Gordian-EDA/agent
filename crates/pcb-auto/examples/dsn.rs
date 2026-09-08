//! Write the Specctra DSN a board would be routed from.
//!
//! ```text
//! cargo run -p pcb-auto --example dsn -- <board.kicad_pcb> <out.dsn>
//! ```

use std::path::PathBuf;

use kicad::KicadInstallation;
use pcb_auto::dsn::write_dsn;
use pcb_auto::model::Board;
use pcb_auto::rules;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() == 2, "usage: dsn <board.kicad_pcb> <out.dsn>");
    let kicad = KicadInstallation::detect().ok_or_else(|| anyhow::anyhow!("no KiCad 10 found"))?;
    let board = Board::load(&PathBuf::from(&args[0]))?;
    let r = rules::infer_rules(&board);
    let widths = rules::router_net_widths(&board, &r);
    let doc = write_dsn(&board, &widths, &r, Some(&kicad))?;
    std::fs::write(&args[1], &doc.text)?;
    println!("{} nets, {} pins held out by the pour", doc.nets.len(), doc.pins_dropped);
    Ok(())
}
