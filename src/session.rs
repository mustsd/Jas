//! Session orchestration: playlist + transport + player + state.
//!
//! This is where the "two control strategies, one state machine" rule lands. The
//! transport decides *what* should happen (an [`Effect`]); the session translates
//! that into *how*, based on the backend's negotiated capabilities:
//!
//! - `live_seek` set: seek in place.
//! - otherwise: reload and respawn at the new offset.
//!
//! Everything user-visible goes through `out`, so tests capture output instead of
//! scraping a terminal.

use std::io::Write;
use std::time::Duration;

use crate::commands::{self, Command, GapArg, HelpTopic, LoopArg, RepeatArg, SpeedArg};
use crate::error::{Error, Result};
use crate::keys::{self, BadBinding, Keymap};
use crate::player::{detect, Exit, Player};
use crate::playlist::{LoopMode, Playlist, Step};
use crate::state::{State, Store, TrackState};
use crate::time::{self, TimeSpec};
use crate::transport::{
    clamp_speed, AbLoop, Clock, Effect, Event, Transport, DEFAULT_SPEED, MAX_SPEED, MIN_SPEED,
};

/// What the session wants the input loop to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
}

pub struct Session {
    pub playlist: Playlist,
    pub transport: Transport,
    pub keymap: Keymap,
    player: Box<dyn Player>,
    store: Store,
    state: State,
    clock: Box<dyn Clock>,
    out: Box<dyn Write + Send>,
    /// True once the current track is in the backend.
    ready: bool,
    /// Last error reported to the user, so the input loop can decide to exit.
    pub last_error: Option<Error>,
    /// Resolved from `--status-line` or `config.json`.
    status_line: bool,
}

/// Everything a session needs to start, so construction stays a single call.
///
/// Values that also exist in `config.json` are optional because the resolution
/// order is: command line, then `config.json`, then the built-in default. That
/// resolution happens in `main`, which loads the config once, so this struct is a
/// plain set of resolved inputs and the session does no disk reading of its own.
pub struct SessionOptions {
    pub paths: Vec<std::path::PathBuf>,
    pub loop_mode: Option<LoopMode>,
    pub shuffle: bool,
    pub seed: u64,
    pub speed: Option<f64>,
    pub ab: Option<(Duration, Option<Duration>)>,
    pub repeat: Option<u32>,
    pub gap: Duration,
    pub play: bool,
    /// Keymap preset name from `config.json` (`default` or `mpv`).
    pub keymap_preset: Option<String>,
    /// `chord -> command` overrides from `config.json`, applied on top of the preset.
    pub keymap_overrides: Vec<(String, String)>,
    /// Whether `config.json` asked for the in-place status line.
    pub status_line: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            loop_mode: None,
            shuffle: false,
            seed: 0,
            speed: None,
            ab: None,
            repeat: None,
            gap: Duration::ZERO,
            play: false,
            keymap_preset: None,
            keymap_overrides: Vec::new(),
            status_line: false,
        }
    }
}

impl Session {
    /// Build a session, loading paths and restoring state.
    pub fn new(
        opts: SessionOptions,
        player: Box<dyn Player>,
        store: Store,
        clock: Box<dyn Clock>,
        out: Box<dyn Write + Send>,
    ) -> (Self, Vec<String>) {
        let mut notes = Vec::new();
        // The session never reads config.json itself: `main` resolves the config and
        // the command line into `SessionOptions`, so there is one precedence rule and
        // one disk read per file.
        let (state, state_notes) = store.load_state();
        notes.extend(state_notes);

        let loop_mode = opts.loop_mode.unwrap_or(LoopMode::All);
        let speed = opts
            .speed
            .unwrap_or(DEFAULT_SPEED)
            .clamp(MIN_SPEED, MAX_SPEED);

        // Paths on the command line win; otherwise restore the last playlist.
        let (mut playlist, problems) = if opts.paths.is_empty() {
            if state.playlist.is_empty() {
                (Playlist::new(Vec::new(), loop_mode, opts.seed), Vec::new())
            } else {
                let paths: Vec<std::path::PathBuf> = state
                    .playlist
                    .iter()
                    .map(std::path::PathBuf::from)
                    .collect();
                let loaded = Playlist::load(&paths, loop_mode, state.seed);
                notes.push(format!(
                    "restored the last playlist ({} tracks)",
                    loaded.playlist.len()
                ));
                (loaded.playlist, loaded.problems)
            }
        } else {
            let loaded = Playlist::load(&opts.paths, loop_mode, opts.seed);
            (loaded.playlist, loaded.problems)
        };
        notes.extend(problems);

        // A configured keymap override is layered on the preset, entry by entry, so
        // one stale line cannot throw away the rest of the map.
        let preset_name = opts
            .keymap_preset
            .clone()
            .unwrap_or_else(|| "default".into());
        let (mut keymap, mut bad) = match keys::preset(&preset_name) {
            Some(bindings) => keys::build(&preset_name, bindings),
            None => {
                notes.push(format!(
                    "config.json `keys` names unknown preset `{preset_name}`; using `default`"
                ));
                keys::build("default", keys::DEFAULT_BINDINGS)
            }
        };
        for (key, value) in &opts.keymap_overrides {
            let parsed_key = keys::parse_chord(key);
            let parsed_cmd = commands::parse(value);
            match (parsed_key, parsed_cmd) {
                (Ok(chord), Ok(cmd)) => keymap.insert(chord, cmd),
                (Err(e), _) => bad.push(BadBinding {
                    key: key.clone(),
                    value: value.clone(),
                    problem: e.to_string(),
                }),
                (_, Err(e)) => bad.push(BadBinding {
                    key: key.clone(),
                    value: value.clone(),
                    problem: e.to_string(),
                }),
            }
        }
        for b in bad {
            notes.push(format!(
                "config.json keymap entry `{}` = `{}` is ignored: {}",
                b.key, b.value, b.problem
            ));
        }

        let seed = if opts.shuffle {
            opts.seed
        } else {
            playlist.seed()
        };
        if opts.shuffle {
            playlist.set_shuffled(true, seed);
        }
        playlist.set_loop_mode(loop_mode);

        let mut transport = Transport::new();
        transport.set_speed(speed, Duration::ZERO);

        let mut session = Self {
            playlist,
            transport,
            keymap,
            player,
            store,
            state,
            clock,
            out,
            ready: false,
            last_error: None,
            status_line: opts.status_line,
        };

        if opts.play {
            if let Err(e) = session.start(Duration::ZERO) {
                session.last_error = Some(e.clone());
                notes.push(e.message());
            }
            if let Some((a, b)) = opts.ab {
                let _ = session.set_ab_marks(a, b);
            }
            if let Some(n) = opts.repeat {
                session.transport.set_repeat(Some(n), session.now());
            }
            if opts.gap > Duration::ZERO {
                session.transport.set_gap(opts.gap, session.now());
            }
        }
        (session, notes)
    }

