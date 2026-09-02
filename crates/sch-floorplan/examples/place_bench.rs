//! Time [`sch_floorplan::live::place_parts`] on a blank sheet, per engine, per
//! validation fixture — the measurement behind the deadline policy in
//! [`sch_floorplan::live::PlacementBudget`].
//!
//! `cargo run --release --example place_bench -- <out_dir> [--budget N]
//! [--engines spine,cluster] <fixture[@parts]>...`
//! writes `<out_dir>/<fixture>.<engine>.kicad_sch` and prints one TSV row per run:
//! fixture, engine, parts, seconds, committed. Without `--budget` the search is
//! unbounded. The optional `@parts` truncates a campaign payload for exact size runs.

use std::time::{Duration, Instant};

use kicad::KicadInstallation;
use sch_floorplan::live::PlacementBudget;
use sch_model::engine::PlacementEngine;
use sch_model::place::PlacementEngineKind;

fn engines(selected: &[String]) -> Vec<(PlacementEngineKind, Box<dyn PlacementEngine>)> {
    let all: Vec<(PlacementEngineKind, Box<dyn PlacementEngine>)> = vec![
        (
            PlacementEngineKind::Cluster,
            Box::new(cluster_place::ClusterPlace),
        ),
        (PlacementEngineKind::Anneal, Box::new(anneal_place::Anneal)),
        (
            PlacementEngineKind::Spine,
            Box::new(spine_place::SpinePlace),
        ),
    ];
    all.into_iter()
        .filter(|(kind, _)| {
            selected.is_empty() || selected.iter().any(|name| *name == engine_label(*kind))
        })
        .collect()
}

fn engine_label(kind: PlacementEngineKind) -> String {
    format!("{kind:?}").to_lowercase()
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
    let selected = args
        .iter()
        .position(|a| a == "--engines")
        .map(|i| {
            let names: Vec<String> = args[i + 1].split(',').map(str::to_string).collect();
            args.drain(i..=i + 1);
            names
        })
        .unwrap_or_default();
    let out = args.remove(0);
    std::fs::create_dir_all(&out).unwrap();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/validation");
    let env = KicadInstallation::detect().expect("no KiCAD installation");

    println!("fixture\tengine\tparts\tseconds\tcommitted");
    for spec in args {
        let (name, requested_parts) =
            spec.split_once('@')
                .map_or((spec.as_str(), None), |(name, parts)| {
                    (
                        name,
                        Some(parts.parse::<usize>().expect("@parts is a number")),
                    )
                });
        let src = std::fs::read_to_string(format!("{dir}/{name}.place-parts.json")).unwrap();
        let mut input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
        if let Some(parts) = requested_parts {
            assert!(
                parts <= input.parts.len(),
                "{name} only has {} parts",
                input.parts.len()
            );
            input.parts.truncate(parts);
        }
        let parts = input.parts.len();
        let fixture =
            requested_parts.map_or_else(|| name.to_string(), |parts| format!("{name}@{parts}"));
        for (kind, engine) in engines(&selected) {
            let mut doc = sch_floorplan::live::blank_sheet().unwrap();
            let t0 = Instant::now();
            let report = sch_floorplan::live::place_parts(
                &env,
                &mut doc,
                &input,
                engine,
                Some(PlacementBudget::within(budget, parts)),
            );
            let secs = t0.elapsed().as_secs_f64();
            let label = engine_label(kind);
            match report {
                Ok(report) => {
                    let why = if report.committed {
                        String::new()
                    } else {
                        let m = &report.mismatch;
                        let shorted: Vec<String> =
                            m.shorted.iter().map(|(a, b)| format!("{a}+{b}")).collect();
                        format!(
                            "\tshorted={} scattered={}\n{}",
                            shorted.join(","),
                            m.scattered.join(","),
                            report
                                .warnings
                                .iter()
                                .map(|w| format!("\t\t{w}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )
                    };
                    println!(
                        "{fixture}\t{label}\t{parts}\t{secs:.1}\t{}{why}",
                        report.committed as u8
                    );
                    doc.write(format!(
                        "{out}/{}.{label}.kicad_sch",
                        fixture.replace('@', "-")
                    ))
                    .unwrap();
                }
                Err(e) => println!("{fixture}\t{label}\t{parts}\t{secs:.1}\tERR {e}"),
            }
        }
    }
}
