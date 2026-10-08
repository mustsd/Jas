//! Logical position clock and playback state machine (pure).
//!
//! Position is computed, never accumulated:
//!
//! ```text
//! position(t) = clamp(anchor_pos + (t - anchor_t0) * speed, 0, duration)
//! ```
//!
//! Every state change (play, pause, seek, speed change, respawn, track change)
//! re-anchors `(anchor_pos, anchor_t0)`. Time always comes from an injected
//! [`Clock`], so tests drive time by hand and assert exact positions with no
//! sleeps.

use std::time::Duration;

pub const MIN_SPEED: f64 = 0.25;
pub const MAX_SPEED: f64 = 4.0;
pub const DEFAULT_SPEED: f64 = 1.0;

/// Clamp a speed into the supported range. `NaN` becomes [`DEFAULT_SPEED`].
pub fn clamp_speed(speed: f64) -> f64 {
    if speed.is_nan() {
        return DEFAULT_SPEED;
    }
    speed.clamp(MIN_SPEED, MAX_SPEED)
}

/// Round a speed for display and storage, so `+0.05` nudges do not drift into
/// values like `0.7500000000000001`.
pub fn round_speed(speed: f64) -> f64 {
    (speed * 1000.0).round() / 1000.0
}

/// Monotonic time source. Implementations must never go backwards.
pub trait Clock {
    /// Time elapsed on a monotonic timeline. Only differences are meaningful.
    fn elapsed(&self) -> Duration;
}

/// The real clock: a monotonic `Instant` captured at construction.
#[derive(Debug, Clone)]
pub struct RealClock {
    origin: std::time::Instant,
}

impl RealClock {
    pub fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn elapsed(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A shared handle is itself a clock, so tests can keep one copy for advancing
/// time and hand another to the session.
impl<T: Clock + ?Sized> Clock for std::rc::Rc<T> {
    fn elapsed(&self) -> Duration {
        (**self).elapsed()
    }
}

impl<T: Clock + ?Sized> Clock for std::sync::Arc<T> {
    fn elapsed(&self) -> Duration {
        (**self).elapsed()
    }
}

/// A hand-driven clock for tests and for the fake backend.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub struct FakeClock {
    now: std::cell::Cell<Duration>,
}

#[cfg(test)]
impl FakeClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&self, by: Duration) {
        self.now.set(self.now.get() + by);
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn elapsed(&self) -> Duration {
        self.now.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Playing,
    Paused,
    Finished,
    Stopped,
}

impl State {
    pub fn is_playing(self) -> bool {
        matches!(self, State::Playing)
    }

    /// The word used in `status` output and one-line confirmations.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Playing => "playing",
            State::Paused => "paused",
            State::Finished => "finished",
            State::Stopped => "stopped",
        }
    }
}

/// What the caller must do to the audio backend after a transport call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Effect {
    /// Nothing to do; the backend keeps doing what it was doing.
    None,
    /// Start or resume from `at`.
    Play { at: Duration },
    /// Stop the audio and hold the position.
    Pause,
    /// Restart the audio at `at` (seek, speed change, respawn, A-B wrap).
    RespawnAt { at: Duration },
    /// Silence; the track is over or the session stopped.
    Stop,
}

/// A-B loop configuration plus drill repeat settings.
#[derive(Debug, Clone, PartialEq)]
pub struct AbLoop {
    pub a: Duration,
    /// `None` means "to the end of the track".
    pub b: Option<Duration>,
    /// Repeats per segment; `None` loops the segment forever.
    pub repeats: Option<u32>,
    /// Silence inserted between repeats.
    pub gap: Duration,
    /// Shift the segment forward by its own length after finishing it.
    pub advance_after: bool,
}

impl AbLoop {
    /// Resolve `b` against the track duration.
    pub fn end(&self, duration: Option<Duration>) -> Option<Duration> {
        match (self.b, duration) {
            (Some(b), _) => Some(b),
            (None, Some(d)) => Some(d),
            (None, None) => None,
        }
    }
}

/// A single completion event produced by advancing the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A-B segment finished; jump back to A (or advance, see [`AbLoop`]).
    SegmentWrapped,
    /// The segment advanced to a new A-B pair.
    SegmentAdvanced,
    /// The whole drill for this track is done.
    DrillFinished,
    /// The track reached its end.
    TrackFinished,
}

/// Drift larger than this is corrected when the backend reports its position.
/// Below it, the computed clock is more stable than the observation.
pub const DRIFT_TOLERANCE: Duration = Duration::from_millis(300);

