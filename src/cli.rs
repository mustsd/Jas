//! Command-line surface: clap definitions and flag validation.
//!
//! Every flag here appears in PLANS.md section 5.1, and `--help` is the contract
//! users read first, so the two must agree.

use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Parser, ValueEnum};

use crate::error::{Error, Result};
use crate::playlist::LoopMode;
use crate::time::{parse_time, TimeSpec};
use crate::transport::{MAX_SPEED, MIN_SPEED};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LoopChoice {
    Off,
    All,
    One,
}

impl From<LoopChoice> for LoopMode {
    fn from(value: LoopChoice) -> Self {
        match value {
            LoopChoice::Off => LoopMode::Off,
            LoopChoice::All => LoopMode::All,
            LoopChoice::One => LoopMode::One,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackendChoice {
    Auto,
    Mpv,
    Ffplay,
    Native,
}

impl BackendChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendChoice::Auto => "auto",
            BackendChoice::Mpv => "mpv",
            BackendChoice::Ffplay => "ffplay",
            BackendChoice::Native => "native",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KeysChoice {
    Default,
    Mpv,
    Off,
}

#[derive(Debug, Parser)]
#[command(
    name = "jas",
    version,
    about = "Local audio player for language learners",
    long_about = "Plays mp3 and other common audio formats from the command line.\n\
                  Built for shadowing drills: A-B loop, segment repeat, gap, and speed\n\
                  with preserved pitch.\n\n\
                  With a terminal on stdin you get single-key control and a `:` prompt.\n\
                  With a pipe or a file you get the same command grammar, one line at a\n\
                  time, so a drill can be scripted.",
    after_help = "KEYBOARD (default keymap)\n  \
                  space    play/pause          n / p    next / prev\n  \
                  left/right  seek -/+5s        a / b    mark A / B\n  \
                  shift+left/right  seek -/+30s c        clear A-B\n  \
                  [ / ]    speed -/+0.05        r        repeat cycle\n  \
                  \\        speed 1.0            g        gap +250 ms\n  \
                  l        cycle loop mode      ?        help keys\n  \
                  :        command prompt       q        quit\n\n\
                  If your terminal is left without echo (only SIGKILL can do that), run:\n  \
                  stty sane"
)]
pub struct Cli {
    /// Audio files, directories (recursive), or .m3u/.m3u8 playlists.
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Loop mode; `None` defers to `config.json`, then to `all`.
    #[arg(long, value_enum)]
    pub r#loop: Option<LoopChoice>,

    /// Shuffle the playlist; combine with --seed for a reproducible order.
    #[arg(long, action = ArgAction::SetTrue)]
    pub shuffle: bool,

    /// Seed for --shuffle. Printed at startup and saved.
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,

    /// Playback speed, 1.0 is normal. Pitch is preserved.
    #[arg(long, value_name = "X", value_parser = parse_speed_arg)]
    pub speed: Option<f64>,

    /// Set mark A at startup.
    #[arg(long, value_name = "T", value_parser = parse_time_arg)]
    pub ab_a: Option<Duration>,

    /// Set mark B at startup.
    #[arg(long, value_name = "T", value_parser = parse_time_arg)]
    pub ab_b: Option<Duration>,

    /// Repeat the A-B segment (or the whole track) N times.
    #[arg(long, value_name = "N", value_parser = parse_repeat_arg)]
    pub repeat: Option<u32>,

    /// Pause between repeats, in milliseconds.
    #[arg(long, value_name = "MS")]
    pub gap: Option<u64>,

    /// Start a specific track, 1-based.
    #[arg(long, value_name = "N")]
    pub track: Option<usize>,

    /// Start playing immediately, without waiting for a keypress.
    #[arg(long, action = ArgAction::SetTrue)]
    pub play: bool,

    /// Print the resolved playlist and exit.
    #[arg(long, action = ArgAction::SetTrue)]
    pub list: bool,

    /// With --list, print JSON.
    #[arg(long, action = ArgAction::SetTrue)]
    pub json: bool,

    /// Print the layout and exit.
    #[arg(long, action = ArgAction::SetTrue)]
    pub doctor: bool,

