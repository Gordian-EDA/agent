//! TEMPORARY calibration harness for `sizing::size_board`. Delete after use.

#[cfg(test)]
mod tests {
    use crate::sizing::{PartExtent, RoutingDemand, size_board};
    use kicad::KicadInstallation;
    use kicad_footprint::FootprintCatalog;
    use pcb_model::{PlacementHints, PlacementView, Rect};
    use std::collections::BTreeSet;
    use std::time::Instant;

    const LADDER: &[f64] = &[
        0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.15, 1.3, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0,
    ];

    #[test]
    #[ignore]
    fn calibrate() {
        let env = KicadInstallation::detect().expect("kicad");
        let catalog = FootprintCatalog::from_root(env.footprint_dir()).expect("catalog");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/pcb_circuits");
        let only: Option<String> = std::env::var("CALIB_ONLY").ok();
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        names.sort();
        println!(
            "name,parts,connectors,tht,courtyard_mm2,mean_ct_mm2,max_ct_mm2,rec_w,rec_h,rec_area,rec_ratio,min_legal_scale,min_legal_ratio,ms"
        );
        for path in names {
            let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
            if let Some(only) = &only
                && !name.contains(only.as_str())
            {
                continue;
            }
            let Ok(board) = crate::corpus::load_corpus_board(&path, &catalog) else {
                println!("{name},LOAD_ERROR");
                continue;
            };
            let started = Instant::now();
            let parts = &board.problem.parts;
            if parts.is_empty() {
                continue;
            }
            let extents: Vec<_> = parts
                .iter()
                .map(|p| PartExtent {
                    w: p.courtyard_w,
                    h: p.courtyard_h,
                    edge_seeking: crate::place::is_connector(
                        board.footprint_of(&p.reference),
                        &p.reference,
                    ),
                })
                .collect();
            let nets: BTreeSet<_> = parts
                .iter()
                .flat_map(|p| p.pads.iter().filter_map(|pad| pad.net.clone()))
                .collect();
            let sizing = size_board(
                &extents,
                1.0,
                RoutingDemand {
                    clearance: board.rules.clearance,
                    track_width: board.rules.min_trace_width,
                    layer_count: board.rules.layer_count,
                    net_count: nets.len(),
                },
            );
            let courtyard: f64 = extents.iter().map(|p| p.w * p.h).sum();
            let max_ct = extents.iter().map(|p| p.w * p.h).fold(0.0, f64::max);
            let connectors = extents.iter().filter(|p| p.edge_seeking).count();
            let tht = parts
                .iter()
                .filter(|p| p.pads.iter().any(|pad| pad.layers.len() > 1))
                .count();
            let rec_area = sizing.recommended_w * sizing.recommended_h;

            let mut min_scale = f64::NAN;
            for &scale in LADDER {
                let k = scale.sqrt();
                let (w, h) = (
                    (sizing.recommended_w * k).ceil(),
                    (sizing.recommended_h * k).ceil(),
                );
                let mut problem = PlacementView {
                    bounds: Rect::new(0.0, 0.0, w, h),
                    outline: None,
                    keepouts: vec![],
                    ..board.problem.clone()
                };
                for part in &mut problem.parts {
                    part.locked = None;
                }
                if pcb_engine::place_tuned(&problem, &PlacementHints::default()).legal {
                    min_scale = scale;
                    break;
                }
            }
            println!(
                "{name},{},{connectors},{tht},{courtyard:.1},{:.1},{max_ct:.1},{},{},{rec_area:.0},{:.2},{min_scale:.2},{:.2},{}",
                parts.len(),
                courtyard / parts.len() as f64,
                sizing.recommended_w,
                sizing.recommended_h,
                rec_area / courtyard,
                rec_area * min_scale / courtyard,
                started.elapsed().as_millis()
            );
        }
    }
}
