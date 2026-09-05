//! Closes the visual-review loop when it stops paying, and refuses the reviews
//! that ask the same question twice.
//!
//! Two suites measured the loop itself, and the operator it runs on — "review,
//! then re-arrange the block the defects name" — is net-negative except on sheets
//! that are still failing. By the first review's mean, over 25 run-v7 trajectories:
//!
//! | first review | runs | best after it | last review | improved |
//! |--------------|------|---------------|-------------|----------|
//! | under 5.5    |  10  | +2.07         | +0.93       | 10/10    |
//! | 5.5 to 7     |  10  | −0.99         | −1.06       | 0/10     |
//! | 7 and over   |   5  | −0.69         | −1.83       | 0/5      |
//!
//! So the first review is usually the best score a sheet ever has, and the loop's
//! job is to end before the second edit undoes it.
//!
//! Three rules, all in-band like the thrash guard:
//!
//! - **stop rule** — the FIRST review that does not beat the best mean of the turn
//!   by [`IMPROVEMENT`] closes the loop: the review result says so, and `arrange` /
//!   `move_symbols` / `rewire` are refused for the rest of the subturn. One bad
//!   edit is the most a sheet may lose. A sheet still under [`CONVERGED`] has not
//!   converged, it is failing, so flat reads there do not close it — that is the
//!   band where re-arranging still pays, 10 times out of 10.
//! - **high-first-review rule** — a first review already at [`CAREFUL`] or better
//!   is told that one targeted fix is allowed and a whole-block re-arrange is not:
//!   every such run that re-arranged lost between 0.6 and 4.3 points.
//! - **no-op rule** — a review with no successful layout edit since the last one
//!   grades an unchanged picture, so it is refused with the standing mean.

use serde_json::{Value, json};

/// How much a review must beat the turn's best mean by to count as progress.
/// The grader reads ±0.3 on an unchanged sheet at seven samples, so anything
/// under that is noise, not a better drawing.
const IMPROVEMENT: f64 = 0.3;

/// The mean at or above which a first review is close enough to right that a
/// whole-block re-arrange can only cost it.
const CAREFUL: f64 = 7.0;

/// The mean below which flat reviews mean the composition is wrong rather than
/// finished, so the loop stays open.
const CONVERGED: f64 = 5.0;

/// What a first review this good is told, before it re-arranges a block that is
/// already reading well and loses the score it came in with.
fn careful_notice(mean: f64) -> String {
    format!(
        "This sheet already reads {mean} on its first review. Every run in the last suite that \
         re-arranged a block scoring {CAREFUL} or better ended lower, by 0.6 to 4.3 points. Make \
         at most ONE targeted fix to the single worst defect — move the one part it names, widen \
         the one gap — and do not re-arrange a whole block. Finishing here is a good outcome; a \
         review that is worse than the previous one means the last edit hurt, so stop and finish."
    )
}

/// The layout edits that only make sense while the review loop is open.
fn is_layout_edit(tool: &str) -> bool {
    matches!(tool, "arrange" | "move_symbols" | "rewire")
}

/// One subturn's review trajectory.
pub(crate) struct ReviewProgress {
    best: Option<f64>,
    last: Option<f64>,
    stopped: bool,
    edited_since_review: bool,
    benched: bool,
}

impl Default for ReviewProgress {
    fn default() -> Self {
        Self {
            best: None,
            last: None,
            stopped: false,
            // The first review of a subturn always has something new to look at.
            edited_since_review: true,
            benched: false,
        }
    }
}

impl ReviewProgress {
    /// A successful layout edit: the next review has a new picture to grade.
    pub(crate) fn note_edit(&mut self) {
        self.edited_since_review = true;
    }

    /// Parts wired by name but not yet drawn, from a check or a mutator report.
    /// `arrange` is the only tool that empties the bench, so a closed loop must
    /// not refuse it while parts are waiting there.
    pub(crate) fn note_bench(&mut self, benched: u64) {
        self.benched = benched > 0;
    }

    /// Whether the sheet has changed since the critic last saw it — so a turn
    /// about to finish would be reporting a mean the drawing no longer earns.
    /// Ten of 26 runs in one suite edited after the review they reported.
    /// A closed loop is exempt: it was told to finish.
    pub(crate) fn needs_review(&self) -> bool {
        self.edited_since_review && !self.stopped
    }

    /// Read one review verdict. Returns the notice to hand back with it when this
    /// is the review that closed the loop.
    pub(crate) fn observe(&mut self, verdict: &Value) -> Option<String> {
        let mean = verdict
            .get("mean")
            .or_else(|| verdict.get("score"))
            .and_then(Value::as_f64)?;
        self.edited_since_review = false;
        self.last = Some(mean);
        let first = self.best.is_none();
        if self.best.is_none_or(|best| mean >= best + IMPROVEMENT) {
            self.best = Some(self.best.map_or(mean, |best| best.max(mean)));
            return (first && mean >= CAREFUL).then(|| careful_notice(mean));
        }
        self.best = Some(self.best.map_or(mean, |best| best.max(mean)));
        let best = self.best.unwrap_or(mean);
        if self.stopped || best < CONVERGED {
            return None;
        }
        self.stopped = true;
        Some(format!(
            "This review did not beat the best mean of {best} this turn (it read {mean}), which \
             means the last edit hurt: do not arrange again, finish. Report {best} as the final \
             mean and do not edit the sheet after the review you finish on. Further arrange, \
             move_symbols and rewire calls will be refused."
        ))
    }

