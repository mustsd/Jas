//! Key event -> `Command` mapping, and the default keymaps (pure).
//!
//! This module defines its own tiny [`Chord`] type instead of using
//! `crossterm`'s `KeyEvent` directly. The terminal layer (`input.rs`) converts
//! platform events into a `Chord`, which keeps this whole module pure: the
//! default keymap can be table-tested with no TTY, and a keymap typo fails the
//! test suite instead of doing nothing at runtime.
//!
//! Exactly one input layer exists (PLANS.md section 5.2): every hotkey maps to
//! the same [`Command`] value that `commands.rs` parses, so hotkeys and typed
//! commands cannot drift apart.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::commands::{self, Command};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyCode {
    Char(char),
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Enter,
    Esc,
    Backspace,
    Delete,
    Tab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
    };

    #[cfg(test)]
    pub fn ctrl() -> Mods {
        Mods {
            ctrl: true,
            ..Mods::NONE
        }
    }

    #[cfg(test)]
    pub fn shift() -> Mods {
        Mods {
            shift: true,
            ..Mods::NONE
        }
    }

    pub fn is_none(self) -> bool {
        !self.ctrl && !self.alt && !self.shift
    }
}

/// One keystroke: a key plus modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: Mods,
}

impl Chord {
    #[cfg(test)]
    pub const fn new(code: KeyCode) -> Self {
        Chord {
            code,
            mods: Mods::NONE,
        }
    }

    #[cfg(test)]
    pub const fn with(code: KeyCode, mods: Mods) -> Self {
        Chord { code, mods }
    }

    /// A plain character key: `Chord::char('n')`.
    #[cfg(test)]
    pub const fn char(c: char) -> Self {
        Chord::new(KeyCode::Char(c))
    }
}

impl fmt::Display for Chord {
    /// Renders the canonical config spelling: `ctrl+shift+left`, `space`, `a`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.ctrl {
            f.write_str("ctrl+")?;
        }
        if self.mods.alt {
            f.write_str("alt+")?;
        }
        if self.mods.shift {
            // A shifted character already carries its case; do not double it.
            if !matches!(self.code, KeyCode::Char(_)) {
                f.write_str("shift+")?;
            }
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("space"),
            KeyCode::Char('\\') => f.write_str("\\\\"),
            KeyCode::Char(c) => f.write_fmt(format_args!("{c}")),
            KeyCode::Space => f.write_str("space"),
            KeyCode::Left => f.write_str("left"),
            KeyCode::Right => f.write_str("right"),
            KeyCode::Up => f.write_str("up"),
            KeyCode::Down => f.write_str("down"),
            KeyCode::Home => f.write_str("home"),
            KeyCode::End => f.write_str("end"),
            KeyCode::Enter => f.write_str("enter"),
            KeyCode::Esc => f.write_str("esc"),
            KeyCode::Backspace => f.write_str("backspace"),
            KeyCode::Delete => f.write_str("delete"),
            KeyCode::Tab => f.write_str("tab"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChordError(String);

impl fmt::Display for ChordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ChordError {}

/// Parse a chord from config or a preset: `a`, `A`, `space`, `ctrl+c`,
/// `shift+left`, `alt+1`, `\`. Case is significant for a bare letter, because
/// `A` and `a` are different keys.
pub fn parse_chord(input: &str) -> Result<Chord, ChordError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(ChordError("empty key name".into()));
    }
    // A lone backslash is written `\` in JSON as `"\\\\"`; accept both spellings.
    let text = if raw == "\\\\" || raw == "\\" {
        "\\"
    } else {
        raw
    };

    let mut parts: Vec<&str> = text.split('+').collect();
    let key_name = parts.pop().unwrap_or("").trim().to_string();
    let mut mods = Mods::NONE;
    for m in parts {
        match m.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "c" => mods.ctrl = true,
            "alt" | "meta" | "option" => mods.alt = true,
            "shift" => mods.shift = true,
            "" => return Err(ChordError(format!("`{input}` has an empty modifier"))),
            other => {
                return Err(ChordError(format!(
                    "`{input}`: `{other}` is not a modifier (use ctrl, alt, or shift)"
                )))
            }
        }
    }

    let code = match key_name.to_ascii_lowercase().as_str() {
        "space" | "spc" => KeyCode::Space,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "tab" => KeyCode::Tab,
        _ => {
            let mut chars = key_name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    // `space` is spelled out; a literal space character is also fine.
                    if c == ' ' {
                        KeyCode::Space
                    } else {
                        KeyCode::Char(c)
                    }
                }
                _ => {
                    return Err(ChordError(format!(
                        "`{input}`: `{key_name}` is not a key name"
                    )))
                }
            }
        }
    };
    // A named key plus shift is `shift+left`; a character plus shift is just the
    // uppercase character, so fold it here rather than carrying a redundant flag.
    if mods.shift {
        if let KeyCode::Char(c) = code {
            let upper: String = c.to_uppercase().collect();
            if upper.chars().count() == 1 {
                return Ok(Chord {
                    code: KeyCode::Char(upper.chars().next().unwrap()),
                    mods: Mods {
                        shift: false,
                        ..mods
                    },
                });
            }
        }
    }
    Ok(Chord { code, mods })
}

