//! The copilot-cockpit **state machine** — pure-ish, non-rendering, testable.
//!
//! [`App`] is the whole UI state. [`App::update`] maps a [`Msg`] (a keypress,
//! an agent event, or a pending-diff arrival) into a state transition and
//! returns an [`Action`] the shell performs (spawn a turn, resolve the
//! apply-gate, cancel, undo, quit). Nothing here touches a terminal or the
//! network, so it is unit-testable in full.
//!
//! The shell ([`super::run`]) owns the terminal, the crossterm event stream, and
//! the agent task; it translates raw input into [`Msg`]s, calls `update`, and
//! acts on the returned [`Action`]. The renderer ([`super::ui`]) reads the `App`
//! and only writes back one thing: a clamped scroll offset (it alone knows the
//! viewport size).
//!
//! The state machine is split by concern: [`state`] holds the [`App`] struct and
//! its turn-lifecycle helpers; [`transcript`] holds the transcript model and the
//! agent-event / unwind writers; [`input`] holds the composer (commands +
//! completion + line editing); [`update`] holds the [`Msg`]→[`Action`] reducer.

mod input;
mod state;
mod transcript;
mod update;

pub use input::*;
pub use state::{App, Status};
pub use transcript::{Entry, NoticeLevel, PendingDiff, Speaker, UnwindPicker};
pub use update::{Action, Msg, TurnEndReason};

#[cfg(test)]
mod tests {
    use super::*;
    use gordian_core::{AgentEvent, TurnOutcomeSummary};
    use serde_json::{json, Value};
    use std::time::Instant;

    fn app() -> App {
        App::new(Status::new(
            "bedrock",
            "us.anthropic.claude-opus-4-5-20251101-v1:0",
            "/tmp/p/design.kicad_sch",
            true,
        ))
    }

    fn type_str(a: &mut App, s: &str) {
        for c in s.chars() {
            a.update(Msg::Char(c));
        }
    }

    fn dry_run_json() -> Value {
        json!({
            "ok": true,
            "would_write": true,
            "diff": {
                "added": ["U1", "R7"],
                "removed": [],
                "changed": ["C2"],
                "nets_before": 3,
                "nets_after": 12
            }
        })
    }

    #[test]
    fn typing_builds_the_input_line() {
        let mut a = app();
        type_str(&mut a, "hello");
        assert_eq!(a.input, "hello");
        a.update(Msg::Backspace);
        assert_eq!(a.input, "hell");
    }

    #[test]
    fn cursor_movement_edits_in_the_middle() {
        let mut a = app();
        type_str(&mut a, "ac");
        a.update(Msg::CursorLeft);
        a.update(Msg::Char('b'));
        assert_eq!(a.input, "abc");
        assert_eq!(a.cursor, 2);
        a.update(Msg::Home);
        a.update(Msg::Delete);
        assert_eq!(a.input, "bc");
        a.update(Msg::End);
        a.update(Msg::Backspace);
        assert_eq!(a.input, "b");
    }

    #[test]
    fn cursor_handles_multibyte_chars() {
        let mut a = app();
        type_str(&mut a, "héllo");
        a.update(Msg::Home);
        a.update(Msg::CursorRight);
        a.update(Msg::CursorRight);
        a.update(Msg::Backspace); // removes the é
        assert_eq!(a.input, "hllo");
    }

    #[test]
    fn ctrl_u_kills_to_line_start() {
        let mut a = app();
        type_str(&mut a, "abc def");
        a.update(Msg::CursorLeft); // cursor between "de" and "f"
        a.update(Msg::KillToStart);
        assert_eq!(a.input, "f");
        assert_eq!(a.cursor, 0);
    }

    #[test]
    fn ctrl_w_kills_the_previous_word() {
        let mut a = app();
        type_str(&mut a, "add a resistor  ");
        a.update(Msg::KillWordBack);
        assert_eq!(a.input, "add a ");
        a.update(Msg::KillWordBack);
        assert_eq!(a.input, "add ");
    }

