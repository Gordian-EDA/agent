//! Translate raw crossterm key events into [`Msg`]s the [`App`] understands.
//!
//! Kept separate from the shell so the mapping is a pure function and easy to
//! reason about: the gate keys (`a`/`r`) are only special while a diff is
//! active; otherwise everything routes to the input line. Up/Down always
//! recall prompt history while editing — never the transcript, which is what
//! makes the split with the mouse wheel legible; the transcript scrolls via
//! Shift/Ctrl/Alt-Up/Down, PageUp/PageDown, and the (now captured, see
//! `super::mod`) mouse wheel.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::{App, Msg};

/// Map a key press to a [`Msg`], given the current [`App`] state (which decides
/// how navigation keys behave). Returns `None` for keys we ignore.
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

    // Dedicated transcript scroll chords. They are handled before the generic
    // Control block so Ctrl-Up/Down do not get swallowed as unknown control
    // chords.
    if key
        .modifiers
        .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        match key.code {
            KeyCode::Up => return Some(Msg::ScrollUp),
            KeyCode::Down => return Some(Msg::ScrollDown),
            _ => {}
        }
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
            KeyCode::Left => Some(Msg::WordLeft),
            KeyCode::Right => Some(Msg::WordRight),
            // Ctrl-End snaps to the live tail from anywhere, even mid-edit.
            KeyCode::End => Some(Msg::ScrollToBottom),
            _ => None,
        };
    }

    match key.code {
        KeyCode::Enter => Some(Msg::Submit),
        KeyCode::Tab => Some(Msg::Complete),
        KeyCode::Backspace => Some(Msg::Backspace),
        KeyCode::Delete => Some(Msg::Delete),
        KeyCode::Left => Some(Msg::CursorLeft),
        KeyCode::Right => Some(Msg::CursorRight),
        KeyCode::Home => Some(Msg::Home),
        // With nothing to move a cursor through, bare End means "catch me up" —
        // the same empty-prompt rule the arrows already follow.
        KeyCode::End if app.input.is_empty() && app.scroll > 0 => Some(Msg::ScrollToBottom),
        KeyCode::End => Some(Msg::End),
        KeyCode::Esc => Some(Msg::Cancel),
        // A full screenful jump; the height tracks the last-drawn viewport.
        KeyCode::PageUp => Some(Msg::PageUp(app.viewport_h)),
        KeyCode::PageDown => Some(Msg::PageDown(app.viewport_h)),
        // Up/Down edit history while the input line is live — always, so the
        // split with the mouse wheel (which now arrives as a real `Event::
        // Mouse`, distinct from a key press) stays simple to reason about.
        KeyCode::Up if app.input_active() => Some(Msg::HistoryPrev),
        KeyCode::Down if app.input_active() => Some(Msg::HistoryNext),
        KeyCode::Up => Some(Msg::ScrollUp),
        KeyCode::Down => Some(Msg::ScrollDown),
        KeyCode::Char(c) => Some(Msg::Char(c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Status;

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
        assert!(matches!(
            map_key(&a, KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
            Some(Msg::WordLeft)
        ));
        assert!(matches!(
            map_key(&a, KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)),
            Some(Msg::WordRight)
        ));
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
    fn up_down_recall_history_from_an_empty_prompt_even_with_scrollback() {
        // Regression guard: Up/Down must never fall back to scrolling just
        // because there's scrollback to move through — that ambiguity is
        // exactly what mouse capture (see `super::super::mod`) exists to
        // remove. The wheel arrives as its own `Event::Mouse` and is mapped
        // independently in the shell, not through this function at all.
        let mut a = app();
        a.scroll_max = 10;
        assert!(matches!(
            map_key(&a, key(KeyCode::Up)),
            Some(Msg::HistoryPrev)
        ));

        a.scroll = 3;
        assert!(matches!(
            map_key(&a, key(KeyCode::Down)),
            Some(Msg::HistoryNext)
        ));
    }

    #[test]
    fn end_catches_up_from_an_empty_prompt_but_still_edits_a_draft() {
        let mut a = app();
        a.scroll = 5;
        assert!(matches!(
            map_key(&a, key(KeyCode::End)),
            Some(Msg::ScrollToBottom)
        ));

        // With a draft in hand End is line editing again — the cursor has
        // somewhere to go, so it wins over the scroll shortcut.
        a.input = "draft".to_string();
        assert!(matches!(map_key(&a, key(KeyCode::End)), Some(Msg::End)));

        // ...and Ctrl-End catches up regardless.
        assert!(matches!(
            map_key(&a, KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)),
            Some(Msg::ScrollToBottom)
        ));
    }

    #[test]
    fn end_is_line_editing_when_already_at_the_tail() {
        let a = app();
        assert!(matches!(map_key(&a, key(KeyCode::End)), Some(Msg::End)));
    }

    #[test]
    fn up_down_keep_history_when_the_prompt_has_text() {
        let mut a = app();
        a.scroll_max = 10;
        a.input = "draft".to_string();
        a.cursor = a.input.chars().count();

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
    fn modified_arrows_scroll_even_when_input_is_live() {
        let a = app();
        let shift_up = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
        let ctrl_down = KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL);

        assert!(matches!(map_key(&a, shift_up), Some(Msg::ScrollUp)));
        assert!(matches!(map_key(&a, ctrl_down), Some(Msg::ScrollDown)));
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
    fn char_routes_to_input() {
        let a = app();
        assert!(matches!(
            map_key(&a, key(KeyCode::Char('x'))),
            Some(Msg::Char('x'))
        ));
    }

    #[test]
    fn a_key_is_plain_text() {
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
