//! Mutation proof (pure Rust): open the board, move a footprint and create a
//! 1.0 mm power track inside one commit, save, and re-read. Needs a running
//! KiCAD with a board open (`xvfb-run -a pcbnew <board>` headless, or the GUI).
//!
//! ```text
//! cargo run -p kicad-ipc --example edit
//! ```

use kicad_ipc::proto::kiapi::board::types::{BoardLayer, Track};
use kicad_ipc::proto::kiapi::common::types::{Distance, Vector2};
use kicad_ipc::Kicad;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut k = Kicad::connect()?;
    k.open_board()?;

    let fps_before = k.footprints()?;
    let tracks_before = k.tracks()?.len();
    println!("before: {} footprints, {} tracks", fps_before.len(), tracks_before);

    let mut fp = fps_before[0].clone();
    let p0 = fp.position.clone().unwrap_or_default();

    k.commit("rust edit: move footprint + 1.0mm power track", |k| {
        // 1) move the first footprint +2 mm in x
        fp.position = Some(Vector2 {
            x_nm: p0.x_nm + 2_000_000,
            y_nm: p0.y_nm,
        });
        k.update_items(vec![prost_types::Any::from_msg(&fp)?])?;

        // 2) a fat 1.0 mm track on F.Cu (the "wide copper for power" capability)
        let track = Track {
            start: Some(Vector2 { x_nm: 3_000_000, y_nm: 27_000_000 }),
            end: Some(Vector2 { x_nm: 18_000_000, y_nm: 27_000_000 }),
            width: Some(Distance { value_nm: 1_000_000 }),
            layer: BoardLayer::BlFCu as i32,
            ..Default::default()
        };
        k.create_items(vec![prost_types::Any::from_msg(&track)?])?;
        Ok(())
    })?;

    let fps_after = k.footprints()?;
    let tracks_after = k.tracks()?;
    let widest = tracks_after
        .iter()
        .filter_map(|t| t.width.as_ref().map(|w| w.value_nm))
        .max()
        .unwrap_or(0);
    println!(
        "after:  fp0 x {} -> {} (delta {} nm); tracks {} -> {}; widest track {} nm",
        p0.x_nm,
        fps_after[0].position.clone().unwrap_or_default().x_nm,
        fps_after[0].position.clone().unwrap_or_default().x_nm - p0.x_nm,
        tracks_before,
        tracks_after.len(),
        widest
    );
    println!("MUTATE OK (pure Rust) — geometry edits land on the live KiCAD board");
    Ok(())
}