    /// The in-band refusal for a call the review rules do not allow, if any.
    pub(crate) fn refusal(&self, tool: &str) -> Option<Value> {
        if self.stopped && !self.benched && is_layout_edit(tool) {
            return Some(json!({
                "error": "review loop closed",
                "note": format!(
                    "The sheet stopped improving at a best mean of {} and the review loop is \
                     closed for this turn, so `{tool}` was NOT applied. Finish with the sheet as \
                     it stands and report that mean.",
                    self.best.map_or(String::new(), |best| best.to_string()),
                ),
            }));
        }
        if tool == "review_schematic" && !self.edited_since_review {
            return Some(json!({
                "error": "nothing changed since the last review",
                "note": format!(
                    "Its result still stands: mean {}. A review of an unchanged sheet costs seven \
                     vision passes and answers the same question. Change the layout with `arrange` \
                     first, or finish.",
                    self.last.map_or(String::new(), |last| last.to_string()),
                ),
            }));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(mean: f64) -> Value {
        json!({"mean": mean, "score": mean.round()})
    }

    /// `555#0` read 6.86, then 6.29, then 6.00 and finished at 6.30: the second
    /// review is where it should have stopped, one edit deep, not three.
    #[test]
    fn the_first_review_that_fails_to_improve_closes_the_loop() {
        let mut progress = ReviewProgress::default();
        assert!(progress.observe(&review(6.86)).is_none(), "the first read");
        progress.note_edit();
        let notice = progress.observe(&review(6.29)).expect("the second closes it");
        assert!(notice.contains("the last edit hurt"), "{notice}");
        assert!(notice.contains("6.86"), "{notice}");
        assert!(progress.refusal("arrange").is_some());
        assert!(progress.refusal("move_symbols").is_some());
        assert!(progress.refusal("rewire").is_some());
        assert!(progress.refusal("place_parts").is_none(), "new work is free");
    }

    /// A real gain keeps the loop open however many rounds it takes.
    #[test]
    fn a_gaining_loop_stays_open() {
        let mut progress = ReviewProgress::default();
        for mean in [5.0, 5.4, 6.0, 6.5] {
            assert!(progress.observe(&review(mean)).is_none(), "{mean}");
            progress.note_edit();
        }
        assert!(progress.refusal("arrange").is_none());
        assert!(progress.observe(&review(6.6)).is_some(), "half the noise floor");
    }

    /// A first review this good is told to stop re-arranging before it does.
    #[test]
    fn a_high_first_review_is_told_to_fix_one_thing_at_most() {
        let mut progress = ReviewProgress::default();
        let notice = progress.observe(&review(7.43)).expect("careful notice");
        assert!(notice.contains("ONE targeted fix"), "{notice}");
        assert!(progress.refusal("arrange").is_none(), "one fix is allowed");
        progress.note_edit();
        assert!(progress.observe(&review(7.57)).is_some(), "and then it closes");
    }

    /// ...which a first review below that bar is not: there the loop pays.
    #[test]
    fn a_middling_first_review_gets_no_careful_notice() {
        let mut progress = ReviewProgress::default();
        assert!(progress.observe(&review(6.86)).is_none());
    }

    /// `rp2040#0` read 4.00 three times and then found 6.71: a sheet that bad is
    /// mis-composed, not converged, so the loop stays open for it.
    #[test]
    fn a_failing_sheet_is_not_treated_as_converged() {
        let mut progress = ReviewProgress::default();
        for mean in [4.0, 4.0, 4.0, 4.0] {
            assert!(progress.observe(&review(mean)).is_none(), "{mean}");
            progress.note_edit();
        }
        assert!(progress.refusal("arrange").is_none());
        assert!(progress.observe(&review(6.71)).is_none(), "a real gain");
        progress.note_edit();
        assert!(progress.observe(&review(6.5)).is_some(), "now it may close");
    }

    /// Only `arrange` empties the bench, so a closed loop still allows it while
    /// parts are wired by name and undrawn.
    #[test]
    fn benched_parts_keep_the_layout_tools_open() {
        let mut progress = ReviewProgress::default();
        for mean in [6.0, 6.0] {
            progress.observe(&review(mean));
            progress.note_edit();
        }
        assert!(progress.refusal("arrange").is_some());
        progress.note_bench(2);
        assert!(progress.refusal("arrange").is_none());
        progress.note_bench(0);
        assert!(progress.refusal("arrange").is_some());
    }

    #[test]
    fn a_review_of_an_unchanged_sheet_is_refused_with_the_standing_mean() {
        let mut progress = ReviewProgress::default();
        assert!(progress.refusal("review_schematic").is_none(), "the first");
        progress.observe(&review(6.14));
        let refusal = progress.refusal("review_schematic").expect("no-op review");
        assert_eq!(refusal["error"], "nothing changed since the last review");
        assert!(refusal["note"].as_str().expect("note").contains("6.14"));
        progress.note_edit();
        assert!(progress.refusal("review_schematic").is_none());
    }

    /// An unparsable verdict grades nothing: it neither counts as flat nor spends
    /// the edit that earned it.
    #[test]
    fn a_verdict_without_a_mean_is_not_a_review() {
        let mut progress = ReviewProgress::default();
        progress.note_edit();
        assert!(progress.observe(&json!({"error": "no verdict"})).is_none());
        assert!(progress.refusal("review_schematic").is_none());
    }
}
