//! Isolate save(): open a board headless and save it with NO edits.
use kicad_ipc::Session;
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let board = std::env::args().nth(1).expect("board path");
    let mut s = Session::launch_headless(Path::new(&board))?;
    let k = s.kicad();
    println!("opened: {} footprints", k.footprints()?.len());
    println!("calling save()...");
    k.save()?;
    println!("SAVE OK");
    Ok(())
}
