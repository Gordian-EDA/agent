//! `region::arrange` — placing a few parts among neighbours that are already on the sheet.
//!
//! SKIPs without a KiCad installation (the adapter runs a real engine over real symbols).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use geom::{Point2, Rect};
use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;
use sch_floorplan::live;
use sch_floorplan::region::{RegionProblem, arrange};
use sch_model::geometry::body_rect;
use sch_model::item::Item;

const SHEET: &str = r#"{
  "parts": [
    {"ref":"PWR1","part":"power:VCC","pins":{"1":"VCC"}},
    {"ref":"PWR2","part":"power:GND","pins":{"1":"GND"}},
    {"ref":"R1","part":"Device:R","value":"1k","pins":{"1":"VCC","2":"N1"}},
    {"ref":"R2","part":"Device:R","value":"2k","pins":{"1":"N1","2":"N2"}},
    {"ref":"R3","part":"Device:R","value":"3k","pins":{"1":"N2","2":"N3"}},
    {"ref":"C1","part":"Device:C","value":"100n","pins":{"1":"N1","2":"GND"}},
    {"ref":"C2","part":"Device:C","value":"100n","pins":{"1":"N2","2":"GND"}},
    {"ref":"R4","part":"Device:R","value":"4k","pins":{"1":"N3","2":"N4"}},
    {"ref":"C3","part":"Device:C","value":"100n","pins":{"1":"N3","2":"GND"}},
    {"ref":"C4","part":"Device:C","value":"100n","pins":{"1":"N4","2":"GND"}}
  ]
}"#;

/// Five neighbours seated on a live sheet, three parts still to place.
const FIXED: &[&str] = &["R1", "R2", "R3", "C1", "C2"];

fn gathered(env: &KicadInstallation) -> (sch_check::Design, Vec<Item>) {
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let input: sch_check::PlacePartsInput = serde_json::from_str(SHEET).unwrap();
    let (design, diagnostics, _) = sch_check::into_design(&input, &provider, &Default::default());
    assert!(!diagnostics.has_errors(), "{:#?}", diagnostics);
    let problem = sch_floorplan::floorplan::place_problem(env, &design, None).unwrap();
    (design, problem.items)
}

/// Split the gathered parts into the movable set and neighbours seated on a live row.
fn split(items: Vec<Item>) -> (Vec<Item>, Vec<Item>) {
    let (mut fixed, movable): (Vec<Item>, Vec<Item>) = items
        .into_iter()
        .partition(|it| FIXED.contains(&it.refdes.as_str()));
    for (n, it) in fixed.iter_mut().enumerate() {
        it.at = Point2::new(100.0 + n as f64 * 25.4, 100.0);
        it.angle = 0.0;
    }
    (movable, fixed)
}

#[test]
fn arrange_places_new_parts_without_disturbing_the_neighbours() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let (design, items) = gathered(&env);
    let (movable, fixed) = split(items);
    assert_eq!(fixed.len(), 5);
    assert_eq!(movable.len(), 3);
    let held: Vec<(String, Point2, f64)> = fixed
        .iter()
        .map(|it| (it.refdes.clone(), it.at, it.angle))
        .collect();
    // Two keepouts straddling the free space just under the neighbour row.
    let obstacles = vec![
        Rect::new(90.0, 112.0, 140.0, 140.0),
        Rect::new(160.0, 112.0, 210.0, 140.0),
    ];

    let ir = floorplan::infer_ir(&env, &design);
    let out = arrange(RegionProblem::new(
        &env,
        movable.clone(),
        fixed.clone(),
        obstacles.clone(),
        ir,
    ));

    assert_eq!(out.poses.len(), 3);
    // The neighbours are returned to the caller's frame untouched.
    for (it, (refdes, at, angle)) in fixed.iter().zip(&held) {
        assert_eq!(&it.refdes, refdes);
        assert_eq!(it.at, *at);
        assert_eq!(it.angle, *angle);
    }

    // Every placed part clears every obstacle, every neighbour, and each other.
    let placed: Vec<Item> = movable
        .iter()
        .zip(&out.poses)
        .map(|(it, p)| {
            let mut it = it.clone();
            it.at = p.at;
            it.angle = p.angle;
            it
        })
        .collect();
    let neighbours: Vec<Rect> = fixed.iter().map(|it| body_rect(it, it.at)).collect();
    for (i, a) in placed.iter().enumerate() {
        let ra = body_rect(a, a.at);
        for o in &obstacles {
            assert!(!ra.overlaps(o), "{} sits on obstacle {o:?}", a.refdes);
        }
        for (n, r) in fixed.iter().zip(&neighbours) {
            assert!(
                !ra.overlaps(r),
                "{} sits on neighbour {}",
                a.refdes,
                n.refdes
            );
        }
        for b in placed.iter().skip(i + 1) {
            assert!(
                !ra.overlaps(&body_rect(b, b.at)),
                "{} sits on {}",
                a.refdes,
                b.refdes
            );
        }
    }
    // The result is in the CALLER's frame, not the engine's normalized one: the new parts
    // land beside the neighbour row rather than back at the sheet margin.
    let near = placed
        .iter()
        .all(|it| (it.at[0] - 150.0).abs() < 250.0 && (it.at[1] - 100.0).abs() < 250.0);
    assert!(
        near,
        "placed away from the live frame: {:?}",
        placed.iter().map(|it| it.at).collect::<Vec<_>>()
    );
    assert_eq!(out.result.truthfulness_breaks, 0);
}

