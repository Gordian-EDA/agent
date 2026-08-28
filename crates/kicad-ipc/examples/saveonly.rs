//! Isolate save(): open a board headless and save it with NO edits.
use kicad_ipc::Session;
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let pcbnew = args.next().expect("pcbnew path");
    let board = args.next().expect("board path");
    let mut s = Session::launch_headless_with(Path::new(&pcbnew), Path::new(&board))?;
    let k = s.kicad();
    println!("opened: {} footprints", k.footprint_positions()?.len());
    println!("calling save()...");
    k.save()?;
    println!("SAVE OK");
    Ok(())
}