/// A keymap: chord -> command text, parsed into real `Command` values.
#[derive(Debug, Clone, PartialEq)]
pub struct Keymap {
    bindings: BTreeMap<Chord, Command>,
    name: String,
}

/// A binding that could not be used, reported at startup and then ignored so a
/// stale config can never stop playback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadBinding {
    pub key: String,
    pub value: String,
    pub problem: String,
}

impl Keymap {
    pub fn lookup(&self, chord: &Chord) -> Option<&Command> {
        self.bindings.get(chord)
    }

    #[cfg(test)]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    #[cfg(test)]
    pub fn bindings(&self) -> impl Iterator<Item = (&Chord, &Command)> {
        self.bindings.iter()
    }

    /// Replace or add one binding, used by `config.json` `"keymap"`.
    pub fn insert(&mut self, chord: Chord, command: Command) {
        self.bindings.insert(chord, command);
    }

    #[cfg(test)]
    pub fn remove(&mut self, chord: &Chord) -> Option<Command> {
        self.bindings.remove(chord)
    }

    /// Every key a user can press, for `help keys`.
    pub fn render(&self) -> String {
        let mut out = format!("keymap `{}`:\n", self.name);
        for (chord, cmd) in &self.bindings {
            out.push_str(&format!(
                "  {:<14} {}\n",
                chord.to_string(),
                command_text(cmd)
            ));
        }
        out.trim_end().to_string()
    }
}

