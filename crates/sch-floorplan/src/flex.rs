//! The typesetter as a placement leaf.
//!
//! [`sch_flex`] is pure geometry over items and trees; this is the adapter that lets the
//! existing region/realise path drive it, so a tree-typeset block reaches the sheet
//! through exactly the machinery a searched one did.

use sch_model::engine::{
    CandidateEvaluator, PlacementEngine, PlacementOutput, SchematicPlaceProblem,
};
use sch_model::place::PlaceResult;

/// Draw every block from its authored tree.
pub struct FlexPlace;

impl PlacementEngine for FlexPlace {
    fn name(&self) -> &'static str {
        "flex"
    }

    fn place(
        &self,
        problem: &mut SchematicPlaceProblem,
        eval: &dyn CandidateEvaluator,
    ) -> PlacementOutput {
        let ir = problem.ir.clone();
        sch_flex::typeset(&mut problem.items, &ir.trees);
        PlacementOutput {
            result: PlaceResult {
                engine: self.name().to_owned(),
                truthfulness_breaks: eval.truthfulness_breaks(&problem.items),
                warnings: eval.warnings(&problem.items),
                crossings: eval.crossings(&problem.items),
                cost: 0.0,
            },
            ir,
        }
    }
}
