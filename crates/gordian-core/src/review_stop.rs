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
//! Above [`BAND`] the first review IS the outcome, so the rules follow the table
//! rather than a stopping heuristic. All three are in-band like the thrash guard:
//!
//! - **at its best** — a FIRST review of [`BAND`] or better closes the loop then
//!   and there: `arrange` / `move_symbols` / `rewire` are refused for the rest of
//!   the subturn and the result says to finish and report that mean. Not even one
//!   targeted fix: none of the five runs that tried above 7 came out ahead.
//! - **still failing** — under [`BAND`] the loop iterates, since that is where
//!   re-composing pays, 10 times out of 10. It closes on the first review that
//!   fails to beat the best by [`IMPROVEMENT`] once the best has reached the band,
//!   and on [`FLAT_LIMIT`] consecutive flat reads below it — `ecg#1` oscillated
//!   3.0-5.3 for 24 reviews and 258 requests, which is the burn this exists to
//!   stop.
//! - **no-op rule** — a review with no successful layout edit since the last one
//!   grades an unchanged picture, so it is refused with the standing mean.

use serde_json::{Value, json};

/// How much a review must beat the turn's best mean by to count as progress.
/// The grader reads ±0.3 on an unchanged sheet at seven samples, so anything
/// under that is noise, not a better drawing.
const IMPROVEMENT: f64 = 0.3;

/// The measured edge between a sheet that is failing and one that is finished:
/// every run whose first review read under this improved on it, and not one at
/// or above it did.
const BAND: f64 = 5.5;

/// Consecutive flat reads a sheet under [`BAND`] may spend before the loop closes
/// anyway. Below the band a flat read means that edit missed, not that the sheet
/// is done — but three in a row is a loop, not a search.
const FLAT_LIMIT: usize = 3;

/// The layout edits that only make sense while the review loop is open.
fn is_layout_edit(tool: &str) -> bool {
    matches!(tool, "arrange" | "move_symbols" | "rewire")
}

/// One subturn's review trajectory.
pub(crate) struct ReviewProgress {
    best: Option<f64>,
    last: Option<f64>,
    flat: usize,
    stopped: bool,
    edited_since_review: bool,
    benched: bool,
}

impl Default for ReviewProgress {
    fn default() -> Self {
        Self {
            best: None,
            last: None,
            flat: 0,
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
        let improved = self.best.is_none_or(|best| mean >= best + IMPROVEMENT);
        self.best = Some(self.best.map_or(mean, |best| best.max(mean)));
        let best = self.best.unwrap_or(mean);
        if improved {
            self.flat = 0;
        } else {
            self.flat += 1;
        }
        if self.stopped {
            return None;
        }
        if first && mean >= BAND {
            self.stopped = true;
            return Some(format!(
                "The sheet is at its best: it reads {mean} on its first review, and in the last \
                 suite no sheet that read {BAND} or better improved on its first read — every one \
                 that was edited again ended lower. Finish and report {mean}. Do not arrange, \
                 move or rewire anything: those calls will be refused."
            ));
        }
        if improved {
            return None;
        }
        if best >= BAND {
            self.stopped = true;
            return Some(format!(
                "This review did not beat the best mean of {best} this turn (it read {mean}), \
                 which means the last edit hurt: do not arrange again, finish. Report {best} as \
                 the final mean and do not edit the sheet after the review you finish on. Further \
                 arrange, move_symbols and rewire calls will be refused."
            ));
        }
        if self.flat < FLAT_LIMIT {
            return None;
        }
        self.stopped = true;
        Some(format!(
            "{FLAT_LIMIT} reviews in a row have failed to beat {best} and the sheet is still \
             under {BAND}: re-arranging is not finding the composition this circuit wants. \
             Finish and report {best}; further arrange, move_symbols and rewire calls will be \
             refused."
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

    /// Ten runs read 5.5 or better first and not one of them improved on it, so
    /// that read is the outcome: the loop closes on the spot.
    #[test]
    fn a_first_review_in_the_band_closes_the_loop_at_once() {
        let mut progress = ReviewProgress::default();
        let notice = progress.observe(&review(6.86)).expect("closes at once");
        assert!(notice.contains("at its best"), "{notice}");
        assert!(notice.contains("6.86"), "{notice}");
        assert!(progress.refusal("arrange").is_some());
        assert!(progress.refusal("move_symbols").is_some());
        assert!(progress.refusal("rewire").is_some());
        assert!(progress.refusal("place_parts").is_none(), "new work is free");
    }

    /// Not even one targeted fix above 7: none of the five runs that tried came
    /// out ahead.
    #[test]
    fn a_high_first_review_gets_no_targeted_fix_either() {
        let mut progress = ReviewProgress::default();
        assert!(progress.observe(&review(8.43)).is_some());
        assert!(progress.refusal("move_symbols").is_some());
    }

    /// Under the band the loop iterates, and closes on the first read that fails
    /// to beat a best that has climbed into the band.
    #[test]
    fn a_climbing_sheet_closes_on_the_first_flat_read_in_the_band() {
        let mut progress = ReviewProgress::default();
        assert!(progress.observe(&review(3.71)).is_none(), "still failing");
        progress.note_edit();
        assert!(progress.observe(&review(4.00)).is_none(), "flat but low");
        progress.note_edit();
        assert!(progress.observe(&review(6.14)).is_none(), "a real gain");
        progress.note_edit();
        let notice = progress.observe(&review(5.86)).expect("the flat read");
        assert!(notice.contains("the last edit hurt"), "{notice}");
        assert!(progress.refusal("arrange").is_some());
    }

    /// `rp2040#0` read 4.00 three times and then found 6.71: a sheet that bad is
    /// mis-composed, not converged, so the loop stays open for it.
    #[test]
    fn a_failing_sheet_keeps_re_composing() {
        let mut progress = ReviewProgress::default();
        for mean in [4.0, 4.0, 4.0] {
            assert!(progress.observe(&review(mean)).is_none(), "{mean}");
            progress.note_edit();
        }
        assert!(progress.refusal("arrange").is_none());
        assert!(progress.observe(&review(6.71)).is_none(), "a real gain");
        progress.note_edit();
        assert!(progress.observe(&review(6.5)).is_some(), "now it may close");
    }

    /// `ecg#1` oscillated 3.0-5.3 for 24 reviews and 258 requests. Three flat
    /// reads under the band is a loop, not a search.
    #[test]
    fn three_flat_reads_under_the_band_close_it_anyway() {
        let mut progress = ReviewProgress::default();
        for mean in [3.29, 3.43, 4.57] {
            assert!(progress.observe(&review(mean)).is_none(), "{mean}");
            progress.note_edit();
        }
        assert!(progress.observe(&review(4.29)).is_none(), "one flat");
        progress.note_edit();
        assert!(progress.observe(&review(4.43)).is_none(), "two flat");
        progress.note_edit();
        let notice = progress.observe(&review(4.71)).expect("three closes it");
        assert!(notice.contains("not finding the composition"), "{notice}");
    }

    /// Only `arrange` empties the bench, so a closed loop still allows it while
    /// parts are wired by name and undrawn.
    #[test]
    fn benched_parts_keep_the_layout_tools_open() {
        let mut progress = ReviewProgress::default();
        progress.observe(&review(6.0));
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