/// The transport: where we are, how fast, and whether sound should be coming out.
#[derive(Debug, Clone)]
pub struct Transport {
    state: State,
    speed: f64,
    duration: Option<Duration>,
    anchor_pos: Duration,
    anchor_t0: Duration,
    ab: Option<AbLoop>,
    /// Repeats completed for the current segment.
    repeats_done: u32,
    /// While true, output is silence between repeats.
    in_gap: bool,
    /// When the current gap ends, on the transport timeline.
    gap_end: Duration,
}

impl Transport {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            speed: DEFAULT_SPEED,
            duration: None,
            anchor_pos: Duration::ZERO,
            anchor_t0: Duration::ZERO,
            ab: None,
            repeats_done: 0,
            in_gap: false,
            gap_end: Duration::ZERO,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    pub fn ab(&self) -> Option<&AbLoop> {
        self.ab.as_ref()
    }

    /// Mark a loaded-but-silent track as paused rather than idle, so `status` and
    /// the banner say "paused" for something that is ready to play.
    pub fn set_ready(&mut self) {
        if self.state == State::Idle {
            self.state = State::Paused;
        }
    }

    /// Update the known track duration, e.g. when a backend reports it later.
    pub fn set_duration(&mut self, duration: Option<Duration>) {
        self.duration = duration;
    }

    /// How many passes of the current segment have completed, for the drill
    /// counter in `status`.
    pub fn repeats_done(&self) -> u32 {
        self.repeats_done
    }

    pub fn in_gap(&self) -> bool {
        self.in_gap
    }

    /// True while no sound should be coming out: paused, idle, or between repeats.
    pub fn is_silent(&self) -> bool {
        !self.state.is_playing() || self.in_gap
    }

    /// Load a track and hold position `at`. Does not start playback.
    pub fn load(&mut self, duration: Option<Duration>, at: Duration, now: Duration) -> Effect {
        self.duration = duration;
        self.state = State::Idle;
        self.repeats_done = 0;
        self.in_gap = false;
        self.reanchor(clamp_to(at, duration), now);
        Effect::Stop
    }

    pub fn play(&mut self, now: Duration) -> Effect {
        if self.state == State::Finished {
            // `play` after the end restarts the track rather than doing nothing.
            self.reanchor(Duration::ZERO, now);
            self.state = State::Paused;
        }
        if self.state.is_playing() && !self.in_gap {
            return Effect::None;
        }
        // Resolve the resume point while the clock is still frozen: flipping the
        // state first would let the paused interval count as played time.
        let at = self.position(now);
        self.state = State::Playing;
        self.in_gap = false;
        self.reanchor(at, now);
        Effect::Play { at }
    }

    pub fn pause(&mut self, now: Duration) -> Effect {
        if !self.state.is_playing() {
            return Effect::None;
        }
        // Freeze the clock first so the reported position is where we stopped.
        self.reanchor(self.position(now), now);
        self.state = State::Paused;
        self.in_gap = false;
        Effect::Pause
    }

    pub fn toggle(&mut self, now: Duration) -> Effect {
        if self.state.is_playing() {
            self.pause(now)
        } else {
            self.play(now)
        }
    }

    pub fn stop(&mut self, now: Duration) -> Effect {
        self.reanchor(self.position(now), now);
        self.state = State::Stopped;
        self.in_gap = false;
        Effect::Stop
    }

    /// Absolute seek. Past the end clamps to the duration when it is known, and
    /// otherwise is accepted as-is (a live-ish stream or an unknown-length file).
    pub fn seek(&mut self, to: Duration, now: Duration) -> Effect {
        let target = clamp_to(to, self.duration);
        self.reanchor(target, now);
        self.repeats_done = 0;
        self.in_gap = false;
        if self.state == State::Finished {
            self.state = State::Paused;
        }
        if self.state.is_playing() {
            Effect::RespawnAt { at: target }
        } else {
            Effect::None
        }
    }

    /// Relative seek; negative values clamp at zero.
    pub fn seek_by(&mut self, delta_ms: i64, now: Duration) -> Effect {
        let base = self.position(now);
        let target = crate::time::offset(base, delta_ms);
        self.seek(target, now)
    }

    /// Change speed, preserving the current position exactly.
    pub fn set_speed(&mut self, speed: f64, now: Duration) -> Effect {
        let clamped = round_speed(clamp_speed(speed));
        if (clamped - self.speed).abs() < f64::EPSILON {
            return Effect::None;
        }
        let held = self.position(now);
        self.speed = clamped;
        self.reanchor(held, now);
        if self.state.is_playing() && !self.in_gap {
            Effect::RespawnAt { at: held }
        } else {
            Effect::None
        }
    }

