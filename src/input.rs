//! The single input layer (PLANS.md section 5.2).
//!
//! There is exactly one input layer and two ways to feed it:
//!
//! | stdin | mode |
//! |---|---|
//! | TTY | **hotkey mode**, with `:` opening the command prompt |
//! | pipe or file | **command mode**, one line at a time, hotkeys off |
//! | neither | no input at all |
//!
//! Both TTY modes speak one grammar: `keys.rs` maps a chord to the same
//! [`Command`] the parser produces, so hotkeys and typed commands cannot drift
//! apart. Nothing is reachable only by keypress.
//!
//! One thread owns the terminal. `crossterm` events feed both modes, and the line
//! editor is in-house (`lineedit.rs`) precisely so a second library is not also
//! toggling termios on the same descriptor.

use std::io::{BufRead, BufReader, IsTerminal};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::commands::{self, Command};
use crate::error::{Error, Result};
use crate::keys::{Chord, KeyCode as ChordCode, Keymap, Mods};
use crate::lineedit::{EditEffect, Editor};

/// Which mode the interactive session is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Hotkey,
    Command,
}

/// One unit of input from the layer.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// A command to run, however it was produced.
    Command(Command),
    /// A line that did not parse. Reported, and the session continues.
    ParseError(String),
    /// The in-place input line changed and must be repainted. This is the echo:
    /// without it the user types into a prompt that never shows what they typed.
    Redraw,
    /// Leave command mode without running anything.
    CancelLine,
    /// Exit with code 130.
    Interrupt,
    /// Nothing yet.
    Idle,
}

/// Translate a `crossterm` event into our own pure [`Chord`].
///
/// This function is the only place that knows about the terminal library, which
/// is what keeps `keys.rs` testable without a TTY.
pub fn translate(event: Event) -> Option<Chord> {
    let Event::Key(key) = event else {
        // Resize, focus, mouse, and paste events are not bindings.
        return None;
    };
    // Windows reports both press and release; acting on release would run every
    // action twice.
    if key.kind == KeyEventKind::Release {
        return None;
    }
    translate_key(key)
}

fn translate_key(key: KeyEvent) -> Option<Chord> {
    let mut mods = Mods::NONE;
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        mods.ctrl = true;
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        mods.alt = true;
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        mods.shift = true;
    }
    let code = match key.code {
        KeyCode::Char(c) => {
            // Control keys arrive as CONTROL+'c' or as the raw code point U+0003,
            // depending on the terminal. Normalize both, or Ctrl+C/Ctrl+D would
            // only sometimes be hotkeys.
            if c.is_ascii_control() {
                mods.ctrl = true;
                let letter = (c as u8).wrapping_add(b'a' - 1) as char;
                ChordCode::Char(letter)
            } else if mods.ctrl {
                ChordCode::Char(c.to_ascii_lowercase())
            } else if c == ' ' {
                // Space is `Space`, not `Char(' ')`, so it can be bound by name. The
                // SHIFT flag is kept rather than cleared: a user who wants
                // `shift+space` for something must be able to have it, and the plain
                // `space` binding still matches only the unmodified key.
                ChordCode::Space
            } else {
                ChordCode::Char(c)
            }
        }
        KeyCode::Enter => ChordCode::Enter,
        KeyCode::Esc => ChordCode::Esc,
        KeyCode::Backspace => ChordCode::Backspace,
        KeyCode::Delete => ChordCode::Delete,
        KeyCode::Left => ChordCode::Left,
        KeyCode::Right => ChordCode::Right,
        KeyCode::Up => ChordCode::Up,
        KeyCode::Down => ChordCode::Down,
        KeyCode::Home => ChordCode::Home,
        KeyCode::End => ChordCode::End,
        KeyCode::Tab => ChordCode::Tab,
        // Nothing else is bindable: paging, function keys, media keys, and the lock
        // keys would only add surprising behaviour.
        _ => return None,
    };
    Some(Chord { code, mods })
}

/// Where input comes from.
enum Source {
    /// A terminal, polled one event at a time.
    Tty,
    /// A pipe or file, read on a background thread so the session loop can keep
    /// ticking the A-B clock while it waits.
    Lines { rx: Receiver<String>, joined: bool },
}

