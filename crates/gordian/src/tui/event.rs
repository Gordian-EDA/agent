//! Translate raw crossterm key events into [`Msg`]s the [`App`] understands.
//!
//! Kept separate from the shell so the mapping is a pure function and easy to
//! reason about: the gate keys (`a`/`r`) are only special while a diff is
//! pending; otherwise everything routes to the input line. Up/Down recall
//! prompt history (like a shell); PageUp/PageDown and the mouse wheel scroll
//! the transcript.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::{App, Msg};

/// The apply-gate keys, defined once here — the single source of truth shared by
/// the key mapping below and the gate card's hint text ([`super::ui`]), so a key
/// and its on-screen label can never drift apart.
pub const APPROVE_KEY: char = 'a';
pub const REJECT_KEY: char = 'r';

/// Map a key press to a [`Msg`], given the current [`App`] state (which decides
/// whether the gate keys are decisions or plain text). Returns `None` for keys we
/// ignore.
pub fn map_key(app: &App, key: KeyEvent) -> Option<Msg> {
    // Only react to presses (Windows also emits Release/Repeat).
    if key.kind == KeyEventKind::Release {
        return None;
    }

    // Shift/Alt+Enter inserts a newline (a multi-line prompt) rather than
    // submitting. Checked before the Control block so a bare Enter still submits.
    if key.code == KeyCode::Enter
        && key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
    {
        return Some(Msg::Newline);
    }

    // Control chords (readline-style line editing + hard quit).
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') if key.kind == KeyEventKind::Press => Some(Msg::ForceQuit),
            KeyCode::Char('c') => None,
            KeyCode::Char('u') => Some(Msg::KillToStart),
            KeyCode::Char('w') => Some(Msg::KillWordBack),
            KeyCode::Char('a') => Some(Msg::Home),
            KeyCode::Char('e') => Some(Msg::End),
            _ => None,
        };
    }

    let gate_open = app.pending.is_some();

    match key.code {
        KeyCode::Enter => Some(Msg::Submit),
        KeyCode::Tab => Some(Msg::Complete),
        KeyCode::Backspace => Some(Msg::Backspace),
        KeyCode::Delete => Some(Msg::Delete),
        KeyCode::Left => Some(Msg::CursorLeft),
        KeyCode::Right => Some(Msg::CursorRight),
        KeyCode::Home => Some(Msg::Home),
        KeyCode::End => Some(Msg::End),
        KeyCode::Esc => Some(Msg::Cancel),
        // A full screenful jump; the height tracks the last-drawn viewport.
        KeyCode::PageUp => Some(Msg::PageUp(app.viewport_h)),
        KeyCode::PageDown => Some(Msg::PageDown(app.viewport_h)),
        // Up/Down edit history while the input line is live; with the gate
        // open they fall back to scrolling the transcript.
        KeyCode::Up if app.input_active() => Some(Msg::HistoryPrev),
        KeyCode::Down if app.input_active() => Some(Msg::HistoryNext),
        KeyCode::Up => Some(Msg::ScrollUp),
        KeyCode::Down => Some(Msg::ScrollDown),
        // While the apply-gate is open, the gate keys are decisions, not text.
        KeyCode::Char(APPROVE_KEY) if gate_open => Some(Msg::Approve),
        KeyCode::Char(REJECT_KEY) if gate_open => Some(Msg::Reject),
        KeyCode::Char(c) => Some(Msg::Char(c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Action, Status};
    use serde_json::json;

    fn app() -> App {
        App::new(Status::new("bedrock", "m", "/tmp/p.kicad_sch", true))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn enter_maps_to_submit() {
        let a = app();
        assert!(matches!(
            map_key(&a, key(KeyCode::Enter)),
            Some(Msg::Submit)
        ));
    }

    #[test]
    fn esc_maps_to_cancel() {
        let a = app();
        assert!(matches!(map_key(&a, key(KeyCode::Esc)), Some(Msg::Cancel)));
    }

    #[test]
    fn tab_maps_to_complete() {
        let a = app();
        assert!(matches!(
            map_key(&a, key(KeyCode::Tab)),
            Some(Msg::Complete)
        ));
    }

    #[test]
    fn ctrl_c_maps_to_quit_request() {
        let a = app();
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(map_key(&a, k), Some(Msg::ForceQuit)));
    }

    #[test]
    fn ctrl_c_repeat_does_not_count_as_a_second_press() {
        let a = app();
        let k = KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        );
        assert!(map_key(&a, k).is_none());
    }

    #[test]
    fn readline_chords_map_to_editing_msgs() {
        let a = app();
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(matches!(map_key(&a, ctrl('u')), Some(Msg::KillToStart)));
        assert!(matches!(map_key(&a, ctrl('w')), Some(Msg::KillWordBack)));
        assert!(matches!(map_key(&a, ctrl('a')), Some(Msg::Home)));
        assert!(matches!(map_key(&a, ctrl('e')), Some(Msg::End)));
    }

    #[test]
    fn arrows_move_the_cursor() {
        let a = app();
        assert!(matches!(
            map_key(&a, key(KeyCode::Left)),
            Some(Msg::CursorLeft)
        ));
        assert!(matches!(
            map_key(&a, key(KeyCode::Right)),
            Some(Msg::CursorRight)
        ));
        assert!(matches!(map_key(&a, key(KeyCode::Home)), Some(Msg::Home)));
        assert!(matches!(map_key(&a, key(KeyCode::End)), Some(Msg::End)));
    }

    #[test]
    fn up_recalls_history_when_input_is_live() {
        let a = app();
        assert!(matches!(
            map_key(&a, key(KeyCode::Up)),
            Some(Msg::HistoryPrev)
        ));
        assert!(matches!(
            map_key(&a, key(KeyCode::Down)),
            Some(Msg::HistoryNext)
        ));
    }

    #[test]
    fn up_scrolls_when_the_gate_is_open() {
        let mut a = app();
        a.update(Msg::PendingDiff(json!({
            "diff": { "added": ["U1"], "removed": [], "changed": [] }
        })));
        assert!(matches!(map_key(&a, key(KeyCode::Up)), Some(Msg::ScrollUp)));
    }

    #[test]
    fn page_keys_jump_by_a_viewport() {
        let mut a = app();
        a.viewport_h = 17;
        assert!(matches!(
            map_key(&a, key(KeyCode::PageUp)),
            Some(Msg::PageUp(17))
        ));
        assert!(matches!(
            map_key(&a, key(KeyCode::PageDown)),
            Some(Msg::PageDown(17))
        ));
    }

    #[test]
    fn shift_or_alt_enter_inserts_a_newline_not_a_submit() {
        let a = app();
        let shift = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        let alt = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert!(matches!(map_key(&a, shift), Some(Msg::Newline)));
        assert!(matches!(map_key(&a, alt), Some(Msg::Newline)));
        // A bare Enter still submits.
        assert!(matches!(
            map_key(&a, key(KeyCode::Enter)),
            Some(Msg::Submit)
        ));
    }

    #[test]
    fn char_routes_to_input_when_no_gate() {
        let a = app();
        assert!(matches!(
            map_key(&a, key(KeyCode::Char('x'))),
            Some(Msg::Char('x'))
        ));
    }

    #[test]
    fn a_key_resolves_gate_when_pending() {
        let mut a = app();
        a.update(Msg::PendingDiff(json!({
            "diff": { "added": ["U1"], "removed": [], "changed": [] }
        })));
        // With the gate open, 'a' maps to Approve and resolves it.
        let msg = map_key(&a, key(KeyCode::Char('a'))).unwrap();
        assert!(matches!(msg, Msg::Approve));
        assert_eq!(a.update(msg), Action::ResolveApproval(true));
    }

    #[test]
    fn a_key_is_plain_text_when_no_gate() {
        let a = app();
        assert!(matches!(
            map_key(&a, key(KeyCode::Char('a'))),
            Some(Msg::Char('a'))
        ));
    }

    #[test]
    fn release_events_are_ignored() {
        let a = app();
        let k = KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);
        assert!(map_key(&a, k).is_none());
    }
}
