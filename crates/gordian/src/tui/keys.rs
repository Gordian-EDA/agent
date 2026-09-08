//! Raw crossterm key events into [`Msg`]s, as a pure function.
//!
//! Up/Down always recall prompt history while editing — never the transcript,
//! which is what keeps the split with the mouse wheel legible; the transcript
//! scrolls via Shift/Ctrl/Alt-Up/Down, PageUp/PageDown and the (captured) wheel.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::{App, Msg};

/// Map a key press to a [`Msg`], or `None` for a key the cockpit ignores.
pub fn map_key(app: &App, key: KeyEvent) -> Option<Msg> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    // Shift/Alt+Enter is a newline, not a submit. Checked before the Control
    // block so a bare Enter still submits.
    if key.code == KeyCode::Enter
        && key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
    {
        return Some(Msg::Newline);
    }

    // The scroll chords, before the generic Control block so Ctrl-Up/Down are
    // not swallowed as unknown chords.
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
        KeyCode::Backspace => Some(Msg::Backspace),
        KeyCode::Delete => Some(Msg::Delete),
        KeyCode::Left => Some(Msg::CursorLeft),
        KeyCode::Right => Some(Msg::CursorRight),
        KeyCode::Home => Some(Msg::Home),
        // With nothing to move a cursor through, bare End means "catch me up".
        KeyCode::End if app.input.is_empty() && app.scroll > 0 => Some(Msg::ScrollToBottom),
        KeyCode::End => Some(Msg::End),
        KeyCode::Esc => Some(Msg::Cancel),
        KeyCode::PageUp => Some(Msg::PageUp(app.viewport_h)),
        KeyCode::PageDown => Some(Msg::PageDown(app.viewport_h)),
        KeyCode::Up => Some(Msg::HistoryPrev),
        KeyCode::Down => Some(Msg::HistoryNext),
        KeyCode::Char(c) => Some(Msg::Char(c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Status;
    use std::path::PathBuf;

    fn app() -> App {
        App::new(Status::new("openai", "m", PathBuf::from("/tmp/p")))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_composer_keys_map_to_line_editing() {
        let a = app();
        assert!(matches!(map_key(&a, key(KeyCode::Enter)), Some(Msg::Submit)));
        assert!(matches!(map_key(&a, key(KeyCode::Esc)), Some(Msg::Cancel)));
        assert!(matches!(
            map_key(&a, key(KeyCode::Char('x'))),
            Some(Msg::Char('x'))
        ));
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(matches!(map_key(&a, ctrl('u')), Some(Msg::KillToStart)));
        assert!(matches!(map_key(&a, ctrl('w')), Some(Msg::KillWordBack)));
        assert!(matches!(map_key(&a, ctrl('c')), Some(Msg::ForceQuit)));
    }

    /// Up/Down are history; only a modifier (or the wheel) scrolls, so the two
    /// never have to be guessed apart.
    #[test]
    fn arrows_recall_history_and_modified_arrows_scroll() {
        let mut a = app();
        a.scroll_max = 10;
        assert!(matches!(
            map_key(&a, key(KeyCode::Up)),
            Some(Msg::HistoryPrev)
        ));
        assert!(matches!(
            map_key(&a, KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)),
            Some(Msg::ScrollUp)
        ));
    }

    #[test]
    fn shift_enter_is_a_newline_and_page_keys_jump_a_viewport() {
        let mut a = app();
        a.viewport_h = 17;
        assert!(matches!(
            map_key(&a, KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(Msg::Newline)
        ));
        assert!(matches!(
            map_key(&a, key(KeyCode::PageUp)),
            Some(Msg::PageUp(17))
        ));
    }

    /// End catches up from an empty composer but still edits a draft.
    #[test]
    fn end_catches_up_only_when_there_is_nothing_to_edit() {
        let mut a = app();
        a.scroll = 5;
        assert!(matches!(
            map_key(&a, key(KeyCode::End)),
            Some(Msg::ScrollToBottom)
        ));
        a.input = "draft".to_string();
        assert!(matches!(map_key(&a, key(KeyCode::End)), Some(Msg::End)));
    }

    #[test]
    fn release_events_are_ignored() {
        let a = app();
        let k = KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);
        assert!(map_key(&a, k).is_none());
    }
}
