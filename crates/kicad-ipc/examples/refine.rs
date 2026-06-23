//! Interactive-refinement round-trip on a real (engine-produced) board, via the
//! session manager: launch headless KiCAD on the board, apply an LLM-style
//! refinement (wide copper for power), and save. The caller then re-runs DRC +
//! render to confirm the IPC round-trip preserved the board.
//!
//! ```text
//! cargo run -p kicad-ipc --example refine -- /tmp/pcb-harness/dual-bga-bus/board.kicad_pcb
//! ```

use kicad_ipc::Session;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let board = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/pcb-harness/dual-bga-bus/board.kicad_pcb".to_string());

    let mut session = Session::launch_headless(Path::new(&board))?;
    let k = session.kicad();

    let fps = k.footprints()?;
    let tracks = k.tracks()?;
    let nets = k.nets()?;
    println!(
        "opened {}: {} footprints, {} tracks, {} nets",
        board,
        fps.len(),
        tracks.len(),
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