    #[test]
    fn history_recall_round_trips() {
        let mut a = app();
        type_str(&mut a, "first");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        type_str(&mut a, "second");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::Completed));

        type_str(&mut a, "draft");
        a.update(Msg::HistoryPrev);
        assert_eq!(a.input, "second");
        a.update(Msg::HistoryPrev);
        assert_eq!(a.input, "first");
        a.update(Msg::HistoryPrev); // already at the oldest — stays
        assert_eq!(a.input, "first");
        a.update(Msg::HistoryNext);
        assert_eq!(a.input, "second");
        a.update(Msg::HistoryNext); // past the newest — the draft returns
        assert_eq!(a.input, "draft");
    }

    #[test]
    fn history_skips_consecutive_duplicates() {
        let mut a = app();
        type_str(&mut a, "same");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        type_str(&mut a, "same");
        a.update(Msg::Submit);
        assert_eq!(a.history, vec!["same"]);
    }

    #[test]
    fn submitting_a_prompt_enqueues_a_turn() {
        let mut a = app();
        type_str(&mut a, "design a board");
        let action = a.update(Msg::Submit);
        assert_eq!(action, Action::SpawnTurn("design a board".to_string()));
        assert!(a.running, "submitting should mark the turn running");
        assert!(a.turn_started.is_some(), "elapsed clock starts");
        assert_eq!(a.status.turn_count, 1);
        assert!(a.input.is_empty(), "input clears on submit");
        // The user message is recorded in the transcript.
        assert!(
            a.transcript
                .iter()
                .any(|e| e.speaker == Speaker::User && e.text == "design a board")
        );
    }

    #[test]
    fn empty_submit_does_nothing() {
        let mut a = app();
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert!(!a.running);
    }

    #[test]
    fn typing_while_running_drafts_but_cannot_submit() {
        let mut a = app();
        type_str(&mut a, "first");
        a.update(Msg::Submit);
        assert!(a.running);
        // Drafting the next prompt while the agent works is allowed…
        type_str(&mut a, "second");
        assert_eq!(a.input, "second");
        // …but submitting it is not; the draft is kept.
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert_eq!(a.input, "second");
        assert_eq!(a.status.turn_count, 1);
    }

    #[test]
    fn commands_still_work_while_running() {
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        type_str(&mut a, "/help");
        a.update(Msg::Submit);
        assert!(a.help, "/help should toggle even mid-turn");
    }

    #[test]
    fn auto_command_toggles_the_gate_flag() {
        let mut a = app();
        assert!(!a.auto);
        type_str(&mut a, "/auto");
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert!(a.auto, "/auto should toggle the flag ON");
        type_str(&mut a, "/auto");
        a.update(Msg::Submit);
        assert!(!a.auto, "/auto again toggles it OFF");
    }

    #[test]
    fn clear_command_resets_transcript_and_requests_context_clear() {
        let mut a = app();
        type_str(&mut a, "hello");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        type_str(&mut a, "/clear");
        assert_eq!(a.update(Msg::Submit), Action::ClearContext);
        assert!(
            a.transcript.is_empty(),
            "transcript wiped; shell adds the note"
        );
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn colon_commands_get_a_migration_hint() {
        let mut a = app();
        type_str(&mut a, ":help");
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert!(!a.help, "the old prefix must not run the command");
        assert!(
            a.transcript
                .iter()
                .any(|e| e.text.contains("commands now start with /")),
            "{:?}",
            a.transcript
        );
    }

    #[test]
    fn quit_is_command_or_ctrl_c_but_not_esc() {
        let mut a = app();
        type_str(&mut a, "/quit");
        assert_eq!(a.update(Msg::Submit), Action::Quit);
        assert!(a.should_quit);

        let mut b = app();
        assert_eq!(b.update(Msg::Cancel), Action::None, "first idle Esc arms");
        assert!(!b.should_quit, "Esc never quits");
    }

    #[test]
    fn esc_clears_a_nonempty_input_then_arms_unwind() {
        let mut a = app();
        type_str(&mut a, "half-typed");
        assert_eq!(a.update(Msg::Cancel), Action::None);
        assert!(a.input.is_empty());
        assert!(!a.esc_armed, "clearing the input is its own Esc step");
        assert_eq!(a.update(Msg::Cancel), Action::None);
        assert!(a.esc_armed, "second Esc arms the unwind");
        assert_eq!(
            a.update(Msg::Cancel),
            Action::OpenUnwind,
            "third Esc opens the picker"
        );
        assert!(!a.esc_armed, "the unwind consumed the arming");
        assert!(!a.should_quit);
    }

    #[test]
    fn typing_disarms_a_pending_unwind() {
        let mut a = app();
        a.update(Msg::Cancel);
        assert!(a.esc_armed);
        a.update(Msg::Char('x'));
        assert!(!a.esc_armed, "any user action disarms");
        // Ticks and agent events must NOT disarm (they arrive on their own).
        a.update(Msg::Cancel);
        a.update(Msg::Cancel);
        let mut b = app();
        b.update(Msg::Cancel);
        b.update(Msg::Tick);
        assert!(b.esc_armed, "ticks don't disarm");
    }

    #[test]
    fn apply_unwind_to_rolls_the_transcript_back_to_before_the_user_turn() {
        let mut a = app();
        type_str(&mut a, "build it");
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::AssistantText("working".into())));
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        let turns_before = a.status.turn_count;

        a.apply_unwind_to(1);
        assert!(
            !a.transcript
                .iter()
                .any(|e| e.speaker == Speaker::User && e.text == "build it"),
            "user entry removed: {:?}",
            a.transcript
        );
        assert!(
            !a.transcript.iter().any(|e| e.text == "working"),
            "assistant reply removed too"
        );
        assert!(
            a.transcript.iter().any(|e| e.text.contains("unwound")),
            "confirmation note shown"
        );
        assert_eq!(a.status.turn_count, turns_before - 1);

        let len = a.transcript.len();
        a.apply_unwind_to(0);
        assert!(
            a.transcript[len..]
                .iter()
                .any(|e| e.text.contains("nothing"))
        );
    }

    #[test]
    fn apply_unwind_to_rolls_back_multiple_turns_at_once() {
        let mut a = app();
        for prompt in ["first", "second", "third"] {
            type_str(&mut a, prompt);
            a.update(Msg::Submit);
            a.update(Msg::Agent(AgentEvent::AssistantText(format!("re: {prompt}"))));
            a.update(Msg::TurnEnded(TurnEndReason::Completed));
        }
        assert_eq!(a.status.turn_count, 3);

        // Pick the 2nd-newest prompt ("second"): drop it and "third".
        a.apply_unwind_to(2);
        assert!(
            a.transcript.iter().any(|e| e.text == "first"),
            "the kept turn survives: {:?}",
            a.transcript
        );
        assert!(
            !a.transcript
                .iter()
                .any(|e| e.text == "second" || e.text == "third"),
            "the selected turn and everything after are gone: {:?}",
            a.transcript
        );
        assert_eq!(a.status.turn_count, 1);
        assert!(a.transcript.iter().any(|e| e.text.contains("2 turns")));
    }

    #[test]
    fn unwind_picker_opens_navigates_and_confirms() {
        let mut a = app();
        // Newest-first prompts, as the agent would report them.
        a.open_unwind(vec![
            "swap the regulator".into(),
            "add usb-c".into(),
            "make the board".into(),
        ]);
        let p = a.unwind.as_ref().expect("picker open");
        assert_eq!(p.selected, 0, "latest turn preselected");

        // Down moves toward older turns; up clamps back at the top.
        a.update(Msg::HistoryNext);
        a.update(Msg::HistoryNext);
        assert_eq!(a.unwind.as_ref().unwrap().selected, 2);
        a.update(Msg::HistoryNext); // clamps at the last row
        assert_eq!(a.unwind.as_ref().unwrap().selected, 2);
        a.update(Msg::HistoryPrev);
        assert_eq!(a.unwind.as_ref().unwrap().selected, 1);

        // Enter confirms: drop selected + 1 = 2 turns, picker closes.
        assert_eq!(a.update(Msg::Submit), Action::UnwindTo(2));
        assert!(a.unwind.is_none(), "confirm closes the picker");
    }

    #[test]
    fn unwind_picker_esc_cancels_without_acting() {
        let mut a = app();
        a.open_unwind(vec!["a".into(), "b".into()]);
        assert!(a.unwind.is_some());
        assert_eq!(a.update(Msg::Cancel), Action::None);
        assert!(a.unwind.is_none(), "Esc closes the picker, no unwind");
    }

    #[test]
    fn unwind_picker_is_empty_when_nothing_to_unwind() {
        let mut a = app();
        a.open_unwind(vec![]);
        assert!(a.unwind.is_none(), "no picker for an empty list");
        assert!(a.transcript.iter().any(|e| e.text.contains("nothing")));
    }

    #[test]
    fn compact_command_spins_like_a_turn() {
        let mut a = app();
        type_str(&mut a, "/compact");
        assert_eq!(a.update(Msg::Submit), Action::Compact);
        assert!(a.running, "compaction shows the working spinner");
        a.update(Msg::TurnEnded(TurnEndReason::Compacted));
        assert!(!a.running);
        assert!(
            !a.transcript.iter().any(|e| e.text.contains("Cogitated")),
            "compaction posts no end indicator (it has its own shrink note)"
        );
    }

    #[test]
    fn compact_while_running_is_refused() {
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        type_str(&mut a, "/compact");
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert!(
            a.transcript
                .iter()
                .any(|e| e.text.contains("can't compact")),
            "{:?}",
            a.transcript
        );
    }

    #[test]
    fn context_command_requests_stats() {
        let mut a = app();
        type_str(&mut a, "/context");
        assert_eq!(a.update(Msg::Submit), Action::ShowContext);
    }

    #[test]
    fn tab_cycles_through_matching_commands() {
        let mut a = app();
        type_str(&mut a, "/c");
        let (matches, idx) = a.completion_view().expect("matches for /c");
        let names: Vec<&str> = matches.iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["/clear", "/context", "/compact"]);
        assert_eq!(idx, None, "nothing highlighted before the first Tab");

        a.update(Msg::Complete);
        assert_eq!(a.input, "/clear");
        a.update(Msg::Complete);
        assert_eq!(a.input, "/context", "Tab cycles against the typed stem");
        a.update(Msg::Complete);
        assert_eq!(a.input, "/compact");
        a.update(Msg::Complete);
        assert_eq!(a.input, "/clear", "cycling wraps");

        // Typing again resets the cycle to the new stem.
        a.update(Msg::Backspace);
        assert!(a.completion_view().is_some());
        assert_eq!(a.completion_idx, None, "edit resets the cycle");
    }

    #[test]
    fn enter_accepts_a_highlighted_completion_instead_of_submitting() {
        let mut a = app();
        type_str(&mut a, "/c");
        // No highlight yet: Enter would submit (here it's an unknown prefix, so
        // submit reports it), not accept.
        assert!(a.completion_idx.is_none());
        // Tab highlights the first match; now Enter accepts it into the input.
        a.update(Msg::Complete);
        assert_eq!(a.input, "/clear");
        assert_eq!(a.update(Msg::Submit), Action::None, "Enter accepts, not submits");
        assert_eq!(a.input, "/clear");
        assert!(a.completion_idx.is_none(), "accepting clears the cycle");
        // A second Enter now runs the confirmed command.
        assert_eq!(a.update(Msg::Submit), Action::ClearContext);
    }

    #[test]
    fn enter_submits_a_complete_command_with_no_highlight() {
        let mut a = app();
        type_str(&mut a, "/help");
        // The popup is open (matches /help) but nothing is highlighted, so Enter
        // runs the command rather than re-accepting it.
        assert!(a.completion_view().is_some());
        a.update(Msg::Submit);
        assert!(a.help, "/help runs on a bare Enter");
    }

    #[test]
    fn completion_does_not_apply_to_prompts_or_arguments() {
        let mut a = app();
        type_str(&mut a, "hello");
        assert!(a.completion_view().is_none());
        a.update(Msg::Complete);
        assert_eq!(a.input, "hello", "Tab is inert outside / commands");

        let mut b = app();
        type_str(&mut b, "/clear now");
        assert!(b.completion_view().is_none(), "no completion after a space");
    }

    #[test]
    fn usage_events_update_token_status() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 1000,
            output_tokens: 200,
        }));
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 1500,
            output_tokens: 300,
        }));
        assert_eq!(a.status.ctx_tokens, 1800, "latest call defines the context");
        assert_eq!(a.status.total_input_tokens, 2500);
        assert_eq!(a.status.total_output_tokens, 500);
    }

    #[test]
    fn compacted_event_notes_the_shrink() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Compacted {
            messages_before: 24,
            messages_after: 2,
        }));
        assert!(
            a.transcript.iter().any(|e| e.text.contains("24 → 2")),
            "{:?}",
            a.transcript
        );
    }

    #[test]
    fn esc_cancels_a_running_turn() {
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        assert!(a.running);
        assert_eq!(a.update(Msg::Cancel), Action::CancelTurn);
        assert!(!a.should_quit, "cancelling a turn must not quit");
        // Esc itself posts no note now; the shell confirms the abort by sending
        // TurnEnded(Interrupted), which posts the interruption indicator.
        a.update(Msg::TurnEnded(TurnEndReason::Interrupted));
        assert!(!a.running);
        assert!(
            a.transcript
                .iter()
                .any(|e| e.text.contains("Interrupted") && e.level == NoticeLevel::Plain),
            "interruption indicator posted: {:?}",
            a.transcript
        );
    }

    #[test]
    fn ctrl_c_force_quits_even_mid_turn() {
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        assert_eq!(a.update(Msg::ForceQuit), Action::Quit);
        assert!(a.should_quit);
    }

    #[test]
    fn tick_advances_the_spinner_only_while_running() {
        let mut a = app();
        a.update(Msg::Tick);
        assert_eq!(a.spinner, 0);
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::Tick);
        a.update(Msg::Tick);
        assert_eq!(a.spinner, 2);
    }

    #[test]
    fn undo_command_returns_undo_action() {
        let mut a = app();
        type_str(&mut a, "/undo");
        assert_eq!(a.update(Msg::Submit), Action::Undo);
    }

    #[test]
    fn help_command_toggles_help_and_esc_dismisses() {
        let mut a = app();
        type_str(&mut a, "/help");
        a.update(Msg::Submit);
        assert!(a.help);
        // Esc dismisses help rather than quitting.
        assert_eq!(a.update(Msg::Cancel), Action::None);
        assert!(!a.help);
        assert!(!a.should_quit);
    }

    #[test]
    fn pending_diff_arrives_and_approve_resolves_it() {
        let mut a = app();
        a.update(Msg::PendingDiff(dry_run_json()));
        let pending = a.pending.as_ref().expect("diff is pending");
        assert_eq!(pending.added, vec!["U1", "R7"]);
        assert_eq!(pending.changed, vec!["C2"]);
        assert_eq!(pending.nets_after, 12);
        assert!(!a.input_active(), "input locked while a gate is open");

        // Pressing 'a' resolves approval and clears the pending diff.
        let action = a.update(Msg::Char('a'));
        assert_eq!(action, Action::ResolveApproval(true));
        assert!(a.pending.is_none());
    }

    #[test]
    fn reject_key_resolves_false() {
        let mut a = app();
        a.update(Msg::PendingDiff(dry_run_json()));
        let action = a.update(Msg::Char('r'));
        assert_eq!(action, Action::ResolveApproval(false));
        assert!(a.pending.is_none());
    }

    #[test]
    fn other_chars_do_not_leak_into_input_while_gate_open() {
        let mut a = app();
        a.update(Msg::PendingDiff(dry_run_json()));
        a.update(Msg::Char('x'));
        assert!(a.input.is_empty(), "gate keys only while pending");
    }

    #[test]
    fn resolving_with_nothing_pending_is_a_noop() {
        let mut a = app();
        assert_eq!(a.update(Msg::Approve), Action::None);
    }

    #[test]
    fn tool_finished_event_appends_or_replaces_a_card() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::ToolStarted {
            name: "search_symbols".into(),
        }));
        assert!(
            a.transcript
                .iter()
                .any(|e| e.speaker == Speaker::Tool && e.text.contains("running"))
        );
        a.update(Msg::Agent(AgentEvent::ToolFinished {
            name: "search_symbols".into(),
            summary: "\"STM32\" → 4 hits".into(),
        }));
        // The running placeholder is replaced in place by the finished card.
        let cards: Vec<&Entry> = a
            .transcript
            .iter()
            .filter(|e| e.speaker == Speaker::Tool)
            .collect();
        assert_eq!(cards.len(), 1, "the card collapses in place");
        assert!(cards[0].text.contains("→ \"STM32\" → 4 hits"));
    }

    #[test]
    fn applied_event_bumps_count_and_notes_erc() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Applied {
            summary: "ERC 0 errors, 2 warnings".into(),
        }));
        assert_eq!(a.status.applied_count, 1);
        assert!(
            a.transcript
                .iter()
                .any(|e| e.text.contains("ERC 0 errors, 2 warnings"))
        );
    }

    #[test]
    fn turn_done_stops_spinner_but_keeps_the_clock_for_turn_ended() {
        // TurnDone now only stops the spinner; it leaves `turn_started` intact so
        // the following TurnEnded can read the elapsed time. TurnEnded owns the
        // rest of teardown.
        let mut a = app();
        a.running = true;
        a.turn_started = Some(Instant::now());
        a.update(Msg::Agent(AgentEvent::TurnDone(TurnOutcomeSummary {
            applied: true,
            tool_calls_made: 3,
            final_text: "done".into(),
        })));
        assert!(!a.running, "spinner stops");
        assert!(
            a.turn_started.is_some(),
            "the clock survives until TurnEnded reads it"
        );

        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        assert!(a.turn_started.is_none(), "TurnEnded clears the clock");
    }

    #[test]
    fn turn_ended_posts_a_labelled_indicator_per_reason() {
        // Completed → green "Cogitated", with a pluralized tool-call count.
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::ToolStarted {
            name: "get_design".into(),
        }));
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        let last = a.transcript.last().unwrap();
        assert!(last.text.contains("Cogitated"), "{}", last.text);
        assert!(last.text.contains("1 tool call"), "singular: {}", last.text);
        assert_eq!(last.level, NoticeLevel::Success);
        assert!(!a.running && a.turn_started.is_none());

        // IterationCap → yellow warning with the resume hint.
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::IterationCap));
        let last = a.transcript.last().unwrap();
        assert!(last.text.contains("step limit"), "{}", last.text);
        assert!(last.text.contains("continue"), "resume hint: {}", last.text);
        assert!(last.text.contains("0 tool calls"), "plural: {}", last.text);
        assert_eq!(last.level, NoticeLevel::Warn);

        // Error → red, carries the message.
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::Error("throttled".into())));
        let last = a.transcript.last().unwrap();
        assert!(last.text.contains("throttled"), "{}", last.text);
        assert_eq!(last.level, NoticeLevel::Error);
    }

    #[test]
    fn turn_done_then_turn_ended_posts_exactly_one_indicator() {
        // The pair can arrive in either select order; only TurnEnded posts, so
        // there is never a double line nor a lost clock.
        for done_first in [true, false] {
            let mut a = app();
            type_str(&mut a, "go");
            a.update(Msg::Submit);
            let done = Msg::Agent(AgentEvent::TurnDone(TurnOutcomeSummary {
                applied: false,
                tool_calls_made: 0,
                final_text: "ok".into(),
            }));
            if done_first {
                a.update(done);
                a.update(Msg::TurnEnded(TurnEndReason::Completed));
            } else {
                a.update(Msg::TurnEnded(TurnEndReason::Completed));
                a.update(done);
            }
            let indicators = a
                .transcript
                .iter()
                .filter(|e| e.text.contains("Cogitated"))
                .count();
            assert_eq!(indicators, 1, "exactly one indicator (done_first={done_first})");
        }
    }

    #[test]
    fn assistant_text_appends_to_transcript() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantText(
            "I'll search for the part.".into(),
        )));
        assert!(
            a.transcript
                .iter()
                .any(|e| e.speaker == Speaker::Assistant && e.text.contains("search"))
        );
    }

    #[test]
    fn assistant_deltas_grow_a_live_entry_then_text_finalizes_it() {
        let mut a = app();
        // The first delta opens a live assistant entry; subsequent ones grow it.
        a.update(Msg::Agent(AgentEvent::AssistantDelta("I'll ".into())));
        a.update(Msg::Agent(AgentEvent::AssistantDelta("search".into())));
        let live = a.live_assistant.expect("a live entry is open mid-stream");
        assert_eq!(a.transcript[live].text, "I'll search");
        assert_eq!(a.transcript[live].speaker, Speaker::Assistant);
        let count = a.transcript.iter().filter(|e| e.speaker == Speaker::Assistant).count();
        assert_eq!(count, 1, "deltas grow ONE entry, not one per chunk");

        // The final text finalizes the same entry in place (no second entry).
        a.update(Msg::Agent(AgentEvent::AssistantText("I'll search for the part.".into())));
        assert!(a.live_assistant.is_none(), "finalized: no live entry");
        let assistants: Vec<&Entry> =
            a.transcript.iter().filter(|e| e.speaker == Speaker::Assistant).collect();
        assert_eq!(assistants.len(), 1, "still one assistant entry (finalized in place)");
        assert_eq!(assistants[0].text, "I'll search for the part.");
    }

    #[test]
    fn a_new_streamed_run_opens_a_fresh_live_entry() {
        // Prose, then tool work, then more prose: two separate streamed runs each
        // get their own finalized entry.
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantDelta("first".into())));
        a.update(Msg::Agent(AgentEvent::AssistantText("first".into())));
        a.update(Msg::Agent(AgentEvent::ToolStarted { name: "get_design".into() }));
        a.update(Msg::Agent(AgentEvent::AssistantDelta("second".into())));
        a.update(Msg::Agent(AgentEvent::AssistantText("second".into())));
        let texts: Vec<&str> = a
            .transcript
            .iter()
            .filter(|e| e.speaker == Speaker::Assistant)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(texts, vec!["first", "second"]);
    }

    #[test]
    fn turn_done_closes_an_unfinalized_live_entry() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantDelta("partial".into())));
        assert!(a.live_assistant.is_some());
        a.update(Msg::Agent(AgentEvent::TurnDone(TurnOutcomeSummary {
            applied: false,
            tool_calls_made: 0,
            final_text: "partial".into(),
        })));
        assert!(a.live_assistant.is_none(), "TurnDone closes the live entry");
    }
}