    /// Nudge the speed by a delta, clamped. `None` means "already at the limit".
    pub fn nudge_speed(&mut self, delta: f64, now: Duration) -> (f64, Effect) {
        let target = round_speed(clamp_speed(self.speed + delta));
        let effect = self.set_speed(target, now);
        (self.speed, effect)
    }

    pub fn set_ab(&mut self, ab: Option<AbLoop>, now: Duration) -> Effect {
        self.ab = ab;
        self.repeats_done = 0;
        self.in_gap = false;
        match (self.ab.is_some(), self.state.is_playing()) {
            (true, true) => {
                let target = self.position(now);
                self.reanchor(target, now);
                Effect::RespawnAt { at: target }
            }
            _ => Effect::None,
        }
    }

    /// Set mark A at the current position. A is clamped to be <= B.
    pub fn mark_a(&mut self, now: Duration) -> (Effect, bool) {
        let a = self.position(now);
        let b = self.ab.as_ref().and_then(|ab| ab.b);
        let keep_repeats = self.ab.as_ref().and_then(|ab| ab.repeats);
        let gap = self.ab.as_ref().map(|ab| ab.gap).unwrap_or_default();
        let advance = self.ab.as_ref().map(|ab| ab.advance_after).unwrap_or(false);
        let mut rejected = false;
        if let Some(b) = b {
            if a > b {
                rejected = true;
            }
        }
        let a = match b {
            Some(b) if a > b => b,
            _ => a,
        };
        let effect = self.set_ab(
            Some(AbLoop {
                a,
                b,
                repeats: keep_repeats,
                gap,
                advance_after: advance,
            }),
            now,
        );
        (effect, rejected)
    }

    /// Set mark B at the current position. B must be after A.
    pub fn mark_b(&mut self, now: Duration) -> (Effect, bool) {
        let b = self.position(now);
        let a = self.ab.as_ref().map(|ab| ab.a).unwrap_or_default();
        if b <= a {
            return (Effect::None, true);
        }
        let keep_repeats = self.ab.as_ref().and_then(|ab| ab.repeats);
        let gap = self.ab.as_ref().map(|ab| ab.gap).unwrap_or_default();
        let advance = self.ab.as_ref().map(|ab| ab.advance_after).unwrap_or(false);
        let effect = self.set_ab(
            Some(AbLoop {
                a,
                b: Some(b),
                repeats: keep_repeats,
                gap,
                advance_after: advance,
            }),
            now,
        );
        (effect, false)
    }

    /// Set the repeat count. With no A-B marks this drills the whole track
    /// (A = start, B = end) rather than silently doing nothing.
    pub fn set_repeat(&mut self, repeats: Option<u32>, now: Duration) -> Effect {
        let held = self.position(now);
        let gap = self.ab.as_ref().map(|ab| ab.gap).unwrap_or_default();
        let b = self.ab.as_ref().and_then(|ab| ab.b);
        let advance = self.ab.as_ref().map(|ab| ab.advance_after).unwrap_or(false);
        let ab = AbLoop {
            a: self.ab.as_ref().map(|ab| ab.a).unwrap_or(Duration::ZERO),
            b,
            repeats,
            gap,
            advance_after: advance,
        };
        self.ab = Some(ab);
        self.repeats_done = 0;
        self.reanchor(held, now);
        if self.state.is_playing() {
            Effect::RespawnAt { at: held }
        } else {
            Effect::None
        }
    }

    /// Set the inter-repeat gap. With no A-B marks this drills the whole track,
    /// mirroring [`Transport::set_repeat`]: a gap that silently did nothing would
    /// be a command that silently no-ops.
    pub fn set_gap(&mut self, gap: Duration, now: Duration) {
        if let Some(ab) = self.ab.as_mut() {
            ab.gap = gap;
            return;
        }
        self.reanchor(self.position(now), now);
        self.ab = Some(AbLoop {
            a: Duration::ZERO,
            b: None,
            repeats: None,
            gap,
            advance_after: false,
        });
    }

    /// The logical position at monotonic time `t`.
    pub fn position(&self, t: Duration) -> Duration {
        if !self.state.is_playing() {
            return clamp_to(self.anchor_pos, self.duration);
        }
        let elapsed = t.saturating_sub(self.anchor_t0);
        let advanced = elapsed.as_secs_f64() * self.speed;
        let pos = self.anchor_pos.as_secs_f64() + advanced;
        clamp_to(Duration::from_secs_f64(pos.max(0.0)), self.duration)
    }

