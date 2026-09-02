//! The copilot-cockpit **state machine** — pure-ish, non-rendering, testable.
//!
//! [`App`] is the whole UI state. [`App::update`] maps a [`Msg`] (a keypress,
//! an agent event, or an async arrival) into a state transition and returns an
//! [`Action`] the shell performs (spawn a turn, cancel, undo, quit). Nothing here touches a terminal or the
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

mod image_cell;
mod input;
mod state;
mod transcript;
mod update;

pub use image_cell::ImageCell;
pub use input::*;
pub use state::{App, PreviewZone, Status};
pub use transcript::{Entry, LiveAssistant, NoticeLevel, Speaker, UnwindPicker};
pub use update::{Action, Msg, TurnEndReason};

#[cfg(test)]
mod tests {
    use super::*;
    use gordian_core::AgentEvent;
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
    fn ctrl_arrows_move_by_words() {
        let mut a = app();
        type_str(&mut a, "add a resistor now");
        a.update(Msg::WordLeft);
        assert_eq!(a.cursor, "add a resistor ".chars().count());
        a.update(Msg::WordLeft);
        assert_eq!(a.cursor, "add a ".chars().count());
        a.update(Msg::WordRight);
        assert_eq!(a.cursor, "add a resistor".chars().count());
        a.update(Msg::WordRight);
        assert_eq!(a.cursor, "add a resistor now".chars().count());
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
    fn submitting_while_running_queues_instead_of_starting_a_second_turn() {
        let mut a = app();
        type_str(&mut a, "first");
        a.update(Msg::Submit);
        assert!(a.running);
        // Drafting the next prompt while the agent works is allowed…
        type_str(&mut a, "second");
        assert_eq!(a.input, "second");
        // …and Enter queues it rather than starting a second turn or dropping it.
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert!(a.input.is_empty(), "the composer clears once queued");
        assert_eq!(a.queued, vec!["second".to_string()]);
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
    fn enter_queues_a_draft_while_running_and_it_auto_submits_on_turn_end() {
        let mut a = app();
        type_str(&mut a, "first");
        a.update(Msg::Submit);
        assert!(a.running);
        // Enter with a plain draft mid-turn queues it (can't submit now) and
        // clears the composer for the next thought.
        type_str(&mut a, "then route it");
        a.update(Msg::Submit);
        assert_eq!(a.queued, vec!["then route it".to_string()]);
        assert!(a.input.is_empty(), "the composer clears after queueing");
        // When the turn ends, the queued prompt auto-submits as a fresh turn.
        let action = a.update(Msg::TurnEnded(TurnEndReason::Completed));
        assert_eq!(action, Action::SpawnTurn("then route it".into()));
        assert!(a.running, "the queued turn starts");
        assert!(a.queued.is_empty(), "the queue is consumed");
    }

    #[test]
    fn further_enters_queue_more_prompts_in_order() {
        let mut a = app();
        type_str(&mut a, "first");
        a.update(Msg::Submit);
        type_str(&mut a, "second");
        a.update(Msg::Submit);
        type_str(&mut a, "third");
        a.update(Msg::Submit);

        assert_eq!(
            a.queued,
            vec!["second".to_string(), "third".to_string()],
            "each Enter queues rather than overwriting or dropping the last"
        );
        assert!(a.input.is_empty());

        // Turns drain the queue one at a time, oldest first.
        let action = a.update(Msg::TurnEnded(TurnEndReason::Completed));
        assert_eq!(action, Action::SpawnTurn("second".into()));
        assert_eq!(a.queued, vec!["third".to_string()]);

        let action = a.update(Msg::TurnEnded(TurnEndReason::Completed));
        assert_eq!(action, Action::SpawnTurn("third".into()));
        assert!(a.queued.is_empty());
    }

    #[test]
    fn interrupting_a_turn_restores_the_oldest_queued_prompt_without_starting_a_phantom_turn() {
        let mut a = app();
        type_str(&mut a, "first");
        assert!(matches!(a.update(Msg::Submit), Action::SpawnTurn(_)));
        type_str(&mut a, "do this later");
        a.update(Msg::Submit);
        type_str(&mut a, "and this after");
        a.update(Msg::Submit);
        assert_eq!(
            a.queued,
            vec!["do this later".to_string(), "and this after".to_string()]
        );

        let action = a.update(Msg::TurnEnded(TurnEndReason::Interrupted));

        assert_eq!(action, Action::None);
        assert!(!a.running, "no task was spawned after cancellation");
        assert_eq!(
            a.queued,
            vec!["and this after".to_string()],
            "only the restored prompt leaves the queue"
        );
        assert_eq!(a.input, "do this later");
        assert_eq!(a.cursor, "do this later".chars().count());
    }

    #[test]
    fn a_large_paste_collapses_to_a_placeholder_then_expands_on_submit() {
        let mut a = app();
        let big = "x".repeat(500);
        a.update(Msg::Paste(big.clone()));
        assert_eq!(
            a.input, "[Pasted 500 chars]",
            "composer shows the placeholder"
        );
        assert_eq!(a.pastes.len(), 1, "real text stashed");
        assert_eq!(a.pastes[0].text, big);
        // Submitting expands the placeholder back to the real pasted text.
        let action = a.update(Msg::Submit);
        assert_eq!(action, Action::SpawnTurn(big.clone()));
        assert!(a.pastes.is_empty(), "the stash clears on submit");
    }

    #[test]
    fn multiple_large_pastes_expand_without_overwriting_each_other() {
        let mut a = app();
        let first = "x".repeat(300);
        let second = "y".repeat(400);
        a.update(Msg::Paste(first.clone()));
        a.update(Msg::Paste(" between ".into()));
        a.update(Msg::Paste(second.clone()));

        assert_eq!(a.input, "[Pasted 300 chars] between [Pasted 400 chars #2]");
        assert_eq!(a.pastes.len(), 2);
        assert_eq!(
            a.update(Msg::Submit),
            Action::SpawnTurn(format!("{first} between {second}"))
        );
        assert!(a.pastes.is_empty());
    }

    #[test]
    fn a_small_paste_is_inserted_verbatim() {
        let mut a = app();
        a.update(Msg::Paste("add a 10k resistor".into()));
        assert_eq!(a.input, "add a 10k resistor");
        assert!(a.pastes.is_empty(), "no stash for a small paste");
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
    fn quit_is_command_or_double_ctrl_c_but_not_esc() {
        let mut a = app();
        type_str(&mut a, "/quit");
        assert_eq!(a.update(Msg::Submit), Action::Quit);
        assert!(a.should_quit);

        let mut b = app();
        assert_eq!(b.update(Msg::ForceQuit), Action::None);
        assert!(b.ctrl_c_armed);
        assert!(!b.should_quit, "first Ctrl-C only arms quit");
        assert!(
            !b.transcript
                .iter()
                .any(|e| e.text.contains("Press Ctrl-C again to exit")),
            "first Ctrl-C leaves the transcript alone: {:?}",
            b.transcript
        );
        assert_eq!(b.update(Msg::ForceQuit), Action::Quit);
        assert!(b.should_quit);

        let mut c = app();
        assert_eq!(c.update(Msg::Cancel), Action::None, "first idle Esc arms");
        assert!(!c.should_quit, "Esc never quits");
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
    fn typing_disarms_ctrl_c_quit_catcher() {
        let mut a = app();
        assert_eq!(a.update(Msg::ForceQuit), Action::None);
        assert!(a.ctrl_c_armed);
        a.update(Msg::Char('x'));
        assert!(!a.ctrl_c_armed, "any user action disarms");
        assert_eq!(a.update(Msg::ForceQuit), Action::None);
        assert!(!a.should_quit, "second non-consecutive Ctrl-C only rearms");
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
            a.update(Msg::Agent(AgentEvent::AssistantText(format!(
                "re: {prompt}"
            ))));
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
            !a.transcript.iter().any(|e| e.text.contains("Worked for")),
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
    fn preview_command_opens_the_latest_render_without_adding_a_row() {
        let mut a = app();
        let path = "/tmp/p/.gordian/renders/001.png";
        a.update(Msg::Agent(AgentEvent::ToolFinished {
            name: "render_board".into(),
            summary: "rendered board to PNG".into(),
            image_path: Some(path.into()),
            elapsed_ms: 0,
            revision: None,
            result: serde_json::json!({}),
        }));
        let transcript_len = a.transcript.len();

        type_str(&mut a, "/preview");
        assert_eq!(a.update(Msg::Submit), Action::OpenPreview(path.into()));
        assert_eq!(a.transcript.len(), transcript_len);
        assert_eq!(a.images.len(), 1);
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
        assert_eq!(
            a.update(Msg::Submit),
            Action::None,
            "Enter accepts, not submits"
        );
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
        // Cold first call: 800 of the 1000 input was a cache write.
        a.update(Msg::Agent(AgentEvent::Usage {
            provider_requests: 1,
            input_tokens: 1000,
            output_tokens: 200,
            cache_write_tokens: 800,
            cache_read_tokens: 0,
        }));
        // Warm second call: reads the 800 back, 700 fresh input.
        a.update(Msg::Agent(AgentEvent::Usage {
            provider_requests: 1,
            input_tokens: 1500,
            output_tokens: 300,
            cache_write_tokens: 0,
            cache_read_tokens: 800,
        }));
        // A failed provider invocation is still counted but has no token report.
        a.update(Msg::Agent(AgentEvent::Usage {
            provider_requests: 1,
            input_tokens: 0,
            output_tokens: 0,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
        }));
        assert_eq!(a.status.ctx_tokens, 1800, "latest call defines the context");
        let l = &a.status.ledger;
        assert_eq!(
            l.input,
            200 + 700,
            "full-price input excludes cache reads/writes"
        );
        assert_eq!(l.output, 500);
        assert_eq!(l.cache_write, 800);
        assert_eq!(l.cache_read, 800);
        assert_eq!(l.provider_requests, 3);
        assert_eq!(l.input_tokens(), 1000 + 1500);
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
                .any(|e| e.text.contains("Worked for") && e.level == NoticeLevel::Plain),
            "interruption indicator posted: {:?}",
            a.transcript
        );
    }

    #[test]
    fn double_ctrl_c_quits_even_mid_turn() {
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        assert_eq!(a.update(Msg::ForceQuit), Action::None);
        assert!(a.running, "first Ctrl-C does not abort the running turn");
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
    fn tool_finished_event_appends_or_replaces_a_card() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::ToolStarted {
            name: "search_symbols".into(),
            args: serde_json::json!({}),
            seq: 0,
        }));
        assert!(
            !a.transcript.iter().any(|e| e.speaker == Speaker::Tool),
            "active tools are shown in the status row, not duplicated in the transcript"
        );
        a.update(Msg::Agent(AgentEvent::ToolFinished {
            name: "search_symbols".into(),
            summary: "\"STM32\" → 4 hits".into(),
            image_path: None,
            elapsed_ms: 0,
            revision: None,
            result: serde_json::json!({}),
        }));
        // The finished card is appended once.
        let cards: Vec<&Entry> = a
            .transcript
            .iter()
            .filter(|e| e.speaker == Speaker::Tool)
            .collect();
        assert_eq!(cards.len(), 1, "the card collapses in place");
        assert!(cards[0].text.contains("→ \"STM32\" → 4 hits"));
    }

    #[test]
    fn turn_done_stops_spinner_but_keeps_the_clock_for_turn_ended() {
        // TurnDone now only stops the spinner; it leaves `turn_started` intact so
        // the following TurnEnded can read the elapsed time. TurnEnded owns the
        // rest of teardown.
        let mut a = app();
        a.running = true;
        a.turn_started = Some(Instant::now());
        a.update(Msg::Agent(AgentEvent::TurnDone));
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
        // Completed → compact Codex-style worked-duration marker.
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::ToolStarted {
            name: "get_design".into(),
            args: serde_json::json!({}),
            seq: 0,
        }));
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        let last = a.transcript.last().unwrap();
        assert!(last.text.contains("Worked for"), "{}", last.text);
        assert!(!last.text.contains("tool call"), "{}", last.text);
        assert_eq!(last.level, NoticeLevel::Plain);
        assert!(!a.running && a.turn_started.is_none());

        // Safety cutoff → red, distinct from a clean completion.
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::ProviderRequestLimit {
            requests: 32,
        }));
        let last = a.transcript.last().unwrap();
        assert!(last.text.contains("32 model requests"), "{}", last.text);
        assert!(last.text.contains("safety limit"), "{}", last.text);
        assert_eq!(last.level, NoticeLevel::Error);

        // Artifact/review quality failure → red and explicit, never "completed".
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::QualityGateFailed {
            failures: 2,
        }));
        let last = a.transcript.last().unwrap();
        assert!(last.text.contains("quality gate failed"), "{}", last.text);
        assert!(last.text.contains("2 unresolved"), "{}", last.text);
        assert_eq!(last.level, NoticeLevel::Error);

        // Non-cancellable mutation timeout → red and explicit about background work.
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::MutationTimedOut));
        let last = a.transcript.last().unwrap();
        assert!(last.text.contains("mutation timed out"), "{}", last.text);
        assert!(
            last.text.contains("may still be finishing"),
            "{}",
            last.text
        );
        assert_eq!(last.level, NoticeLevel::Error);

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
            let done = Msg::Agent(AgentEvent::TurnDone);
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
                .filter(|e| e.text.contains("Worked for"))
                .count();
            assert_eq!(
                indicators, 1,
                "exactly one indicator (done_first={done_first})"
            );
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
    fn a_partial_paragraph_stays_off_screen_until_it_is_finished() {
        let mut a = app();
        // Mid-sentence deltas buffer; nothing reaches the transcript yet, so the
        // reader never watches a sentence assemble itself.
        a.update(Msg::Agent(AgentEvent::AssistantDelta("I'll ".into())));
        a.update(Msg::Agent(AgentEvent::AssistantDelta("search".into())));
        assert!(
            a.transcript.iter().all(|e| e.speaker != Speaker::Assistant),
            "a half-written paragraph does not render"
        );

        // Closing the paragraph publishes it — and only it.
        a.update(Msg::Agent(AgentEvent::AssistantDelta(
            " for the part.\n\nThen I".into(),
        )));
        let live = a
            .live_assistant
            .as_ref()
            .expect("the run is still open")
            .entry
            .expect("the first paragraph opened an entry");
        assert_eq!(a.transcript[live].text, "I'll search for the part.");
        assert_eq!(a.transcript[live].speaker, Speaker::Assistant);

        // The final text finalizes the same entry in place (no second entry).
        a.update(Msg::Agent(AgentEvent::AssistantText(
            "I'll search for the part.\n\nThen I'll wire it up.".into(),
        )));
        assert!(a.live_assistant.is_none(), "finalized: no live entry");
        let assistants: Vec<&Entry> = a
            .transcript
            .iter()
            .filter(|e| e.speaker == Speaker::Assistant)
            .collect();
        assert_eq!(
            assistants.len(),
            1,
            "still one assistant entry (finalized in place)"
        );
        assert_eq!(
            assistants[0].text,
            "I'll search for the part.\n\nThen I'll wire it up."
        );
    }

    #[test]
    fn each_finished_paragraph_lands_in_the_one_live_entry() {
        let mut a = app();
        for chunk in ["one.\n\n", "two.\n\n", "three"] {
            a.update(Msg::Agent(AgentEvent::AssistantDelta(chunk.into())));
        }
        let assistants: Vec<&str> = a
            .transcript
            .iter()
            .filter(|e| e.speaker == Speaker::Assistant)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(
            assistants,
            vec!["one.\n\ntwo."],
            "paragraphs grow ONE entry, and the unfinished tail is withheld"
        );
    }

    #[test]
    fn a_new_streamed_run_opens_a_fresh_live_entry() {
        // Prose, then tool work, then more prose: two separate streamed runs each
        // get their own finalized entry.
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantDelta("first".into())));
        a.update(Msg::Agent(AgentEvent::AssistantText("first".into())));
        a.update(Msg::Agent(AgentEvent::ToolStarted {
            name: "get_design".into(),
            args: serde_json::json!({}),
            seq: 0,
        }));
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
    fn turn_done_flushes_an_unfinalized_live_entry_instead_of_dropping_it() {
        // A regression guard: a short reply with no blank line in it never
        // reaches a paragraph break, so if the turn ends without a proper
        // `AssistantText` finalize (a missing or empty one — a provider quirk,
        // not something the UI can rely on never happening), TurnDone used to
        // just drop the buffered text. A completed turn must never show nothing
        // for a reply the model actually sent.
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantDelta(
            "Hi, I'm Gordian.".into(),
        )));
        assert!(a.live_assistant.is_some());
        a.update(Msg::Agent(AgentEvent::TurnDone));
        assert!(a.live_assistant.is_none(), "TurnDone closes the live entry");
        let assistants: Vec<&str> = a
            .transcript
            .iter()
            .filter(|e| e.speaker == Speaker::Assistant)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(
            assistants,
            vec!["Hi, I'm Gordian."],
            "the streamed reply is not lost just because it never got a paragraph break"
        );
    }

    #[test]
    fn a_normal_finalize_still_wins_over_the_turn_done_fallback() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantDelta("Hi".into())));
        a.update(Msg::Agent(AgentEvent::AssistantText("Hi there!".into())));
        a.update(Msg::Agent(AgentEvent::TurnDone));
        let assistants: Vec<&str> = a
            .transcript
            .iter()
            .filter(|e| e.speaker == Speaker::Assistant)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(
            assistants,
            vec!["Hi there!"],
            "AssistantText already took live_assistant, so TurnDone has nothing left to flush"
        );
    }
}
