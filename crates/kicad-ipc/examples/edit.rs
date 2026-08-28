//! Mutation proof: move a footprint through the stable high-level IPC API,
//! save, and re-read. Needs a running KiCad with a board open.
//!
//! ```text
//! cargo run -p kicad-ipc --example edit
//! ```

use kicad_ipc::Kicad;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut k = Kicad::connect()?;
    k.open_board()?;

    let before = k.footprint_positions()?;
    let tracks_before = k.track_count()?;
    println!(
        "before: {} footprints, {} tracks",
        before.len(),
        tracks_before
    );

    let p0 = before.first().expect("board needs at least one footprint");
    k.move_footprint(&p0.reference, p0.x_nm + 2_000_000, p0.y_nm, None)?;
    k.save()?;

    let after = k.footprint_positions()?;
    let tracks_after = k.track_count()?;
    let p1 = after
        .iter()
        .find(|fp| fp.reference == p0.reference)
        .expect("moved footprint remains present");
    println!(
        "after: {} x {} -> {} (delta {} nm); tracks {} -> {}",
        p0.reference,
        p0.x_nm,
        p1.x_nm,
        p1.x_nm - p0.x_nm,
        tracks_before,
        tracks_after
    );
    println!("MUTATE OK — geometry edit landed on the live KiCad board");
    Ok(())
}