    /// Force a backend instead of auto-detection. `None` lets `config.json`, then
    /// auto-detection, decide.
    #[arg(long, value_enum)]
    pub backend: Option<BackendChoice>,

    /// Keymap preset for hotkey mode.
    #[arg(long, value_enum)]
    pub keys: Option<KeysChoice>,

    /// Start in command mode with hotkeys disabled.
    #[arg(long = "no-keys", action = ArgAction::SetTrue)]
    pub no_keys: bool,

    /// Keep one status line updated in place (TTY only).
    #[arg(long = "status-line", action = ArgAction::SetTrue)]
    pub status_line: bool,

    /// Do not read or write resume state.
    #[arg(long = "no-state", action = ArgAction::SetTrue)]
    pub no_state: bool,

    /// Use an alternate config/state directory.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Suppress informational output.
    #[arg(short = 'q', long = "quiet", action = ArgAction::SetTrue)]
    pub quiet: bool,

    /// Print more detail about what is happening.
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
    pub verbose: u8,
}

fn parse_speed_arg(raw: &str) -> std::result::Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number (expected {MIN_SPEED}-{MAX_SPEED})"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!(
            "speed must be greater than 0 (expected {MIN_SPEED}-{MAX_SPEED})"
        ));
    }
    if !(MIN_SPEED..=MAX_SPEED).contains(&value) {
        return Err(format!(
            "speed {value} is outside {MIN_SPEED}-{MAX_SPEED}; the value was not silently clamped"
        ));
    }
    Ok(value)
}

fn parse_time_arg(raw: &str) -> std::result::Result<Duration, String> {
    match parse_time(raw) {
        Ok(TimeSpec::Absolute(d)) => Ok(d),
        Ok(TimeSpec::Relative(_)) => Err(format!(
            "`{raw}` is relative; use an absolute time like 0:12 here"
        )),
        Err(e) => Err(e.to_string()),
    }
}

fn parse_repeat_arg(raw: &str) -> std::result::Result<u32, String> {
    match raw.parse::<u32>() {
        Ok(0) => Err("repeat must be at least 1 (omit the flag to disable repeating)".to_string()),
        Ok(n) => Ok(n),
        Err(_) => Err(format!(
            "`{raw}` is not a count (use a number, or omit the flag)"
        )),
    }
}

/// Everything the CLI resolves to, after validation.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub paths: Vec<PathBuf>,
    /// `None` means "not given", so `config.json` and then `all` can decide.
    pub loop_mode: Option<LoopMode>,
    pub shuffle: bool,
    pub seed: Option<u64>,
    pub speed: Option<f64>,
    pub ab: Option<(Duration, Option<Duration>)>,
    pub repeat: Option<u32>,
    pub gap: Duration,
    pub track: Option<usize>,
    pub play: bool,
    pub list: bool,
    pub json: bool,
    pub doctor: bool,
    pub backend: Option<BackendChoice>,
    pub keys: Option<KeysChoice>,
    pub hotkeys: bool,
    pub status_line: bool,
    pub no_state: bool,
    pub config: Option<PathBuf>,
    pub quiet: bool,
    pub verbose: u8,
}

impl Invocation {
    /// Validate flag combinations that clap cannot express, and that would
    /// otherwise be silently ignored.
    pub fn from_cli(cli: Cli) -> Result<Self> {
        if cli.ab_a.is_none() && cli.ab_b.is_some() {
            return Err(Error::usage(
                "--ab-b was given without --ab-a; B is meaningless without A",
            ));
        }
        if let (Some(a), Some(b)) = (cli.ab_a, cli.ab_b) {
            if b <= a {
                return Err(Error::usage(format!(
                    "--ab-b ({}) must be after --ab-a ({}); use `ab-a`/`ab-b` to mark the current position",
                    crate::time::format_time(b),
                    crate::time::format_time(a),
                )));
            }
        }
        if cli.json && !cli.list {
            return Err(Error::usage("--json only applies to --list"));
        }
        if cli.shuffle && cli.seed.is_none() {
            // A seed is required for reproducibility, and the plan requires the
            // seed to be printed, so one is derived below by the caller.
        }
        if cli.track == Some(0) {
            return Err(Error::usage("--track is 1-based, so 0 is not a track"));
        }
        if cli.no_keys && cli.keys == Some(KeysChoice::Default) {
            return Err(Error::usage(
                "--no-keys and --keys default contradict each other",
            ));
        }
        if cli.play && cli.list {
            return Err(Error::usage("--play and --list contradict each other"));
        }

        let hotkeys = !cli.no_keys && cli.keys != Some(KeysChoice::Off);
        Ok(Self {
            paths: cli.paths,
            loop_mode: cli.r#loop.map(Into::into),
            shuffle: cli.shuffle,
            seed: cli.seed,
            speed: cli.speed,
            ab: cli.ab_a.map(|a| (a, cli.ab_b)),
            repeat: cli.repeat,
            gap: Duration::from_millis(cli.gap.unwrap_or(0)),
            track: cli.track,
            play: cli.play,
            list: cli.list,
            json: cli.json,
            doctor: cli.doctor,
            backend: cli.backend,
            keys: cli.keys,
            hotkeys,
            status_line: cli.status_line,
            no_state: cli.no_state,
            config: cli.config,
            quiet: cli.quiet,
            verbose: cli.verbose,
        })
    }