    /// Advance the clock and resolve anything that happened: A-B wrap, gap end,
    /// track end. Called once per polling tick (and once per input event).
    ///
    /// Returns every event that occurred, in order, so a long stall cannot hide
    /// a wrap behind a single event.
    pub fn tick(&mut self, now: Duration) -> Vec<Event> {
        let mut events = Vec::new();
        if !self.state.is_playing() {
            return events;
        }

        // Coming out of a gap: the segment restarts at A.
        if self.in_gap {
            if now < self.gap_end {
                return events;
            }
            self.in_gap = false;
            let resume = self.ab.as_ref().map(|ab| ab.a).unwrap_or(self.anchor_pos);
            self.reanchor(resume, now);
            events.push(Event::SegmentWrapped);
            return events;
        }

        // Without an A-B loop only the end of the track matters.
        if self.ab.is_none() {
            if let Some(d) = self.duration {
                if self.position(now) >= d {
                    self.reanchor(d, now);
                    self.state = State::Finished;
                    events.push(Event::TrackFinished);
                }
            }
            return events;
        }

        // Bounded so a degenerate segment (A >= B, or sub-millisecond) cannot spin.
        for _ in 0..64 {
            let pos = self.position(now);
            let (a, b, repeats, gap, advance) = match &self.ab {
                Some(ab) => (
                    ab.a,
                    ab.end(self.duration),
                    ab.repeats,
                    ab.gap,
                    ab.advance_after,
                ),
                None => (Duration::ZERO, self.duration, None, Duration::ZERO, false),
            };
            let Some(b) = b else {
                // Unknown length: only the backend can tell us we finished.
                return events;
            };
            if pos < b {
                return events;
            }

            if let Some(limit) = repeats {
                if self.repeats_done + 1 >= limit {
                    // The requested number of passes is complete.
                    self.reanchor(b, now);
                    self.state = State::Paused;
                    events.push(Event::DrillFinished);
                    return events;
                }
            }
            self.repeats_done += 1;

            if advance {
                let len = b.saturating_sub(a);
                if let Some(ab) = self.ab.as_mut() {
                    ab.a = b;
                    ab.b = Some(b + len);
                }
                self.reanchor(b, now);
                events.push(Event::SegmentAdvanced);
            } else if gap > Duration::ZERO {
                self.in_gap = true;
                self.reanchor(b, now);
                self.gap_end = now + gap;
                events.push(Event::SegmentWrapped);
                return events;
            } else {
                self.reanchor(a, now);
                events.push(Event::SegmentWrapped);
            }

            // A zero-length or sub-millisecond segment would spin; stop here.
            if b.saturating_sub(a) < Duration::from_millis(50) {
                return events;
            }
        }
        events
    }

    /// The position to resume audio at, given the transport's own timeline.
    pub fn resume_target(&self, now: Duration) -> Duration {
        let pos = self.position(now);
        match (&self.ab, self.state.is_playing(), self.in_gap) {
            (Some(ab), true, false) if pos >= ab.a => pos,
            (Some(ab), true, false) => ab.a,
            _ => pos,
        }
    }

    /// Correct accumulated drift from a backend-reported position. Only a
    /// disagreement larger than [`DRIFT_TOLERANCE`] moves the anchor, so normal
    /// reporting jitter cannot make the position jitter too.
    pub fn correct_position(&mut self, observed: Duration, now: Duration) -> bool {
        if !self.state.is_playing() {
            return false;
        }
        let current = self.position(now);
        // `Duration::abs_diff` is newer than the declared MSRV, so compare both
        // directions explicitly rather than bumping the MSRV for one call.
        let ahead = current.saturating_sub(observed);
        let behind = observed.saturating_sub(current);
        if ahead <= DRIFT_TOLERANCE && behind <= DRIFT_TOLERANCE {
            return false;
        }
        self.reanchor(clamp_to(observed, self.duration), now);
        true
    }

    fn reanchor(&mut self, pos: Duration, now: Duration) {
        self.anchor_pos = pos;
        self.anchor_t0 = now;
    }
}

impl Default for Transport {
    fn default() -> Self {
        Self::new()
    }
}

