//! The session loop: read input, dispatch commands, print status.
//!
//! The loop has one job beyond dispatch: it must keep ticking. `session.tick()`
//! advances the A-B clock and notices a track end, so it runs on every iteration,
//! including while waiting for input. That is why `input.rs` polls with a timeout
//! instead of blocking on `read()`.
//!
//! # Layout rule
//!
//! **Exactly one thing may own the cursor row at a time**, and every *message* must
//! begin at column 0. The two are related: an in-place line (the `:` prompt, or the
//! status line) is painted without a trailing newline, so anything printed while it
//! is on screen would land in the middle of it -- which is precisely how a new line
//! of output ends up not starting at the beginning of a line.
//!
//! [`ScreenWriter`] enforces this. It erases the in-place line lazily, on the first
//! byte of the next message, so a message always starts at column 0. Lazy matters:
//! erasing eagerly on every loop iteration would blank and repaint the row 25 times
//! a second and flicker on a slow terminal.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::input::{Input, InputLayer, Mode};
use crate::session::{Outcome, Session};

/// How long to wait for input before ticking again. Small enough that an A-B wrap
/// is noticed within the documented +/-150 ms, large enough to stay idle.
pub const TICK_INTERVAL: Duration = Duration::from_millis(40);

/// How often progress is written to state while playing.
const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default)]
pub struct ReplOptions {
    /// Keep one status line updated in place with `\r` (TTY only).
    pub status_line: bool,
    /// There is a human at a terminal: echo the command line and draw in place.
    /// This is *not* the same question as "are hotkeys on" -- `--no-keys` at a
    /// terminal still needs a visible prompt.
    pub interactive: bool,
    /// A fixed terminal width, for tests. `None` means "ask the terminal each
    /// iteration", which also picks up a mid-session resize.
    pub fixed_width: Option<usize>,
}

/// Return to column 0 and erase the rest of the row.
const ERASE_LINE: &[u8] = b"\r\x1b[2K";

/// Which end of an over-long line to keep when it has to be shortened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truncate {
    /// Keep the beginning. Right for a status line, where `state=` and the track
    /// number are the fields worth seeing.
    Head,
    /// Keep the end, marked with a leading `…`. Right for a command being typed,
    /// where the cursor -- and the user's attention -- is at the end.
    Tail,
}

/// Shorten `text` to at most `limit` display columns.
///
/// An in-place line that wraps is worse than a truncated one: `\r\x1b[2K` only
/// erases the row the cursor is on, so a wrapped line leaves the row above showing
/// the tail of the previous line, and every later line appears to start in the
/// wrong place. Keeping the line inside the terminal is what makes one erase enough.
///
/// Width is measured in display columns rather than characters, because a CJK
/// character is two columns wide: counting characters would let a line of Chinese
/// text overflow a terminal that it looks short enough for.
fn fit(text: &str, limit: usize, keep: Truncate) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    let limit = limit.max(1);
    if text.width() <= limit {
        return text.to_string();
    }
    match keep {
        Truncate::Head => {
            let mut out = String::new();
            let mut width = 0;
            for c in text.chars() {
                let cw = c.width().unwrap_or(0);
                if width + cw > limit {
                    break;
                }
                out.push(c);
                width += cw;
            }
            // Something was dropped (this branch only runs when it was), so prefer
            // to end on a field boundary: `... dur=?` reads as a deliberate
            // abbreviation, while `... speed=` looks like a rendering glitch.
            if let Some(cut) = out.rfind(' ') {
                if cut > 0 {
                    out.truncate(cut);
                }
            }
            out
        }
        Truncate::Tail => {
            // One column goes to the marker, so the result still fits.
            let budget = limit.saturating_sub(1);
            let mut kept: Vec<char> = Vec::new();
            let mut width = 0;
            for c in text.chars().rev() {
                let cw = c.width().unwrap_or(0);
                if width + cw > budget {
                    break;
                }
                kept.push(c);
                width += cw;
            }
            let tail: String = kept.into_iter().rev().collect();
            format!("…{tail}")
        }
    }
}

pub struct ScreenWriter<W: Write> {
    inner: W,
    occupied: Arc<AtomicBool>,
    /// Terminal width in columns. In-place lines are fitted to it so they cannot
    /// wrap; see [`fit`].
    width: usize,
}

impl<W: Write> ScreenWriter<W> {
    pub fn new(inner: W, occupied: Arc<AtomicBool>, width: usize) -> Self {
        Self {
            inner,
            occupied,
            width,
        }
    }

    /// Update the known terminal width, so a resize does not start producing
    /// lines wide enough to wrap.
    pub fn set_width(&mut self, width: usize) {
        self.width = width;
    }