    /// True when the process should do its work and exit without a session.
    pub fn is_oneshot(&self) -> bool {
        self.list || self.doctor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Invocation> {
        let cli = Cli::try_parse_from(std::iter::once("jas").chain(args.iter().copied()))
            .map_err(|e| Error::usage(e.to_string()))?;
        Invocation::from_cli(cli)
    }

    fn ok(args: &[&str]) -> Invocation {
        parse(args).unwrap_or_else(|e| panic!("expected {args:?} to parse: {e}"))
    }

    fn err(args: &[&str]) -> String {
        match parse(args) {
            Ok(inv) => panic!("expected {args:?} to fail, got {inv:?}"),
            Err(e) => {
                assert_eq!(
                    e.exit_code(),
                    crate::error::EXIT_USAGE,
                    "wrong exit code for {args:?}"
                );
                e.message()
            }
        }
    }

    #[test]
    fn the_clap_definition_is_internally_consistent() {
        // Catches a duplicated short flag or a broken derive before runtime.
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_match_the_documented_ones() {
        let inv = ok(&[]);
        // `--loop`, `--speed`, and `--backend` are unset here, so config.json
        // decides and then the built-in default does: `all`, 1.0, and auto.
        assert_eq!(inv.loop_mode, None);
        assert!(!inv.shuffle);
        assert_eq!(inv.speed, None);
        assert!(!inv.play);
        assert!(inv.hotkeys);
        assert!(!inv.status_line);
        assert!(!inv.no_state);
        assert_eq!(inv.gap, Duration::ZERO);
        assert_eq!(inv.backend, None);
    }

    #[test]
    fn the_backend_flag_maps_to_the_documented_names() {
        assert_eq!(
            ok(&["--backend", "auto"]).backend,
            Some(BackendChoice::Auto)
        );
        assert_eq!(ok(&["--backend", "mpv"]).backend, Some(BackendChoice::Mpv));
        assert_eq!(
            ok(&["--backend", "ffplay"]).backend,
            Some(BackendChoice::Ffplay)
        );
        assert_eq!(
            ok(&["--backend", "native"]).backend,
            Some(BackendChoice::Native)
        );
        assert!(err(&["--backend", "vlc"]).contains("invalid value"));
    }

    #[test]
    fn paths_are_collected_in_order() {
        let inv = ok(&["a.mp3", "b/", "list.m3u"]);
        assert_eq!(
            inv.paths,
            vec![
                PathBuf::from("a.mp3"),
                PathBuf::from("b/"),
                PathBuf::from("list.m3u")
            ]
        );
    }

    #[test]
    fn loop_modes_map_to_the_playlist_enum() {
        assert_eq!(ok(&["--loop", "off"]).loop_mode, Some(LoopMode::Off));
        assert_eq!(ok(&["--loop", "all"]).loop_mode, Some(LoopMode::All));
        assert_eq!(ok(&["--loop", "one"]).loop_mode, Some(LoopMode::One));
        assert!(err(&["--loop", "sometimes"]).contains("invalid value"));
    }

    #[test]
    fn a_b_flags_are_validated_as_a_pair() {
        let inv = ok(&["--ab-a", "0:12", "--ab-b", "0:31"]);
        assert_eq!(
            inv.ab,
            Some((Duration::from_secs(12), Some(Duration::from_secs(31))))
        );
        // B alone is meaningless.
        assert!(err(&["--ab-b", "0:31"]).contains("without --ab-a"));
        // B before A is a usage error, not a silent reorder.
        assert!(err(&["--ab-a", "0:31", "--ab-b", "0:12"]).contains("must be after"));
        // Relative times are rejected here; they only make sense at the prompt.
        assert!(err(&["--ab-a", "+5"]).contains("relative"));
    }

    #[test]
    fn speeding_outside_the_range_is_an_error_not_a_clamp() {
        assert_eq!(ok(&["--speed", "0.75"]).speed, Some(0.75));
        let msg = err(&["--speed", "10"]);
        assert!(msg.contains("outside"), "{msg}");
        assert!(msg.contains("not silently clamped"), "{msg}");
        assert!(err(&["--speed", "0"]).contains("greater than 0"));
        assert!(err(&["--speed", "abc"]).contains("not a number"));
    }

    #[test]
    fn repeat_and_gap_are_validated() {
        assert_eq!(ok(&["--repeat", "3"]).repeat, Some(3));
        assert!(err(&["--repeat", "0"]).contains("at least 1"));
        assert!(err(&["--repeat", "many"]).contains("not a count"));
        assert_eq!(ok(&["--gap", "250"]).gap, Duration::from_millis(250));
        // `--gap=-5` reaches the value parser and fails there; `--gap -5` is
        // rejected by clap as an unexpected argument. Either way it is a usage
        // error rather than a silently accepted negative gap.
        assert!(err(&["--gap=-5"]).contains("invalid value"));
        assert!(!err(&["--gap", "-5"]).is_empty());
    }

    #[test]
    fn the_keys_flags_agree_with_each_other() {
        assert!(!ok(&["--no-keys"]).hotkeys);
        assert!(!ok(&["--keys", "off"]).hotkeys);
        assert!(ok(&["--keys", "mpv"]).hotkeys);
        assert!(err(&["--no-keys", "--keys", "default"]).contains("contradict"));
    }

    #[test]
    fn contradictory_actions_are_rejected() {
        assert!(err(&["--play", "--list"]).contains("contradict"));
        assert!(err(&["--json"]).contains("--list"));
        assert!(err(&["--track", "0"]).contains("1-based"));
    }

    #[test]
    fn help_mentions_the_documented_surface() {
        let text = Cli::command().render_long_help().to_string();
        for flag in [
            "--loop",
            "--shuffle",
            "--seed",
            "--speed",
            "--ab-a",
            "--ab-b",
            "--repeat",
            "--gap",
            "--play",
            "--list",
            "--json",
            "--backend",
            "--keys",
            "--no-keys",
            "--status-line",
            "--no-state",
            "--config",
        ] {
            assert!(text.contains(flag), "`--help` does not mention {flag}");
        }
        // And documents the terminal-restore escape hatch required by R10.
        assert!(
            text.contains("stty sane"),
            "`--help` must document the recovery"
        );
        assert!(text.contains("space"), "`--help` should list the keys");
    }

    #[test]
    fn the_config_flag_takes_a_path() {
        assert_eq!(
            ok(&["--config", "/tmp/jas"]).config,
            Some(PathBuf::from("/tmp/jas"))
        );
    }

    #[test]
    fn non_ascii_paths_survive_argument_parsing() {
        let inv = ok(&["/موسيقى/درس.mp3", "/音乐/第二课.mp3", "/x/🎧.mp3"]);
        assert_eq!(inv.paths.len(), 3);
        assert_eq!(inv.paths[0], PathBuf::from("/موسيقى/درس.mp3"));
        assert_eq!(inv.paths[2], PathBuf::from("/x/🎧.mp3"));
    }

    #[test]
    fn quiet_and_verbose_are_accepted_in_any_order() {
        assert!(ok(&["-q"]).quiet);
        assert_eq!(ok(&["-v", "-v"]).verbose, 2);
        assert!(ok(&["-v", "-q"]).quiet);
    }
}
