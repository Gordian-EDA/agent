//! Report DRC, completion and renders for an existing board.
//!
//! ```text
//! cargo run -p pcb-auto --example inspect -- <board.kicad_pcb> [out_dir]
//! ```

use std::path::PathBuf;

use kicad::KicadInstallation;
use pcb_auto::checks::check;
use pcb_auto::render::{render, Side};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(!args.is_empty(), "usage: inspect <board.kicad_pcb> [out_dir]");
    let pcb = PathBuf::from(&args[0]);
    let out = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| "target/inspect".into()));
    let kicad = KicadInstallation::detect().ok_or_else(|| anyhow::anyhow!("no KiCad 10 found"))?;
    let r = check(&kicad, &pcb)?;
    println!("{r:#?}");
    for (side, name) in [(Side::Front, "front"), (Side::Back, "back")] {
        let png = out.join(format!("{name}.png"));
        render(&kicad, &pcb, &png, side)?;
        println!("render {}", png.display());
    }
    Ok(())
}