    /// The usable column count: one less than the terminal width, so the cursor
    /// stays strictly inside the last column and the terminal never applies its
    /// deferred wrap.
    fn limit(&self) -> usize {
        self.width.saturating_sub(1).max(1)
    }

    /// Paint the in-place line, keeping its beginning when it is too wide. No
    /// newline, so the row stays owned until something else clears it.
    pub fn paint(&mut self, text: &str) -> io::Result<()> {
        let fitted = fit(text, self.limit(), Truncate::Head);
        self.paint_raw(&fitted)
    }

    /// Paint the in-place line, keeping its end when it is too wide: this is the
    /// command prompt, and the part being typed is the part worth seeing.
    pub fn paint_prompt(&mut self, text: &str) -> io::Result<()> {
        let fitted = fit(text, self.limit(), Truncate::Tail);
        self.paint_raw(&fitted)
    }

    fn paint_raw(&mut self, text: &str) -> io::Result<()> {
        self.inner.write_all(ERASE_LINE)?;
        self.inner.write_all(text.as_bytes())?;
        // No newline follows, so a line-buffered sink would not flush on its own.
        self.inner.flush()?;
        self.occupied.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Keep the in-place line as a permanent row and move under it. Used when a
    /// typed command is submitted: the transcript should show what was typed, with
    /// the command's output on the following row.
    pub fn keep_line(&mut self) -> io::Result<()> {
        if self.occupied.swap(false, Ordering::SeqCst) {
            // `\r\n`, not `\n`: in raw mode `OPOST` may be off, in which case a bare
            // newline moves down without returning to column 0 and every line would
            // drift right. The extra `\r` is harmless when the kernel adds its own.
            self.inner.write_all(b"\r\n")?;
            self.inner.flush()?;
        }
        Ok(())
    }

    /// True while an in-place line currently owns the row.
    pub fn is_painted(&self) -> bool {
        self.occupied.load(Ordering::SeqCst)
    }
}

impl<W: Write> Write for ScreenWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !buf.is_empty() && self.occupied.swap(false, Ordering::SeqCst) {
            self.inner.write_all(ERASE_LINE)?;
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Run the session to completion. Returns the process exit code.
pub fn run<W: Write>(
    session: &mut Session,
    layer: &mut InputLayer,
    opts: &ReplOptions,
    out: &mut ScreenWriter<W>,
) -> Result<i32> {
    let mut last_save = Instant::now();
    // The last in-place text drawn, and whether it is still on screen. Repainting an
    // identical line 25 times a second is wasted work and visible flicker.
    let mut last_paint: Option<String> = None;
    // A runtime failure ends the session with its own exit code, unless the user
    // later quits cleanly: a scripted run must be able to notice the failure.
    let mut pending_code = crate::error::EXIT_OK;

    loop {
        // 1. Advance the clock and react. This happens before input so a wrap is
        //    heard as soon as it is due, not after the next keypress. Anything this
        //    prints goes through the session's own `ScreenWriter`, which clears the
        //    in-place line first because the two share the occupancy flag.
        session.tick()?;

        if let Some(err) = session.last_error.clone() {
            writeln!(out, "error: {}", err.message())?;
            session.last_error = None;
            pending_code = err.exit_code();
        }

        // 2. Wait for input, but never longer than one tick.
        let event = layer.poll(&session.keymap, TICK_INTERVAL)?;

        // 3. Handle it. Nothing is painted yet, so whatever this prints starts at
        //    column 0.
        match event {
            Input::Idle | Input::Redraw => {}
            Input::ParseError(msg) => {
                writeln!(out, "{msg}")?;
            }
            Input::CancelLine => {
                // The abandoned line is erased by the repaint below, so a cancelled
                // command leaves no half-typed debris behind.
            }
            Input::Interrupt => {
                session.save_progress();
                return Ok(crate::error::EXIT_INTERRUPTED);
            }
            Input::Command(cmd) => {
                // Terminate the typed line first, so the transcript reads as "what I
                // typed" followed by "what happened". Without this the command's own
                // output is appended to the prompt row.
                if opts.interactive && layer.mode == Mode::Command {
                    out.keep_line()?;
                    last_paint = None;
                }
                let outcome = session.run(cmd);
                layer.after_submit();
                match outcome {
                    Outcome::Quit => {
                        session.save_progress();
                        return Ok(crate::error::EXIT_OK);
                    }
                    Outcome::Continue => {}
                }
            }
        }

        // 4. Repaint whatever owns the row, if its text changed or the row was taken
        //    by a message. Command mode wins: a half-typed command is more useful to
        //    see than a position readout, and one owner keeps the two from fighting.
        // Keep the paint width current: a fixed width for tests, otherwise the real
        // terminal, re-read each iteration so a resize mid-session does not start
        // producing lines wide enough to wrap.
        if let Some(width) = opts.fixed_width {
            out.set_width(width);
        } else if opts.interactive {
            if let Some(width) = crate::term::terminal_width() {
                out.set_width(width);
            }
        }

        let wanted = if opts.interactive && layer.mode == Mode::Command {
            Some((layer.prompt(), true))
        } else if opts.status_line {
            Some((session.status_line(), false))
        } else {
            None
        };
        match wanted {
            Some((text, is_prompt)) => {
                // `is_painted` is false when a message cleared the row, so the line
                // is redrawn even though its text may be unchanged.
                if !out.is_painted() || last_paint.as_deref() != Some(text.as_str()) {
                    if is_prompt {
                        out.paint_prompt(&text)?;
                    } else {
                        out.paint(&text)?;
                    }
                    last_paint = Some(text);
                }
            }
            None => {
                if out.is_painted() {
                    out.keep_line()?;
                    last_paint = None;
                }
            }
        }

        // 5. Autosave while playing, so a crash costs at most 15 s of position.
        if last_save.elapsed() >= AUTOSAVE_INTERVAL {
            session.save_progress();
            last_save = Instant::now();
        }

        // 6. Decide whether to keep going. Scripted input that has run out only
        //    ends the session once there is also no sound left to make -- which is
        //    what lets `jas --play list.m3u < drill.txt` keep playing to the end.
        if layer.is_exhausted() && !session.transport.is_silent() {
            continue;
        }
        if layer.is_exhausted() {
            // Leave the terminal on a clean row rather than mid-status.
            out.keep_line()?;
            session.save_progress();
            return Ok(pending_code);
        }
    }
}

/// Print the banner shown once at startup: what was loaded and which backend.
pub fn banner(session: &Session, backend_note: Option<&str>) -> String {
    let mut text = format!(
        "jas {} | backend {} ({}) | {} track(s)\n",
        env!("CARGO_PKG_VERSION"),
        session.player_name(),
        session.capabilities().summary(),
        session.playlist.len()
    );
    if let Some(note) = backend_note {
        text.push_str(note);
        text.push('\n');
    }
    text.push_str("type `help` for commands, or press `?` (Ctrl+C exits)\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Command, HelpTopic};
    use crate::keys::{self, Chord, KeyCode};
    use crate::player::{Capabilities, Exit, FakePlayer};
    use crate::playlist::LoopMode;
    use crate::session::SessionOptions;
    use crate::state::Store;
    use crate::transport::Clock;

    /// A clock that advances on every read.
    ///
    /// The loop under test is a `loop {}` that ends when playback ends, so a frozen
    /// fake clock would make it spin forever. This stands in for real time passing:
    /// every tick moves the transport forward.
    #[derive(Debug, Default, Clone)]
    struct SteppingClock {
        now: std::cell::Cell<Duration>,
        step: Duration,
    }

    impl Clock for SteppingClock {
        fn elapsed(&self) -> Duration {
            let next = self.now.get() + self.step;
            self.now.set(next);
            next
        }
    }

    /// A sink shared by the session's writer and the screen, so a test sees the exact
    /// interleaving a terminal would receive.
    ///
    /// Sharing matters: the whole point of the occupancy flag is that the *session's*
    /// writer clears the row the *loop* painted, so they must be built together.
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Capture {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }

        /// The screen a terminal of `width` columns would be showing.
        fn screen(&self, width: usize) -> Vec<String> {
            render_at(&self.bytes(), width)
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.bytes()).into_owned()
        }
    }

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A session wired to a capture, with the loop's screen sharing one occupancy
    /// flag with the session's own writer.
    struct Harness {
        session: Session,
        layer: InputLayer,
        out: ScreenWriter<Capture>,
        capture: Capture,
        dir: std::path::PathBuf,
        width: usize,
    }

