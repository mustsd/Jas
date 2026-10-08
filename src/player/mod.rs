//! The `Player` trait, negotiated capabilities, and the recording fake.
//!
//! `player/*` does audio I/O and nothing else. Position lives in
//! `transport.rs`, never in a backend, so a backend that cannot be controlled
//! (see the respawn strategy in `ffplay.rs`) behaves identically to one that can.

use std::path::Path;
use std::time::Duration;

#[cfg(test)]
use crate::error::Error;
use crate::error::Result;

pub mod detect;
pub mod ffplay;
#[cfg(unix)]
pub mod mpv;

/// What a backend can do without emulation. Every capability is negotiated,
/// never assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// Pause without restarting audio.
    pub live_pause: bool,
    /// Seek without restarting audio.
    pub live_seek: bool,
    /// Change speed mid-track.
    pub live_speed: bool,
    /// The backend loops an A-B segment itself.
    pub native_ab_loop: bool,
    /// The backend reports true position, rather than Jas computing it.
    pub position_feedback: bool,
    /// The backend reports the track duration, so the transport can clamp.
    pub duration_feedback: bool,
}

impl Capabilities {
    /// Everything an external player cannot do: the respawn strategy's profile.
    pub const EMULATED: Capabilities = Capabilities {
        live_pause: false,
        live_seek: false,
        live_speed: false,
        native_ab_loop: false,
        position_feedback: false,
        duration_feedback: false,
    };

    /// What mpv over JSON IPC can do.
    pub const LIVE: Capabilities = Capabilities {
        live_pause: true,
        live_seek: true,
        live_speed: true,
        native_ab_loop: true,
        position_feedback: true,
        duration_feedback: true,
    };

    /// A short list of the capabilities, for `jas doctor`.
    pub fn summary(&self) -> String {
        let mut flags = Vec::new();
        for (on, name) in [
            (self.live_pause, "live-pause"),
            (self.live_seek, "live-seek"),
            (self.live_speed, "live-speed"),
            (self.native_ab_loop, "native-ab"),
            (self.position_feedback, "position"),
            (self.duration_feedback, "duration"),
        ] {
            if on {
                flags.push(name);
            }
        }
        if flags.is_empty() {
            "emulated control only".to_string()
        } else {
            flags.join(", ")
        }
    }
}

pub trait Player {
    /// The backend name used in messages and `status`.
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    /// Load a track, ready to play from `at`. Returns the duration when known.
    fn load(&mut self, track: &Path, at: Duration) -> Result<Option<Duration>>;

    fn play(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn seek(&mut self, to: Duration) -> Result<()>;
    fn set_speed(&mut self, speed: f64) -> Result<()>;
    /// Loop `a..b` inside the backend, when `native_ab_loop` is set. `b = None`
    /// means "to the end of the track". `None` clears the loop.
    fn set_ab_loop(&mut self, ab: Option<(Duration, Option<Duration>)>) -> Result<()>;
    fn stop(&mut self) -> Result<()>;

    /// True position, when `position_feedback` is set.
    fn position(&self) -> Option<Duration> {
        None
    }

    /// Reports a child process that ended on its own, once. `None` means either
    /// "still running" or "not applicable" -- mpv, for instance, is one long-lived
    /// process that does not exit per track.
    ///
    /// This is deliberately not a `bool`-ish "is it alive?" check. A backend that
    /// cannot report a duration signals the end of a track *by exiting*, so
    /// conflating "exited" with "died" reports a normal track end as a crash.
    fn poll_exit(&mut self) -> Option<Exit> {
        None
    }

    /// The last thing the backend said on stderr, when it captures it.
    /// Included in the failure message so a decode error is visible rather than
    /// just a change in state.
    fn diagnostics(&self) -> Option<String> {
        None
    }

    /// Test hook: make the backend report a child exit, so the session's exit
    /// handling can be exercised without spawning anything. Implemented only by the
    /// fake.
    #[cfg(test)]
    #[doc(hidden)]
    fn set_exit_for_test(&mut self, exit: Exit) {
        let _ = exit;
    }
}

/// How a backend's child process ended, when it ended on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exited successfully. For a backend that cannot report the duration, this is
    /// how a track ends -- the session advances rather than complaining.
    Finished,
    /// Exited with a failure, e.g. a decode error or a crashed player.
    Failed,
}

