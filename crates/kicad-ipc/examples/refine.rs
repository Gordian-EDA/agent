//! Interactive-refinement round-trip on a real (engine-produced) board, via the
//! session manager: launch headless KiCAD on the board, apply an LLM-style
//! refinement (wide copper for power), and save. The caller then re-runs DRC +
//! render to confirm the IPC round-trip preserved the board.
//!
//! ```text
//! cargo run -p kicad-ipc --example refine -- /path/to/pcbnew /tmp/board.kicad_pcb
//! ```

use kicad_ipc::Session;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let pcbnew = args.next().expect("pcbnew path");
    let board = args.next().expect("board path");

    let mut session = Session::launch_headless_with(Path::new(&pcbnew), Path::new(&board))?;
    let k = session.kicad();

    let footprint_count = k.footprint_positions()?.len();
    let track_count = k.track_count()?;
    let nets = k.nets()?;
    println!(
        "opened {}: {} footprints, {} tracks, {} nets",
        board,
        footprint_count,
        track_count,
        nets.len()
    );

    // LLM-style refinement: widen the power/ground nets ("wide copper for power").
    let power: Vec<&str> = nets
        .iter()
        .map(|s| s.as_str())
        .filter(|n| {
            matches!(
                n.to_uppercase().as_str(),
                "GND" | "VCC" | "VDD" | "VIN" | "VOUT" | "3V3" | "5V" | "VBAT" | "GROUND"
            )
        })
        .collect();
    if !power.is_empty() {
        k.set_net_class("Power", 800_000, 200_000, &power)?;
        println!("refinement: Power net class @ 0.8mm / 0.2mm on {power:?}");
    } else {
        println!("refinement: (no power nets matched; round-trip only)");
    }

    k.save()?;
    println!("saved via IPC — round-trip complete");
    Ok(())
}