    impl Harness {
        fn new(name: &str, tracks: usize, width: usize) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("jas-repl-{name}-{}-{width}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let mut paths = Vec::new();
            for i in 0..tracks {
                let p = dir.join(format!("t{i}.mp3"));
                std::fs::write(&p, b"x").unwrap();
                paths.push(p);
            }

            let capture = Capture::default();
            let occupied = Arc::new(AtomicBool::new(false));
            let (session, _) = Session::new(
                SessionOptions {
                    paths,
                    loop_mode: Some(LoopMode::All),
                    ..SessionOptions::default()
                },
                Box::new(FakePlayer::with_caps(Capabilities::LIVE)),
                Store::disabled(Some(dir.join("cfg"))),
                Box::new(SteppingClock {
                    step: Duration::from_millis(500),
                    ..Default::default()
                }),
                Box::new(ScreenWriter::new(capture.clone(), occupied.clone(), width)),
            );
            let out = ScreenWriter::new(capture.clone(), occupied, width);
            Self {
                session,
                layer: InputLayer::detached(),
                out,
                capture,
                dir,
                width,
            }
        }

        fn opts(&self, status_line: bool, interactive: bool) -> ReplOptions {
            ReplOptions {
                status_line,
                interactive,
                fixed_width: Some(self.width),
            }
        }