/// The command text a `Command` came from, used by `help keys`.
pub fn command_text(cmd: &Command) -> String {
    match cmd {
        Command::Noop => "#".into(),
        Command::Play => "play".into(),
        Command::Pause => "pause".into(),
        Command::Toggle => "toggle".into(),
        Command::Next => "next".into(),
        Command::Prev => "prev".into(),
        Command::Goto(n) => format!("goto {n}"),
        Command::Seek(spec) => match spec {
            crate::time::TimeSpec::Absolute(d) => {
                format!("seek {}", crate::time::format_time_precise(*d))
            }
            crate::time::TimeSpec::Relative(ms) => format!("seek {}", signed_time(*ms)),
        },
        Command::Speed(speed) => match speed {
            commands::SpeedArg::Set(x) => format!("speed {x}"),
            commands::SpeedArg::Nudge(x) => format!("speed {x:+}"),
        },
        Command::Ab { a, b } => {
            let a = match a {
                crate::time::TimeSpec::Absolute(d) => crate::time::format_time_precise(*d),
                crate::time::TimeSpec::Relative(ms) => signed_time(*ms),
            };
            match b {
                Some(b) => {
                    let b = match b {
                        crate::time::TimeSpec::Absolute(d) => crate::time::format_time_precise(*d),
                        crate::time::TimeSpec::Relative(ms) => signed_time(*ms),
                    };
                    format!("ab {a} {b}")
                }
                None => format!("ab {a}"),
            }
        }
        Command::AbMarkA => "ab-a".into(),
        Command::AbMarkB => "ab-b".into(),
        Command::AbClear => "ab clear".into(),
        Command::Repeat(r) => match r {
            commands::RepeatArg::Count(n) => format!("repeat {n}"),
            commands::RepeatArg::Off => "repeat off".into(),
            commands::RepeatArg::Next => "repeat next".into(),
            commands::RepeatArg::Cycle => "repeat cycle".into(),
        },
        Command::Gap(g) => match g {
            // `gap` counts milliseconds, so it renders milliseconds, not a time
            // literal; the two grammars must not be confused.
            commands::GapArg::Set(d) => format!("gap {}", d.as_millis()),
            commands::GapArg::Nudge(ms) => format!("gap {ms:+}"),
        },
        Command::Loop(Some(l)) => format!("loop {}", l.as_str()),
        Command::Loop(None) => "loop".into(),
        Command::Shuffle(Some(true)) => "shuffle on".into(),
        Command::Shuffle(Some(false)) => "shuffle off".into(),
        Command::Shuffle(None) => "shuffle".into(),
        Command::List => "list".into(),
        Command::Status => "status".into(),
        Command::Save => "save".into(),
        Command::Backend(Some(b)) => format!("backend {b}"),
        Command::Backend(None) => "backend".into(),
        Command::Keys(Some(k)) => format!("keys {k}"),
        Command::Keys(None) => "keys".into(),
        Command::Help(commands::HelpTopic::Keys) => "help keys".into(),
        Command::Help(commands::HelpTopic::General) => "help".into(),
        Command::Quit => "quit".into(),
    }
}

/// A signed time literal, in the precise form the parser accepts.
fn signed_time(ms: i64) -> String {
    let magnitude = crate::time::format_time_precise(Duration::from_millis(ms.unsigned_abs()));
    if ms < 0 {
        format!("-{magnitude}")
    } else {
        format!("+{magnitude}")
    }
}

/// Build a keymap from `(key, command)` pairs, collecting every bad entry.
pub fn build(name: &str, entries: &[(&str, &str)]) -> (Keymap, Vec<BadBinding>) {
    let mut map = Keymap {
        bindings: BTreeMap::new(),
        name: name.to_string(),
    };
    let mut bad = Vec::new();
    for (key, value) in entries {
        let chord = match parse_chord(key) {
            Ok(c) => c,
            Err(e) => {
                bad.push(BadBinding {
                    key: (*key).into(),
                    value: (*value).into(),
                    problem: e.to_string(),
                });
                continue;
            }
        };
        match commands::parse(value) {
            Ok(cmd) => {
                map.bindings.insert(chord, cmd);
            }
            Err(e) => {
                bad.push(BadBinding {
                    key: (*key).into(),
                    value: (*value).into(),
                    problem: e.to_string(),
                });
            }
        }
    }
    (map, bad)
}

/// The default map from PLANS.md section 5.3.
pub const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("space", "toggle"),
    ("enter", "toggle"),
    ("right", "seek +5"),
    ("left", "seek -5"),
    ("shift+right", "seek +30"),
    ("shift+left", "seek -30"),
    ("n", "next"),
    ("p", "prev"),
    ("[", "speed -0.05"),
    ("]", "speed +0.05"),
    ("\\", "speed 1.0"),
    ("a", "ab-a"),
    ("b", "ab-b"),
    ("c", "ab clear"),
    ("r", "repeat cycle"),
    ("g", "gap +250"),
    ("l", "loop"),
    // `:` is not here on purpose: it switches to command mode and is handled by
    // the input layer, not by a Command, so it is not a bindable key.
    ("?", "help keys"),
    ("q", "quit"),
    ("ctrl+d", "quit"),
];