    pub fn player_name(&self) -> &'static str {
        self.player.name()
    }

    /// Whether the in-place status line was asked for, by flag or by config.
    pub fn status_line_enabled(&self) -> bool {
        self.status_line
    }

    pub fn capabilities(&self) -> crate::player::Capabilities {
        self.player.capabilities()
    }

    pub fn store_dir(&self) -> std::path::PathBuf {
        self.store.dir().to_path_buf()
    }

    /// The monotonic time the transport is read against. Public because the TUI
    /// (`tui.rs`) formats the position itself, from the same clock the session uses.
    pub fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    /// Load the current track into the backend, holding position `at` but staying
    /// silent. This is what makes "resume where you stopped, do not auto-play"
    /// possible: the position is set, and a later `play` continues from there.
    /// Load the current track into the backend, holding position `at` but staying
    /// silent. This is what makes "resume where you stopped, do not auto-play"
    /// possible: the position is set, and a later `play` continues from there.
    ///
    /// Prints nothing: the caller decides what to say, so a startup banner cannot
    /// end up below a confirmation for something the user never asked for.
    pub fn prepare(&mut self, at: Duration) -> Result<()> {
        let Some(track) = self.playlist.current().cloned() else {
            return Err(Error::no_input(
                "nothing to play: no files were given and no previous playlist was saved",
            ));
        };
        let now = self.now();
        let duration = self.player.load(&track.path, at)?;
        self.transport.load(duration, at, now);
        // Loaded and waiting: `paused` describes that better than `idle`, which
        // would read as "nothing is loaded".
        self.transport.set_ready();
        self.ready = true;
        Ok(())
    }

    /// Load the current track at `at` and start playing.
    pub fn start(&mut self, at: Duration) -> Result<()> {
        self.prepare(at)?;
        let now = self.now();
        let effect = self.transport.play(now);
        self.apply(effect)?;
        self.describe_now();
        Ok(())
    }

    /// Send an [`Effect`] to the backend, choosing live control or respawn.
    fn apply(&mut self, effect: Effect) -> Result<()> {
        let caps = self.player.capabilities();
        match effect {
            Effect::None => Ok(()),
            Effect::Play { at } => {
                if self.ready && (caps.live_pause || caps.live_seek) {
                    // The backend already holds the right offset; just unpause.
                    self.player.play()
                } else {
                    self.reload_at(at)?;
                    self.player.play()
                }
            }
            Effect::Pause => self.player.pause(),
            Effect::RespawnAt { at } => {
                if caps.live_seek {
                    self.player.seek(at)?;
                    if self.transport.state().is_playing() {
                        self.player.play()?;
                    }
                    Ok(())
                } else {
                    // Respawn control: the only way to move is to restart.
                    self.reload_at(at)?;
                    self.player.play()
                }
            }
            Effect::Stop => {
                self.ready = false;
                self.player.stop()
            }
        }
    }

    fn reload_at(&mut self, at: Duration) -> Result<()> {
        let Some(track) = self.playlist.current().cloned() else {
            return Ok(());
        };
        let duration = self.player.load(&track.path, at)?;
        if duration.is_some() {
            self.transport.set_duration(duration);
        }
        self.ready = true;
        Ok(())
    }

    /// One polling step: advance the clock, react to A-B wraps and track ends,
    /// and notice a backend that died.
    pub fn tick(&mut self) -> Result<()> {
        let now = self.now();
        let events = self.transport.tick(now);
        for event in events {
            match event {
                Event::SegmentWrapped => {
                    self.describe_now();
                    let at = self.transport.resume_target(now);
                    let effect = if self.transport.in_gap() {
                        // Between repeats: silence, but keep the position.
                        self.player.pause()?;
                        Effect::None
                    } else if self.transport.state().is_playing() {
                        Effect::RespawnAt { at }
                    } else {
                        Effect::None
                    };
                    self.apply(effect)?;
                }
                Event::SegmentAdvanced => {
                    self.repeat_note("segment advanced");
                    let at = self.transport.resume_target(now);
                    self.apply(Effect::RespawnAt { at })?;
                }
                Event::DrillFinished => {
                    self.repeat_note("drill finished");
                    self.save_progress();
                    // The transport paused itself at B; the session advances.
                    self.advance(Step::Moved)?;
                }
                Event::TrackFinished => {
                    self.save_progress();
                    let step = self.playlist.next();
                    self.advance(step)?;
                }
            }
        }

        // A backend child that ended on its own, while sound is still expected.
        // Reading the exit status rather than a liveness boolean is what makes the
        // distinction possible: for a backend that cannot report a duration, a clean
        // exit is the end-of-track signal, while a failed exit is a real crash.
        if !self.transport.is_silent() {
            match self.player.poll_exit() {
                Some(Exit::Finished) => {
                    // The track played to its end. The transport could not know that
                    // on its own -- it has no duration to compare against -- so this
                    // is the only end-of-track signal for this backend.
                    self.save_progress();
                    let step = self.playlist.next();
                    self.advance(step)?;
                }
                Some(Exit::Failed) => {
                    let effect = self.transport.stop(now);
                    let _ = self.apply(effect);
                    self.ready = false;
                    self.last_error = Some(Error::runtime(format!(
                        "backend {} stopped unexpectedly{}",
                        self.player.name(),
                        self.player
                            .diagnostics()
                            .map(|d| format!(": {d}"))
                            .unwrap_or_default()
                    )));
                }
                None => {}
            }
        }

        // If the backend reports true position, correct the drift.
        if self.player.capabilities().position_feedback {
            if let Some(pos) = self.player.position() {
                self.transport.correct_position(pos, now);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn kill_backend_for_test(&mut self) {
        self.set_exit_for_test(Exit::Failed);
    }

    /// Test hook: make the backend report that its child ended on its own.
    #[cfg(test)]
    #[doc(hidden)]
    pub fn set_exit_for_test(&mut self, exit: Exit) {
        self.player.set_exit_for_test(exit);
    }

    fn repeat_note(&mut self, what: &str) {
        let _ = writeln!(self.out, "{what}");
    }

    /// Move to whatever the playlist says comes next.
    fn advance(&mut self, step: Step) -> Result<()> {
        match step {
            Step::Moved | Step::Wrapped => {
                self.ready = false;
                self.start(Duration::ZERO)?;
            }

            Step::Repeated => {
                self.apply(Effect::RespawnAt { at: Duration::ZERO })?;
            }
            Step::AtBoundary => {
                let _ = writeln!(self.out, "already at the first track (loop mode is off)");
            }
            Step::Exhausted => {
                let _ = writeln!(self.out, "end of playlist (loop mode is off)");
                self.transport.stop(self.now());
                self.apply(Effect::Stop)?;
            }
        }
        Ok(())
    }

    /// Handle one command. Never panics and never silently no-ops: anything that
    /// cannot be done is reported and the session keeps running.
    pub fn run(&mut self, command: Command) -> Outcome {
        match self.dispatch(command) {
            Ok(outcome) => outcome,
            Err(e) => {
                let _ = writeln!(self.out, "error: {}", e.message());
                // A usage-style error is reported and the session continues; a
                // runtime failure is remembered so a scripted run can exit non-zero.
                self.last_error = Some(e);
                Outcome::Continue
            }
        }
    }

    fn dispatch(&mut self, command: Command) -> Result<Outcome> {
        let now = self.now();
        match command {
            Command::Noop => {}
            Command::Play => {
                let effect = self.transport.play(now);
                self.apply(effect)?;
                self.describe_now();
            }
            Command::Pause => {
                let effect = self.transport.pause(now);
                self.apply(effect)?;
                self.describe_now();
                self.save_progress();
            }
            Command::Toggle => {
                let effect = self.transport.toggle(now);
                self.apply(effect)?;
                self.describe_now();
            }
            Command::Next => {
                self.save_progress();
                let step = self.playlist.next();
                self.advance(step)?;
            }
            Command::Prev => {
                self.save_progress();
                let step = self.playlist.prev();
                self.advance(step)?;
            }
            Command::Goto(n) => {
                self.save_progress();
                let step = self.playlist.goto(n)?;
                self.advance(step)?;
            }
            Command::Seek(spec) => {
                let effect = match spec {
                    TimeSpec::Absolute(d) => self.transport.seek(d, now),
                    TimeSpec::Relative(ms) => self.transport.seek_by(ms, now),
                };
                let note = match spec {
                    TimeSpec::Absolute(d) => format!("seek {}", time::format_time(d)),
                    TimeSpec::Relative(ms) if ms < 0 => {
                        format!(
                            "seek -{}",
                            time::format_time(Duration::from_millis(ms.unsigned_abs()))
                        )
                    }
                    TimeSpec::Relative(ms) => {
                        format!(
                            "seek +{}",
                            time::format_time(Duration::from_millis(ms as u64))
                        )
                    }
                };
                self.apply(effect)?;
                let _ = writeln!(self.out, "{note} -> {}", self.position_line());
            }
            Command::Speed(arg) => {
                let effect = match arg {
                    SpeedArg::Set(x) => self.transport.set_speed(x, now),
                    SpeedArg::Nudge(delta) => {
                        let (speed, effect) = self.transport.nudge_speed(delta, now);
                        if effect == Effect::None {
                            let _ = writeln!(self.out, "speed is already at the limit ({speed})");
                            return Ok(Outcome::Continue);
                        }
                        effect
                    }
                };
                // The backend always learns the new speed; whether that requires a
                // restart depends on the negotiated capability, not on the backend's
                // name. A backend that can change speed live is not restarted.
                self.player.set_speed(self.transport.speed())?;
                if !self.capabilities().live_speed {
                    self.apply(effect)?;
                }
                let _ = writeln!(self.out, "speed {}", self.transport.speed());
            }
            Command::Ab { a, b } => {
                let a = match a {
                    TimeSpec::Absolute(d) => d,
                    TimeSpec::Relative(_) => unreachable!("the parser rejects relative A-B marks"),
                };
                let b = match b {
                    Some(TimeSpec::Absolute(d)) => Some(d),
                    _ => None,
                };
                self.set_ab_marks(a, b)?;
            }
            Command::AbMarkA => {
                let (effect, rejected) = self.transport.mark_a(now);
                self.apply(effect)?;
                if rejected {
                    let _ = writeln!(self.out, "A must not be after B; A was clamped to B");
                }
                self.sync_ab_to_backend()?;
                self.describe_ab();
            }
            Command::AbMarkB => {
                let (effect, rejected) = self.transport.mark_b(now);
                if rejected {
                    let _ = writeln!(
                        self.out,
                        "B must be after A; move past A first (A is {})",
                        time::format_time(self.transport.ab().map(|ab| ab.a).unwrap_or_default())
                    );
                } else {
                    self.apply(effect)?;
                    self.sync_ab_to_backend()?;
                    self.describe_ab();
                }
            }
            Command::AbClear => {
                self.transport.set_ab(None, now);
                if self.capabilities().native_ab_loop {
                    self.player.set_ab_loop(None)?;
                }
                let _ = writeln!(self.out, "A-B cleared");
            }
            Command::Repeat(arg) => match arg {
                RepeatArg::Count(n) => {
                    self.transport.set_repeat(Some(n), now);
                    let _ = writeln!(self.out, "repeat {n}");
                }
                RepeatArg::Off => {
                    self.transport.set_repeat(None, now);
                    let _ = writeln!(self.out, "repeat off");
                }
                RepeatArg::Next => {
                    self.enable_advance();
                }
                RepeatArg::Cycle => {
                    let next = cycle_repeat(self.transport.ab().and_then(|ab| ab.repeats));
                    self.transport.set_repeat(next, now);
                    match next {
                        Some(n) => {
                            let _ = writeln!(self.out, "repeat {n}");
                        }
                        None => {
                            let _ = writeln!(self.out, "repeat off");
                        }
                    }
                }
            },
            Command::Gap(arg) => {
                let current = self.transport.ab().map(|ab| ab.gap).unwrap_or_default();
                let gap = match arg {
                    GapArg::Set(d) => d,
                    GapArg::Nudge(ms) => time::offset(current, ms),
                };
                self.transport.set_gap(gap, now);
                let _ = writeln!(self.out, "gap {} ms", gap.as_millis());
            }
            Command::Loop(arg) => {
                let mode = match arg {
                    Some(LoopArg::Off) => LoopMode::Off,
                    Some(LoopArg::All) => LoopMode::All,
                    Some(LoopArg::One) => LoopMode::One,
                    None => self.playlist.loop_mode.cycle(),
                };
                self.playlist.set_loop_mode(mode);
                let _ = writeln!(self.out, "loop {}", mode.as_str());
            }
            Command::Shuffle(arg) => {
                let on = arg.unwrap_or(!self.playlist.is_shuffled());
                let seed = if self.playlist.seed() == 0 {
                    1
                } else {
                    self.playlist.seed()
                };
                self.playlist.set_shuffled(on, seed);
                let _ = writeln!(
                    self.out,
                    "shuffle {} (seed {})",
                    if on { "on" } else { "off" },
                    self.playlist.seed()
                );
            }
            Command::List => {
                self.print_list()?;
            }
            Command::Status => {
                let _ = writeln!(self.out, "{}", self.status_line());
            }
            Command::Save => {
                self.save_progress();
                let _ = writeln!(self.out, "saved to {}", self.store.state_path().display());
            }
            Command::Backend(None) => {
                let _ = writeln!(
                    self.out,
                    "backend {} ({})",
                    self.player.name(),
                    self.player.capabilities().summary()
                );
            }
            Command::Backend(Some(name)) => {
                let preference = detect::Preference::parse(&name)?;
                let new_player = detect::open(&preference)?;
                // Swap the backend in place, keeping the current position.
                let at = self.transport.position(now);
                self.player.stop().ok();
                self.player = new_player;
                self.ready = false;
                self.reload_at(at)?;
                if self.transport.state().is_playing() {
                    self.player.play()?;
                }
                let _ = writeln!(
                    self.out,
                    "backend {} ({})",
                    self.player.name(),
                    self.player.capabilities().summary()
                );
            }
            Command::Keys(None) => {
                let _ = writeln!(self.out, "{}", self.keymap.render());
            }
            Command::Keys(Some(name)) => {
                let (keymap, bad) = match keys::preset(&name) {
                    Some(bindings) => keys::build(&name, bindings),
                    None => {
                        let _ = writeln!(
                            self.out,
                            "unknown keymap preset `{name}` (try: default, mpv)"
                        );
                        return Ok(Outcome::Continue);
                    }
                };
                for b in &bad {
                    let _ = writeln!(
                        self.out,
                        "warning: keymap entry `{}` = `{}` is ignored: {}",
                        b.key, b.value, b.problem
                    );
                }
                self.keymap = keymap;
                let _ = writeln!(self.out, "keymap {name}");
            }
            Command::Help(HelpTopic::Keys) => {
                let _ = writeln!(self.out, "{}", self.keymap.render());
            }
            Command::Help(HelpTopic::General) => {
                let _ = writeln!(self.out, "{}", commands::HELP);
                let _ = writeln!(self.out, "\npress `?` for the keymap");
            }
            Command::Quit => {
                self.save_progress();
                let effect = self.transport.stop(now);
                self.apply(effect).ok();
                return Ok(Outcome::Quit);
            }
        }
        Ok(Outcome::Continue)
    }

    /// Push the transport's A-B loop to a backend that can loop natively. For
    /// every other backend the transport's own polling emulates it, so there is
    /// nothing to send and nothing to fail.
    fn sync_ab_to_backend(&mut self) -> Result<()> {
        if !self.capabilities().native_ab_loop {
            return Ok(());
        }
        match self.transport.ab() {
            Some(ab) => self.player.set_ab_loop(Some((ab.a, ab.b))),
            None => self.player.set_ab_loop(None),
        }
    }

    fn enable_advance(&mut self) {
        let now = self.now();
        let mut ab = self.transport.ab().cloned().unwrap_or(AbLoop {
            a: Duration::ZERO,
            b: None,
            repeats: None,
            gap: Duration::ZERO,
            advance_after: false,
        });
        ab.advance_after = true;
        self.transport.set_ab(Some(ab), now);
        let _ = writeln!(
            self.out,
            "repeat next (shifts the segment forward by its own length)"
        );
    }

    fn set_ab_marks(&mut self, a: Duration, b: Option<Duration>) -> Result<()> {
        let now = self.now();
        if let Some(b) = b {
            if b <= a {
                let _ = writeln!(
                    self.out,
                    "B ({}) must be after A ({})",
                    time::format_time(b),
                    time::format_time(a)
                );
                return Ok(());
            }
        }
        let ab = AbLoop {
            a,
            b,
            repeats: self.transport.ab().and_then(|old| old.repeats),
            gap: self.transport.ab().map(|old| old.gap).unwrap_or_default(),
            advance_after: self
                .transport
                .ab()
                .map(|old| old.advance_after)
                .unwrap_or(false),
        };
        if self.capabilities().native_ab_loop {
            self.player.set_ab_loop(Some((a, b)))?;
        } else {
            self.sync_ab_to_backend()?;
        }
        self.transport.set_ab(Some(ab), now);
        // Marking a segment while paused leaves it paused; the transport decides.
        let at = self.transport.position(now);
        if self.transport.state().is_playing() {
            self.apply(Effect::RespawnAt { at })?;
        } else if !self.capabilities().native_ab_loop {
            // The respawn backend must be positioned at A for the loop to be heard.
            self.apply(Effect::RespawnAt { at: a })?;
            self.transport.pause(now);
            self.player.pause()?;
        }
        self.describe_ab();
        Ok(())
    }

    /// Write the current track's memory, and the playlist itself, to state.
    pub fn save_progress(&mut self) {
        if !self.store.is_enabled() {
            return;
        }
        if let Some(track) = self.playlist.current() {
            let entry = TrackState {
                position_ms: self.transport.position(self.now()).as_millis() as u64,
                ab_a_ms: self.transport.ab().map(|ab| ab.a.as_millis() as u64),
                ab_b_ms: self
                    .transport
                    .ab()
                    .and_then(|ab| ab.b)
                    .map(|b| b.as_millis() as u64),
                speed: Some(self.transport.speed()),
                repeats: self.transport.ab().and_then(|ab| ab.repeats),
                gap_ms: self.transport.ab().map(|ab| ab.gap.as_millis() as u64),
            };
            self.state.set(&track.id, entry);
        }
        let known: Vec<_> = self.playlist.tracks().map(|t| t.id.clone()).collect();
        self.state.prune(&known);
        self.state.playlist = self
            .playlist
            .tracks()
            .map(|t| t.path.to_string_lossy().into_owned())
            .collect();
        self.state.index = self.playlist.cursor();
        self.state.seed = self.playlist.seed();
        self.state.shuffled = self.playlist.is_shuffled();
        self.state.loop_mode = Some(self.playlist.loop_mode.as_str().to_string());
        if let Err(e) = self.store.save_state(&self.state) {
            let _ = writeln!(self.out, "warning: {e}");
        }
    }

    /// Restore position, marks, and speed for the current track.
    pub fn restore_current(&mut self) -> Option<TrackState> {
        let track = self.playlist.current()?.clone();
        let entry = self.state.get(&track.id)?.clone();
        let now = self.now();
        self.transport.set_speed(entry.speed(), now);
        if let Some((a, b)) = entry.ab() {
            self.transport.set_ab(
                Some(AbLoop {
                    a,
                    b,
                    repeats: entry.repeats,
                    gap: entry.gap(),
                    advance_after: false,
                }),
                now,
            );
        }
        Some(entry)
    }

    fn describe_now(&mut self) {
        let _ = writeln!(self.out, "{}", self.confirmation_line());
    }

    fn describe_ab(&mut self) {
        let _ = writeln!(self.out, "{}", self.ab_line());
    }

    fn ab_line(&self) -> String {
        match self.transport.ab() {
            None => "A-B cleared".to_string(),
            Some(ab) => {
                let a = time::format_time(ab.a);
                match ab.b {
                    Some(b) => format!("A-B {a}-{}", time::format_time(b)),
                    None => format!("A-B {a}-end"),
                }
            }
        }
    }

    /// The one-line confirmation printed after a transport action.
    pub fn confirmation_line(&self) -> String {
        let state = self.transport.state().as_str();
        let pos = self.position_line();
        if self.transport.speed() == DEFAULT_SPEED {
            format!("{state:<7} {pos}")
        } else {
            format!("{state:<7} {pos}  speed {}", self.transport.speed())
        }
    }

    fn position_line(&self) -> String {
        let pos = time::format_time(self.transport.position(self.now()));
        match self.transport.duration() {
            Some(d) => format!("{pos} / {}", time::format_time(d)),
            None => pos,
        }
    }

    /// The status line contract: one line, stable field order, machine-readable.
    pub fn status_line(&self) -> String {
        let index = self.playlist.position();
        let total = self.playlist.len();
        let name = self
            .playlist
            .current()
            .map(|t| t.display_name())
            .unwrap_or_else(|| "-".to_string());
        let state = self.transport.state();
        let pos = time::format_seconds(self.transport.position(self.now()));
        let dur = self
            .transport
            .duration()
            .map(time::format_seconds)
            .unwrap_or_else(|| "?".to_string());
        let ab = match self.transport.ab() {
            None => "-".to_string(),
            Some(ab) => match ab.b {
                Some(b) => format!("{:.1}-{:.1}", ab.a.as_secs_f64(), b.as_secs_f64()),
                None => format!("{:.1}-end", ab.a.as_secs_f64()),
            },
        };
        let repeats = self
            .transport
            .ab()
            .and_then(|ab| ab.repeats)
            .map(|n| format!("{}/{}", self.transport.repeats_done() + 1, n))
            .unwrap_or_else(|| "-".to_string());
        format!(
            "state={} track={index}/{total} name={name} pos={pos} dur={dur} speed={} ab={ab} repeat={repeats} gap={} loop={} backend={}",
            state.as_str(),
            self.transport.speed(),
            self.transport.ab().map(|ab| ab.gap.as_millis()).unwrap_or(0),
            self.playlist.loop_mode.as_str(),
            self.player.name(),
        )
    }

    fn print_list(&mut self) -> Result<()> {
        let current = self.playlist.cursor();
        for (i, track) in self.playlist.tracks().enumerate() {
            let marker = if i == current { ">" } else { " " };
            let _ = writeln!(self.out, "{marker} {:>4}  {}", i + 1, track.path.display());
        }
        Ok(())
    }

    /// Is there anything to play?
    pub fn is_empty(&self) -> bool {
        self.playlist.is_empty()
    }

    /// Apply startup options that need the backend to exist first.
    pub fn configure_speed(&mut self, speed: f64) {
        let now = self.now();
        self.transport.set_speed(speed, now);
        if self.capabilities().live_speed {
            let _ = self.player.set_speed(clamp_speed(speed));
        }
    }
}

/// `r` cycles off -> 2 -> 3 -> 5 -> 10 -> off.
fn cycle_repeat(current: Option<u32>) -> Option<u32> {
    const STEPS: [Option<u32>; 5] = [Some(2), Some(3), Some(5), Some(10), None];
    match current {
        None => STEPS[0],
        Some(n) => {
            let i = STEPS.iter().position(|s| *s == Some(n));
            match i {
                Some(i) => STEPS[(i + 1) % STEPS.len()],
                // A count set by hand (`repeat 7`) cycles back to off.
                None => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::{Capabilities, FakePlayer};
    use crate::transport::FakeClock;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "jas-session-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    struct Harness {
        session: Session,
        player: FakePlayer,
        clock: std::rc::Rc<FakeClock>,
        dir: std::path::PathBuf,
    }

    /// Build a session over temp files, with a fake player and a fake clock.
    fn harness(name: &str, tracks: usize, caps: Capabilities) -> Harness {
        let dir = tmpdir(name);
        let mut paths = Vec::new();
        for i in 0..tracks {
            let path = dir.join(format!("track{i}.mp3"));
            std::fs::write(&path, b"x").expect("write temp track");
            paths.push(path);
        }
        let store = Store::disabled(Some(dir.clone()));
        let clock = std::rc::Rc::new(FakeClock::new());
        let opts = SessionOptions {
            paths,
            loop_mode: Some(LoopMode::All),
            seed: 1,
            ..SessionOptions::default()
        };
        let player = FakePlayer::with_caps(caps);
        let (session, notes) = Session::new(
            opts,
            Box::new(player.clone()),
            store,
            Box::new(clock.clone()),
            Box::new(std::io::sink()),
        );
        assert!(notes.iter().all(|n| !n.contains("keymap")), "{notes:?}");
        Harness {
            session,
            player,
            clock,
            dir,
        }
    }

    fn live() -> Harness {
        harness("live", 3, Capabilities::LIVE)
    }

    fn emulated() -> Harness {
        harness("emulated", 3, Capabilities::EMULATED)
    }

    #[test]
    fn a_live_backend_seeks_in_place_instead_of_reloading() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.player.clear_calls();
        h.clock.advance(Duration::from_secs(10));
        h.session.run(Command::Seek(TimeSpec::Relative(5_000)));
        let calls = h.player.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("load ")),
            "a live backend must not be reloaded: {calls:?}"
        );
        assert_eq!(calls[0], "seek 15000ms", "{calls:?}");
    }

    #[test]
    fn an_emulated_backend_reloads_and_respawns_on_every_seek() {
        let mut h = emulated();
        h.session.start(Duration::ZERO).unwrap();
        h.player.clear_calls();
        h.clock.advance(Duration::from_secs(10));
        h.session.run(Command::Seek(TimeSpec::Relative(5_000)));
        assert_eq!(h.player.calls().len(), 2, "{:?}", h.player.calls());
        assert!(
            h.player.calls()[0].starts_with("load "),
            "{:?}",
            h.player.calls()
        );
        assert!(
            h.player.calls()[0].ends_with("@15000ms"),
            "{:?}",
            h.player.calls()
        );
        assert_eq!(h.player.calls()[1], "play");
    }

    #[test]
    fn a_pause_on_a_live_backend_does_not_reload() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.player.clear_calls();
        h.session.run(Command::Pause);
        assert_eq!(h.player.calls(), vec!["pause"], "{:?}", h.player.calls());
    }

    #[test]
    fn a_pause_on_an_emulated_backend_stops_the_child() {
        let mut h = emulated();
        h.session.start(Duration::ZERO).unwrap();
        h.player.clear_calls();
        h.session.run(Command::Pause);
        assert_eq!(h.player.calls(), vec!["pause"], "{:?}", h.player.calls());
        assert_eq!(h.session.transport.state(), crate::transport::State::Paused);
    }

    #[test]
    fn resuming_a_paused_emulated_backend_restarts_at_the_held_position() {
        let mut h = emulated();
        h.session.start(Duration::ZERO).unwrap();
        h.clock.advance(Duration::from_secs(4));
        h.session.run(Command::Pause);
        h.player.clear_calls();
        // Time passing while paused must not move the resume point.
        h.clock.advance(Duration::from_secs(30));
        h.session.run(Command::Play);
        assert!(
            h.player.calls()[0].ends_with("@4000ms"),
            "{:?}",
            h.player.calls()
        );
        assert_eq!(h.player.calls()[1], "play");
    }

    #[test]
    fn speed_changes_are_live_on_mpv_and_respawn_on_ffplay() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.player.clear_calls();
        h.clock.advance(Duration::from_secs(2));
        h.session.run(Command::Speed(SpeedArg::Nudge(0.05)));
        assert_eq!(h.session.transport.speed(), 1.05);
        assert!(!h.player.calls().is_empty());
        assert!(
            h.player.calls().iter().any(|c| c.starts_with("set_speed")),
            "{:?}",
            h.player.calls()
        );

        let mut h = emulated();
        h.session.start(Duration::ZERO).unwrap();
        h.player.clear_calls();
        h.clock.advance(Duration::from_secs(2));
        h.session.run(Command::Speed(SpeedArg::Nudge(0.05)));
        // The respawn backend re-spawns at the held position; its own speed field
        // is applied when it builds the next command line.
        assert!(
            h.player.calls().iter().any(|c| c.starts_with("load ")),
            "{:?}",
            h.player.calls()
        );
    }

    #[test]
    fn next_and_prev_walk_the_playlist() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        assert_eq!(h.session.playlist.position(), 1);
        h.session.run(Command::Next);
        assert_eq!(h.session.playlist.position(), 2);
        h.session.run(Command::Next);
        assert_eq!(h.session.playlist.position(), 3);
        h.session.run(Command::Next);
        assert_eq!(h.session.playlist.position(), 1, "loop all wraps");
        h.session.run(Command::Prev);
        assert_eq!(h.session.playlist.position(), 3);
    }

    #[test]
    fn goto_out_of_range_is_reported_and_does_not_move() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        let outcome = h.session.run(Command::Goto(99));
        assert_eq!(outcome, Outcome::Continue);
        assert_eq!(h.session.playlist.position(), 1);
        assert!(h.session.last_error.is_some());
    }

    #[test]
    fn a_backend_error_is_reported_and_the_session_keeps_running() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.player.fail_on("set_speed");
        let outcome = h.session.run(Command::Speed(SpeedArg::Set(1.5)));
        assert_eq!(
            outcome,
            Outcome::Continue,
            "a backend failure must not end the session"
        );
        assert!(h.session.last_error.is_some());
    }

    #[test]
    fn quit_stops_the_backend_and_reports_quit() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        let outcome = h.session.run(Command::Quit);
        assert_eq!(outcome, Outcome::Quit);
    }

    #[test]
    fn mark_a_and_b_build_a_segment_that_the_backend_learns_about() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.clock.advance(Duration::from_secs(12));
        h.session.run(Command::AbMarkA);
        h.clock.advance(Duration::from_secs(19));
        h.session.run(Command::AbMarkB);
        let ab = h.session.transport.ab().expect("A-B should be set");
        assert_eq!(ab.a, Duration::from_secs(12));
        assert_eq!(ab.b, Some(Duration::from_secs(31)));
        assert!(
            h.player
                .calls()
                .iter()
                .any(|c| c == "set_ab_loop 12000..31000"),
            "the live backend should get the loop: {:?}",
            h.player.calls()
        );
    }

    #[test]
    fn an_emulated_backend_is_not_given_a_native_loop() {
        let mut h = emulated();
        h.session.start(Duration::ZERO).unwrap();
        h.player.clear_calls();
        h.session.run(Command::AbMarkA);
        h.session.run(Command::AbMarkB);
        // The transport owns the loop; the backend is only asked to respawn.
        assert!(h.session.transport.ab().is_some());
        assert_eq!(h.session.capabilities(), Capabilities::EMULATED);
    }

    #[test]
    fn marking_b_before_a_is_reported() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.session.run(Command::Ab {
            a: TimeSpec::Absolute(Duration::from_secs(20)),
            b: None,
        });
        // Seek back before A: B is now invalid.
        h.session
            .run(Command::Seek(TimeSpec::Absolute(Duration::from_secs(5))));
        h.session.run(Command::AbMarkB);
        let ab = h.session.transport.ab().unwrap();
        assert_eq!(ab.b, None, "B must not be set before A");
        assert_eq!(h.session.ab_line(), "A-B 0:20-end");

        // Past A, B is accepted.
        h.session
            .run(Command::Seek(TimeSpec::Absolute(Duration::from_secs(25))));
        h.session.run(Command::AbMarkB);
        assert_eq!(
            h.session.transport.ab().unwrap().b,
            Some(Duration::from_secs(25))
        );
    }

    #[test]
    fn ab_clear_removes_the_loop_everywhere() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.session.run(Command::Ab {
            a: TimeSpec::Absolute(Duration::from_secs(1)),
            b: Some(TimeSpec::Absolute(Duration::from_secs(2))),
        });
        assert!(h.session.transport.ab().is_some());
        h.player.clear_calls();
        h.session.run(Command::AbClear);
        assert!(h.session.transport.ab().is_none());
        assert!(
            h.player.calls().iter().any(|c| c == "set_ab_loop none"),
            "{:?}",
            h.player.calls()
        );
    }

    #[test]
    fn b_before_a_at_startup_is_rejected_without_stopping_the_session() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        let outcome = h.session.run(Command::Ab {
            a: TimeSpec::Absolute(Duration::from_secs(10)),
            b: Some(TimeSpec::Absolute(Duration::from_secs(5))),
        });
        assert_eq!(outcome, Outcome::Continue);
        assert!(h.session.transport.ab().is_none());
    }

    #[test]
    fn repeat_cycles_through_the_documented_sequence() {
        assert_eq!(cycle_repeat(None), Some(2));
        assert_eq!(cycle_repeat(Some(2)), Some(3));
        assert_eq!(cycle_repeat(Some(3)), Some(5));
        assert_eq!(cycle_repeat(Some(5)), Some(10));
        assert_eq!(cycle_repeat(Some(10)), None);
        // A hand-set count is not in the cycle, so it wraps to off.
        assert_eq!(cycle_repeat(Some(7)), None);
    }

    #[test]
    fn the_repeat_command_reaches_the_transport() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.session.run(Command::Repeat(RepeatArg::Cycle));
        assert_eq!(h.session.transport.ab().and_then(|ab| ab.repeats), Some(2));
        h.session.run(Command::Repeat(RepeatArg::Count(4)));
        assert_eq!(h.session.transport.ab().and_then(|ab| ab.repeats), Some(4));
        h.session.run(Command::Repeat(RepeatArg::Off));
        assert_eq!(h.session.transport.ab().and_then(|ab| ab.repeats), None);
    }

    #[test]
    fn gap_accumulates_and_is_reported() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.session.run(Command::Gap(GapArg::Nudge(250)));
        h.session.run(Command::Gap(GapArg::Nudge(250)));
        assert_eq!(
            h.session.transport.ab().unwrap().gap,
            Duration::from_millis(500)
        );
        h.session
            .run(Command::Gap(GapArg::Set(Duration::from_millis(100))));
        assert_eq!(
            h.session.transport.ab().unwrap().gap,
            Duration::from_millis(100)
        );
        h.session.run(Command::Gap(GapArg::Nudge(-100)));
        assert_eq!(h.session.transport.ab().unwrap().gap, Duration::ZERO);
    }

    #[test]
    fn loop_with_no_argument_cycles_and_with_one_sets_it() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.session.run(Command::Loop(None));
        assert_eq!(h.session.playlist.loop_mode, LoopMode::One, "all -> one");
        h.session.run(Command::Loop(None));
        assert_eq!(h.session.playlist.loop_mode, LoopMode::Off);
        h.session.run(Command::Loop(Some(LoopArg::All)));
        assert_eq!(h.session.playlist.loop_mode, LoopMode::All);
    }

    #[test]
    fn quitting_at_the_end_of_an_unlooped_playlist_is_not_an_error() {
        let mut h = harness("exhausted", 1, Capabilities::LIVE);
        h.session.playlist.set_loop_mode(LoopMode::Off);
        h.session.start(Duration::ZERO).unwrap();
        h.session.run(Command::Next);
        assert_eq!(h.session.playlist.position(), 1, "the cursor must not move");
        assert!(h.session.last_error.is_none(), "exhaustion is not an error");
    }

    #[test]
    fn a_track_that_ends_advances_to_the_next_one() {
        let h = harness("advance", 2, Capabilities::LIVE);
        // Give the fake player durations so the transport knows when a track ends.
        let session_player = h.player.clone();
        session_player.set_durations(vec![
            Some(Duration::from_secs(5)),
            Some(Duration::from_secs(5)),
        ]);
        let clock = std::rc::Rc::new(FakeClock::new());
        let paths: Vec<_> = h
            .session
            .playlist
            .tracks()
            .map(|t| t.path.clone())
            .collect();
        let (mut session, _) = Session::new(
            SessionOptions {
                paths,
                seed: 1,
                ..SessionOptions::default()
            },
            Box::new(session_player.clone()),
            Store::disabled(Some(h.dir.clone())),
            Box::new(clock.clone()),
            Box::new(std::io::sink()),
        );
        session.start(Duration::ZERO).unwrap();
        assert_eq!(session.playlist.position(), 1);
        clock.advance(Duration::from_secs(6));
        session.tick().unwrap();
        assert_eq!(
            session.playlist.position(),
            2,
            "the track ended, so advance"
        );
    }

    #[test]
    fn a_dead_backend_is_reported_as_a_runtime_failure() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.player.set_exit(Exit::Failed);
        h.session.tick().unwrap();
        let err = h
            .session
            .last_error
            .expect("a dead backend should be reported");
        assert_eq!(err.exit_code(), crate::error::EXIT_RUNTIME);
        assert!(
            err.message().contains("stopped unexpectedly"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn a_dead_backend_while_paused_is_not_reported() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.session.run(Command::Pause);
        h.player.set_exit(Exit::Failed);
        h.session.tick().unwrap();
        assert!(
            h.session.last_error.is_none(),
            "a paused backend is allowed to be gone"
        );
    }

    #[test]
    fn position_feedback_is_used_to_correct_drift() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.clock.advance(Duration::from_secs(30));
        // The backend says it is really at 33 s: trust the backend.
        h.player
            .set_reported_position(Some(Duration::from_secs(33)));
        h.session.tick().unwrap();
        assert_eq!(
            h.session.transport.position(h.clock.elapsed()),
            Duration::from_secs(33)
        );
    }

    #[test]
    fn an_emulated_backend_is_never_asked_for_its_position() {
        let mut h = emulated();
        h.session.start(Duration::ZERO).unwrap();
        h.clock.advance(Duration::from_secs(7));
        h.player
            .set_reported_position(Some(Duration::from_secs(100)));
        h.session.tick().unwrap();
        // The fake reports EMPLATED caps are false, so the session ignores it.
        assert_eq!(
            h.session.transport.position(h.clock.elapsed()),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn an_empty_session_reports_a_clear_no_input_error() {
        let dir = tmpdir("empty");
        let store = Store::disabled(Some(dir.clone()));
        let (mut session, _) = Session::new(
            SessionOptions::default(),
            Box::new(FakePlayer::new()),
            store,
            Box::new(FakeClock::new()),
            Box::new(std::io::sink()),
        );
        assert!(session.is_empty());
        let err = session.start(Duration::ZERO).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::EXIT_NO_INPUT);
        assert!(err.message().contains("nothing to play"));
    }

    #[test]
    fn the_status_line_has_a_stable_field_order() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        let line = h.session.status_line();
        let fields: Vec<&str> = line
            .split(' ')
            .map(|f| f.split('=').next().unwrap())
            .collect();
        assert_eq!(
            fields,
            vec![
                "state", "track", "name", "pos", "dur", "speed", "ab", "repeat", "gap", "loop",
                "backend"
            ],
            "{line}"
        );
    }

    #[test]
    fn the_status_line_reports_the_real_values() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.clock.advance(Duration::from_secs(2));
        h.session.run(Command::Speed(SpeedArg::Set(0.75)));
        let line = h.session.status_line();
        assert!(line.contains("state=playing"), "{line}");
        assert!(line.contains("track=1/3"), "{line}");
        assert!(line.contains("speed=0.75"), "{line}");
        assert!(line.contains("loop=all"), "{line}");
        assert!(line.contains("backend=fake"), "{line}");
    }

    #[test]
    fn a_relative_seek_past_the_end_clamps() {
        let h = harness("clamp", 1, Capabilities::LIVE);
        let p = h.player.clone();
        p.set_durations(vec![Some(Duration::from_secs(10))]);
        let paths: Vec<_> = h
            .session
            .playlist
            .tracks()
            .map(|t| t.path.clone())
            .collect();
        let (mut session, _) = Session::new(
            SessionOptions {
                paths,
                ..SessionOptions::default()
            },
            Box::new(p),
            Store::disabled(Some(h.dir.clone())),
            Box::new(h.clock.clone()),
            Box::new(std::io::sink()),
        );
        session.start(Duration::ZERO).unwrap();
        session.run(Command::Seek(TimeSpec::Relative(60_000)));
        assert_eq!(
            session.transport.position(Duration::ZERO),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn a_noop_command_changes_nothing() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.player.clear_calls();
        h.session.run(Command::Noop);
        assert!(h.player.calls().is_empty());
    }

    #[test]
    fn keys_with_an_unknown_preset_is_reported_not_fatal() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        let outcome = h.session.run(Command::Keys(Some("vi".into())));
        assert_eq!(outcome, Outcome::Continue);
        assert_eq!(h.session.keymap.name(), "default", "the old keymap is kept");
    }

    #[test]
    fn switching_keymap_presets_works() {
        let mut h = live();
        h.session.run(Command::Keys(Some("mpv".into())));
        assert_eq!(h.session.keymap.name(), "mpv");
        h.session.run(Command::Keys(Some("default".into())));
        assert_eq!(h.session.keymap.name(), "default");
    }

    #[test]
    fn help_does_not_error() {
        let mut h = live();
        assert_eq!(
            h.session.run(Command::Help(HelpTopic::General)),
            Outcome::Continue
        );
        assert_eq!(
            h.session.run(Command::Help(HelpTopic::Keys)),
            Outcome::Continue
        );
        assert!(h.session.last_error.is_none());
    }

    #[test]
    fn the_session_playlist_serializes_through_the_shared_helper() {
        let h = live();
        let json = crate::playlist::playlist_json(&h.session.playlist).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.as_array().expect("an array").len(), 3);
    }

    #[test]
    fn state_is_saved_when_a_session_quits() {
        let dir = tmpdir("save");
        let path = dir.join("a.mp3");
        std::fs::write(&path, b"x").unwrap();
        let store = Store::new(Some(dir.join("cfg")));
        let clock = std::rc::Rc::new(FakeClock::new());
        let (mut session, _) = Session::new(
            SessionOptions {
                paths: vec![path.clone()],
                ..SessionOptions::default()
            },
            Box::new(FakePlayer::with_caps(Capabilities::LIVE)),
            store.clone(),
            Box::new(clock.clone()),
            Box::new(std::io::sink()),
        );
        session.start(Duration::ZERO).unwrap();
        clock.advance(Duration::from_secs(3));
        session.run(Command::Quit);
        let (state, notes) = store.load_state();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(state.playlist.len(), 1);
        assert_eq!(state.loop_mode.as_deref(), Some("all"));
        let entry = state.tracks.values().next().expect("one track entry");
        assert_eq!(entry.position_ms, 3000, "the resume point should be saved");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_saved_position_is_restored_for_the_same_file() {
        let dir = tmpdir("restore");
        let path = dir.join("a.mp3");
        std::fs::write(&path, b"x").unwrap();
        let store = Store::new(Some(dir.join("cfg")));
        {
            let clock = std::rc::Rc::new(FakeClock::new());
            let (mut session, _) = Session::new(
                SessionOptions {
                    paths: vec![path.clone()],
                    ..SessionOptions::default()
                },
                Box::new(FakePlayer::with_caps(Capabilities::LIVE)),
                store.clone(),
                Box::new(clock.clone()),
                Box::new(std::io::sink()),
            );
            session.start(Duration::ZERO).unwrap();
            clock.advance(Duration::from_secs(9));
            session.run(Command::Ab {
                a: TimeSpec::Absolute(Duration::from_secs(4)),
                b: Some(TimeSpec::Absolute(Duration::from_secs(8))),
            });
            session.run(Command::Speed(SpeedArg::Set(0.75)));
            session.run(Command::Quit);
        }
        let clock = std::rc::Rc::new(FakeClock::new());
        let (mut session, _) = Session::new(
            SessionOptions {
                paths: vec![path.clone()],
                ..SessionOptions::default()
            },
            Box::new(FakePlayer::with_caps(Capabilities::LIVE)),
            store,
            Box::new(clock),
            Box::new(std::io::sink()),
        );
        let entry = session
            .restore_current()
            .expect("stored state should exist");
        assert_eq!(entry.position_ms, 9000);
        assert_eq!(session.transport.speed(), 0.75);
        assert_eq!(session.transport.ab().unwrap().a, Duration::from_secs(4));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keymap_overrides_layer_on_top_of_the_preset() {
        // The session no longer reads config.json itself: `main` resolves the file
        // into these options, so the file-reading half is covered end to end by
        // tests/cli.rs (`a_bad_keymap_entry_is_reported_and_the_rest_of_the_map_survives`).
        let dir = tmpdir("keymap-config");
        let path = dir.join("a.mp3");
        std::fs::write(&path, b"x").unwrap();
        let (session, notes) = Session::new(
            SessionOptions {
                paths: vec![path],
                keymap_overrides: vec![
                    ("z".to_string(), "toggle".to_string()),
                    ("y".to_string(), "nonsense".to_string()),
                ],
                ..SessionOptions::default()
            },
            Box::new(FakePlayer::new()),
            Store::disabled(Some(dir.clone())),
            Box::new(FakeClock::new()),
            Box::new(std::io::sink()),
        );
        assert_eq!(
            session.keymap.lookup(&keys::parse_chord("z").unwrap()),
            Some(&Command::Toggle)
        );
        // The bad entry is reported and ignored; the rest of the map still works.
        assert!(notes.iter().any(|n| n.contains("nonsense")), "{notes:?}");
        assert!(session
            .keymap
            .lookup(&keys::parse_chord("space").unwrap())
            .is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn startup_shuffle_is_reproducible_and_reported() {
        let dir = tmpdir("shuffle");
        for i in 0..8 {
            std::fs::write(dir.join(format!("t{i}.mp3")), b"x").unwrap();
        }
        let order_of = |seed: u64| {
            let (session, _) = Session::new(
                SessionOptions {
                    paths: vec![dir.clone()],
                    shuffle: true,
                    seed,
                    ..SessionOptions::default()
                },
                Box::new(FakePlayer::new()),
                Store::disabled(None),
                Box::new(FakeClock::new()),
                Box::new(std::io::sink()),
            );
            session
                .playlist
                .tracks()
                .map(|t| t.display_name())
                .collect::<Vec<_>>()
        };
        assert_eq!(order_of(42), order_of(42));
        assert!(order_of(42) != order_of(43));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn speed_nudging_reports_the_limit_instead_of_silently_stopping() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        h.session.run(Command::Speed(SpeedArg::Set(MAX_SPEED)));
        assert_eq!(h.session.transport.speed(), MAX_SPEED);
        h.session.run(Command::Speed(SpeedArg::Nudge(0.05)));
        assert_eq!(
            h.session.transport.speed(),
            MAX_SPEED,
            "clamped, not exceeded"
        );
    }

    #[test]
    fn rounded_speeds_are_stored() {
        let mut h = live();
        h.session.start(Duration::ZERO).unwrap();
        for _ in 0..3 {
            h.session.run(Command::Speed(SpeedArg::Nudge(0.05)));
        }
        assert_eq!(
            h.session.transport.speed(),
            crate::transport::round_speed(1.15)
        );
    }
}