pub struct InputLayer {
    source: Source,
    /// Which mode is active. Public so the loop can decide what owns the cursor row.
    pub mode: Mode,
    editor: Editor,
    hotkeys: bool,
}

impl InputLayer {
    /// Choose the mode from stdin, per the table in section 5.2.
    ///
    /// `hotkeys_enabled` is `--no-keys` / `keys off`: the session then starts in
    /// command mode even on a TTY.
    pub fn new(hotkeys_enabled: bool) -> Self {
        let stdin_is_tty = std::io::stdin().is_terminal();
        let source = if stdin_is_tty {
            Source::Tty
        } else {
            // A pipe, a file, or a closed stdin: read lines on a thread, so the
            // session loop keeps ticking while it waits for input.
            Source::Lines {
                rx: spawn_line_reader(),
                joined: false,
            }
        };
        let mode = if stdin_is_tty && hotkeys_enabled {
            Mode::Hotkey
        } else {
            Mode::Command
        };
        Self {
            source,
            mode,
            editor: Editor::new(),
            hotkeys: hotkeys_enabled,
        }
    }

    /// A layer with no usable stdin, for tests.
    ///
    /// The sender is dropped immediately, so the first poll reports the channel
    /// disconnected and the layer counts as exhausted -- exactly what a run with
    /// `< /dev/null` does.
    #[cfg(test)]
    pub fn detached() -> Self {
        let (_tx, rx) = mpsc::channel();
        Self {
            source: Source::Lines { rx, joined: false },
            mode: Mode::Command,
            editor: Editor::new(),
            hotkeys: false,
        }
    }

    #[cfg(test)]
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// A layer fed from a fixed list of lines, as a pipe would be.
    #[cfg(test)]
    pub fn lines_for_test(lines: &[&str], hotkeys: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        for line in lines {
            tx.send((*line).to_string()).ok();
        }
        drop(tx);
        Self {
            source: Source::Lines { rx, joined: false },
            mode: if hotkeys { Mode::Hotkey } else { Mode::Command },
            editor: Editor::new(),
            hotkeys,
        }
    }

    /// Test hook: apply one chord through the normal path, using the default keymap.
    #[cfg(test)]
    pub fn handle_chord_for_test(&mut self, chord: Chord) -> Input {
        let map = crate::keys::build("default", crate::keys::DEFAULT_BINDINGS).0;
        self.handle_chord(chord, &map)
    }

    /// True when the input source has been exhausted (a pipe at EOF).
    pub fn is_exhausted(&self) -> bool {
        match &self.source {
            Source::Lines { joined, .. } => *joined,
            Source::Tty => false,
        }
    }