/// The `mpv` preset: the same actions on the keys mpv users already know.
pub const MPV_BINDINGS: &[(&str, &str)] = &[
    ("space", "toggle"),
    ("enter", "toggle"),
    ("right", "seek +5"),
    ("left", "seek -5"),
    ("up", "seek +60"),
    ("down", "seek -60"),
    ("n", "next"),
    ("p", "prev"),
    ("[", "speed -0.1"),
    ("]", "speed +0.1"),
    ("backspace", "speed 1.0"),
    ("a", "ab-a"),
    ("b", "ab-b"),
    ("c", "ab clear"),
    ("r", "repeat cycle"),
    ("g", "gap +250"),
    ("l", "loop"),
    ("?", "help keys"),
    ("q", "quit"),
    ("ctrl+d", "quit"),
];

/// Resolve a `--keys` / `keys` preset name. `off` disables hotkeys entirely,
/// which is the same as `--no-keys`.
pub fn preset(name: &str) -> Option<&'static [(&'static str, &'static str)]> {
    match name {
        "default" => Some(DEFAULT_BINDINGS),
        "mpv" => Some(MPV_BINDINGS),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{parse as parse_command, GapArg, LoopArg, RepeatArg, SpeedArg};
    use crate::time::TimeSpec;
    use std::time::Duration;

    #[test]
    fn parses_chord_names() {
        assert_eq!(parse_chord("a").unwrap(), Chord::char('a'));
        assert_eq!(parse_chord("A").unwrap(), Chord::char('A'));
        assert_eq!(parse_chord("space").unwrap(), Chord::new(KeyCode::Space));
        assert_eq!(parse_chord("SPACE").unwrap(), Chord::new(KeyCode::Space));
        assert_eq!(parse_chord("left").unwrap(), Chord::new(KeyCode::Left));
        assert_eq!(
            parse_chord("ctrl+c").unwrap(),
            Chord::with(KeyCode::Char('c'), Mods::ctrl())
        );
        assert_eq!(
            parse_chord("shift+left").unwrap(),
            Chord::with(KeyCode::Left, Mods::shift())
        );
        assert_eq!(
            parse_chord("ctrl+shift+left").unwrap(),
            Chord::with(
                KeyCode::Left,
                Mods {
                    ctrl: true,
                    alt: false,
                    shift: true
                }
            )
        );
        assert_eq!(parse_chord("\\").unwrap(), Chord::char('\\'));
    }

    #[test]
    fn rejects_bad_chord_names() {
        for bad in [
            "",
            "  ",
            "ctrl+",
            "hyper+x",
            "ab",
            "notakey",
            "shift+shift+",
        ] {
            assert!(parse_chord(bad).is_err(), "expected `{bad}` to be rejected");
        }
        let msg = parse_chord("hyper+x").unwrap_err().to_string();
        assert!(msg.contains("modifier"), "{msg}");
    }

    #[test]
    fn shift_on_a_letter_folds_into_the_character() {
        // `shift+a` and `A` are the same keystroke, so they must be the same chord.
        assert_eq!(parse_chord("shift+a").unwrap(), parse_chord("A").unwrap());
        assert_eq!(parse_chord("shift+a").unwrap(), Chord::char('A'));
    }

    #[test]
    fn chords_round_trip_through_their_display_form() {
        for key in [
            "a",
            "A",
            "space",
            "left",
            "shift+left",
            "ctrl+d",
            "enter",
            "esc",
            "?",
        ] {
            let chord = parse_chord(key).unwrap();
            let rendered = chord.to_string();
            let again = parse_chord(&rendered)
                .unwrap_or_else(|e| panic!("`{key}` rendered as `{rendered}`: {e}"));
            assert_eq!(again, chord, "`{key}` did not round-trip via `{rendered}`");
        }
    }

    #[test]
    fn default_keymap_matches_the_documented_table() {
        let (map, bad) = build("default", DEFAULT_BINDINGS);
        assert!(bad.is_empty(), "{bad:?}");
        let get = |k: &str| map.lookup(&parse_chord(k).unwrap()).cloned();
        assert_eq!(get("space"), Some(Command::Toggle));
        assert_eq!(get("enter"), Some(Command::Toggle));
        assert_eq!(get("right"), Some(Command::Seek(TimeSpec::Relative(5_000))));
        assert_eq!(get("left"), Some(Command::Seek(TimeSpec::Relative(-5_000))));
        assert_eq!(
            get("shift+right"),
            Some(Command::Seek(TimeSpec::Relative(30_000)))
        );
        assert_eq!(
            get("shift+left"),
            Some(Command::Seek(TimeSpec::Relative(-30_000)))
        );
        assert_eq!(get("n"), Some(Command::Next));
        assert_eq!(get("p"), Some(Command::Prev));
        assert_eq!(get("["), Some(Command::Speed(SpeedArg::Nudge(-0.05))));
        assert_eq!(get("]"), Some(Command::Speed(SpeedArg::Nudge(0.05))));
        assert_eq!(get("\\"), Some(Command::Speed(SpeedArg::Set(1.0))));
        assert_eq!(get("a"), Some(Command::AbMarkA));
        assert_eq!(get("b"), Some(Command::AbMarkB));
        assert_eq!(get("c"), Some(Command::AbClear));
        assert_eq!(get("r"), Some(Command::Repeat(RepeatArg::Cycle)));
        assert_eq!(get("g"), Some(Command::Gap(GapArg::Nudge(250))));
        assert_eq!(get("l"), Some(Command::Loop(None)));
        assert_eq!(get("?"), Some(Command::Help(commands::HelpTopic::Keys)));
        assert_eq!(get("q"), Some(Command::Quit));
        assert_eq!(get("ctrl+d"), Some(Command::Quit));
        assert_eq!(get("z"), None);
    }

    #[test]
    fn every_bound_command_is_accepted_by_the_parser() {
        // The key drift guard from PLANS.md section 10: the text of every binding
        // must be a command the parser accepts, so a keymap typo fails here.
        for (name, bindings) in [("default", DEFAULT_BINDINGS), ("mpv", MPV_BINDINGS)] {
            let (map, bad) = build(name, bindings);
            assert!(bad.is_empty(), "{name}: {bad:?}");
            for (chord, cmd) in map.bindings() {
                let text = command_text(cmd);
                let reparsed = parse_command(&text).unwrap_or_else(|e| {
                    panic!("{name}: `{chord}` renders as `{text}`, which the parser rejects: {e}")
                });
                assert_eq!(
                    &reparsed, cmd,
                    "{name}: `{chord}` -> `{text}` did not parse back to the same command"
                );
            }
        }
    }

    #[test]
    fn every_documented_key_exists_in_the_default_map() {
        for key in [
            "space",
            "enter",
            "right",
            "left",
            "shift+right",
            "shift+left",
            "n",
            "p",
            "[",
            "]",
            "\\",
            "a",
            "b",
            "c",
            "r",
            "g",
            "l",
            "?",
            "q",
            "ctrl+d",
        ] {
            let (map, _) = build("default", DEFAULT_BINDINGS);
            assert!(
                map.lookup(&parse_chord(key).unwrap()).is_some(),
                "documented key `{key}` is not bound"
            );
        }
    }

    #[test]
    fn the_mpv_preset_is_usable_and_distinct() {
        let (mpv, bad) = build("mpv", MPV_BINDINGS);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(
            mpv.lookup(&parse_chord("up").unwrap()),
            Some(&Command::Seek(TimeSpec::Relative(60_000)))
        );
        assert_eq!(
            mpv.lookup(&parse_chord("backspace").unwrap()),
            Some(&Command::Speed(SpeedArg::Set(1.0)))
        );
        assert_eq!(
            mpv.lookup(&parse_chord("[").unwrap()),
            Some(&Command::Speed(SpeedArg::Nudge(-0.1)))
        );
        assert!(preset("default").is_some());
        assert!(preset("mpv").is_some());
        assert!(preset("off").is_none());
        assert!(
            preset("vi").is_none(),
            "the vi preset is still an open question"
        );
    }

    #[test]
    fn an_unparseable_entry_is_reported_and_ignored() {
        let (map, bad) = build(
            "custom",
            &[("z", "toggle"), ("y", "not a command"), ("hyper+k", "next")],
        );
        assert_eq!(
            map.lookup(&parse_chord("z").unwrap()),
            Some(&Command::Toggle)
        );
        assert_eq!(bad.len(), 2);
        assert!(bad
            .iter()
            .any(|b| b.key == "y" && b.problem.contains("unknown command")));
        assert!(bad
            .iter()
            .any(|b| b.key == "hyper+k" && b.problem.contains("modifier")));
        assert_eq!(map.lookup(&parse_chord("y").unwrap()), None);
    }

    #[test]
    fn bindings_can_be_overridden_and_removed() {
        let mut map = build("custom", DEFAULT_BINDINGS).0;
        map.insert(parse_chord("z").unwrap(), Command::Toggle);
        assert_eq!(
            map.lookup(&parse_chord("z").unwrap()),
            Some(&Command::Toggle)
        );
        map.insert(parse_chord("z").unwrap(), Command::Pause);
        assert_eq!(
            map.lookup(&parse_chord("z").unwrap()),
            Some(&Command::Pause)
        );
        assert_eq!(map.remove(&parse_chord("z").unwrap()), Some(Command::Pause));
        assert_eq!(map.lookup(&parse_chord("z").unwrap()), None);
    }

    #[test]
    fn no_hotkey_uses_a_single_letter_that_the_parser_reserves_elsewhere() {
        // PLANS.md section 5.4: a bare letter typed at the prompt is an error when
        // the keymap uses it for something else, so no letter means two things.
        let (map, _) = build("default", DEFAULT_BINDINGS);
        for (chord, cmd) in map.bindings() {
            if let KeyCode::Char(c) = chord.code {
                if chord.mods.is_none() && c.is_ascii_alphanumeric() {
                    let text = command_text(cmd);
                    let first = text.split_whitespace().next().unwrap_or("");
                    assert_ne!(
                        first,
                        c.to_string(),
                        "key `{c}` maps to `{text}`, which collides with a command alias"
                    );
                }
            }
        }
    }

    #[test]
    fn render_lists_every_binding() {
        let (map, _) = build("default", DEFAULT_BINDINGS);
        let text = map.render();
        assert!(text.contains("keymap `default`"));
        assert!(text.contains("space"));
        assert!(text.contains("ab-a"));
        assert_eq!(text.lines().count(), map.len() + 1);
    }

    #[test]
    fn command_text_covers_every_command_variant() {
        // A missing arm would be a compile error; this checks the round-trip for
        // the variants with arguments, which is where drift actually happens.
        let cases = [
            Command::Goto(4),
            Command::Seek(TimeSpec::Relative(-1_500)),
            Command::Seek(TimeSpec::Absolute(Duration::from_secs(90))),
            Command::Speed(SpeedArg::Nudge(-0.05)),
            Command::Speed(SpeedArg::Set(1.5)),
            Command::Ab {
                a: TimeSpec::Absolute(Duration::from_secs(12)),
                b: Some(TimeSpec::Absolute(Duration::from_secs(31))),
            },
            Command::Ab {
                a: TimeSpec::Absolute(Duration::from_secs(12)),
                b: None,
            },
            Command::Repeat(RepeatArg::Count(7)),
            Command::Gap(GapArg::Nudge(-250)),
            Command::Gap(GapArg::Set(Duration::from_millis(500))),
            Command::Loop(Some(LoopArg::One)),
            Command::Loop(None),
            Command::Shuffle(None),
            Command::Shuffle(Some(false)),
            Command::Backend(Some("mpv".into())),
            Command::Keys(Some("mpv".into())),
        ];
        for cmd in cases {
            let text = command_text(&cmd);
            let back = parse_command(&text)
                .unwrap_or_else(|e| panic!("`{text}` from {cmd:?} does not parse: {e}"));
            assert_eq!(back, cmd, "`{text}` round-trip changed the command");
        }
    }
}
