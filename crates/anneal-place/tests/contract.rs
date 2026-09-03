//! The `Anneal` leaf's contract: deterministic given a seed, deadline-honouring, and
//! sound about what it returns. Runs on the golden problem corpus with a stub evaluator,
//! so `cargo test -p anneal-place` needs neither KiCAD nor the realiser.

use std::time::Duration;

use geom::Rect;
use sch_model::engine::{
    CandidateEvaluator, CohesionPlan, PlacementEngine, RawMetrics, SchematicPlaceProblem,
};
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};
use sch_model::place::{Crossings, Deadline};
use sch_model::stub::StubEvaluator;

/// Poses are compared as plain numbers so a failure prints something readable.
type Pose = (String, u8, f64, f64, f64, bool);

fn place(problem: &mut SchematicPlaceProblem) -> Vec<Pose> {
    let (inc, ir) = (problem.inc.clone(), problem.ir.clone());
    anneal_place::Anneal.place(problem, &StubEvaluator::new(&inc, &ir));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PoseBits {
    x: u64,
    y: u64,
    angle: u64,
    mirror: bool,
}

impl From<&Item> for PoseBits {
    fn from(item: &Item) -> Self {
        Self {
            x: item.at.x.to_bits(),
            y: item.at.y.to_bits(),
            angle: item.angle.to_bits(),
            mirror: item.mirror,
        }
    }
}

struct FrozenPoseGuard<'a> {
    inc: &'a Incidence,
    ir: &'a LayoutIr,
    frozen: Vec<(usize, PoseBits)>,
}

impl FrozenPoseGuard<'_> {
    fn check(&self, items: &[Item]) {
        for &(index, expected) in &self.frozen {
            assert_eq!(
                PoseBits::from(&items[index]),
                expected,
                "frozen region neighbor {} was selected as an anchor or move member",
                items[index].refdes
            );
        }
    }

    fn stub(&self) -> StubEvaluator<'_> {
        StubEvaluator::new(self.inc, self.ir)
    }
}

impl CandidateEvaluator for FrozenPoseGuard<'_> {
    fn measure(&self, items: &[Item]) -> RawMetrics {
        self.check(items);
        self.stub().measure(items)
    }

    fn warnings(&self, items: &[Item]) -> usize {
        self.check(items);
        self.stub().warnings(items)
    }

    fn crossings(&self, items: &[Item]) -> Crossings {
        self.check(items);
        self.stub().crossings(items)
    }

    fn truthfulness_breaks(&self, items: &[Item]) -> usize {
        self.check(items);
        self.stub().truthfulness_breaks(items)
    }

    fn rendered(&self, items: &[Item]) -> Option<(usize, Rect)> {
        self.check(items);
        self.stub().rendered(items)
    }

    fn shipped(&self, items: &[Item]) -> Option<(Crossings, usize, Rect)> {
        self.check(items);
        self.stub().shipped(items)
    }

    fn warning_messages(&self, items: &[Item]) -> Vec<String> {
        self.check(items);
        self.stub().warning_messages(items)
    }

    fn cohesion_plans(&self, items: &[Item]) -> Vec<CohesionPlan> {
        self.check(items);
        self.stub().cohesion_plans(items)
    }

    fn with_ir<'a>(&'a self, ir: &'a LayoutIr) -> Box<dyn CandidateEvaluator + 'a> {
        Box::new(FrozenPoseGuard {
            inc: self.inc,
            ir,
            frozen: self.frozen.clone(),
        })
    }
}

#[test]
fn region_anneal_never_selects_frozen_neighbors_as_anchors() {
    let (_, mut problem) = sch_model::golden_problems()
        .into_iter()
        .find(|(name, _)| name == "uart-level-translator")
        .expect("UART fixture exists");
    problem.ir.mirror.clear();
    let anchors: Vec<usize> = problem
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| (item.geom.pins.len() >= 3).then_some(i))
        .collect();
    assert_eq!(anchors.len(), 2, "fixture must exercise two frozen anchors");
    for (n, &index) in anchors.iter().enumerate() {
        let item = &mut problem.items[index];
        item.at = [101.6 + n as f64 * 50.8, 50.8 + n as f64 * 25.4].into();
        item.angle = if n == 0 { 90.0 } else { 270.0 };
        item.mirror = n == 1;
        item.frozen = true;
        item.preseeded = true;
    }
    let frozen: Vec<_> = anchors
        .iter()
        .map(|&index| (index, PoseBits::from(&problem.items[index])))
        .collect();
    let (inc, ir) = (problem.inc.clone(), problem.ir.clone());
    let eval = FrozenPoseGuard {
        inc: &inc,
        ir: &ir,
        frozen: frozen.clone(),
    };

    anneal_place::Anneal.place(&mut problem, &eval);

    for (index, expected) in frozen {
        assert_eq!(PoseBits::from(&problem.items[index]), expected);
    }
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
