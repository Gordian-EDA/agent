//! The composer: the `/command` catalog and its Tab completion, plus the
//! readline-style input-line editing and history-recall helpers that mutate the
//! `input`/`cursor`/`history` fields.

use super::App;

/// One `/command` the input line accepts, for dispatch and Tab completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    /// Full spelling including the leading slash, e.g. `/help`.
    pub name: &'static str,
    /// One-line description shown in the completion popup and help.
    pub desc: &'static str,
}

/// Every command, in display order.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/help",
        desc: "show keys and commands",
    },
    CommandSpec {
        name: "/auto",
        desc: "toggle auto-approve (yolo)",
    },
    CommandSpec {
        name: "/undo",
        desc: "restore the previous schematic",
    },
    CommandSpec {
        name: "/clear",
        desc: "clear the transcript AND the agent's context",
    },
    CommandSpec {
        name: "/context",
        desc: "show project paths and context/token stats",
    },
    CommandSpec {
        name: "/compact",
        desc: "summarize the conversation to shrink context",
    },
    CommandSpec {
        name: "/preview",
        desc: "show the latest board/schematic render inline",
    },
    CommandSpec {
        name: "/quit",
        desc: "exit",
    },
];

impl App {
    // ── `/command` Tab completion ─────────────────────────────────────

    /// The commands matching a `/`-prefix stem (no completion once a space is
    /// typed — arguments are not completable).
    fn matches_for(stem: &str) -> Vec<&'static CommandSpec> {
        if !stem.starts_with('/') || stem.contains(' ') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .filter(|c| c.name.starts_with(stem))
            .collect()
    }

    /// What the completion popup should show for the current input: the
    /// matching commands and which one (if any) the input currently is.
    /// `None` when completion does not apply.
    pub fn completion_view(&self) -> Option<(Vec<&'static CommandSpec>, Option<usize>)> {
        if self.pending.is_some() {
            return None;
        }
        let stem = self.completion_stem.as_deref().unwrap_or(&self.input);
        let matches = Self::matches_for(stem);
        if matches.is_empty() {
            return None;
        }
        Some((matches, self.completion_idx))
    }

    /// Tab: fill the input with the next command matching the typed stem.
    pub(super) fn complete_next(&mut self) {
        let stem = self
            .completion_stem
            .clone()
            .unwrap_or_else(|| self.input.clone());
        let matches = Self::matches_for(&stem);
        if matches.is_empty() {
            return;
        }
        let idx = match self.completion_idx {
            Some(i) => (i + 1) % matches.len(),
            None => 0,
        };
        self.completion_stem = Some(stem);
        self.completion_idx = Some(idx);
        self.input = matches[idx].name.to_string();
        self.cursor = self.char_len();
    }

    // ── input-line editing helpers ────────────────────────────────────

    pub(super) fn char_len(&self) -> usize {
        self.input.chars().count()
    }

    /// Byte offset of the `n`th char (or the end of the string).
    pub(super) fn byte_at(&self, n: usize) -> usize {
        self.input
            .char_indices()
            .nth(n)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    pub(super) fn insert_char(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.input.insert(at, c);
        self.cursor += 1;
    }

    /// Ctrl-W: delete trailing spaces before the cursor, then the word.
    pub(super) fn kill_word_back(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut new_cursor = self.cursor;
        while new_cursor > 0 && chars[new_cursor - 1].is_whitespace() {
            new_cursor -= 1;
        }
        while new_cursor > 0 && !chars[new_cursor - 1].is_whitespace() {
            new_cursor -= 1;
        }
        let from = self.byte_at(new_cursor);
        let to = self.byte_at(self.cursor);
        self.input.drain(from..to);
        self.cursor = new_cursor;
    }

    pub(super) fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_pos = None;
        self.completion_stem = None;
        self.completion_idx = None;
    }

    /// Up: step back through history, stashing the live draft first.
    pub(super) fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.draft = std::mem::take(&mut self.input);
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.history_pos = Some(pos);
        self.input = self.history[pos].clone();
        self.cursor = self.char_len();
    }

    /// Down: step forward; past the newest entry, restore the live draft.
    pub(super) fn history_next(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            self.input = self.history[pos + 1].clone();
        } else {
            self.history_pos = None;
            self.input = std::mem::take(&mut self.draft);
        }
        self.cursor = self.char_len();
    }
}
