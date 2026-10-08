//! Entry point: parse args, dispatch, map errors to exit codes.
//!
//! Exit codes are the CLI contract (PLANS.md section 5.6):
//! 0 success, 1 usage, 2 runtime, 3 no playable input, 130 interrupted.

mod cli;
mod commands;
mod error;
mod input;
mod keys;
mod lineedit;
mod player;
mod playlist;
mod repl;
mod session;
mod state;
mod term;
mod time;
mod transport;

use std::io::{IsTerminal, Write};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;

use crate::playlist::playlist_json;
use cli::{Cli, Invocation, KeysChoice};
use error::{Error, Result};
use input::InputLayer;
use player::detect::{self, Preference};
use playlist::{LoopMode, Playlist};
use session::{Session, SessionOptions};
use state::Store;
use transport::RealClock;

fn main() {
    // Installed before anything can enter raw mode, so a panic anywhere below
    // restores the terminal (R10).
    term::install_panic_hook();

    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("jas: {}", e.message());
            if matches!(e, Error::Usage(_)) {
                eprintln!("try `jas --help`");
            }
            e.exit_code()
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32> {
    let cli = match Cli::try_parse_from(std::env::args_os()) {
        Ok(cli) => cli,
        Err(e) => {
            // clap prints help/version to stdout; a real error goes to stderr and
            // exits 1 by our contract rather than clap's default 2.
            if e.use_stderr() {
                eprint!("{e}");
                return Ok(error::EXIT_USAGE);
            }
            print!("{e}");
            return Ok(error::EXIT_OK);
        }
    };

    if cli.verbose > 0 {
        // Verbose logging is deliberately minimal: one line per phase.
        eprintln!("jas: verbose mode");
    }
    let inv = Invocation::from_cli(cli)?;

    // The store is built first because the config influences everything below:
    // the backend choice, the defaults, the keymap, and the status line. Config is
    // read exactly once, here.
    let store = if inv.no_state {
        Store::disabled(inv.config.clone())
    } else {
        Store::new(inv.config.clone())
    };
    let (config, config_notes) = store.load_config();
    for note in &config_notes {
        if !inv.quiet {
            eprintln!("jas: {note}");
        }
    }

    // Resolution order for anything that lives in both places: command line, then
    // config.json, then the built-in default.
    let preference = match &inv.backend {
        Some(choice) => Preference::Named(choice.as_str().to_string()),
        None => match config.backend.as_deref() {
            Some(name) if name != "auto" => Preference::parse(name)?,
            _ => Preference::Auto,
        },
    };

    // 1. One-shot modes never need a backend. `--list` resolves the playlist on
    //    its own; `--doctor` reports what is installed.
    if inv.is_oneshot() {
        if inv.doctor {
            print!("{}", detect::doctor_report(&preference));
            return Ok(error::EXIT_OK);
        }
        return list_mode(&inv);
    }

    // 2. Open the backend before touching the terminal, so a missing dependency
    //    is reported on a normal terminal rather than in raw mode.
    let player = detect::open(&preference)?;

    // 3. Build the session.
    let seed = inv.seed.unwrap_or_else(default_seed);
    let opts = SessionOptions {
        paths: inv.paths.clone(),
        loop_mode: inv.loop_mode.or_else(|| config.loop_mode()),
        shuffle: inv.shuffle,
        seed,
        speed: inv.speed.or_else(|| config.speed()),
        ab: inv.ab,
        repeat: inv.repeat,
        gap: inv.gap,
        keymap_preset: match inv.keys {
            Some(KeysChoice::Mpv) => Some("mpv".to_string()),
            Some(KeysChoice::Default) => Some("default".to_string()),
            Some(KeysChoice::Off) | None => config.keys.clone(),
        },
        keymap_overrides: config
            .keymap
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        status_line: inv.status_line || config.status_line.unwrap_or(false),
        // Playback is started below, after the track and resume point are known.
        play: false,
    };
    // The occupancy flag is shared between the session's writer and the loop's, so
    // a message from either one clears an in-place line before it prints. Without
    // that, a confirmation would be appended to the prompt row.
    let occupied = Arc::new(AtomicBool::new(false));
    let (mut session, notes) = Session::new(
        opts,
        player,
        store,
        Box::new(RealClock::new()),
        Box::new(repl::ScreenWriter::new(
            std::io::stdout(),
            occupied.clone(),
            term::terminal_width().unwrap_or(80),
        )),
    );

    let mut stderr = std::io::stderr();
    for note in &notes {
        if !inv.quiet {
            writeln!(stderr, "jas: {note}").ok();
        }
    }

    if session.is_empty() {
        return Err(Error::no_input(
            "no playable files found. Give Jas a file, a directory, or a playlist; \
             `jas --help` lists the options",
        ));
    }

    // Select the starting track and restore state first, so the banner below can
    // describe what will actually play.
    if let Some(n) = inv.track {
        session.playlist.goto(n)?;
    }
    let restored = session.restore_current();
    let resume_at = restored
        .as_ref()
        .map(|e| e.position())
        .unwrap_or(Duration::ZERO);

    // 4. Interactive setup. The guard is created for a TTY only, and restores on
    //    every exit path including a panic unwind.
    //
    //    `at_a_terminal` drives echoing and in-place drawing; `inv.hotkeys` only
    //    decides which mode to start in. Conflating the two was a bug: `--no-keys`
    //    on a TTY left the user typing into a prompt that was never drawn.
    let at_a_terminal = std::io::stdin().is_terminal();
    let mut guard = match term::TerminalGuard::enter(at_a_terminal) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("jas: {}", e.message());
            eprintln!("{}", term::RAW_MODE_RECOVERY_HINT);
            return Err(Error::runtime("could not take control of the terminal"));
        }
    };
    if guard.is_active() {
        term::install_signal_handlers()?;
    }

    let mut layer = InputLayer::new(inv.hotkeys);
    let mut screen = repl::ScreenWriter::new(
        std::io::stdout(),
        occupied.clone(),
        term::terminal_width().unwrap_or(80),
    );

    // The banner goes first, before anything the session might report. Every line
    // that follows -- a resume note, an A-B mark from `--ab-a`, the first
    // confirmation -- then reads in the order it happened.
    if !inv.quiet {
        write!(screen, "{}", repl::banner(&session, None))?;
        screen.flush()?;
    }
    if inv.verbose > 0 {
        writeln!(
            stderr,
            "jas: state directory {}",
            session.store_dir().display()
        )
        .ok();
    }
    if let Some(entry) = &restored {
        if !inv.quiet && entry.position_ms > 0 {
            writeln!(
                stderr,
                "jas: resuming at {}",
                time::format_time(entry.position())
            )
            .ok();
        }
    }
    if inv.shuffle {
        writeln!(screen, "shuffle seed {}", session.playlist.seed())?;
    }

    // CLI overrides win over stored state. These run *after* the banner so their
    // confirmations appear below it rather than above.
    if let Some(speed) = inv.speed {
        session.configure_speed(speed);
    }
    if let Some((a, b)) = inv.ab {
        session.run(commands::Command::Ab {
            a: time::TimeSpec::Absolute(a),
            b: b.map(time::TimeSpec::Absolute),
        });
    }
    if let Some(n) = inv.repeat {
        session.run(commands::Command::Repeat(commands::RepeatArg::Count(n)));
    }
    if inv.gap > Duration::ZERO {
        session.run(commands::Command::Gap(commands::GapArg::Set(inv.gap)));
    }
    if let Some(KeysChoice::Mpv) = inv.keys {
        session.run(commands::Command::Keys(Some("mpv".into())));
    }

    if inv.play {
        session.start(resume_at)?;
    } else {
        // Load the track and hold at the resume point, silently: the first `play`
        // or space bar then continues from there instead of restarting the file.
        session.prepare(resume_at)?;
    }

    let repl_opts = repl::ReplOptions {
        status_line: session.status_line_enabled() && at_a_terminal && !inv.quiet,
        interactive: at_a_terminal,
        // The real width is read from the terminal on each iteration.
        fixed_width: None,
    };
    let code = repl::run(&mut session, &mut layer, &repl_opts, &mut screen);

    // Restore before printing anything else, so the final line is on a normal
    // terminal rather than appended to an in-place status line.
    guard.restore();
    session.save_progress();
    code
}
/// `--list`: resolve the playlist and print it, without needing a backend.
fn list_mode(inv: &Invocation) -> Result<i32> {
    let paths = if inv.paths.is_empty() {
        // Fall back to the last playlist, so `jas --list` after a session shows it.
        let store = Store::new(inv.config.clone());
        let (state, notes) = store.load_state();
        for note in &notes {
            eprintln!("jas: {note}");
        }
        state
            .playlist
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect()
    } else {
        inv.paths.clone()
    };

    let loaded = Playlist::load(&paths, LoopMode::All, inv.seed.unwrap_or(1));
    for problem in &loaded.problems {
        eprintln!("jas: {problem}");
    }
    if loaded.playlist.is_empty() {
        return Err(Error::no_input("no playable files found"));
    }
    let mut out = std::io::stdout();
    if inv.json {
        writeln!(out, "{}", playlist_json(&loaded.playlist)?)?;
    } else {
        for (i, track) in loaded.playlist.tracks().enumerate() {
            writeln!(out, "{:>4}  {}", i + 1, track.path.display())?;
        }
    }
    Ok(error::EXIT_OK)
}

/// A seed that is stable enough to reproduce but different per run when the user
/// did not ask for a specific one.
fn default_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        | 1
}
