//! Route a fixture (or any `RouteProblem` JSON) through `route_auto` and
//! rasterize the board to a PNG you can open — the no-credentials way to *see*
//! the autorouter work. This is the "render + eyeball" loop the engine was built
//! around, packaged as a one-command convenience.
//!
//! ```text
//! cargo run -p agent --example eyeball                 # all bundled fixtures
//! cargo run -p agent --example eyeball -- quad         # one named fixture
//! cargo run -p agent --example eyeball -- path/to.json # an arbitrary problem
//! ```
//!
//! Legend: red = top-layer copper, blue = bottom layer, magenta rings = vias,
//! orange crosses = nets the router could not complete. PNGs land in
//! `$TMPDIR/autopcb-eyeball/`.

use std::path::{Path, PathBuf};

use negotiated_mesh::pipeline::route_auto;
use pcb_model::RouteProblem;
use pcb_svg::svg::render_svg;

/// The fixtures shipped with `pcb-engine`, rendered when no argument is given.
const DEFAULT_FIXTURES: &[&str] = &["led-r", "quad", "congested-relief"];

/// Pixels-per-mm multiplier over the SVG's native 10 px/mm — 6× gives a crisp
/// ~60 px/mm raster without re-laying-out the vector art.
const SCALE: f32 = 6.0;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../pcb-engine/fixtures")
}

/// Resolve a CLI argument to a problem-JSON path: a bare name like `quad` maps to
/// the bundled `fixtures/quad.json`; anything containing a `/` or ending in
/// `.json` is used verbatim.
fn resolve(arg: &str) -> PathBuf {
    if arg.contains('/') || arg.ends_with(".json") {
        PathBuf::from(arg)
    } else {
        fixtures_dir().join(format!("{arg}.json"))
    }
}

/// Rasterize an SVG string to PNG bytes at [`SCALE`]× on a white background
/// (the same path `gordian_kicad::render::svg_to_png` uses, inlined so the example has
/// no internal-API dependency).
fn rasterize(svg: &str) -> Vec<u8> {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(svg, &opt).expect("parse svg");
    let size = tree.size();
    let w = ((size.width() * SCALE).ceil() as u32).max(1);
    let h = ((size.height() * SCALE).ceil() as u32).max(1);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).expect("alloc pixmap");
    pixmap.fill(resvg::tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(SCALE, SCALE),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().expect("encode png")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let targets: Vec<String> = if args.is_empty() {
        DEFAULT_FIXTURES.iter().map(|s| s.to_string()).collect()
    } else {
        args
    };

    let out_dir = std::env::temp_dir().join("autopcb-eyeball");
    std::fs::create_dir_all(&out_dir).expect("create output dir");

    for target in &targets {
        let path = resolve(target);
        let json = match std::fs::read_to_string(&path) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("skip {target}: cannot read {} ({e})", path.display());
                continue;
            }
        };
        let problem: RouteProblem = match serde_json::from_str(&json) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skip {target}: not a RouteProblem ({e})");
                continue;
            }
        };

        let result = route_auto(&problem);
        let svg = render_svg(&problem, &result.solution, &result.failed);
        let png = rasterize(&svg);

        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("board");
        let out = out_dir.join(format!("{stem}.png"));
        std::fs::write(&out, &png).expect("write png");

        println!(
            "{stem}: router={:?} failed={} traces={} vias={} -> {}",
            result.router,
            result.failed.len(),
            result.solution.traces.len(),
            result.solution.vias.len(),
            out.display(),
        );
        for f in &result.failed {
            println!("  unrouted: {} — {}", f.connection, f.reason);
        }
    }
}
