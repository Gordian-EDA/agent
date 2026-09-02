//! Time the primitives on a real sheet: `cargo run --release --example smoke -- FILE`.

use std::time::Instant;

use sch_doc::SchDoc;
use sch_drag::{Placement, Sheet, drag, measure};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: smoke FILE.kicad_sch");
    let mut doc = SchDoc::read(&path).expect("parse");

    let t = Instant::now();
    let sheet = Sheet::of(&doc);
    let build = t.elapsed();
    let t = Instant::now();
    let metrics = measure(&sheet);
    println!(
        "{} symbols, {} wires — sheet {:?}, measure {:?}",
        sheet.bodies.len(),
        sheet.wires.len(),
        build,
        t.elapsed()
    );
    println!("{metrics:#?}");

    let refs: Vec<String> = doc
        .symbols()
        .filter(|s| !s.refdes().starts_with('#'))
        .map(|s| s.refdes().to_string())
        .collect();
    let mut ok = 0;
    let mut refused = 0;
    let mut labels = 0;
    let t = Instant::now();
    for id in refs.iter().take(40) {
        let Some(here) = Placement::of(&doc, id) else {
            continue;
        };
        let to = Placement {
            at: geom::Point2::new(here.at.x + 5.08, here.at.y),
            ..here
        };
        match drag(&mut doc, id, to) {
            Ok(r) => {
                ok += 1;
                labels += r.labels_added;
            }
            Err(e) => {
                refused += 1;
                if refused <= 6 {
                    println!("  {id}: {e}");
                }
            }
        }
    }
    println!(
        "{ok} drags ok, {refused} refused, {labels} labels, {:?} total",
        t.elapsed()
    );
    println!("{:#?}", measure(&Sheet::of(&doc)));
}