/// Clamp a position into `0..=duration` when the duration is known.
pub fn clamp_to(pos: Duration, duration: Option<Duration>) -> Duration {
    match duration {
        Some(d) => pos.min(d),
        None => pos,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn loaded(duration: Option<Duration>) -> (Transport, FakeClock) {
        let clock = FakeClock::new();
        let mut t = Transport::new();
        t.load(duration, Duration::ZERO, clock.elapsed());
        (t, clock)
    }

    #[test]
    fn position_advances_with_time_at_speed_one() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.play(clock.elapsed());
        clock.advance(ms(1500));
        assert_eq!(t.position(clock.elapsed()), ms(1500));
        clock.advance(S(3));
        assert_eq!(t.position(clock.elapsed()), ms(4500));
    }

    #[test]
    fn position_is_computed_not_accumulated() {
        // Many small ticks must give the same answer as one big jump.
        let (mut t, clock) = loaded(Some(S(60)));
        t.play(clock.elapsed());
        for _ in 0..1000 {
            clock.advance(ms(1));
        }
        assert_eq!(t.position(clock.elapsed()), ms(1000));

        let (mut t2, clock2) = loaded(Some(S(60)));
        t2.play(clock2.elapsed());
        clock2.advance(S(1));
        assert_eq!(t2.position(clock2.elapsed()), ms(1000));
    }

    #[test]
    fn pause_freezes_position_and_resume_continues_from_there() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.play(clock.elapsed());
        clock.advance(S(2));
        assert_eq!(t.pause(clock.elapsed()), Effect::Pause);
        let held = t.position(clock.elapsed());
        assert_eq!(held, S(2));
        assert_eq!(t.state(), State::Paused);

        // Time passing while paused must not move the position.
        clock.advance(S(30));
        assert_eq!(t.position(clock.elapsed()), held);

        assert_eq!(t.play(clock.elapsed()), Effect::Play { at: S(2) });
        clock.advance(S(1));
        assert_eq!(t.position(clock.elapsed()), S(3));
    }

    #[test]
    fn toggle_flips_between_play_and_pause() {
        let (mut t, clock) = loaded(Some(S(10)));
        assert_eq!(
            t.toggle(clock.elapsed()),
            Effect::Play { at: Duration::ZERO }
        );
        clock.advance(S(1));
        assert_eq!(t.toggle(clock.elapsed()), Effect::Pause);
        assert_eq!(t.state(), State::Paused);
        assert_eq!(t.toggle(clock.elapsed()), Effect::Play { at: S(1) });
    }

    #[test]
    fn seek_absolute_relative_and_clamping() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.play(clock.elapsed());

        assert_eq!(
            t.seek(S(4), clock.elapsed()),
            Effect::RespawnAt { at: S(4) }
        );
        assert_eq!(t.position(clock.elapsed()), S(4));

        // Relative forward.
        t.seek_by(2_500, clock.elapsed());
        assert_eq!(t.position(clock.elapsed()), ms(6500));

        // Relative backward past zero clamps.
        t.seek_by(-30_000, clock.elapsed());
        assert_eq!(t.position(clock.elapsed()), Duration::ZERO);

        // Past the end clamps to the duration.
        t.seek(S(99), clock.elapsed());
        assert_eq!(t.position(clock.elapsed()), S(10));
    }

    #[test]
    fn seek_while_paused_does_not_produce_audio() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.play(clock.elapsed());
        clock.advance(S(1));
        t.pause(clock.elapsed());
        assert_eq!(t.seek(S(5), clock.elapsed()), Effect::None);
        assert_eq!(t.position(clock.elapsed()), S(5));
        assert_eq!(t.state(), State::Paused);
    }

    #[test]
    fn unknown_duration_is_not_clamped() {
        let (mut t, clock) = loaded(None);
        t.play(clock.elapsed());
        clock.advance(S(500));
        assert_eq!(t.position(clock.elapsed()), S(500));
        t.seek(S(1000), clock.elapsed());
        assert_eq!(t.position(clock.elapsed()), S(1000));
    }

    #[test]
    fn speed_change_preserves_position_exactly() {
        let (mut t, clock) = loaded(Some(S(100)));
        t.play(clock.elapsed());
        clock.advance(S(10));
        assert_eq!(
            t.set_speed(2.0, clock.elapsed()),
            Effect::RespawnAt { at: S(10) }
        );
        assert_eq!(t.position(clock.elapsed()), S(10));
        clock.advance(S(5));
        assert_eq!(t.position(clock.elapsed()), S(20));

        assert_eq!(
            t.set_speed(0.5, clock.elapsed()),
            Effect::RespawnAt { at: S(20) }
        );
        clock.advance(S(10));
        assert_eq!(t.position(clock.elapsed()), S(25));
    }

    #[test]
    fn speed_is_clamped_and_nudges_round_cleanly() {
        assert_eq!(clamp_speed(99.0), MAX_SPEED);
        assert_eq!(clamp_speed(0.0), MIN_SPEED);
        assert_eq!(clamp_speed(-1.0), MIN_SPEED);
        assert_eq!(clamp_speed(f64::NAN), DEFAULT_SPEED);
        assert_eq!(clamp_speed(f64::INFINITY), MAX_SPEED);
        assert_eq!(round_speed(0.75 + 0.05), 0.8);
        assert_eq!(round_speed(0.1 + 0.2), 0.3);

        let (mut t, clock) = loaded(Some(S(100)));
        let (speed, _) = t.nudge_speed(0.05, clock.elapsed());
        assert_eq!(speed, 1.05);
        for _ in 0..200 {
            t.nudge_speed(0.05, clock.elapsed());
        }
        assert_eq!(t.speed(), MAX_SPEED);
        for _ in 0..500 {
            t.nudge_speed(-0.05, clock.elapsed());
        }
        assert_eq!(t.speed(), MIN_SPEED);
    }

    #[test]
    fn setting_the_same_speed_is_a_noop_effect() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.play(clock.elapsed());
        assert_eq!(t.set_speed(1.0, clock.elapsed()), Effect::None);
    }

    #[test]
    fn ab_loop_wraps_at_b() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.set_ab(
            Some(AbLoop {
                a: S(2),
                b: Some(S(4)),
                repeats: None,
                gap: Duration::ZERO,
                advance_after: false,
            }),
            clock.elapsed(),
        );
        t.seek(S(2), clock.elapsed());
        t.play(clock.elapsed());

        clock.advance(S(3));
        let events = t.tick(clock.elapsed());
        assert!(events.contains(&Event::SegmentWrapped), "{events:?}");
        assert_eq!(t.position(clock.elapsed()), S(2));
        assert_eq!(t.state(), State::Playing);

        // And it wraps again.
        clock.advance(S(2));
        assert!(t.tick(clock.elapsed()).contains(&Event::SegmentWrapped));
    }

    #[test]
    fn ab_loop_does_not_wrap_before_b() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.set_ab(
            Some(AbLoop {
                a: S(2),
                b: Some(S(4)),
                repeats: None,
                gap: Duration::ZERO,
                advance_after: false,
            }),
            clock.elapsed(),
        );
        t.play(clock.elapsed());
        clock.advance(ms(3500));
        assert!(t.tick(clock.elapsed()).is_empty());
        assert_eq!(t.position(clock.elapsed()), ms(3500));
    }

    #[test]
    fn ab_b_defaults_to_end_of_track() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.set_ab(
            Some(AbLoop {
                a: S(5),
                b: None,
                repeats: None,
                gap: Duration::ZERO,
                advance_after: false,
            }),
            clock.elapsed(),
        );
        t.seek(S(5), clock.elapsed());
        t.play(clock.elapsed());
        clock.advance(S(5));
        assert!(t.tick(clock.elapsed()).contains(&Event::SegmentWrapped));
        assert_eq!(t.position(clock.elapsed()), S(5));
    }

    #[test]
    fn repeat_count_finishes_the_drill() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.set_ab(
            Some(AbLoop {
                a: S(1),
                b: Some(S(2)),
                repeats: Some(3),
                gap: Duration::ZERO,
                advance_after: false,
            }),
            clock.elapsed(),
        );
        t.seek(S(1), clock.elapsed());
        t.play(clock.elapsed());

        for expected in 1..=2u32 {
            clock.advance(ms(1200));
            let events = t.tick(clock.elapsed());
            assert!(events.contains(&Event::SegmentWrapped), "{events:?}");
            assert_eq!(t.repeats_done(), expected);
        }
        clock.advance(ms(1200));
        let events = t.tick(clock.elapsed());
        assert!(events.contains(&Event::DrillFinished), "{events:?}");
        assert_eq!(t.state(), State::Paused);
        assert_eq!(t.position(clock.elapsed()), S(2));
    }

    #[test]
    fn gap_inserts_silence_between_repeats() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.set_ab(
            Some(AbLoop {
                a: S(1),
                b: Some(S(2)),
                repeats: None,
                gap: ms(500),
                advance_after: false,
            }),
            clock.elapsed(),
        );
        t.seek(S(1), clock.elapsed());
        t.play(clock.elapsed());

        clock.advance(ms(1200));
        assert!(t.tick(clock.elapsed()).contains(&Event::SegmentWrapped));
        assert!(t.in_gap());
        assert!(t.is_silent());
        assert_eq!(t.position(clock.elapsed()), S(2));

        // Inside the gap: nothing new happens.
        clock.advance(ms(200));
        assert!(t.tick(clock.elapsed()).is_empty());
        assert!(t.in_gap());

        // After the gap: back to A and audible again.
        clock.advance(ms(400));
        t.tick(clock.elapsed());
        assert!(!t.in_gap());
        assert!(!t.is_silent());
        assert_eq!(t.position(clock.elapsed()), S(1));
    }

    #[test]
    fn repeat_next_advances_the_segment() {
        let (mut t, clock) = loaded(Some(S(100)));
        t.set_ab(
            Some(AbLoop {
                a: S(10),
                b: Some(S(20)),
                repeats: None,
                gap: Duration::ZERO,
                advance_after: true,
            }),
            clock.elapsed(),
        );
        t.seek(S(10), clock.elapsed());
        t.play(clock.elapsed());

        clock.advance(S(11));
        let events = t.tick(clock.elapsed());
        assert!(events.contains(&Event::SegmentAdvanced), "{events:?}");
        let ab = t.ab().expect("A-B still set");
        assert_eq!(ab.a, S(20));
        assert_eq!(ab.b, Some(S(30)));
        assert_eq!(t.position(clock.elapsed()), S(20));
    }

    #[test]
    fn mark_a_and_b_from_the_current_position() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.play(clock.elapsed());
        clock.advance(S(3));
        let (_, rejected) = t.mark_a(clock.elapsed());
        assert!(!rejected);
        assert_eq!(t.ab().unwrap().a, S(3));

        clock.advance(S(2));
        let (_, rejected) = t.mark_b(clock.elapsed());
        assert!(!rejected);
        assert_eq!(t.ab().unwrap().b, Some(S(5)));
    }

    #[test]
    fn marking_b_before_a_is_rejected_not_silently_reordered() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.play(clock.elapsed());
        clock.advance(S(3));
        t.mark_a(clock.elapsed());
        clock.advance(S(2));
        t.mark_b(clock.elapsed()); // b = 5

        t.seek(S(1), clock.elapsed());
        t.play(clock.elapsed());
        let (_, rejected) = t.mark_b(clock.elapsed());
        assert!(rejected);
        assert_eq!(t.ab().unwrap().b, Some(S(5)));
    }

    #[test]
    fn ab_clear_removes_the_loop() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.set_ab(
            Some(AbLoop {
                a: S(1),
                b: Some(S(2)),
                repeats: Some(2),
                gap: Duration::ZERO,
                advance_after: false,
            }),
            clock.elapsed(),
        );
        assert!(t.ab().is_some());
        assert_eq!(t.set_ab(None, clock.elapsed()), Effect::None);
        assert!(t.ab().is_none());
    }

    #[test]
    fn tick_does_not_run_while_paused() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.set_ab(
            Some(AbLoop {
                a: S(1),
                b: Some(S(2)),
                repeats: None,
                gap: Duration::ZERO,
                advance_after: false,
            }),
            clock.elapsed(),
        );
        t.seek(S(1), clock.elapsed());
        clock.advance(S(60));
        assert!(t.tick(clock.elapsed()).is_empty());
        assert_eq!(t.position(clock.elapsed()), S(1));
    }

    #[test]
    fn a_track_whose_backend_stops_is_stopped_by_the_session() {
        // `finish` used to handle the "backend reported the end of an
        // unknown-length file" case; the session now stops the transport when the
        // backend disappears, which covers the same ground with less API.
        let (mut t, clock) = loaded(None);
        t.play(clock.elapsed());
        clock.advance(S(30));
        assert_eq!(t.position(clock.elapsed()), S(30));
        assert!(
            t.tick(clock.elapsed()).is_empty(),
            "unknown length: the clock decides"
        );
        assert_eq!(t.stop(clock.elapsed()), Effect::Stop);
        assert_eq!(t.state(), State::Stopped);
        assert!(t.is_silent());
    }

    #[test]
    fn finished_track_clamps_at_the_duration() {
        let (mut t, clock) = loaded(Some(S(5)));
        t.play(clock.elapsed());
        clock.advance(S(30));
        assert_eq!(t.position(clock.elapsed()), S(5));
        let events = t.tick(clock.elapsed());
        assert_eq!(events, vec![Event::TrackFinished]);
        assert_eq!(t.state(), State::Finished);
        // Playing again restarts from the top.
        assert_eq!(t.play(clock.elapsed()), Effect::Play { at: Duration::ZERO });
    }

    #[test]
    fn degenerate_ab_segment_does_not_spin() {
        // A == B would otherwise loop forever inside one tick.
        let (mut t, clock) = loaded(Some(S(10)));
        t.set_ab(
            Some(AbLoop {
                a: S(4),
                b: Some(S(4)),
                repeats: None,
                gap: Duration::ZERO,
                advance_after: false,
            }),
            clock.elapsed(),
        );
        t.play(clock.elapsed());
        clock.advance(S(1));
        let events = t.tick(clock.elapsed());
        assert!(events.len() <= 64);
        assert_eq!(t.state(), State::Playing);
    }

    #[test]
    fn a_long_stall_does_not_lose_segment_wraps() {
        let (mut t, clock) = loaded(Some(S(100)));
        t.set_ab(
            Some(AbLoop {
                a: S(0),
                b: Some(S(1)),
                repeats: None,
                gap: Duration::ZERO,
                advance_after: false,
            }),
            clock.elapsed(),
        );
        t.play(clock.elapsed());
        clock.advance(ms(3500));
        let events = t.tick(clock.elapsed());
        // Wraps are reported; the clock is re-anchored to A so position stays sane.
        assert!(!events.is_empty());
        assert!(t.position(clock.elapsed()) <= S(1));
    }

    #[test]
    fn resume_target_prefers_the_segment_start() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.set_ab(
            Some(AbLoop {
                a: S(2),
                b: Some(S(4)),
                repeats: None,
                gap: Duration::ZERO,
                advance_after: false,
            }),
            clock.elapsed(),
        );
        t.seek(S(2), clock.elapsed());
        t.play(clock.elapsed());
        clock.advance(ms(500));
        assert_eq!(t.resume_target(clock.elapsed()), ms(2500));
    }

    #[test]
    fn stop_holds_the_position_and_playing_again_resumes_it() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.play(clock.elapsed());
        clock.advance(S(4));
        assert_eq!(t.stop(clock.elapsed()), Effect::Stop);
        assert_eq!(t.state(), State::Stopped);
        assert_eq!(t.position(clock.elapsed()), S(4));
        assert_eq!(t.play(clock.elapsed()), Effect::Play { at: S(4) });
    }

    #[test]
    fn a_gap_with_no_ab_marks_drills_the_whole_track() {
        let (mut t, _clock) = loaded(Some(S(10)));
        assert!(t.ab().is_none());
        t.set_gap(ms(500), Duration::ZERO);
        let ab = t.ab().expect("a gap implies a drill, not a no-op");
        assert_eq!(ab.gap, ms(500));
        assert_eq!(ab.a, Duration::ZERO);
        assert_eq!(ab.b, None);
        assert_eq!(ab.end(Some(S(10))), Some(S(10)));
    }

    #[test]
    fn setting_a_gap_keeps_an_existing_segment() {
        let (mut t, _clock) = loaded(Some(S(10)));
        t.set_ab(
            Some(AbLoop {
                a: S(2),
                b: Some(S(4)),
                repeats: Some(3),
                gap: Duration::ZERO,
                advance_after: false,
            }),
            Duration::ZERO,
        );
        t.set_gap(ms(250), Duration::ZERO);
        let ab = t.ab().unwrap();
        assert_eq!(ab.a, S(2));
        assert_eq!(ab.b, Some(S(4)));
        assert_eq!(ab.repeats, Some(3));
        assert_eq!(ab.gap, ms(250));
    }

    #[test]
    fn drift_is_corrected_only_beyond_the_tolerance() {
        let (mut t, clock) = loaded(Some(S(100)));
        t.play(clock.elapsed());
        clock.advance(S(10));
        assert_eq!(t.position(clock.elapsed()), S(10));

        // A small disagreement is ignored: the computed clock is steadier.
        assert!(!t.correct_position(ms(10_100), clock.elapsed()));
        assert_eq!(t.position(clock.elapsed()), S(10));

        // A large one moves the anchor to the observation.
        assert!(t.correct_position(ms(11_000), clock.elapsed()));
        assert_eq!(t.position(clock.elapsed()), ms(11_000));
        clock.advance(S(2));
        assert_eq!(
            t.position(clock.elapsed()),
            ms(13_000),
            "the clock keeps running"
        );
    }

    #[test]
    fn drift_is_never_corrected_while_paused() {
        let (mut t, clock) = loaded(Some(S(100)));
        t.play(clock.elapsed());
        clock.advance(S(5));
        t.pause(clock.elapsed());
        assert!(!t.correct_position(S(50), clock.elapsed()));
        assert_eq!(t.position(clock.elapsed()), S(5));
    }

    #[test]
    fn drift_correction_respects_the_duration() {
        let (mut t, clock) = loaded(Some(S(10)));
        t.play(clock.elapsed());
        assert!(t.correct_position(S(999), clock.elapsed()));
        assert_eq!(t.position(clock.elapsed()), S(10));
    }

    #[test]
    fn a_rewound_clock_reports_the_anchor_rather_than_underflowing() {
        // The clock is documented as monotonic. If a caller ever hands us an
        // earlier instant, position must hold still, not wrap or go negative.
        let (mut t, clock) = loaded(Some(S(10)));
        t.play(clock.elapsed());
        clock.advance(S(5));
        assert_eq!(t.position(clock.elapsed()), S(5));
        // Move the anchor to t = 5s, so a `now` before it is genuinely stale.
        t.pause(clock.elapsed());
        t.play(clock.elapsed());
        let stale = t.position(Duration::from_millis(1000));
        assert!(stale >= Duration::ZERO);
        // Elapsed time saturates at zero, so the anchor position holds.
        assert_eq!(stale, S(5));
    }
}
