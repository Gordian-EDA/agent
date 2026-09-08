//! Print the outline `auto_layout` would size a board at, with and without edge assignments.
//!
//! ```text
//! cargo run -p pcb-auto --example outline -- <board.kicad_pcb>
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use kicad::KicadInstallation;
use pcb_auto::pipeline::{auto_layout, AutoOptions, Outline};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() == 2, "usage: outline <in.kicad_pcb> <scratch.kicad_pcb>");
    let kicad = KicadInstallation::detect().ok_or_else(|| anyhow::anyhow!("no KiCad 10 found"))?;
    for (label, edge_for) in [
        ("no edge_for", BTreeMap::new()),
        (
            "J2 left, J3 right, J1 top",
            BTreeMap::from([
                ("J2".to_string(), "left".to_string()),
                ("J3".to_string(), "right".to_string()),
                ("J1".to_string(), "top".to_string()),
            ]),
        ),
    ] {
        std::fs::copy(&args[0], &args[1])?;
        let r = auto_layout(
            &kicad,
            &PathBuf::from(&args[1]),
            &AutoOptions { outline: Outline::Suggest, edge_for, timeout_s: 1, ..Default::default() },
        )?;
        println!("{label}: {:.1} x {:.1} mm", r.outline_mm.0, r.outline_mm.1);
    }
    Ok(())
}