/// All mutable state lives behind an `Rc<RefCell<..>>`, so a clone handed to a
/// session through a `Box<dyn Player>` still reports into the original. Without
/// that, a test would inspect a copy the session never touched.
///
/// A `Player` that records every call. Session logic, command dispatch, and
/// capability negotiation are tested against this: no audio device, no backend
/// binary, and no timing flakiness.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct FakeState {
    pub calls: Vec<String>,
    pub loaded: Option<(std::path::PathBuf, Duration)>,
    pub speed: f64,
    pub ab: Option<(Duration, Option<Duration>)>,
    pub playing: bool,
    /// Durations to report, in order of `load` calls.
    pub durations: Vec<Option<Duration>>,
    /// Position override, for testing the position-feedback path.
    pub reported_position: Option<Duration>,
    pub fail_next: Option<String>,
    /// A pending self-termination, reported once by `poll_exit`.
    pub exit: Option<Exit>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct FakePlayer {
    pub caps: Capabilities,
    state: std::rc::Rc<std::cell::RefCell<FakeState>>,
}

#[cfg(test)]
impl Default for FakePlayer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl FakePlayer {
    pub fn new() -> Self {
        Self {
            caps: Capabilities::EMULATED,
            state: std::rc::Rc::new(std::cell::RefCell::new(FakeState {
                speed: 1.0,
                ..FakeState::default()
            })),
        }
    }

    pub fn with_caps(caps: Capabilities) -> Self {
        Self {
            caps,
            ..Self::new()
        }
    }

    /// Every recorded call, oldest first.
    pub fn calls(&self) -> Vec<String> {
        self.state.borrow().calls.clone()
    }

    pub fn clear_calls(&self) {
        self.state.borrow_mut().calls.clear();
    }

    pub fn loaded(&self) -> Option<(std::path::PathBuf, Duration)> {
        self.state.borrow().loaded.clone()
    }

    pub fn is_playing(&self) -> bool {
        self.state.borrow().playing
    }

    /// Queue the durations reported by successive `load` calls.
    pub fn set_durations(&self, durations: Vec<Option<Duration>>) {
        self.state.borrow_mut().durations = durations;
    }

    pub fn set_reported_position(&self, position: Option<Duration>) {
        self.state.borrow_mut().reported_position = position;
    }

    /// Make the backend report that its child ended on its own, once.
    pub fn set_exit(&self, exit: Exit) {
        self.state.borrow_mut().exit = Some(exit);
    }

    /// Make the next call that matches `name` fail, to exercise error paths.
    pub fn fail_on(&self, name: &str) {
        self.state.borrow_mut().fail_next = Some(name.to_string());
    }

    fn check(&self, name: &str) -> Result<()> {
        let mut state = self.state.borrow_mut();
        if state.fail_next.as_deref() == Some(name) {
            state.fail_next = None;
            return Err(Error::runtime(format!("backend {name} failed (injected)")));
        }
        Ok(())
    }
}

#[cfg(test)]
impl Player for FakePlayer {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn load(&mut self, track: &Path, at: Duration) -> Result<Option<Duration>> {
        self.check("load")?;
        let mut state = self.state.borrow_mut();
        state
            .calls
            .push(format!("load {} @{}ms", track.display(), at.as_millis()));
        state.loaded = Some((track.to_path_buf(), at));
        let duration = if state.durations.is_empty() {
            None
        } else {
            state.durations.remove(0)
        };
        Ok(duration)
    }

    fn play(&mut self) -> Result<()> {
        self.check("play")?;
        let mut state = self.state.borrow_mut();
        state.calls.push("play".into());
        state.playing = true;
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        self.check("pause")?;
        let mut state = self.state.borrow_mut();
        state.calls.push("pause".into());
        state.playing = false;
        Ok(())
    }

    fn seek(&mut self, to: Duration) -> Result<()> {
        self.check("seek")?;
        self.state
            .borrow_mut()
            .calls
            .push(format!("seek {}ms", to.as_millis()));
        Ok(())
    }

    fn set_speed(&mut self, speed: f64) -> Result<()> {
        self.check("set_speed")?;
        let mut state = self.state.borrow_mut();
        state.calls.push(format!("set_speed {speed}"));
        state.speed = speed;
        Ok(())
    }

    fn set_ab_loop(&mut self, ab: Option<(Duration, Option<Duration>)>) -> Result<()> {
        self.check("set_ab_loop")?;
        let mut state = self.state.borrow_mut();
        state.calls.push(format!(
            "set_ab_loop {}",
            match ab {
                None => "none".to_string(),
                Some((a, Some(b))) => format!("{}..{}", a.as_millis(), b.as_millis()),
                Some((a, None)) => format!("{}..", a.as_millis()),
            }
        ));
        state.ab = ab;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.check("stop")?;
        let mut state = self.state.borrow_mut();
        state.calls.push("stop".into());
        state.playing = false;
        Ok(())
    }

