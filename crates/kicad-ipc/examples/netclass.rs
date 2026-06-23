//! Prove the "wide copper for power" lever: set a Power net class at 1.0mm and
//! assign the power nets. Needs a running KiCAD with a board open.
use kicad_ipc::Kicad;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut k = Kicad::connect()?;
    k.open_board()?;
    let nets = k.nets()?;
    println!("nets: {nets:?}");
    let power: Vec<&str> = nets.iter().map(|s| s.as_str())
        .filter(|n| matches!(*n, "VIN" | "VOUT" | "GND")).collect();
    k.set_net_class("Power", 1_000_000, 300_000, &power)?;
    println!("set net class Power @ 1.0mm width / 0.3mm clearance on {power:?}");
    println!("NETCLASS OK");
    Ok(())
}
