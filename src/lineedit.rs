//! Line buffer and history for the `:` command prompt (pure).
//!
//! The editor is a pure `(buffer, key) -> (buffer, effect)` machine. `input.rs`
//! renders the buffer and feeds it keys; everything here is testable without a
//! terminal. That split is deliberate (PLANS.md section 5.2): line editing needs
//! raw mode and so do hotkeys, so one owner (this module plus `input.rs`) holds
//! the terminal and no second library fights it for termios.

use crate::keys::{Chord, KeyCode};

/// Hard cap on the buffer, so a paste bomb cannot grow it without bound.
pub const MAX_BUFFER_CHARS: usize = 4096;
/// History depth kept in memory. Never persisted.
pub const MAX_HISTORY: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditEffect {
    /// The buffer changed; redraw it.
    Redraw,
    /// Nothing to redraw (e.g. a no-op key).
    Ignored,
    /// Enter: the line is complete.
    Submit(String),
    /// Enter on an empty line: nothing to run.
    SubmitEmpty,
    /// Esc or Ctrl+C: leave command mode, discarding the buffer.
    Cancel,
    /// The user asked for the whole keymap (Ctrl+? style help shortcut).
    ShowHelp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    /// The line being edited.
    buffer: String,
    /// Cursor position as a character index, always on a character boundary.
    cursor: usize,
    /// Most recent entries, oldest first.
    history: Vec<String>,
    /// Index into `history` while browsing; `None` means "editing a fresh line".
    browsing: Option<usize>,
    /// The line that was in progress before browsing started.
    draft: String,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            history: Vec::new(),
            browsing: None,
            draft: String::new(),
        }
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Cursor position as a character index. Exposed for tests and for the prompt.
    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Clear the buffer for a fresh line, keeping history.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.browsing = None;
        self.draft.clear();
    }

    /// Replace the buffer wholesale, e.g. for a multi-character paste.
    #[cfg(test)]
    pub fn push_str(&mut self, text: &str) -> EditEffect {
        let mut changed = false;
        for c in text.chars() {
            // Newlines never enter the buffer; a paste of a whole script is
            // stopped here rather than silently joined into one giant command.
            if c == '\n' || c == '\r' {
                continue;
            }
            if self.insert_char(c) {
                changed = true;
            }
        }
        if changed {
            EditEffect::Redraw
        } else {
            EditEffect::Ignored
        }
    }

    /// Apply one keystroke.
    pub fn apply(&mut self, chord: Chord) -> EditEffect {
        // Control chords first: they are not text.
        if chord.mods.ctrl {
            return match chord.code {
                // Cancelling throws the line away; leaving it in the buffer would
                // make a later Enter run a command the user already abandoned.
                KeyCode::Char('c') => {
                    self.reset();
                    EditEffect::Cancel
                }
                KeyCode::Char('d') => {
                    if self.buffer.is_empty() {
                        EditEffect::Cancel
                    } else {
                        self.delete_forward();
                        EditEffect::Redraw
                    }
                }
                KeyCode::Char('u') => {
                    if self.buffer.is_empty() {
                        EditEffect::Ignored
                    } else {
                        self.buffer.clear();
                        self.cursor = 0;
                        EditEffect::Redraw
                    }
                }
                KeyCode::Char('w') => {
                    if self.delete_word() {
                        EditEffect::Redraw
                    } else {
                        EditEffect::Ignored
                    }
                }
                KeyCode::Char('a') => {
                    self.cursor = 0;
                    EditEffect::Redraw
                }
                KeyCode::Char('e') => {
                    self.cursor = self.char_len();
                    EditEffect::Redraw
                }
                KeyCode::Char('h') => {
                    self.delete_backward();
                    EditEffect::Redraw
                }
                KeyCode::Char('?') => EditEffect::ShowHelp,
                _ => EditEffect::Ignored,
            };
        }

        match chord.code {
            KeyCode::Enter => {
                let line = self.buffer.trim().to_string();
                if line.is_empty() {
                    self.reset();
                    EditEffect::SubmitEmpty
                } else {
                    self.remember(&line);
                    self.reset();
                    EditEffect::Submit(line)
                }
            }
            KeyCode::Esc => {
                self.reset();
                EditEffect::Cancel
            }
            KeyCode::Backspace => {
                if self.delete_backward() {
                    EditEffect::Redraw
                } else {
                    EditEffect::Ignored
                }
            }
            KeyCode::Delete => {
                self.delete_forward();
                EditEffect::Redraw
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                EditEffect::Redraw
            }
            KeyCode::Right => {
                if self.cursor < self.char_len() {
                    self.cursor += 1;
                }
                EditEffect::Redraw
            }
            KeyCode::Home => {
                self.cursor = 0;
                EditEffect::Redraw
            }
            KeyCode::End => {
                self.cursor = self.char_len();
                EditEffect::Redraw
            }
            KeyCode::Up => {
                self.history_prev();
                EditEffect::Redraw
            }
            KeyCode::Down => {
                self.history_next();
                EditEffect::Redraw
            }
            KeyCode::Tab => EditEffect::Ignored,
            KeyCode::Space => {
                self.insert_char(' ');
                EditEffect::Redraw
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                EditEffect::Redraw
            }
        }
    }

    fn char_len(&self) -> usize {
        self.buffer.chars().count()
    }

    /// Byte offset of a character index. Keeps the cursor out of the middle of a
    /// multi-byte character, which is what would corrupt Arabic or Chinese text.
    fn byte_offset(&self, char_index: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(char_index)
            .map(|(i, _)| i)
            .unwrap_or(self.buffer.len())
    }

    /// Returns true when the buffer changed.
    fn insert_char(&mut self, c: char) -> bool {
        if self.char_len() >= MAX_BUFFER_CHARS {
            return false;
        }
        let at = self.byte_offset(self.cursor);
        self.buffer.insert(at, c);
        self.cursor += 1;
        // Editing a line ends the history browsing session.
        self.browsing = None;
        true
    }

    fn delete_backward(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = self.byte_offset(self.cursor - 1);
        let end = self.byte_offset(self.cursor);
        self.buffer.replace_range(start..end, "");
        self.cursor -= 1;
        true
    }

    fn delete_forward(&mut self) -> bool {
        if self.cursor >= self.char_len() {
            return false;
        }
        let start = self.byte_offset(self.cursor);
        let end = self.byte_offset(self.cursor + 1);
        self.buffer.replace_range(start..end, "");
        true
    }

    /// Ctrl+W: delete the word before the cursor, plus any spaces before it.
    fn delete_word(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let chars: Vec<char> = self.buffer.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let from = self.byte_offset(start);
        let to = self.byte_offset(self.cursor);
        self.buffer.replace_range(from..to, "");
        self.cursor = start;
        true
    }

    fn remember(&mut self, line: &str) {
        // Do not store consecutive duplicates: pressing Enter twice on the same
        // command should not fill the history with copies.
        if self.history.last().map(|l| l.as_str()) != Some(line) {
            self.history.push(line.to_string());
        }
        while self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.browsing {
            None => {
                self.draft = self.buffer.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.browsing = Some(next);
        self.load_history(next);
    }

    fn history_next(&mut self) {
        let Some(current) = self.browsing else {
            return;
        };
        if current + 1 < self.history.len() {
            let next = current + 1;
            self.browsing = Some(next);
            self.load_history(next);
        } else {
            // Past the newest entry: restore the line that was being typed.
            self.browsing = None;
            self.buffer = std::mem::take(&mut self.draft);
            self.cursor = self.char_len();
        }
    }

    fn load_history(&mut self, index: usize) {
        if let Some(line) = self.history.get(index).cloned() {
            self.buffer = line;
            self.cursor = self.char_len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Mods;

    fn ch(c: char) -> Chord {
        Chord::char(c)
    }

    fn key(code: KeyCode) -> Chord {
        Chord::new(code)
    }

    fn ctrl(c: char) -> Chord {
        Chord::with(KeyCode::Char(c), Mods::ctrl())
    }

    fn typed(s: &str) -> Editor {
        let mut e = Editor::new();
        for c in s.chars() {
            e.apply(ch(c));
        }
        e
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut e = typed("abc");
        assert_eq!(e.buffer(), "abc");
        assert_eq!(e.cursor(), 3);
        e.apply(key(KeyCode::Left));
        e.apply(ch('X'));
        assert_eq!(e.buffer(), "abXc");
        assert_eq!(e.cursor(), 3);
    }

    #[test]
    fn backspace_and_delete_do_the_right_thing() {
        let mut e = typed("abc");
        assert_eq!(e.apply(key(KeyCode::Backspace)), EditEffect::Redraw);
        assert_eq!(e.buffer(), "ab");
        e.apply(key(KeyCode::Home));
        assert_eq!(e.apply(key(KeyCode::Delete)), EditEffect::Redraw);
        assert_eq!(e.buffer(), "b");
    }

    #[test]
    fn backspace_at_the_start_and_delete_at_the_end_are_ignored() {
        let mut e = typed("ab");
        e.apply(key(KeyCode::Home));
        assert_eq!(e.apply(key(KeyCode::Backspace)), EditEffect::Ignored);
        e.apply(key(KeyCode::End));
        assert_eq!(e.apply(key(KeyCode::Delete)), EditEffect::Redraw);
        assert_eq!(e.buffer(), "ab");
    }

    #[test]
    fn home_end_and_arrows_clamp_at_the_edges() {
        let mut e = typed("ab");
        e.apply(key(KeyCode::Left));
        e.apply(key(KeyCode::Left));
        e.apply(key(KeyCode::Left));
        assert_eq!(e.cursor(), 0);
        e.apply(key(KeyCode::Right));
        assert_eq!(e.cursor(), 1);
        e.apply(key(KeyCode::End));
        assert_eq!(e.cursor(), 2);
        e.apply(key(KeyCode::End));
        assert_eq!(e.cursor(), 2);
        e.apply(key(KeyCode::Home));
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn enter_submits_the_trimmed_line_and_clears_the_buffer() {
        let mut e = typed("  seek +5  ");
        match e.apply(key(KeyCode::Enter)) {
            EditEffect::Submit(line) => assert_eq!(line, "seek +5"),
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(e.buffer(), "");
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn enter_on_an_empty_line_reports_an_empty_submit() {
        let mut e = Editor::new();
        assert_eq!(e.apply(key(KeyCode::Enter)), EditEffect::SubmitEmpty);
        let mut e = typed("   ");
        assert_eq!(e.apply(key(KeyCode::Enter)), EditEffect::SubmitEmpty);
    }

    #[test]
    fn esc_and_ctrl_c_cancel_without_running_anything() {
        let mut e = typed("pause");
        assert_eq!(e.apply(key(KeyCode::Esc)), EditEffect::Cancel);
        let mut e = typed("pause");
        assert_eq!(e.apply(ctrl('c')), EditEffect::Cancel);
    }

    #[test]
    fn ctrl_u_clears_and_ctrl_w_deletes_a_word() {
        let mut e = typed("ab clear");
        e.apply(ctrl('w'));
        assert_eq!(e.buffer(), "ab ");
        e.apply(ctrl('w'));
        assert_eq!(e.buffer(), "");
        let mut e = typed("hello");
        e.apply(ctrl('u'));
        assert_eq!(e.buffer(), "");
        assert_eq!(e.apply(ctrl('u')), EditEffect::Ignored);
    }

    #[test]
    fn ctrl_a_and_ctrl_e_move_to_the_ends() {
        let mut e = typed("abc");
        e.apply(ctrl('a'));
        assert_eq!(e.cursor(), 0);
        e.apply(ctrl('e'));
        assert_eq!(e.cursor(), 3);
    }

    #[test]
    fn ctrl_d_cancels_when_empty_and_deletes_otherwise() {
        let mut e = typed("ab");
        e.apply(ctrl('a'));
        assert_eq!(e.apply(ctrl('d')), EditEffect::Redraw);
        assert_eq!(e.buffer(), "b");
        let mut e = Editor::new();
        assert_eq!(e.apply(ctrl('d')), EditEffect::Cancel);
    }

    #[test]
    fn history_recalls_newest_first_and_wraps_back_to_the_draft() {
        let mut e = Editor::new();
        for line in ["play", "pause"] {
            for c in line.chars() {
                e.apply(ch(c));
            }
            e.apply(key(KeyCode::Enter));
        }
        assert_eq!(e.history(), &["play".to_string(), "pause".to_string()]);

        for c in "stat".chars() {
            e.apply(ch(c));
        }
        e.apply(key(KeyCode::Up));
        assert_eq!(e.buffer(), "pause");
        e.apply(key(KeyCode::Up));
        assert_eq!(e.buffer(), "play");
        // At the oldest entry, another Up stays put.
        e.apply(key(KeyCode::Up));
        assert_eq!(e.buffer(), "play");

        e.apply(key(KeyCode::Down));
        assert_eq!(e.buffer(), "pause");
        e.apply(key(KeyCode::Down));
        assert_eq!(
            e.buffer(),
            "stat",
            "past the newest entry the draft comes back"
        );
    }

    #[test]
    fn history_skips_consecutive_duplicates() {
        let mut e = Editor::new();
        for _ in 0..3 {
            for c in "play".chars() {
                e.apply(ch(c));
            }
            e.apply(key(KeyCode::Enter));
        }
        assert_eq!(e.history().len(), 1);
    }

    #[test]
    fn history_is_bounded() {
        let mut e = Editor::new();
        for i in 0..(MAX_HISTORY + 25) {
            e.push_str(&format!("goto {i}"));
            e.apply(key(KeyCode::Enter));
        }
        assert_eq!(e.history().len(), MAX_HISTORY);
        // The oldest entries were dropped, the newest kept.
        assert!(e
            .history()
            .last()
            .unwrap()
            .contains(&format!("{}", MAX_HISTORY + 24)));
        assert!(e.history().first().unwrap().contains("25"));
    }

    #[test]
    fn history_does_not_grow_from_empty_submits() {
        let mut e = Editor::new();
        e.apply(key(KeyCode::Enter));
        e.apply(key(KeyCode::Enter));
        assert!(e.history().is_empty());
    }

    #[test]
    fn editing_ends_the_history_browsing_session() {
        let mut e = Editor::new();
        e.push_str("play");
        e.apply(key(KeyCode::Enter));
        e.apply(key(KeyCode::Up));
        assert_eq!(e.buffer(), "play");
        e.apply(ch('x'));
        assert_eq!(e.buffer(), "playx");
        // Typing clears browsing, so Down no longer has a place to return to.
        e.apply(key(KeyCode::Down));
        assert_eq!(e.buffer(), "playx");
    }

    #[test]
    fn multibyte_characters_edit_one_unit_at_a_time() {
        // Arabic (RTL), Chinese, and an emoji: the cursor is a character index, so
        // deleting must never split a code point.
        for text in ["عربي", "中文测试", "a🎧b"] {
            let mut e = typed(text);
            assert_eq!(e.buffer(), text);
            assert_eq!(e.cursor(), text.chars().count());
            e.apply(key(KeyCode::Backspace));
            let expected: String = text.chars().take(text.chars().count() - 1).collect();
            assert_eq!(e.buffer(), expected, "backspace mangled `{text}`");
            e.apply(key(KeyCode::End));
            e.apply(ch('!'));
            assert_eq!(e.buffer(), format!("{expected}!"));
        }
    }

    #[test]
    fn cursor_movement_isolates_a_single_multibyte_character() {
        let mut e = typed("中文");
        e.apply(key(KeyCode::Left));
        assert_eq!(e.cursor(), 1);
        e.apply(key(KeyCode::Delete));
        assert_eq!(
            e.buffer(),
            "中",
            "delete removed the character after the cursor"
        );
        // Backspace at the very start has nothing to remove.
        e.apply(key(KeyCode::Left));
        assert_eq!(e.cursor(), 0);
        assert_eq!(e.apply(key(KeyCode::Backspace)), EditEffect::Ignored);
        assert_eq!(e.buffer(), "中");
        e.apply(key(KeyCode::End));
        e.apply(key(KeyCode::Backspace));
        assert_eq!(e.buffer(), "");
    }

    #[test]
    fn ctrl_w_handles_multibyte_words() {
        let mut e = typed("seek 中文");
        e.apply(ctrl('w'));
        assert_eq!(e.buffer(), "seek ");
    }

    #[test]
    fn the_buffer_is_bounded_even_under_a_paste_bomb() {
        let mut e = Editor::new();
        let huge = "x".repeat(MAX_BUFFER_CHARS * 2);
        e.push_str(&huge);
        assert_eq!(e.buffer().chars().count(), MAX_BUFFER_CHARS);
        // Still usable afterwards.
        e.apply(key(KeyCode::Backspace));
        assert_eq!(e.buffer().chars().count(), MAX_BUFFER_CHARS - 1);
    }

    #[test]
    fn a_pasted_newline_is_never_inserted_into_the_buffer() {
        // A pasted script must not become one line containing newlines.
        let mut e = Editor::new();
        e.push_str("play\npause\r\n");
        assert_eq!(e.buffer(), "playpause");
        assert!(!e.buffer().contains('\n'));
    }

    #[test]
    fn tab_and_unknown_control_keys_are_ignored() {
        let mut e = typed("ab");
        assert_eq!(e.apply(key(KeyCode::Tab)), EditEffect::Ignored);
        assert_eq!(e.apply(ctrl('q')), EditEffect::Ignored);
        assert_eq!(e.apply(ctrl('?')), EditEffect::ShowHelp);
        assert_eq!(e.buffer(), "ab");
    }

    #[test]
    fn a_space_key_inserts_a_space() {
        let mut e = typed("ab");
        e.apply(key(KeyCode::Space));
        assert_eq!(e.buffer(), "ab ");
    }

    #[test]
    fn reset_clears_the_buffer_but_keeps_history() {
        let mut e = Editor::new();
        e.push_str("play");
        e.apply(key(KeyCode::Enter));
        e.push_str("pause");
        e.reset();
        assert_eq!(e.buffer(), "");
        assert_eq!(e.cursor(), 0);
        assert_eq!(e.history().len(), 1);
    }
}