    fn position(&self) -> Option<Duration> {
        self.state.borrow().reported_position
    }

    fn poll_exit(&mut self) -> Option<Exit> {
        // Reported once: a real child is reaped, and re-reporting the same exit
        // would make the session act on it twice.
        self.state.borrow_mut().exit.take()
    }

    fn diagnostics(&self) -> Option<String> {
        None
    }

    fn set_exit_for_test(&mut self, exit: Exit) {
        self.state.borrow_mut().exit = Some(exit);
    }
}

/// Build the argument list for `ffmpeg`-style tempo filters, chaining `atempo`
/// because a single filter only accepts 0.5-2.0. `None` means "play at 1.0x",
/// so the caller adds no filter at all.
pub fn atempo_filter(speed: f64) -> Option<String> {
    if !speed.is_finite() || speed <= 0.0 {
        return None;
    }
    if (speed - 1.0).abs() < 1e-4 {
        return None;
    }
    let mut remaining = speed;
    let mut parts: Vec<String> = Vec::new();
    while remaining > 2.0 {
        parts.push("atempo=2.0".to_string());
        remaining /= 2.0;
    }
    while remaining < 0.5 {
        parts.push("atempo=0.5".to_string());
        remaining /= 0.5;
    }
    if (remaining - 1.0).abs() > 1e-4 {
        parts.push(format!("atempo={remaining:.4}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_render_readably() {
        assert_eq!(Capabilities::EMULATED.summary(), "emulated control only");
        let live = Capabilities::LIVE.summary();
        assert!(live.contains("live-pause"));
        assert!(live.contains("native-ab"));
        assert!(!live.contains("emulated"));
    }

    #[test]
    fn the_fake_player_records_calls_in_order() {
        let mut p = FakePlayer::new();
        p.load(Path::new("/a.mp3"), Duration::from_millis(500))
            .unwrap();
        p.play().unwrap();
        p.pause().unwrap();
        assert_eq!(p.calls(), vec!["load /a.mp3 @500ms", "play", "pause"]);
        assert_eq!(p.loaded().unwrap().1, Duration::from_millis(500));
        assert!(!p.is_playing());
    }

    #[test]
    fn a_clone_shares_the_recording_state() {
        // This is what makes the fake usable behind `Box<dyn Player>`: the
        // session must record into the same place the test reads.
        let p = FakePlayer::new();
        let mut clone = p.clone();
        clone.play().unwrap();
        assert_eq!(p.calls(), vec!["play"]);
        p.fail_on("pause");
        assert!(
            clone.pause().is_err(),
            "the clone sees the injected failure"
        );
    }

    #[test]
    fn the_fake_player_can_inject_a_failure_once() {
        let mut p = FakePlayer::new();
        p.fail_on("play");
        assert!(p.play().is_err());
        // The injected failure is consumed, so the next call succeeds.
        assert!(p.play().is_ok());
    }

    #[test]
    fn atempo_is_absent_at_normal_speed() {
        assert_eq!(atempo_filter(1.0), None);
        assert_eq!(atempo_filter(1.0000001), None);
    }

    #[test]
    fn atempo_handles_the_single_filter_range() {
        assert_eq!(atempo_filter(0.75).as_deref(), Some("atempo=0.7500"));
        assert_eq!(atempo_filter(2.0).as_deref(), Some("atempo=2.0000"));
        assert_eq!(atempo_filter(0.5).as_deref(), Some("atempo=0.5000"));
    }

    #[test]
    fn atempo_chains_outside_the_single_filter_range() {
        assert_eq!(
            atempo_filter(4.0).as_deref(),
            Some("atempo=2.0,atempo=2.0000")
        );
        assert_eq!(
            atempo_filter(0.25).as_deref(),
            Some("atempo=0.5,atempo=0.5000")
        );
        let chain = atempo_filter(3.0).unwrap();
        assert_eq!(chain.matches("atempo").count(), 2);
        // 0.1 needs three 0.5x steps plus the remainder: 0.5^3 * 0.8 = 0.1.
        let chain = atempo_filter(0.1).unwrap();
        assert_eq!(chain.matches("atempo").count(), 4);
        assert!(chain.ends_with("atempo=0.8000"), "{chain}");
    }

    #[test]
    fn atempo_rejects_impossible_speeds() {
        assert_eq!(atempo_filter(0.0), None);
        assert_eq!(atempo_filter(-1.0), None);
        assert_eq!(atempo_filter(f64::NAN), None);
        assert_eq!(atempo_filter(f64::INFINITY), None);
    }
}