#[test]
fn arrange_with_no_neighbours_is_the_bulk_placement_path() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let (design, items) = gathered(&env);
    let n = items.len();
    let ir = floorplan::infer_ir(&env, &design);
    let out = arrange(RegionProblem::new(
        &env,
        items,
        Vec::new(),
        Vec::new(),
        ir,
    ));
    assert_eq!(out.poses.len(), n);
    assert_eq!(out.result.truthfulness_breaks, 0);
}

fn passive_block(first: usize, last: usize, block: &str) -> sch_check::PlacePartsInput {
    let parts: Vec<serde_json::Value> = (first..=last)
        .flat_map(|index| {
            [
                serde_json::json!({
                    "ref": format!("R{index}"),
                    "part": "Device:R",
                    "value": "1k",
                    "pins": {"1": "VCC", "2": format!("FILTER_{index}")}
                }),
                serde_json::json!({
                    "ref": format!("C{index}"),
                    "part": "Device:C",
                    "value": "100n",
                    "pins": {"1": format!("FILTER_{index}"), "2": "GND"}
                }),
            ]
        })
        .collect();
    serde_json::from_value(serde_json::json!({"block": block, "parts": parts})).unwrap()
}

#[test]
fn thirty_part_named_block_uses_the_region_path_on_a_sixty_part_sheet() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let base = passive_block(1, 30, "filters-a");
    let added = passive_block(31, 45, "filters-b");

    let mut doc = live::blank_sheet().unwrap();
    let seeded = live::place_parts(&env, &mut doc, &base).unwrap();
    assert!(seeded.committed, "base refused: {:?}", seeded.mismatch);
    let before: BTreeMap<(String, u32), (sch_doc::Pose, sch_doc::Mirror)> = doc
        .symbols()
        .map(|symbol| {
            (
                (symbol.refdes().to_string(), symbol.unit),
                (symbol.at, symbol.mirror),
            )
        })
        .collect();

    let started = Instant::now();
    let report = live::place_parts(&env, &mut doc, &added).unwrap();
    let elapsed = started.elapsed();
    eprintln!("60 existing + 30-part region: {elapsed:.3?}");

    assert!(report.committed, "block refused: {:?}", report.mismatch);
    assert_eq!(report.placed.len(), 30);
    assert!(
        elapsed <= Duration::from_secs(20),
        "region took {elapsed:?}"
    );
    let after: BTreeMap<(String, u32), (sch_doc::Pose, sch_doc::Mirror)> = doc
        .symbols()
        .filter(|symbol| before.contains_key(&(symbol.refdes().to_string(), symbol.unit)))
        .map(|symbol| {
            (
                (symbol.refdes().to_string(), symbol.unit),
                (symbol.at, symbol.mirror),
            )
        })
        .collect();
    assert_eq!(after, before, "region placement moved an existing symbol");
}
