//! `region::arrange` — placing a few parts among neighbours that are already on the sheet.
//!
//! SKIPs without a KiCad installation (the adapter runs a real engine over real symbols).

use geom::{Point2, Rect};
use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::contract::SchematicPlaceProblem;
use sch_floorplan::engine_support::item_rect;
use sch_floorplan::floorplan;
use sch_floorplan::region::{RegionProblem, arrange};
use sch_place::item::Item;

const SHEET: &str = r#"
version: 1
name: region-sheet
blocks:
  power:
    components:
      PWR1: {part: power:VCC, pins: {1: VCC}}
      PWR2: {part: power:GND, pins: {1: GND}}
  existing:
    components:
      R1: {part: Device:R, value: 1k, between: [VCC, N1]}
      R2: {part: Device:R, value: 2k, between: [N1, N2]}
      R3: {part: Device:R, value: 3k, between: [N2, N3]}
      C1: {part: Device:C, value: 100n, between: [N1, GND]}
      C2: {part: Device:C, value: 100n, between: [N2, GND]}
  added:
    components:
      R4: {part: Device:R, value: 4k, between: [N3, N4]}
      C3: {part: Device:C, value: 100n, between: [N3, GND]}
      C4: {part: Device:C, value: 100n, between: [N4, GND]}
"#;

/// Five neighbours seated on a live sheet, three parts still to place.
const FIXED: &[&str] = &["R1", "R2", "R3", "C1", "C2"];

fn gathered(env: &KicadInstallation) -> (circuit_lang::model::Design, Vec<Item>) {
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let compiled = circuit_lang::compile(SHEET, &provider);
    assert!(
        !compiled.diagnostics.has_errors(),
        "{:#?}",
        compiled.diagnostics
    );
    let design = compiled.design.unwrap();
    let problem = SchematicPlaceProblem::from_design(env, &design).unwrap();
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
        &design,
        movable.clone(),
        fixed.clone(),
        obstacles.clone(),
        ir,
        &spine_place::SpinePlace,
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
    let neighbours: Vec<Rect> = fixed.iter().map(|it| item_rect(it, it.at)).collect();
    for (i, a) in placed.iter().enumerate() {
        let ra = item_rect(a, a.at);
        for o in &obstacles {
            assert!(!ra.overlaps(o), "{} sits on obstacle {o:?}", a.refdes);
        }
        for (n, r) in fixed.iter().zip(&neighbours) {
            assert!(!ra.overlaps(r), "{} sits on neighbour {}", a.refdes, n.refdes);
        }
        for b in placed.iter().skip(i + 1) {
            assert!(
                !ra.overlaps(&item_rect(b, b.at)),
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
        &design,
        items,
        Vec::new(),
        Vec::new(),
        ir,
        &spine_place::SpinePlace,
    ));
    assert_eq!(out.poses.len(), n);
    assert_eq!(out.result.truthfulness_breaks, 0);
}