    /// Wait up to `timeout` for one unit of input.
    pub fn poll(&mut self, keymap: &Keymap, timeout: Duration) -> Result<Input> {
        match &mut self.source {
            Source::Lines { rx, joined } => match rx.recv_timeout(timeout) {
                Ok(line) => Ok(self.handle_line(&line)),
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(Input::Idle),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    *joined = true;
                    Ok(Input::Idle)
                }
            },
            Source::Tty => {
                if !crossterm::event::poll(timeout).unwrap_or(false) {
                    return Ok(Input::Idle);
                }
                let event = crossterm::event::read()
                    .map_err(|e| Error::runtime(format!("cannot read a key event: {e}")))?;
                let Some(chord) = translate(event) else {
                    return Ok(Input::Idle);
                };
                Ok(self.handle_chord(chord, keymap))
            }
        }
    }

    /// Handle one line from a pipe or file: always command mode, never hotkeys.
    fn handle_line(&mut self, line: &str) -> Input {
        let line = line.trim_end_matches(['\r', '\n']);
        match commands::parse(line) {
            Ok(Command::Noop) => Input::Idle,
            Ok(cmd) => Input::Command(cmd),
            Err(e) => Input::ParseError(e.to_string()),
        }
    }

    /// Handle one keystroke, in whichever mode is active.
    fn handle_chord(&mut self, chord: Chord, keymap: &Keymap) -> Input {
        match self.mode {
            Mode::Command => match self.editor.apply(chord) {
                EditEffect::Submit(line) => match commands::parse(&line) {
                    Ok(Command::Noop) => Input::Idle,
                    Ok(cmd) => Input::Command(cmd),
                    Err(e) => Input::ParseError(e.to_string()),
                },
                EditEffect::SubmitEmpty => Input::Idle,
                EditEffect::Cancel => {
                    // Ctrl+C on an empty prompt leaves command mode; that is the
                    // documented "cancel the line, do not lose the drill" rule.
                    self.mode = if self.hotkeys {
                        Mode::Hotkey
                    } else {
                        Mode::Command
                    };
                    if self.hotkeys {
                        Input::CancelLine
                    } else {
                        Input::Interrupt
                    }
                }
                EditEffect::ShowHelp => Input::Command(Command::Help(commands::HelpTopic::Keys)),
                // A keystroke that changed the buffer is the echo the user is
                // typing; reporting it as `Idle` would draw nothing at all.
                EditEffect::Redraw => Input::Redraw,
                EditEffect::Ignored => Input::Idle,
            },
            Mode::Hotkey => {
                // `:` enters the command prompt, and is the one key that is not a
                // binding (so it cannot shadow a command).
                if chord.code == ChordCode::Char(':') && chord.mods.is_none() {
                    self.mode = Mode::Command;
                    return Input::Redraw;
                }
                if chord.mods.ctrl && chord.code == ChordCode::Char('c') {
                    // Ctrl+C in hotkey mode exits, and `main` restores first.
                    return Input::Interrupt;
                }
                match keymap.lookup(&chord) {
                    Some(cmd) => Input::Command(cmd.clone()),
                    None => Input::Idle,
                }
            }
        }
    }

    /// Called after a line is submitted, to return to hotkey mode.
    pub fn after_submit(&mut self) {
        if self.hotkeys {
            self.mode = Mode::Hotkey;
        }
    }

    /// The prompt to draw before the current command-mode buffer.
    ///
    /// A `:` is always shown in command mode, including under `--no-keys` where
    /// there is no hotkey mode to return to: the marker is what tells the user this
    /// is a command line rather than an empty screen. Whether anything is drawn at
    /// all is the caller's decision, since a pipe must not be given a prompt.
    pub fn prompt(&self) -> String {
        if self.mode == Mode::Command {
            format!(":{}", self.editor.buffer())
        } else {
            self.editor.buffer().to_string()
        }
    }
}

