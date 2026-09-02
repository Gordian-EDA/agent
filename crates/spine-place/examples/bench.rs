//! Run the `SpinePlace` placement leaf standalone over the golden problem corpus.
//!
//! `cargo run -p spine-place --example bench [--release] [name-substring]` needs no
//! agent, no KiCAD and no network: every problem is a frozen `.problem.json` and the
//! oracle is `sch_model::stub::StubEvaluator`. Prints one TSV row per fixture —
//! items, milliseconds, and the leaf's own end-state metrics.

use std::time::Instant;

use sch_model::engine::{CandidateEvaluator, PlacementEngine};
use sch_model::stub::StubEvaluator;

fn main() {
    let filter = std::env::args().nth(1);
    println!("fixture\titems\tms\toverlaps\thpwl\tspread\trelation");
    for (name, mut problem) in sch_model::golden_problems() {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        let (inc, ir) = (problem.inc.clone(), problem.ir.clone());
        let eval = StubEvaluator::new(&inc, &ir);
        let items = problem.items.len();
        let t0 = Instant::now();
        spine_place::SpinePlace.place(&mut problem, &eval);
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        let m = eval.measure(&problem.items);
        println!(
            "{name}\t{items}\t{ms:.1}\t{}\t{:.0}\t{:.0}\t{}",
            m.overlaps, m.length, m.spread, m.relation
        );
    }
}
