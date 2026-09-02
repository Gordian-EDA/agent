//! Time [`sch_floorplan::live::place_parts`] on a blank sheet, per engine, per
//! validation fixture — the measurement behind the deadline policy in
//! [`sch_floorplan::live::PlacementBudget`].
//!
//! `cargo run --release --example place_bench -- <out_dir> [--budget N] <fixture>...`
//! writes `<out_dir>/<fixture>.<engine>.kicad_sch` and prints one TSV row per run:
//! fixture, engine, parts, seconds, committed. Without `--budget` the search is
//! unbounded — the baseline the policy was chosen from.

use std::time::{Duration, Instant};

use kicad::KicadInstallation;
use sch_floorplan::contract::PlacementEngine;
use sch_floorplan::live::PlacementBudget;
use sch_place::place::PlacementEngineKind;

fn engines() -> Vec<(PlacementEngineKind, Box<dyn PlacementEngine>)> {
    vec![
        (
            PlacementEngineKind::Cluster,
            Box::new(cluster_place::ClusterPlace),
        ),
        (PlacementEngineKind::Anneal, Box::new(anneal_place::Anneal)),
        (
            PlacementEngineKind::Spine,
            Box::new(spine_place::SpinePlace),
        ),
    ]
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let budget = args
        .iter()
        .position(|a| a == "--budget")
        .map(|i| {
            let secs: u64 = args[i + 1].parse().expect("--budget takes seconds");
            args.drain(i..=i + 1);
            Duration::from_secs(secs)
        })
        .unwrap_or(Duration::from_secs(24 * 3600));
    let out = args.remove(0);
    std::fs::create_dir_all(&out).unwrap();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/validation");
    let env = KicadInstallation::detect().expect("no KiCAD installation");

    println!("fixture\tengine\tparts\tseconds\tcommitted");
    for name in args {
        let src = std::fs::read_to_string(format!("{dir}/{name}.place-parts.json")).unwrap();
        let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
        let parts = input.parts.len();
        for (kind, engine) in engines() {
            let mut doc = sch_floorplan::live::blank_sheet().unwrap();
            let t0 = Instant::now();
            let report = sch_floorplan::live::place_parts(
                &env,
                &mut doc,
                &input,
                engine.as_ref(),
                Some(PlacementBudget::within(budget, parts)),
            );
            let secs = t0.elapsed().as_secs_f64();
            let label = format!("{kind:?}").to_lowercase();
            match report {
                Ok(report) => {
                    println!(
                        "{name}\t{label}\t{parts}\t{secs:.1}\t{}",
                        report.committed as u8
                    );
                    doc.write(format!("{out}/{name}.{label}.kicad_sch"))
                        .unwrap();
                }
                Err(e) => println!("{name}\t{label}\t{parts}\t{secs:.1}\tERR {e}"),
            }
        }
    }
}
