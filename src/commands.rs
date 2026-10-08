//! The command grammar: one line -> one `Command` (pure).
//!
//! The grammar is identical on a TTY, on a pipe, and in a file, and every hotkey
//! in `keys.rs` maps onto one of these commands, so nothing is reachable only by
//! keypress.

use std::fmt;
use std::time::Duration;

use crate::time::{parse_time, TimeSpec};
use crate::transport::clamp_speed;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// An empty or comment-only line. Ignored by the session loop.
    Noop,
    Play,
    Pause,
    Toggle,
    Next,
    Prev,
    Goto(usize),
    Seek(TimeSpec),
    Speed(SpeedArg),
    Ab {
        a: TimeSpec,
        b: Option<TimeSpec>,
    },
    AbMarkA,
    AbMarkB,
    AbClear,
    Repeat(RepeatArg),
    Gap(GapArg),
    /// `loop` with no argument cycles off -> all -> one.
    Loop(Option<LoopArg>),
    /// `shuffle` with no argument toggles.
    Shuffle(Option<bool>),
    List,
    Status,
    Save,
    /// `backend` prints the resolved backend; `backend <name>` switches it.
    Backend(Option<String>),
    /// `keys` prints the active keymap; `keys <preset>` switches preset.
    Keys(Option<String>),
    Help(HelpTopic),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpeedArg {
    Set(f64),
    Nudge(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatArg {
    Count(u32),
    Off,
    /// Shift the segment forward by its own length after the repeats finish.
    Next,
    /// Step through off -> 2 -> 3 -> 5 -> 10 -> off, for the `r` hotkey.
    Cycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapArg {
    Set(Duration),
    Nudge(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopArg {
    Off,
    All,
    One,
}

impl LoopArg {
    pub fn as_str(self) -> &'static str {
        match self {
            LoopArg::Off => "off",
            LoopArg::All => "all",
            LoopArg::One => "one",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTopic {
    General,
    Keys,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(String);

impl ParseError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

/// Longest accepted command line, defensive against paste bombs.
const MAX_LINE_BYTES: usize = 4096;

pub const HELP: &str = "\
play | pause | toggle            transport control
next | prev | goto <N>           playlist navigation (N is 1-based)
seek <T> | seek +T | seek -T     absolute, forward, backward
speed <X> | speed +X | speed -X  playback speed (clamped to 0.25-4.0)
ab <A> <B> | ab <A> | ab clear   set or clear the A-B loop
ab-a | ab-b                      mark A and B at the current position
repeat <N|off|next|cycle>        segment repeat count; bare `cycle` steps it
gap <MS> | gap +MS | gap -MS     pause between repeats
loop [off|all|one]               loop mode; with no value, cycles it
shuffle [on|off]                 seeded shuffle; with no value, toggles
list | status | save             info and session control
backend [name] | keys [preset]   show or switch backend and keymap
help [keys] | quit               this list, the keymap, and exit";

/// Parse one command line. `Err` carries a message meant to be shown to the user.
pub fn parse(line: &str) -> Result<Command, ParseError> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(Command::Noop);
    }
    if trimmed.len() > MAX_LINE_BYTES {
        return Err(ParseError::new(format!(
            "command line is too long ({} bytes, max {MAX_LINE_BYTES})",
            trimmed.len()
        )));
    }
    let mut parts = trimmed.split_whitespace();
    let name = parts.next().unwrap_or("").to_ascii_lowercase();
    let args: Vec<&str> = parts.collect();

    match name.as_str() {
        "play" => no_args(&name, &args, Command::Play),
        "pause" => no_args(&name, &args, Command::Pause),
        "toggle" => no_args(&name, &args, Command::Toggle),
        "next" => no_args(&name, &args, Command::Next),
        "prev" => no_args(&name, &args, Command::Prev),
        "goto" => {
            let raw = single_arg(&name, &args)?;
            let n: usize = raw
                .parse()
                .map_err(|_| ParseError::new(format!("goto: `{raw}` is not a track number")))?;
            if n == 0 {
                return Err(ParseError::new("goto: track numbers start at 1"));
            }
            Ok(Command::Goto(n))
        }
        "seek" => {
            let raw = single_arg(&name, &args)?;
            parse_time(raw)
                .map(Command::Seek)
                .map_err(|e| ParseError::new(format!("seek: {e} (try: seek +5 or seek 1:30)")))
        }
        "speed" => {
            let raw = single_arg(&name, &args)?;
            Ok(Command::Speed(parse_speed(raw)?))
        }
        "ab" => {
            if args.is_empty() {
                return Err(ParseError::new(
                    "ab needs times: `ab <A> <B>`, `ab <A>`, or `ab clear`",
                ));
            }
            if args.len() == 1 && args[0].eq_ignore_ascii_case("clear") {
                return Ok(Command::AbClear);
            }
            if args.len() > 2 {
                return Err(ParseError::new("ab takes at most two times (A and B)"));
            }
            let a = parse_absolute("ab", args[0])?;
            let b = match args.get(1) {
                Some(raw) => Some(parse_absolute("ab", raw)?),
                None => None,
            };
            Ok(Command::Ab { a, b })
        }
        "ab-a" | "aba" => no_args(&name, &args, Command::AbMarkA),
        "ab-b" | "abb" => no_args(&name, &args, Command::AbMarkB),
        "repeat" => {
            let raw = single_arg(&name, &args)?;
            let lower = raw.to_ascii_lowercase();
            match lower.as_str() {
                "off" | "none" => Ok(Command::Repeat(RepeatArg::Off)),
                "next" => Ok(Command::Repeat(RepeatArg::Next)),
                "cycle" => Ok(Command::Repeat(RepeatArg::Cycle)),
                _ => match raw.parse::<u32>() {
                    Ok(0) => Err(ParseError::new("repeat: use `repeat off`, not `repeat 0`")),
                    Ok(n) => Ok(Command::Repeat(RepeatArg::Count(n))),
                    Err(_) => Err(ParseError::new(format!(
                        "repeat: `{raw}` is not a count, `off`, or `next`"
                    ))),
                },
            }
        }
        "gap" => {
            let raw = single_arg(&name, &args)?;
            let (sign, digits) = match raw.strip_prefix('+') {
                Some(rest) => (1i64, rest),
                None => match raw.strip_prefix('-') {
                    Some(rest) => (-1i64, rest),
                    None => (0i64, raw),
                },
            };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ParseError::new(format!(
                    "gap: `{raw}` is not a millisecond count (try: gap 500)"
                )));
            }
            let value: i64 = digits
                .parse()
                .map_err(|_| ParseError::new(format!("gap: `{raw}` is out of range")))?;
            if value > 60_000 {
                return Err(ParseError::new("gap: at most 60000 ms"));
            }
            Ok(Command::Gap(match sign {
                0 => GapArg::Set(Duration::from_millis(value as u64)),
                s => GapArg::Nudge(s * value),
            }))
        }
        "loop" => match args.len() {
            0 => Ok(Command::Loop(None)),
            1 => match args[0].to_ascii_lowercase().as_str() {
                "off" | "none" => Ok(Command::Loop(Some(LoopArg::Off))),
                "all" => Ok(Command::Loop(Some(LoopArg::All))),
                "one" => Ok(Command::Loop(Some(LoopArg::One))),
                other => Err(ParseError::new(format!(
                    "loop: `{other}` is not off, all, or one"
                ))),
            },
            _ => Err(ParseError::new("loop takes at most one value")),
        },
        "shuffle" => match args.len() {
            0 => Ok(Command::Shuffle(None)),
            1 => match args[0].to_ascii_lowercase().as_str() {
                "on" | "true" | "yes" => Ok(Command::Shuffle(Some(true))),
                "off" | "false" | "no" => Ok(Command::Shuffle(Some(false))),
                other => Err(ParseError::new(format!(
                    "shuffle: `{other}` is neither on nor off"
                ))),
            },
            _ => Err(ParseError::new("shuffle takes at most one value")),
        },
        "list" | "ls" => no_args(&name, &args, Command::List),
        "status" | "st" => no_args(&name, &args, Command::Status),
        "save" => no_args(&name, &args, Command::Save),
        "backend" => match args.len() {
            0 => Ok(Command::Backend(None)),
            1 => Ok(Command::Backend(Some(args[0].to_ascii_lowercase()))),
            _ => Err(ParseError::new("backend takes at most one name")),
        },
        "keys" => match args.len() {
            0 => Ok(Command::Keys(None)),
            1 => Ok(Command::Keys(Some(args[0].to_ascii_lowercase()))),
            _ => Err(ParseError::new("keys takes at most one preset name")),
        },
        "help" | "?" => match args.len() {
            0 => Ok(Command::Help(HelpTopic::General)),
            1 if args[0].eq_ignore_ascii_case("keys") => Ok(Command::Help(HelpTopic::Keys)),
            1 => Ok(Command::Help(HelpTopic::General)),
            _ => Err(ParseError::new("help takes at most one topic")),
        },
        "quit" | "exit" | "q" | "x" => no_args(&name, &args, Command::Quit),
        other => Err(ParseError::new(format!(
            "unknown command `{other}` (try: help)"
        ))),
    }
}

fn no_args(name: &str, args: &[&str], cmd: Command) -> Result<Command, ParseError> {
    if args.is_empty() {
        Ok(cmd)
    } else {
        Err(ParseError::new(format!("{name} takes no arguments")))
    }
}

fn single_arg<'a>(name: &str, args: &'a [&str]) -> Result<&'a str, ParseError> {
    match args.len() {
        1 => Ok(args[0]),
        0 => Err(ParseError::new(format!("{name} needs a value (try: help)"))),
        _ => Err(ParseError::new(format!("{name} takes exactly one value"))),
    }
}

fn parse_absolute(name: &str, raw: &str) -> Result<TimeSpec, ParseError> {
    match parse_time(raw) {
        Ok(TimeSpec::Absolute(d)) => Ok(TimeSpec::Absolute(d)),
        Ok(TimeSpec::Relative(_)) => Err(ParseError::new(format!(
            "{name}: `{raw}` is relative; use absolute times here or `ab-a`/`ab-b` to mark the current position"
        ))),
        Err(e) => Err(ParseError::new(format!("{name}: {e}"))),
    }
}

fn parse_speed(raw: &str) -> Result<SpeedArg, ParseError> {
    let (nudge, digits) = match raw.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => match raw.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, raw),
        },
    };
    let negative = raw.starts_with('-');
    let value: f64 = digits
        .parse()
        .map_err(|_| ParseError::new(format!("speed: `{raw}` is not a number")))?;
    if !value.is_finite() {
        return Err(ParseError::new(format!("speed: `{raw}` is not a number")));
    }
    if value == 0.0 {
        return Err(ParseError::new("speed: must be greater than 0"));
    }
    let signed = if negative { -value } else { value };
    if nudge {
        Ok(SpeedArg::Nudge(signed))
    } else {
        Ok(SpeedArg::Set(clamp_speed(signed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::TimeSpec;
    use crate::transport::{MAX_SPEED, MIN_SPEED};

    fn ok(line: &str) -> Command {
        match parse(line) {
            Ok(c) => c,
            Err(e) => panic!("expected `{line}` to parse, got error: {e}"),
        }
    }

    fn err(line: &str) -> String {
        match parse(line) {
            Ok(c) => panic!("expected `{line}` to fail, got {c:?}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn parses_every_documented_command() {
        assert_eq!(ok("play"), Command::Play);
        assert_eq!(ok("  PAUSE  "), Command::Pause);
        assert_eq!(ok("toggle"), Command::Toggle);
        assert_eq!(ok("next"), Command::Next);
        assert_eq!(ok("prev"), Command::Prev);
        assert_eq!(ok("goto 3"), Command::Goto(3));
        assert_eq!(
            ok("seek 1:30"),
            Command::Seek(TimeSpec::Absolute(Duration::from_secs(90)))
        );
        assert_eq!(ok("seek +5"), Command::Seek(TimeSpec::Relative(5_000)));
        assert_eq!(ok("seek -5"), Command::Seek(TimeSpec::Relative(-5_000)));
        assert_eq!(ok("speed 0.75"), Command::Speed(SpeedArg::Set(0.75)));
        assert_eq!(ok("speed +0.05"), Command::Speed(SpeedArg::Nudge(0.05)));
        assert_eq!(ok("speed -0.05"), Command::Speed(SpeedArg::Nudge(-0.05)));
        assert_eq!(ok("speed 1.0"), Command::Speed(SpeedArg::Set(1.0)));
        assert_eq!(
            ok("ab 0:12 0:31"),
            Command::Ab {
                a: TimeSpec::Absolute(Duration::from_secs(12)),
                b: Some(TimeSpec::Absolute(Duration::from_secs(31))),
            }
        );
        assert_eq!(
            ok("ab 0:12"),
            Command::Ab {
                a: TimeSpec::Absolute(Duration::from_secs(12)),
                b: None
            }
        );
        assert_eq!(ok("ab clear"), Command::AbClear);
        assert_eq!(ok("ab-a"), Command::AbMarkA);
        assert_eq!(ok("ab-b"), Command::AbMarkB);
        assert_eq!(ok("repeat 3"), Command::Repeat(RepeatArg::Count(3)));
        assert_eq!(ok("repeat off"), Command::Repeat(RepeatArg::Off));
        assert_eq!(ok("repeat next"), Command::Repeat(RepeatArg::Next));
        assert_eq!(ok("repeat cycle"), Command::Repeat(RepeatArg::Cycle));
        assert_eq!(
            ok("gap 500"),
            Command::Gap(GapArg::Set(Duration::from_millis(500)))
        );
        assert_eq!(ok("gap +250"), Command::Gap(GapArg::Nudge(250)));
        assert_eq!(ok("gap -250"), Command::Gap(GapArg::Nudge(-250)));
        assert_eq!(ok("gap 0"), Command::Gap(GapArg::Set(Duration::ZERO)));
        assert_eq!(ok("loop off"), Command::Loop(Some(LoopArg::Off)));
        assert_eq!(ok("loop all"), Command::Loop(Some(LoopArg::All)));
        assert_eq!(ok("loop one"), Command::Loop(Some(LoopArg::One)));
        // Bare `loop` cycles the mode; that is what the `l` hotkey sends.
        assert_eq!(ok("loop"), Command::Loop(None));
        assert_eq!(ok("shuffle on"), Command::Shuffle(Some(true)));
        assert_eq!(ok("shuffle off"), Command::Shuffle(Some(false)));
        assert_eq!(ok("shuffle"), Command::Shuffle(None));
        assert_eq!(ok("list"), Command::List);
        assert_eq!(ok("ls"), Command::List);
        assert_eq!(ok("status"), Command::Status);
        assert_eq!(ok("st"), Command::Status);
        assert_eq!(ok("save"), Command::Save);
        assert_eq!(ok("backend"), Command::Backend(None));
        assert_eq!(ok("backend mpv"), Command::Backend(Some("mpv".into())));
        assert_eq!(ok("keys"), Command::Keys(None));
        assert_eq!(ok("keys mpv"), Command::Keys(Some("mpv".into())));
        assert_eq!(ok("help"), Command::Help(HelpTopic::General));
        assert_eq!(ok("?"), Command::Help(HelpTopic::General));
        assert_eq!(ok("help keys"), Command::Help(HelpTopic::Keys));
        assert_eq!(ok("quit"), Command::Quit);
        assert_eq!(ok("q"), Command::Quit);
        assert_eq!(ok("x"), Command::Quit);
        assert_eq!(ok("exit"), Command::Quit);
    }

    #[test]
    fn blank_and_comment_lines_are_noops() {
        assert_eq!(ok(""), Command::Noop);
        assert_eq!(ok("   \t "), Command::Noop);
        assert_eq!(ok("# a note"), Command::Noop);
    }

    #[test]
    fn single_letters_are_not_aliases() {
        // The keymap is the one-key layer; a bare letter must not mean two things.
        for letter in ["p", "n", "g", "r", "s", "a", "b", "c", "l"] {
            let msg = err(letter);
            assert!(msg.contains("unknown command"), "{letter}: {msg}");
        }
    }

    #[test]
    fn rejects_invalid_input_with_actionable_messages() {
        assert!(err("seek").contains("needs a value"));
        assert!(err("seek abc").contains("seek:"));
        assert!(err("seek 1:90").contains("0-59"));
        assert!(err("seek +5 6").contains("exactly one"));
        assert!(err("speed").contains("needs a value"));
        assert!(err("speed abc").contains("not a number"));
        assert!(err("speed 0").contains("greater than 0"));
        assert!(err("speed -0").contains("greater than 0"));
        assert!(err("speed inf").contains("not a number"));
        assert!(err("goto 0").contains("start at 1"));
        assert!(err("goto x").contains("not a track number"));
        assert!(err("repeat 0").contains("repeat off"));
        assert!(err("repeat banana").contains("not a count"));
        assert!(err("gap abc").contains("millisecond count"));
        assert!(err("gap 70000").contains("at most 60000"));
        assert!(err("loop maybe").contains("off, all, or one"));
        assert!(err("loop all one").contains("at most one"));
        assert!(err("shuffle maybe").contains("neither on nor off"));
        assert!(err("shuffle on off").contains("at most one"));
        assert!(err("ab").contains("ab needs times"));
        assert!(err("ab +5").contains("relative"));
        assert!(err("ab 0:01 0:02 0:03").contains("at most two"));
        assert!(err("play extra").contains("takes no arguments"));
        assert!(err("nonsense").contains("unknown command"));
        assert!(err("backend a b").contains("at most one"));
    }

    #[test]
    fn speed_is_clamped_not_rejected() {
        assert_eq!(ok("speed 10"), Command::Speed(SpeedArg::Set(MAX_SPEED)));
        assert_eq!(ok("speed 0.01"), Command::Speed(SpeedArg::Set(MIN_SPEED)));
        // A nudge is not clamped here; the transport clamps after applying it.
        assert_eq!(ok("speed +10"), Command::Speed(SpeedArg::Nudge(10.0)));
    }

    #[test]
    fn help_text_lists_every_command_name() {
        for name in [
            "play", "pause", "toggle", "next", "prev", "goto", "seek", "speed", "ab", "ab-a",
            "ab-b", "repeat", "gap", "loop", "shuffle", "list", "status", "save", "backend",
            "keys", "help", "quit",
        ] {
            assert!(HELP.contains(name), "help text does not mention `{name}`");
        }
        assert!(
            HELP.contains("0.25-4.0"),
            "help should state the speed range"
        );
    }

    #[test]
    fn overlong_lines_are_rejected_not_truncated() {
        let long = "x".repeat(5000);
        assert!(err(&long).contains("too long"));
    }

    #[test]
    fn multibyte_arguments_survive_parsing() {
        // Paths never appear as command arguments, but keys presets/backends must not
        // panic on non-ASCII input.
        // `to_ascii_lowercase` deliberately leaves non-ASCII alone rather than
        // mangling bytes it cannot case-fold.
        assert_eq!(
            ok("backend БЭКЕНД"),
            Command::Backend(Some("БЭКЕНД".into()))
        );
        assert!(err("键").contains("unknown command"));
    }
}
