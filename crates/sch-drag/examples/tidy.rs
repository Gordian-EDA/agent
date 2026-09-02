//! The experiment harness.
//!
//! ```text
//! tidy measure  FILE                 — the evaluator's reading of a sheet
//! tidy scramble SEED IN OUT          — a truthful but badly placed version of a sheet
//! tidy tidy     SECONDS IN OUT       — the search
//! ```
//!
//! Every mode prints one JSON object, so a driver can render, export netlists
//! and run the critic around it.

use geom::Point2;
use sch_doc::SchDoc;
use sch_drag::{Metrics, Placement, Sheet, TidyOptions, Weights, drag, measure, tidy};

/// Every part the search may move, by UUID — a multi-unit symbol places each
/// unit separately, and a reference designator cannot tell them apart.
fn movable(doc: &SchDoc) -> Vec<String> {
    doc.symbols()
        .filter(|s| !s.refdes().starts_with('#') && !s.refdes().is_empty())
        .map(|s| s.uuid.clone())
        .collect()
}

fn json(m: &Metrics, w: &Weights) -> String {
    format!(
        "{{\"wire_length\":{:.1},\"bends\":{},\"crossings\":{},\"through_bodies\":{},\
\"body_overlaps\":{},\"dangling_ends\":{},\"stranded_labels\":{},\"junction_faults\":{},\"text_collisions\":{},\
\"crowding\":{},\"labels\":{},\"net_spread\":{:.1},\"misalignment\":{:.1},\"faults\":{},\
\"score\":{:.1}}}",
        m.wire_length,
        m.bends,
        m.crossings,
        m.through_bodies,
        m.body_overlaps,
        m.dangling_ends,
        m.stranded_labels,
        m.junction_faults,
        m.text_collisions,
        m.crowding,
        m.labels,
        m.net_spread,
        m.misalignment,
        m.faults(),
        m.score(w)
    )
}

/// Push every movable part somewhere else on the page, keeping the netlist
/// intact, so the human original stays the answer key.
fn scramble(doc: &mut SchDoc, seed: u64) -> (usize, usize) {
    let mut rng = fastrand::Rng::with_seed(seed);
    let [width, height] = doc.page().unwrap_or([297.0, 210.0]);
    let refs = movable(doc);
    let (mut moved, mut refused) = (0, 0);
    for id in &refs {
        let Some(here) = Placement::of(doc, id) else {
            continue;
        };
        let mut done = false;
        for _ in 0..8 {
            let mut grid = |span: f64| ((rng.f64() * (span - 40.0) + 20.0) / 2.54).round() * 2.54;
            let (x, y) = (grid(width), grid(height));
            let to = Placement {
                at: Point2::new(x, y),
                ..here
            };
            if drag(doc, id, to).is_ok() {
                done = true;
                break;
            }
        }
        if done {
            moved += 1;
        } else {
            refused += 1;
        }
    }
    (moved, refused)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let weights = Weights::default();
    match args.first().map(String::as_str) {
        Some("measure") => {
            let doc = SchDoc::read(&args[1]).expect("parse");
            println!(
                "{{\"metrics\":{}}}",
                json(&measure(&Sheet::of(&doc)), &weights)
            );
        }
        Some("scramble") => {
            let seed: u64 = args[1].parse().expect("seed");
            let mut doc = SchDoc::read(&args[2]).expect("parse");
            let before = measure(&Sheet::of(&doc));
            let (moved, refused) = scramble(&mut doc, seed);
            doc.write(&args[3]).expect("write");
            println!(
                "{{\"moved\":{moved},\"refused\":{refused},\"human\":{},\"scrambled\":{}}}",
                json(&before, &weights),
                json(&measure(&Sheet::of(&doc)), &weights)
            );
        }
        Some("tidy") => {
            let seconds: f64 = args[1].parse().expect("seconds");
            let mut doc = SchDoc::read(&args[2]).expect("parse");
            let refs = movable(&doc);
            let options = TidyOptions {
                seconds,
                ..Default::default()
            };
            let report = tidy(&mut doc, &refs, &options);
            doc.write(&args[3]).expect("write");
            println!(
                "{{\"before\":{},\"after\":{},\"proposed\":{},\"accepted\":{},\
\"improved\":{},\"refused\":{},\"seconds\":{:.1}}}",
                json(&report.before, &weights),
                json(&report.after, &weights),
                report.proposed,
                report.accepted,
                report.improved,
                report.refused,
                report.seconds
            );
        }
        _ => eprintln!("usage: tidy measure|scramble|tidy ..."),
    }
}