        fn run(&mut self, status_line: bool, interactive: bool) -> Result<i32> {
            let opts = self.opts(status_line, interactive);
            run(&mut self.session, &mut self.layer, &opts, &mut self.out)
        }

        fn screen(&self) -> Vec<String> {
            self.capture.screen(self.width)
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// A session with no capture, for tests that only care about the exit code or
    /// about session state rather than about what was printed.
    fn plain(
        name: &str,
        tracks: usize,
        duration: Option<Duration>,
        loop_mode: LoopMode,
    ) -> (Session, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "jas-repl-plain-{name}-{}-{}",
            std::process::id(),
            duration.map(|d| d.as_millis()).unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for i in 0..tracks {
            let p = dir.join(format!("t{i}.mp3"));
            std::fs::write(&p, b"x").unwrap();
            paths.push(p);
        }
        let player = FakePlayer::with_caps(Capabilities::LIVE);
        if let Some(d) = duration {
            player.set_durations((0..tracks).map(|_| Some(d)).collect());
        }
        let (session, _) = Session::new(
            SessionOptions {
                paths,
                loop_mode: Some(loop_mode),
                ..SessionOptions::default()
            },
            Box::new(player),
            Store::disabled(Some(dir.join("cfg"))),
            Box::new(SteppingClock {
                step: Duration::from_millis(500),
                ..Default::default()
            }),
            Box::new(std::io::sink()),
        );
        (session, dir)
    }

    fn screen_only(capture: &Capture, width: usize) -> ScreenWriter<Capture> {
        ScreenWriter::new(capture.clone(), Arc::new(AtomicBool::new(false)), width)
    }

    /// Replay the byte stream onto a grid, the way a terminal would, **including
    /// wrapping**.
    ///
    /// Modelling the wrap is the point: it is what turns "the line is too wide"
    /// into "the erase only cleared the second row, so the first row keeps the old
    /// text and the next line appears not to start at the beginning".
    ///
    /// Trailing blank rows are dropped, so a render can be compared to the list of
    /// lines a user would say they can see.
    fn render_at(bytes: &[u8], width: usize) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes);
        let mut rows: Vec<Vec<char>> = vec![Vec::new()];
        let (mut row, mut col) = (0usize, 0usize);
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\x1b' {
                // Skip a CSI sequence, acting only on the erase-line one. `2K`
                // erases the whole row the cursor is on, not just the part right
                // of it.
                //
                // The final byte is part of the sequence, so it is included in
                // `seq`: comparing only the numeric parameters against "2K" never
                // matches, which silently makes the renderer disagree with a real
                // terminal about erasing.
                let mut j = i + 2;
                let start = j;
                while j < chars.len() && !chars[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let end = (j + 1).min(chars.len());
                let seq: String = chars[start..end].iter().collect();
                if seq == "2K" {
                    rows[row] = Vec::new();
                }
                i = end;
                continue;
            }
            if c == '\n' {
                row += 1;
                col = 0;
                while rows.len() <= row {
                    rows.push(Vec::new());
                }
                i += 1;
                continue;
            }
            if c == '\r' {
                col = 0;
                i += 1;
                continue;
            }
            if col >= width {
                row += 1;
                col = 0;
            }
            while rows.len() <= row {
                rows.push(Vec::new());
            }
            while rows[row].len() < col {
                rows[row].push(' ');
            }
            if col < rows[row].len() {
                rows[row][col] = c;
            } else {
                rows[row].push(c);
            }
            col += 1;
            i += 1;
        }
        let mut lines: Vec<String> = rows
            .iter()
            .map(|r| r.iter().collect::<String>().trim_end().to_string())
            .collect();
        while lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines
    }

    // ---- The loop's own behaviour ----