/// Read stdin lines on a background thread.
///
/// The session loop must keep ticking (the A-B clock depends on it), so a
/// blocking `read_line` on the main thread would freeze playback. The thread ends
/// at EOF or on a read error; either way the channel disconnects and the layer
/// reports itself exhausted.
fn spawn_line_reader() -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("jas-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            let reader = BufReader::new(stdin.lock());
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .ok();
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{self, Keymap};

    fn keymap() -> Keymap {
        keys::build("default", keys::DEFAULT_BINDINGS).0
    }

    fn ev(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn chord(code: KeyCode, mods: KeyModifiers) -> Chord {
        translate(ev(code, mods)).expect("should translate")
    }

    #[test]
    fn ordinary_keys_translate_to_chords() {
        assert_eq!(
            chord(KeyCode::Char('n'), KeyModifiers::NONE),
            Chord::char('n')
        );
        assert_eq!(
            chord(KeyCode::Char(' '), KeyModifiers::NONE),
            Chord::new(ChordCode::Space)
        );
        assert_eq!(
            chord(KeyCode::Enter, KeyModifiers::NONE),
            Chord::new(ChordCode::Enter)
        );
        assert_eq!(
            chord(KeyCode::Left, KeyModifiers::NONE),
            Chord::new(ChordCode::Left)
        );
        assert_eq!(
            chord(KeyCode::Left, KeyModifiers::SHIFT),
            Chord::with(ChordCode::Left, Mods::shift())
        );
        assert_eq!(
            chord(KeyCode::Esc, KeyModifiers::NONE),
            Chord::new(ChordCode::Esc)
        );
    }

    #[test]
    fn control_keys_are_normalized_from_either_spelling() {
        // Terminals differ: some send Ctrl+C as CONTROL+'c', others as U+0003.
        assert_eq!(
            chord(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Chord::with(ChordCode::Char('c'), Mods::ctrl())
        );
        assert_eq!(
            chord(KeyCode::Char('\u{3}'), KeyModifiers::NONE),
            Chord::with(ChordCode::Char('c'), Mods::ctrl())
        );
        // And uppercase control letters fold down, so Ctrl+D matches the keymap.
        assert_eq!(
            chord(KeyCode::Char('D'), KeyModifiers::CONTROL),
            Chord::with(ChordCode::Char('d'), Mods::ctrl())
        );
    }

    #[test]
    fn key_release_events_are_ignored() {
        // Windows sends both press and release; acting on both would run every
        // action twice.
        let mut release = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(translate(Event::Key(release)), None);
        let mut press = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        press.kind = KeyEventKind::Press;
        assert_eq!(translate(Event::Key(press)), Some(Chord::char('n')));
    }

    #[test]
    fn shift_space_stays_distinguishable_from_space() {
        // Otherwise a `shift+space` binding in config.json could never fire.
        assert_eq!(
            chord(KeyCode::Char(' '), KeyModifiers::NONE),
            Chord::new(ChordCode::Space)
        );
        assert_eq!(
            chord(KeyCode::Char(' '), KeyModifiers::SHIFT),
            Chord::with(ChordCode::Space, Mods::shift())
        );
        assert_ne!(
            chord(KeyCode::Char(' '), KeyModifiers::SHIFT),
            chord(KeyCode::Char(' '), KeyModifiers::NONE)
        );
    }

    #[test]
    fn a_custom_shift_space_binding_is_reachable() {
        // The README documents this exact override, so it is checked here.
        let (map, bad) = keys::build(
            "custom",
            &[("space", "toggle"), ("shift+space", "seek +15")],
        );
        assert!(bad.is_empty(), "{bad:?}");
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Hotkey;
        assert_eq!(
            layer.handle_chord(chord(KeyCode::Char(' '), KeyModifiers::SHIFT), &map),
            Input::Command(Command::Seek(crate::time::TimeSpec::Relative(15_000)))
        );
        assert_eq!(
            layer.handle_chord(chord(KeyCode::Char(' '), KeyModifiers::NONE), &map),
            Input::Command(Command::Toggle)
        );
    }

    #[test]
    fn unbound_terminal_events_are_ignored() {
        assert_eq!(translate(Event::Resize(80, 24)), None);
        assert_eq!(translate(ev(KeyCode::F(5), KeyModifiers::NONE)), None);
        assert_eq!(translate(ev(KeyCode::Insert, KeyModifiers::NONE)), None);
    }

    #[test]
    fn a_hotkey_produces_the_same_command_the_parser_would() {
        let map = keymap();
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Hotkey;
        layer.hotkeys = true;
        let input = layer.handle_chord(Chord::char('n'), &map);
        assert_eq!(input, Input::Command(Command::Next));
        // The same command, typed.
        assert_eq!(commands::parse("next").unwrap(), Command::Next);
    }

    #[test]
    fn an_unbound_hotkey_does_nothing_rather_than_guessing() {
        let map = keymap();
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Hotkey;
        assert_eq!(layer.handle_chord(Chord::char('z'), &map), Input::Idle);
    }

    #[test]
    fn colon_enters_command_mode_and_is_not_a_binding() {
        let map = keymap();
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Hotkey;
        layer.hotkeys = true;
        let input = layer.handle_chord(Chord::char(':'), &map);
        assert_eq!(layer.mode, Mode::Command);
        // The transition asks for a repaint: without it the prompt never appears and
        // the user types into a screen that does not change.
        assert_eq!(input, Input::Redraw);
        // And it is not shadowing a bindable key.
        assert_eq!(map.lookup(&Chord::char(':')), None);
    }

    #[test]
    fn typed_lines_are_parsed_and_submitted_on_enter() {
        let map = keymap();
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Command;
        for c in "seek +5".chars() {
            layer.handle_chord(Chord::char(c), &map);
        }
        let input = layer.handle_chord(Chord::new(ChordCode::Enter), &map);
        assert_eq!(
            input,
            Input::Command(Command::Seek(crate::time::TimeSpec::Relative(5_000)))
        );
        assert_eq!(layer.editor().buffer(), "", "the buffer clears after Enter");
    }

    #[test]
    fn a_bad_typed_line_is_reported_not_run() {
        let map = keymap();
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Command;
        for c in "frobnicate".chars() {
            layer.handle_chord(Chord::char(c), &map);
        }
        let input = layer.handle_chord(Chord::new(ChordCode::Enter), &map);
        match input {
            Input::ParseError(msg) => assert!(msg.contains("unknown command"), "{msg}"),
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn escape_leaves_command_mode_without_running_anything() {
        let map = keymap();
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Command;
        layer.hotkeys = true;
        for c in "quit".chars() {
            layer.handle_chord(Chord::char(c), &map);
        }
        let input = layer.handle_chord(Chord::new(ChordCode::Esc), &map);
        assert_eq!(input, Input::CancelLine);
        assert_eq!(layer.mode, Mode::Hotkey);
    }

    #[test]
    fn ctrl_c_in_hotkey_mode_interrupts_and_in_command_mode_cancels() {
        let map = keymap();
        let ctrl_c = Chord::with(ChordCode::Char('c'), Mods::ctrl());

        let mut hotkey = InputLayer::detached();
        hotkey.mode = Mode::Hotkey;
        assert_eq!(hotkey.handle_chord(ctrl_c, &map), Input::Interrupt);

        let mut command = InputLayer::detached();
        command.mode = Mode::Command;
        command.hotkeys = true;
        for c in "pause".chars() {
            command.handle_chord(Chord::char(c), &map);
        }
        assert_eq!(command.handle_chord(ctrl_c, &map), Input::CancelLine);
        assert_eq!(command.mode, Mode::Hotkey);
        assert_eq!(command.editor().buffer(), "", "the typed line is discarded");
    }

    #[test]
    fn with_hotkeys_disabled_command_mode_stays_put() {
        let map = keymap();
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Command;
        layer.hotkeys = false;
        for c in "x".chars() {
            layer.handle_chord(Chord::char(c), &map);
        }
        // `q` would be a hotkey, but here it is just a character. It reports a
        // repaint so the typed `q` is echoed, like any other character.
        let input = layer.handle_chord(Chord::char('q'), &map);
        assert_eq!(input, Input::Redraw);
        assert_eq!(layer.editor().buffer(), "xq");
        assert_eq!(layer.mode, Mode::Command);
    }

    #[test]
    fn entering_a_blank_line_does_nothing() {
        let map = keymap();
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Command;
        assert_eq!(
            layer.handle_chord(Chord::new(ChordCode::Enter), &map),
            Input::Idle
        );
    }

    #[test]
    fn a_comment_line_from_a_script_is_ignored() {
        let mut layer = InputLayer::detached();
        assert_eq!(layer.handle_line("# a note"), Input::Idle);
        assert_eq!(layer.handle_line("   "), Input::Idle);
    }

    #[test]
    fn a_script_line_is_parsed() {
        let mut layer = InputLayer::detached();
        assert_eq!(
            layer.handle_line("goto 2\r\n"),
            Input::Command(Command::Goto(2)),
            "CRLF must be accepted"
        );
        match layer.handle_line("nope") {
            Input::ParseError(m) => assert!(m.contains("unknown command")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_prompt_is_marked_and_follows_the_buffer() {
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Command;
        assert_eq!(layer.prompt(), ":", "command mode is always marked");
        layer.hotkeys = true;
        layer.editor.push_str("pl");
        assert_eq!(layer.prompt(), ":pl");
        // `--no-keys` has no hotkey mode to return to, but the marker still helps:
        // otherwise the user is looking at a blank row with no cue at all.
        layer.hotkeys = false;
        assert_eq!(layer.prompt(), ":pl");
        // In hotkey mode there is no line to edit.
        layer.mode = Mode::Hotkey;
        assert_eq!(layer.prompt(), "pl");
    }

    #[test]
    fn a_detached_layer_returns_no_input_and_reports_itself_exhausted() {
        let map = keymap();
        let mut layer = InputLayer::detached();
        // Before the first poll the disconnect has not been observed yet.
        assert!(layer.poll(&map, Duration::from_millis(1)).unwrap() == Input::Idle);
        assert!(
            layer.is_exhausted(),
            "a closed stdin is exhausted, not interactive"
        );
    }

    #[test]
    fn after_submit_returns_to_hotkey_mode_only_when_hotkeys_are_on() {
        let mut with = InputLayer::detached();
        with.hotkeys = true;
        with.mode = Mode::Command;
        with.after_submit();
        assert_eq!(with.mode, Mode::Hotkey);

        let mut without = InputLayer::detached();
        without.hotkeys = false;
        without.mode = Mode::Command;
        without.after_submit();
        assert_eq!(without.mode, Mode::Command);
    }
}
