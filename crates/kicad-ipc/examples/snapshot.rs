use kicad_ipc::Session;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let pcbnew = args.next().expect("pcbnew path");
    let board = args.next().expect("board path");
    let mut session = Session::launch_headless_with(Path::new(&pcbnew), Path::new(&board))?;
    let snapshot = session.kicad().board_snapshot()?;
    println!(
        "snapshot: {} layers, {} parts, {} obstacles, {} nets, {} traces, {} vias, bounds {:.3}x{:.3}",
        snapshot.problem.layer_count,
        snapshot.imported.parts.len(),
        snapshot.problem.obstacles.len(),
        snapshot.problem.connections.len(),
        snapshot.copper.traces.len(),
        snapshot.copper.vias.len(),
        snapshot.problem.bounds.width(),
        snapshot.problem.bounds.height()
    );
    Ok(())
}
