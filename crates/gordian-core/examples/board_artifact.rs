//! Drive the full create -> place -> route -> check flow on a small board and
//! dump the saved `.kicad_pcb` to a known path so it can be rendered with
//! KiCAD's own renderer (`kicad-cli pcb export svg`). This is the "professional
//! artifact" path, as opposed to the engine's debug SVG.
//!
//! ```text
//! cargo run --release -p agent --example board_artifact -- /tmp/artifact
//! ```

use std::path::PathBuf;

use gordian_core::tools::{PcbToolCtx, run_tool};
use serde_json::json;

fn main() {
    let out_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/artifact".into());
    std::fs::create_dir_all(&out_dir).unwrap();

    // Stage the vendored fixture footprints into a .pretty dir for the index.
    let src =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../kicad-sexpr/tests/fixtures/footprints");
    let staging = PathBuf::from(&out_dir).join("fp");
    let pretty = staging.join("Fixtures.pretty");
    std::fs::create_dir_all(&pretty).unwrap();
    for name in [
        "R_0603_1608Metric.kicad_mod",
        "SOT-23.kicad_mod",
        "PinHeader_1x02_P2.54mm_Vertical.kicad_mod",
    ] {
        std::fs::copy(src.join(name), pretty.join(name)).unwrap();
    }

    let ctx = PcbToolCtx::with_footprint_dir_for_test(staging).expect("ctx");

    // A voltage divider with a 2-pin power/ground header.
    let board = json!({
        "bounds": { "min_x": 0.0, "max_x": 30.0, "min_y": 0.0, "max_y": 20.0 },
        "parts": [
            { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "VIN", "2": "MID" } },
            { "reference": "R2", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "MID", "2": "GND" } },
            { "reference": "J1", "footprint": "Fixtures:PinHeader_1x02_P2.54mm_Vertical",
              "pad_nets": { "1": "VIN", "2": "GND" } }
        ]
    });
    let r = gordian_core::tools_pcb::build_seed_board(board, &ctx).unwrap();
    println!("build_seed_board: ok={}", r["ok"]);
    let r = run_tool("place_board", json!({}), &ctx).unwrap();
    println!("place_board: legal={} hpwl={}", r["legal"], r["hpwl"]);
    let r = run_tool("route_board", json!({}), &ctx).unwrap();
    println!(
        "route_board: router={} failed={} metrics={}",
        r["router"], r["failed"], r["metrics"]
    );
    let r = run_tool("check_board", json!({}), &ctx).unwrap();
    println!("check_board: ok={} drc={}", r["ok"], r);

    // Copy the saved active board out to the stable artifact path.
    let dest = PathBuf::from(&out_dir).join("board.kicad_pcb");
    std::fs::copy(ctx.pcb_path(), &dest).unwrap();
    println!("artifact: {}", dest.display());
}
