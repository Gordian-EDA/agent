//! The `ClusterPlace` leaf's contract: deterministic given a seed, deadline-honouring, and
//! sound about what it returns. Runs on the golden problem corpus with a stub evaluator,
//! so `cargo test -p cluster-place` needs neither KiCAD nor the realiser.

use std::time::Duration;

use sch_model::engine::{PlacementEngine, SchematicPlaceProblem};
use sch_model::place::Deadline;
use sch_model::stub::StubEvaluator;

/// Poses are compared as plain numbers so a failure prints something readable.
type Pose = (String, u8, f64, f64, f64, bool);

fn place(problem: &mut SchematicPlaceProblem) -> Vec<Pose> {
    let (inc, ir) = (problem.inc.clone(), problem.ir.clone());
    cluster_place::ClusterPlace.place(problem, &StubEvaluator::new(&inc, &ir));
    problem
        .items
        .iter()
        .map(|it| {
            (
                it.refdes.clone(),
                it.unit,
                it.at.x,
                it.at.y,
                it.angle,
                it.mirror,
            )
        })
        .collect()
}

/// The search is expensive; the contract holds on every size, so exercise it on the
/// small end of the corpus and keep `cargo test` interactive.
fn small_problems() -> Vec<(String, SchematicPlaceProblem)> {
    sch_model::golden_problems()
        .into_iter()
        .filter(|(_, p)| p.items.len() <= 15)
        .collect()
}

#[test]
fn same_seed_places_identically() {
    let mut runs = small_problems();
    let repeat = small_problems();
    for ((name, a), (_, mut b)) in runs.iter_mut().zip(repeat) {
        assert_eq!(place(a), place(&mut b), "{name} is not deterministic");
    }
}

#[test]
fn an_expired_deadline_still_returns_every_item() {
    for (name, mut problem) in small_problems() {
        let before: Vec<_> = problem
            .items
            .iter()
            .map(|it| (it.refdes.clone(), it.unit))
            .collect();
        problem.deadline = Some(Deadline::after(Duration::ZERO));
        let poses = place(&mut problem);
        assert_eq!(
            poses
                .iter()
                .map(|(r, u, ..)| (r.clone(), *u))
                .collect::<Vec<_>>(),
            before,
            "{name} lost or reordered items under a spent deadline"
        );
        assert!(
            poses.iter().all(|(_, _, x, y, ..)| x.is_finite() && y.is_finite()),
            "{name} placed an item at a non-finite position"
        );
    }
}

/// What every engine must return whatever it searched: the caller's item list, in order,
/// with each part on the schematic grid at a cardinal angle.
#[test]
fn output_is_the_same_item_list_on_grid_at_a_cardinal_angle() {
    for (name, mut problem) in small_problems() {
        let before: Vec<_> = problem
            .items
            .iter()
            .map(|it| (it.refdes.clone(), it.unit))
            .collect();
        let poses = place(&mut problem);
        assert_eq!(
            poses
                .iter()
                .map(|(r, u, ..)| (r.clone(), *u))
                .collect::<Vec<_>>(),
            before,
            "{name}: the item list changed"
        );
        for (refdes, _, x, y, angle, _) in &poses {
            let on_grid = |v: f64| (v - geom::GRID_50_MIL.snap(v)).abs() < 1e-6;
            assert!(on_grid(*x) && on_grid(*y), "{name}: {refdes} is off-grid");
            assert!(
                [0.0, 90.0, 180.0, 270.0].contains(angle),
                "{name}: {refdes} has a non-cardinal angle {angle}"
            );
        }
    }
}
