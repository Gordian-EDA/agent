//! Closes the visual-review loop when it stops paying, and refuses the reviews
//! that ask the same question twice.
//!
//! Measured over one full suite: the loop wins when it wins fast (current-sense
//! read 4.00, one `arrange`, 8.71) and otherwise wanders. `current-sense#1` spent
//! 16 reviews and 17 `arrange`s to finish at 5.86; `stm32#0` spent 12 reviews to
//! finish at 6.00 having peaked at 6.29 on its third. The prompt already said to
//! stop when the mean stops improving; nothing enforced it, and each wasted review
//! is seven vision calls.
//!
//! Two rules, both in-band like the thrash guard:
//!
//! - **stop rule** — a review that does not beat the best mean of the turn by
//!   [`IMPROVEMENT`] is flat. Two flat reviews in a row close the loop: the review
//!   result says so, and `arrange` / `move_symbols` / `rewire` are refused for the
//!   rest of the subturn. A sheet still under [`CONVERGED`] has not converged, it
//!   is failing, so flat reads there do not close it — replayed over the suite that
//!   exemption costs 3 of 57 saved reviews and keeps the one run that read 4.00
//!   three times running to the 6.71 it eventually reached.
//! - **no-op rule** — a review with no successful layout edit since the last one
//!   grades an unchanged picture, so it is refused with the standing mean.

use serde_json::{Value, json};

/// How much a review must beat the turn's best mean by to count as progress.
/// The grader reads ±0.3 on an unchanged sheet at seven samples, so anything
/// under that is noise, not a better drawing.
const IMPROVEMENT: f64 = 0.3;

/// Flat reviews in a row that close the loop.
const FLAT_LIMIT: usize = 2;

/// The mean below which flat reviews mean the composition is wrong rather than
/// finished, so the loop stays open.
const CONVERGED: f64 = 5.0;

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
        if self.best.is_none_or(|best| mean >= best + IMPROVEMENT) {
            self.best = Some(self.best.map_or(mean, |best| best.max(mean)));
            self.flat = 0;
            return None;
        }
        self.best = Some(self.best.map_or(mean, |best| best.max(mean)));
        self.flat += 1;
        let best = self.best.unwrap_or(mean);
        if self.flat < FLAT_LIMIT || self.stopped || best < CONVERGED {
            return None;
        }
        self.stopped = true;
        Some(format!(
            "The sheet stopped improving: {FLAT_LIMIT} reviews in a row failed to beat the best \
             mean of {best} this turn (this one read {mean}). Finish now — report {best} as the \
             final mean and do not edit the sheet after the review you finish on. Further \
             arrange, move_symbols and rewire calls will be refused."
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

    /// The trajectory `stm32#0` actually ran: it peaked on its third review and
    /// spent nine more getting nowhere.
    #[test]
    fn two_flat_reviews_in_a_row_close_the_loop() {
        let mut progress = ReviewProgress::default();
        for mean in [3.57, 3.86, 6.29, 6.00] {
            assert!(progress.observe(&review(mean)).is_none(), "{mean}");
            progress.note_edit();
        }
        let notice = progress.observe(&review(6.00)).expect("the fifth closes it");
        assert!(notice.contains("6.29"), "{notice}");
        assert!(progress.refusal("arrange").is_some());
        assert!(progress.refusal("move_symbols").is_some());
        assert!(progress.refusal("rewire").is_some());
        assert!(progress.refusal("place_parts").is_none(), "new work is free");
    }

    /// A real gain resets the count: a loop that is still paying stays open.
    #[test]
    fn an_improvement_reopens_the_count() {
        let mut progress = ReviewProgress::default();
        for mean in [6.29, 6.00, 7.00, 5.71] {
            assert!(progress.observe(&review(mean)).is_none(), "{mean}");
            progress.note_edit();
        }
        assert!(progress.observe(&review(5.71)).is_some());
    }

    /// Half a point of grader noise is not an improvement worth another round.
    #[test]
    fn a_gain_under_the_noise_floor_is_flat() {
        let mut progress = ReviewProgress::default();
        progress.observe(&review(6.00));
        progress.note_edit();
        assert!(progress.observe(&review(6.29)).is_none(), "one flat is fine");
        progress.note_edit();
        assert!(progress.observe(&review(6.20)).is_some(), "two is not");
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
        progress.observe(&review(6.5));
        progress.note_edit();
        assert!(progress.observe(&review(6.6)).is_some(), "now it may close");
    }

    /// Only `arrange` empties the bench, so a closed loop still allows it while
    /// parts are wired by name and undrawn.
    #[test]
    fn benched_parts_keep_the_layout_tools_open() {
        let mut progress = ReviewProgress::default();
        for mean in [6.0, 6.0, 6.0] {
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