    #[test]
    fn an_idle_headless_session_exits_immediately() {
        let (mut session, dir) = plain("idle", 1, None, LoopMode::All);
        let mut layer = InputLayer::detached();
        let mut out = screen_only(&Capture::default(), 80);
        let code = run(&mut session, &mut layer, &ReplOptions::default(), &mut out).unwrap();
        assert_eq!(code, crate::error::EXIT_OK);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_playing_session_keeps_running_until_the_track_ends() {
        // Exhausted input must not end a session that still has sound to make: this
        // is what lets `jas --play list.m3u < drill.txt` finish the music.
        let (mut session, dir) = plain("playing", 1, Some(Duration::from_secs(4)), LoopMode::Off);
        session.start(Duration::ZERO).unwrap();
        let mut layer = InputLayer::detached();
        let mut out = screen_only(&Capture::default(), 80);
        let code = run(&mut session, &mut layer, &ReplOptions::default(), &mut out).unwrap();
        assert_eq!(
            code,
            crate::error::EXIT_OK,
            "the track ended and the loop returned"
        );
        assert!(
            !session.transport.state().is_playing(),
            "the loop should only return once playback stopped"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dead_backend_ends_a_headless_session_with_a_runtime_failure() {
        let (mut session, dir) = plain("dead", 1, None, LoopMode::All);
        session.start(Duration::ZERO).unwrap();
        session.kill_backend_for_test();
        let mut layer = InputLayer::detached();
        let mut out = screen_only(&Capture::default(), 80);
        let code = run(&mut session, &mut layer, &ReplOptions::default(), &mut out).unwrap();
        assert_eq!(code, crate::error::EXIT_RUNTIME);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failing_backend_is_reported_once_not_forever() {
        let mut h = Harness::new("failed-once", 1, 80);
        h.session.start(Duration::ZERO).unwrap();
        h.session.set_exit_for_test(Exit::Failed);
        let code = h.run(false, false).unwrap();
        assert_eq!(code, crate::error::EXIT_RUNTIME);
        assert_eq!(
            h.capture.text().matches("stopped unexpectedly").count(),
            1,
            "the failure must be reported exactly once: {}",
            h.capture.text()
        );
    }

    #[test]
    fn a_finished_backend_advances_instead_of_reporting_a_crash() {
        // The bug the pty harness found: `ffplay` exits when the track ends, which
        // used to be reported as "stopped unexpectedly". It is a normal finish, and
        // for a backend with no duration feedback it is the end-of-track signal.
        let (mut session, dir) = plain("finished", 2, None, LoopMode::All);
        session.start(Duration::ZERO).unwrap();
        assert_eq!(session.playlist.position(), 1);
        session.set_exit_for_test(Exit::Finished);
        session.tick().unwrap();
        assert_eq!(
            session.playlist.position(),
            2,
            "a clean exit should advance the playlist"
        );
        assert!(
            session.last_error.is_none(),
            "a finished track is not an error: {:?}",
            session.last_error
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_banner_names_the_backend_and_capabilities() {
        let h = Harness::new("banner", 2, 80);
        let text = banner(&h.session, None);
        assert!(text.contains("backend fake"));
        assert!(text.contains("2 track(s)"));
        assert!(text.contains("help"));
        assert!(
            text.ends_with('\n'),
            "the banner must not leave the cursor mid-row"
        );
    }

    #[test]
    fn hotkeys_still_reach_the_same_commands_through_the_loop_helpers() {
        // The loop itself is I/O; what matters here is that a chord still maps to a
        // parser-accepted command (the drift guard from section 10).
        let map = keys::build("default", keys::DEFAULT_BINDINGS).0;
        for (chord, cmd) in map.bindings() {
            let text = keys::command_text(cmd);
            assert!(
                crate::commands::parse(&text).is_ok(),
                "`{chord}` -> `{text}` does not parse"
            );
        }
        assert_eq!(map.lookup(&Chord::char('q')), Some(&Command::Quit));
        assert_eq!(
            map.lookup(&Chord::new(KeyCode::Space)),
            Some(&Command::Toggle)
        );
    }

    #[test]
    fn the_tick_interval_is_short_enough_for_the_documented_a_b_tolerance() {
        // PLANS.md documents +/-150 ms for an emulated A-B wrap, so the polling
        // interval has to be well inside that.
        assert!(TICK_INTERVAL < Duration::from_millis(150));
    }

    // ---- Screen discipline: one owner per row, messages start at column 0 ----

    #[test]
    fn a_message_with_nothing_painted_is_not_prefixed_with_an_erase() {
        let capture = Capture::default();
        let mut out = screen_only(&capture, 80);
        writeln!(out, "one").unwrap();
        writeln!(out, "two").unwrap();
        assert_eq!(
            capture.bytes().iter().filter(|b| **b == 0x1b).count(),
            0,
            "no erase is needed when nothing is painted"
        );
        assert_eq!(capture.screen(80), vec!["one", "two"]);
    }

    #[test]
    fn a_message_replaces_an_in_place_line_instead_of_joining_it() {
        // The reported symptom: the message must not be appended to the prompt.
        let capture = Capture::default();
        let mut out = screen_only(&capture, 80);
        out.paint(":seek +5").unwrap();
        writeln!(out, "seek +5 -> 0:05 / 3:12").unwrap();
        assert_eq!(
            capture.screen(80),
            vec!["seek +5 -> 0:05 / 3:12"],
            "the erase removes the in-place line, so the message is the only row"
        );
    }

    #[test]
    fn an_erase_is_emitted_once_per_occupied_row_not_once_per_write() {
        let capture = Capture::default();
        let mut out = screen_only(&capture, 80);
        out.paint(":x").unwrap();
        // `paint` erases whatever was there, so the first message adds a second
        // erase. The point of the test is that further writes in the same message
        // add none.
        write!(out, "a").unwrap();
        let after_first = capture.bytes().iter().filter(|b| **b == 0x1b).count();
        write!(out, "b").unwrap();
        writeln!(out).unwrap();
        let after_rest = capture.bytes().iter().filter(|b| **b == 0x1b).count();
        assert_eq!(
            after_first, after_rest,
            "subsequent writes in one message must not erase again"
        );
        assert_eq!(capture.screen(80), vec!["ab"]);
    }

    #[test]
    fn an_empty_write_does_not_clear_the_row() {
        // A stray flushed buffer must not blank the prompt.
        let capture = Capture::default();
        let mut out = screen_only(&capture, 80);
        out.paint(":pl").unwrap();
        out.write_all(b"").unwrap();
        assert!(out.is_painted(), "the row should still be owned");
        assert_eq!(capture.screen(80), vec![":pl"]);
    }

    #[test]
    fn keep_line_turns_the_in_place_line_into_a_kept_row() {
        let capture = Capture::default();
        let mut out = screen_only(&capture, 80);
        out.paint(":status").unwrap();
        out.keep_line().unwrap();
        writeln!(out, "state=paused").unwrap();
        assert_eq!(
            capture.bytes().iter().filter(|b| **b == 0x1b).count(),
            1,
            "keep_line terminated the row, so the message needs no erase"
        );
        assert_eq!(capture.screen(80), vec![":status", "state=paused"]);
    }

    #[test]
    fn keep_line_with_nothing_painted_writes_nothing() {
        let capture = Capture::default();
        let mut out = screen_only(&capture, 80);
        out.keep_line().unwrap();
        assert!(capture.bytes().is_empty());
    }

    #[test]
    fn a_longer_line_erases_the_remains_of_a_shorter_one() {
        // Without the erase, shrinking text leaves the tail of the old line behind.
        let capture = Capture::default();
        let mut out = screen_only(&capture, 80);
        out.paint("state=playing track=1/1 pos=41.2").unwrap();
        out.paint("paused").unwrap();
        assert_eq!(capture.screen(80), vec!["paused"]);
    }

    #[test]
    fn is_painted_reports_the_row_ownership() {
        let capture = Capture::default();
        let mut out = screen_only(&capture, 80);
        assert!(!out.is_painted());
        out.paint("x").unwrap();
        assert!(out.is_painted());
        writeln!(out).unwrap();
        assert!(!out.is_painted(), "a message takes the row back");
    }

    // ---- Fitting: an in-place line must never wrap ----

    #[test]
    fn the_renderer_used_by_these_tests_agrees_with_a_terminal_about_erasing() {
        // A test helper that quietly ignores the erase would make every layout
        // assertion below pass for the wrong reason. The original bug was that the
        // CSI parser compared only the numeric parameters, so "2K" never matched.
        assert_eq!(render_at(b"old text\r\x1b[2Knew", 80), vec!["new"]);
        assert_eq!(render_at(b"old text\r\x1b[2K", 80), vec![""]);
    }

    #[test]
    fn fit_leaves_a_short_line_alone() {
        assert_eq!(fit("paused", 10, Truncate::Head), "paused");
        assert_eq!(fit("paused", 6, Truncate::Head), "paused");
        assert_eq!(fit("paused", 10, Truncate::Tail), "paused");
    }

    #[test]
    fn fit_keeps_the_head_of_a_status_line() {
        let fitted = fit(
            "state=playing track=1/12 name=chapter-one.mp3",
            20,
            Truncate::Head,
        );
        assert_eq!(fitted, "state=playing");
    }

    #[test]
    fn fit_cuts_a_status_line_at_a_field_boundary() {
        // A mid-field stub (`... speed=`) looks like a glitch; ending on a whole
        // field looks like an abbreviation. The real status line is chosen so the
        // hard cut would land inside `speed=1`.
        let line = "state=paused track=1/1 name=lesson.wav pos=0.0 dur=? speed=1 \
                    ab=- repeat=- gap=0 loop=all backend=ffplay";
        let rows = {
            let capture = Capture::default();
            let mut out = screen_only(&capture, 60);
            out.paint(line).unwrap();
            capture.screen(60)
        };
        assert_eq!(rows.len(), 1, "still one row: {rows:?}");
        assert_eq!(
            rows[0], "state=paused track=1/1 name=lesson.wav pos=0.0 dur=?",
            "the fitted line ends on a whole field"
        );
    }

    #[test]
    fn fit_falls_back_to_a_hard_cut_when_there_is_no_boundary() {
        // A single long token has nowhere tidy to cut, so it is cut anyway rather
        // than being emitted at full width and wrapping.
        use unicode_width::UnicodeWidthStr;
        let fitted = fit("aaaaaaaaaaaaaaaaaaaa", 5, Truncate::Head);
        assert_eq!(fitted, "aaaaa");
        assert!(fitted.width() <= 5);
    }

    #[test]
    fn fit_keeps_the_tail_of_a_prompt_and_marks_it() {
        // The cursor sits at the end of a typed command, so the end is what matters.
        use unicode_width::UnicodeWidthStr;
        let fitted = fit("ab 0:12.500 0:31.200", 10, Truncate::Tail);
        assert!(fitted.starts_with('…'), "{fitted:?}");
        assert!(fitted.ends_with("31.200"), "{fitted:?}");
        assert!(
            fitted.width() <= 10,
            "{fitted:?} is {} wide",
            fitted.width()
        );
    }

    #[test]
    fn fit_measures_display_columns_not_characters() {
        // A CJK character is two columns wide, so counting characters would let a
        // line of Chinese text overflow a terminal it looks short enough for.
        //
        // Both literals are space-free: the field-boundary cut would otherwise mask
        // what is being measured here.
        use unicode_width::UnicodeWidthStr;
        let chinese = "第二课中文测试第二课";
        let arabic = "درسالعربية";

        assert_eq!(chinese.chars().count(), 10);
        assert_eq!(chinese.width(), 20);
        assert_eq!(arabic.chars().count(), 10);
        assert_eq!(arabic.width(), 10, "Arabic letters are single width");

        // Ten columns therefore holds five CJK characters, or all ten letters.
        let fitted = fit(chinese, 10, Truncate::Head);
        assert_eq!(fitted.chars().count(), 5);
        assert!(fitted.width() <= 10);
        assert_eq!(fit(arabic, 10, Truncate::Head), arabic);
    }

    #[test]
    fn fit_only_ever_returns_characters_from_its_input() {
        // Building from whole `char`s is what guarantees no code point is split.
        for limit in 1..12 {
            for keep in [Truncate::Head, Truncate::Tail] {
                let fitted = fit("中文🎧درس", limit, keep);
                assert!(
                    fitted.chars().all(|c| "中文🎧درس…".contains(c)),
                    "{fitted:?}"
                );
            }
        }
    }

    #[test]
    fn fit_at_a_degenerate_width_still_returns_usable_text() {
        assert_eq!(fit("paused", 0, Truncate::Head), "p");
        assert!(out_is_painted_at_width(0));
        assert!(out_is_painted_at_width(1));
    }

    fn out_is_painted_at_width(width: usize) -> bool {
        let capture = Capture::default();
        let mut out = screen_only(&capture, width);
        out.paint("state=playing").unwrap();
        out.is_painted()
    }

    #[test]
    fn a_painted_line_never_wraps_even_when_wider_than_the_terminal() {
        // The reproduced bug: at 60 columns the status line wrapped onto a second
        // row, so `\r\x1b[2K` cleared only that row and the row above kept the tail
        // of the previous line.
        let wide = "state=paused track=1/1 name=lesson.wav pos=0.0 dur=? speed=1 \
                    ab=- repeat=- gap=0 loop=all backend=ffplay";
        assert!(wide.len() > 60);
        let capture = Capture::default();
        let mut out = screen_only(&capture, 60);
        out.paint(wide).unwrap();
        out.paint("paused").unwrap();
        assert_eq!(
            capture.screen(60),
            vec!["paused"],
            "a fitted line occupies exactly one row, leaving no debris"
        );
    }

    #[test]
    fn an_over_wide_prompt_keeps_its_end_within_the_terminal() {
        let capture = Capture::default();
        let mut out = screen_only(&capture, 20);
        out.paint_prompt(":ab 0:12.500 0:31.200").unwrap();
        let rows = capture.screen(20);
        assert_eq!(rows.len(), 1, "the prompt must stay on one row: {rows:?}");
        assert!(rows[0].ends_with("0:31.200"), "{:?}", rows[0]);
        assert!(
            rows[0].contains('…'),
            "{:?} should show it was shortened",
            rows[0]
        );
    }

    #[test]
    fn a_message_after_an_over_wide_line_still_starts_at_column_zero() {
        let capture = Capture::default();
        let mut out = screen_only(&capture, 20);
        out.paint("state=playing track=1/12 name=a-very-long-file-name.mp3")
            .unwrap();
        writeln!(out, "next").unwrap();
        assert_eq!(capture.screen(20), vec!["next"]);
    }

    // ---- End to end through the loop ----

    #[test]
    fn the_banner_is_the_first_thing_on_screen() {
        // Startup used to print a `paused 0:00` confirmation above the banner.
        let mut h = Harness::new("order", 1, 80);
        h.layer.mode = Mode::Command;
        let banner_text = banner(&h.session, None);
        write!(h.out, "{banner_text}").unwrap();
        h.out.flush().unwrap();
        h.session.prepare(Duration::ZERO).unwrap();
        h.session.run(Command::Status);

        let rows = h.screen();
        assert!(
            rows[0].starts_with("jas 0."),
            "the banner comes first: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.starts_with("state=paused")),
            "the status output is below it, at column 0: {rows:?}"
        );
    }

    #[test]
    fn a_submitted_command_reads_as_typed_then_output() {
        let mut h = Harness::new("submit", 1, 80);
        h.layer.mode = Mode::Command;

        // Type `help`, repainting after each keystroke exactly as the loop does when
        // the input layer reports `Redraw`.
        for c in "help".chars() {
            h.out.paint_prompt(&h.layer.prompt()).unwrap();
            assert_eq!(h.layer.handle_chord_for_test(Chord::char(c)), Input::Redraw);
        }
        h.out.paint_prompt(&h.layer.prompt()).unwrap();
        let event = h.layer.handle_chord_for_test(Chord::new(KeyCode::Enter));
        assert!(matches!(event, Input::Command(Command::Help(_))));

        h.out.keep_line().unwrap();
        h.session.run(Command::Help(HelpTopic::General));

        let rows = h.screen();
        assert_eq!(rows[0], ":help", "what was typed stays visible: {rows:?}");
        assert!(
            rows[1].starts_with("play | pause"),
            "the output starts its own row at column 0: {rows:?}"
        );
    }

    #[test]
    fn nothing_typed_at_the_prompt_can_be_glued_to_the_next_line() {
        // The exact reported symptom, asserted at the byte level: the bug produced
        // `\r\x1b[2K:state=...`, with the message appended to the prompt. A wide
        // terminal is used so the check is about the prompt, not about wrapping.
        let mut h = Harness::new("glued", 1, 200);
        h.layer = InputLayer::lines_for_test(&["status", "quit"], false);
        let code = h.run(false, true).unwrap();
        assert_eq!(code, crate::error::EXIT_OK);

        let raw = h.capture.text();
        assert!(
            !raw.contains(":state="),
            "the status line must not be glued to the prompt: {raw:?}"
        );
        let rows = h.screen();
        // The first scripted line arrives before a prompt has ever been painted, so
        // the order is message-then-prompt. What matters is that the two are whole
        // separate rows and that the message begins at column 0.
        assert!(
            rows.iter().any(|r| r.starts_with("state=")),
            "the message begins at column 0 of its own row: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r == ":"),
            "the prompt is a row of its own: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.starts_with(":state")),
            "nothing is glued onto the prompt: {rows:?}"
        );
    }

    #[test]
    fn a_status_line_is_only_repainted_when_it_changes() {
        // Repainting an identical line every 40 ms is wasted work and flickers.
        let mut h = Harness::new("repaint", 1, 80);
        h.layer = InputLayer::lines_for_test(&["quit"], false);
        h.run(true, true).unwrap();
        let erases = h.capture.bytes().iter().filter(|b| **b == 0x1b).count();
        // A handful of iterations, not dozens: the loop cannot have repainted on
        // every tick because the text did not change.
        assert!(
            erases < 20,
            "expected few repaints, saw {erases} erase sequences"
        );
    }
}
