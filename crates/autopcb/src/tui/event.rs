//! Translate raw crossterm key events into [`Msg`]s the [`App`] understands.
//!
//! Kept separate from the shell so the mapping is a pure function and easy to
//! reason about: the gate keys (`a`/`r`) are only special while a diff is
//! pending; otherwise everything routes to the input line.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::{App, Msg};

/// Map a key press to a [`Msg`], given the current [`App`] state (which decides
/// whether `a`/`r` are gate keys or plain text). Returns `None` for keys we
/// ignore.
pub fn map_key(app: &App, key: KeyEvent) -> Option<Msg> {
    // Only react to presses (Windows also emits Release/Repeat).
    if key.kind == KeyEventKind::Release {
        return None;
    }

    // Ctrl-C always quits.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(Msg::Cancel);
    }

    let gate_open = app.pending.is_some();

    match key.code {
        KeyCode::Enter => Some(Msg::Submit),
        KeyCode::Backspace => Some(Msg::Backspace),
        KeyCode::Esc => Some(Msg::Cancel),
        KeyCode::PageUp | KeyCode::Up => Some(Msg::ScrollUp),
        KeyCode::PageDown | KeyCode::Down => Some(Msg::ScrollDown),
        // While the apply-gate is open, a/r are decisions, not text.
        KeyCode::Char('a') if gate_open => Some(Msg::Approve),
        KeyCode::Char('r') if gate_open => Some(Msg::Reject),
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
    fn ctrl_c_cancels() {
        let a = app();
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(map_key(&a, k), Some(Msg::Cancel)));
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
