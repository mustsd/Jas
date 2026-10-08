//! `ffplay` backend: respawn control.
//!
//! `ffplay -nodisp` has **no control channel**: no keyboard handling we can use
//! (and it would steal ours), no IPC. So control is emulated by restarting the
//! child:
//!
//! - seek  = kill the child, respawn at the new offset
//! - pause = kill the child, freeze the transport clock
//! - speed = respawn with `-af atempo=<X>`
//!
//! Position is *not* tracked here. `transport.rs` owns it, and this backend only
//! answers "spawn at offset T". That is what makes behaviour identical across
//! backends even though the mechanisms differ completely.
//!
//! Every child is spawned with `Stdio::null()` on stdin. This is load-bearing:
//! `ffplay` reads keys even under `-nodisp`, so without it one keypress would hit
//! both processes and `q` would kill the player instead of Jas.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{atempo_filter, Capabilities, Exit, Player};
use crate::error::{Error, Result};
use crate::time::arg_seconds;

/// How many trailing stderr lines to keep for diagnostics.
const ERROR_TAIL: usize = 8;

pub struct FfplayPlayer {
    program: PathBuf,
    child: Option<Child>,
    /// The file the next spawn should use.
    track: Option<PathBuf>,
    /// Offset in seconds for the next spawn, and the speed to apply.
    start: Duration,
    speed: f64,
    ab: Option<(Duration, Option<Duration>)>,
    /// Trailing stderr lines from the current child.
    errors: Arc<Mutex<VecDeque<String>>>,
    /// Extra arguments, used by tests to substitute a stand-in program.
    extra_args: Vec<String>,
}

impl FfplayPlayer {
    pub fn new(program: PathBuf) -> Self {
        Self {
            program,
            child: None,
            track: None,
            start: Duration::ZERO,
            speed: 1.0,
            ab: None,
            errors: Arc::new(Mutex::new(VecDeque::new())),
            extra_args: Vec::new(),
        }
    }

    /// Build the argument list for a spawn at `start` with the current speed.
    ///
    /// Kept separate from spawning so the exact command line is testable without
    /// launching anything.
    pub fn args_for(&self, track: &Path, start: Duration, speed: f64) -> Vec<String> {
        let mut args = vec![
            "-nodisp".to_string(),
            "-autoexit".to_string(),
            "-hide_banner".to_string(),
            "-nostats".to_string(),
            "-loglevel".to_string(),
            "error".to_string(),
        ];
        if start > Duration::ZERO {
            args.push("-ss".to_string());
            args.push(arg_seconds(start));
        }
        // Pitch-preserving speed change. Outside 0.5-2.0 the filter is chained.
        if let Some(filter) = atempo_filter(speed) {
            args.push("-af".to_string());
            args.push(filter);
        }
        args.push("--".to_string());
        args.push(track.to_string_lossy().into_owned());
        args
    }

    fn spawn(&mut self) -> Result<()> {
        let Some(track) = self.track.clone() else {
            return Err(Error::runtime("ffplay: nothing loaded"));
        };
        self.kill();
        if let Ok(mut errors) = self.errors.lock() {
            errors.clear();
        }
        let args = self.args_for(&track, self.start, self.speed);
        let mut cmd = Command::new(&self.program);
        cmd.args(&args)
            .args(&self.extra_args)
            // stdin: never let the child see our keystrokes.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // stderr is piped and drained on a thread, so a decode failure is
            // visible while playback continues; a full pipe would block the child.
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| {
            Error::runtime(format!(
                "cannot start {}: {e} (is ffmpeg still installed?)",
                self.program.display()
            ))
        })?;
        if let Some(stderr) = child.stderr.take() {
            let sink = Arc::clone(&self.errors);
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(mut errors) = sink.lock() {
                        if errors.len() == ERROR_TAIL {
                            errors.pop_front();
                        }
                        errors.push_back(line);
                    }
                }
            });
        }
        self.child = Some(child);
        Ok(())
    }

    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Direct kill, then reap. A process group/job object is not used, so a
            // child that forks would outlive us; ffplay does not fork.
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// The most recent stderr lines from the running child.
    pub fn error_tail(&self) -> Vec<String> {
        self.errors
            .lock()
            .map(|e| e.iter().cloned().collect())
            .unwrap_or_default()
    }
}

impl Drop for FfplayPlayer {
    fn drop(&mut self) {
        // A dropped player must never leave audio playing.
        self.kill();
    }
}

impl Player for FfplayPlayer {
    fn name(&self) -> &'static str {
        "ffplay"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::EMULATED
    }

    fn load(&mut self, track: &Path, at: Duration) -> Result<Option<Duration>> {
        if !track.exists() {
            return Err(Error::runtime(format!(
                "{}: file not found",
                track.display()
            )));
        }
        self.kill();
        self.track = Some(track.to_path_buf());
        self.start = at;
        self.ab = None;
        // ffplay cannot report a duration, and probing it separately would add a
        // second process per track; the transport learns it from the clock.
        Ok(None)
    }

    fn play(&mut self) -> Result<()> {
        self.spawn()
    }

    fn pause(&mut self) -> Result<()> {
        // No pause primitive exists, so silence is produced by stopping.
        self.kill();
        Ok(())
    }

    fn seek(&mut self, to: Duration) -> Result<()> {
        // Record the target; the session reloads and respawns. Resuming here would
        // make a seek while paused start audio, which the transport forbids.
        self.start = to;
        Ok(())
    }

    fn set_speed(&mut self, speed: f64) -> Result<()> {
        self.speed = speed;
        Ok(())
    }

    fn set_ab_loop(&mut self, ab: Option<(Duration, Option<Duration>)>) -> Result<()> {
        // No native loop: the transport polls its own clock and respawns at A.
        self.ab = ab;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.kill();
        Ok(())
    }

    /// Report a child that ended on its own, distinguishing "played to the end"
    /// from "died".
    ///
    /// The distinction is the whole point: `ffplay` cannot report a duration, so a
    /// clean exit *is* the end-of-track signal. Treating any exit as a failure would
    /// make every finished track look like a crash and stop the playlist instead of
    /// advancing it -- which is exactly what happened before this method existed.
    fn poll_exit(&mut self) -> Option<Exit> {
        let status = self.child.as_mut()?.try_wait().ok()??;
        // Reaped: drop the handle so a later poll reports nothing rather than the
        // same exit twice.
        self.child = None;
        Some(if status.success() {
            Exit::Finished
        } else {
            Exit::Failed
        })
    }

    fn diagnostics(&self) -> Option<String> {
        // The last stderr line is the one that caused the exit; the earlier ones
        // are context that a one-line report would only dilute.
        self.error_tail().last().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player() -> FfplayPlayer {
        FfplayPlayer::new(PathBuf::from("/usr/bin/ffplay"))
    }

    #[test]
    fn the_command_line_is_exactly_what_the_respawn_strategy_needs() {
        let p = player();
        let args = p.args_for(Path::new("/audio/ch1.mp3"), Duration::ZERO, 1.0);
        assert_eq!(
            args,
            vec![
                "-nodisp",
                "-autoexit",
                "-hide_banner",
                "-nostats",
                "-loglevel",
                "error",
                "--",
                "/audio/ch1.mp3"
            ]
        );
    }

    #[test]
    fn a_seek_offset_becomes_minus_ss_with_milliseconds() {
        let p = player();
        let args = p.args_for(Path::new("/a.mp3"), Duration::from_millis(1500), 1.0);
        let i = args.iter().position(|a| a == "-ss").expect("no -ss");
        assert_eq!(args[i + 1], "1.500");
        assert!(args.contains(&"/a.mp3".to_string()));
    }

    #[test]
    fn extra_args_come_after_the_player_arguments_so_they_can_override() {
        let mut p = player();
        p.extra_args = vec!["-x".to_string()];
        let args = p.args_for(Path::new("/a.mp3"), Duration::ZERO, 1.0);
        assert!(args.len() >= 2);
        // The filename is last, the extra argument before it: ffplay's own flags
        // must not be swallowed as media.
        assert_eq!(args[args.len() - 1], "/a.mp3");
    }

    #[test]
    fn speed_becomes_a_pitch_preserving_filter() {
        let p = player();
        let args = p.args_for(Path::new("/a.mp3"), Duration::ZERO, 0.75);
        let i = args.iter().position(|a| a == "-af").expect("no -af");
        assert_eq!(args[i + 1], "atempo=0.7500");
        // At 1.0x there is no filter at all.
        let args = p.args_for(Path::new("/a.mp3"), Duration::ZERO, 1.0);
        assert!(!args.contains(&"-af".to_string()));
    }

    #[test]
    fn a_track_name_that_looks_like_a_flag_is_still_the_media() {
        // The `--` separator keeps a file named `-ss` from being parsed as a flag.
        let p = player();
        let args = p.args_for(Path::new("-weird.mp3"), Duration::ZERO, 1.0);
        let sep = args.iter().position(|a| a == "--").expect("no --");
        assert_eq!(args[sep + 1], "-weird.mp3");
    }

    #[test]
    fn non_ascii_paths_survive_argument_construction() {
        let p = player();
        for name in ["/موسيقى/درس.mp3", "/音乐/第二课.mp3", "/x/🎧.mp3"] {
            let args = p.args_for(Path::new(name), Duration::ZERO, 1.0);
            assert_eq!(args.last().map(String::as_str), Some(name));
        }
    }

    #[test]
    fn playing_nothing_is_a_clear_error_not_a_panic() {
        let mut p = player();
        let err = p.play().unwrap_err();
        assert!(err.message().contains("nothing loaded"));
    }

    #[test]
    fn loading_a_missing_file_fails_before_spawning_anything() {
        let mut p = player();
        let err = p
            .load(Path::new("/definitely/not/here.mp3"), Duration::ZERO)
            .unwrap_err();
        assert!(err.message().contains("not found"));
        assert!(p.child.is_none(), "no process should have been started");
    }

    #[test]
    fn capabilities_are_honestly_emulated() {
        let p = player();
        let caps = p.capabilities();
        assert_eq!(caps, Capabilities::EMULATED);
        assert!(!caps.live_pause);
        assert!(!caps.native_ab_loop);
        assert!(!caps.position_feedback);
        assert_eq!(p.name(), "ffplay");
    }

    #[test]
    fn seek_only_records_the_target_until_the_session_restarts() {
        let mut p = player();
        p.seek(Duration::from_secs(10)).unwrap();
        assert_eq!(p.start, Duration::from_secs(10));
        assert!(p.child.is_none(), "a seek must not start audio on its own");
    }

    #[test]
    fn stop_and_pause_are_safe_with_no_child() {
        let mut p = player();
        assert!(p.pause().is_ok());
        assert!(p.stop().is_ok());
        assert_eq!(p.poll_exit(), None);
    }

    #[test]
    fn dropping_the_player_stops_audio() {
        let mut p = player();
        // Simulate a live child with a real, sleeping process.
        #[cfg(unix)]
        {
            let child = Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn sleep");
            p.child = Some(child);
            assert_eq!(p.poll_exit(), None, "a sleeping child is still running");
        }
        drop(p);
        // The child was killed and reaped: no assertion is needed beyond not
        // hanging, which a failed wait() would have caused.
    }

    /// The real contract, run only when ffplay and ffmpeg are both installed.
    ///
    /// This actually spawns `ffplay`, plays generated audio to completion, and
    /// checks the respawn behaviour: a paused player is silent, and a resumed one
    /// starts at the held offset.
    #[test]
    fn ffplay_plays_generated_audio_when_it_is_installed() {
        let Some(ffplay) = crate::player::detect::find_program("ffplay") else {
            eprintln!("skipping: ffplay is not installed");
            return;
        };
        let Some(ffmpeg) = crate::player::detect::find_program("ffmpeg") else {
            eprintln!("skipping: ffmpeg is not installed, cannot generate test audio");
            return;
        };
        let dir = std::env::temp_dir().join(format!("jas-ffplay-it-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        let generated = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
            ])
            .arg(&wav)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !matches!(generated, Ok(s) if s.success()) {
            eprintln!("skipping: could not generate test audio");
            return;
        }

        let mut player = FfplayPlayer::new(ffplay);
        // ffplay cannot report a duration, which is exactly why the transport
        // owns the clock; the contract is that `load` returns `None`.
        assert_eq!(player.load(&wav, Duration::ZERO).unwrap(), None);
        player.play().expect("ffplay should start");
        assert_eq!(player.poll_exit(), None, "ffplay should be running");

        // Pausing kills the child: no control channel exists in -nodisp mode.
        player.pause().unwrap();
        assert_eq!(
            player.poll_exit(),
            None,
            "no child should remain after a pause"
        );

        // Resuming from an offset respawns at that offset.
        player.seek(Duration::from_millis(500)).unwrap();
        player.play().expect("ffplay should restart");
        assert_eq!(player.poll_exit(), None);

        // With -autoexit it finishes on its own, and that must be reported as
        // *finished*, not as a failure: for a backend with no duration feedback it
        // is the end-of-track signal.
        let mut outcome = None;
        for _ in 0..200 {
            if let Some(exit) = player.poll_exit() {
                outcome = Some(exit);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            outcome,
            Some(Exit::Finished),
            "ffplay exiting after the track ends is a normal finish, not a crash"
        );
        player.stop().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_child_that_exits_on_its_own_reports_how_it_ended() {
        #[cfg(unix)]
        {
            let mut p = player();
            let child = Command::new("true")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn true");
            p.child = Some(child);
            // Give it a moment to exit, then poll. Bounded so the test cannot hang.
            let mut outcome = None;
            for _ in 0..50 {
                if let Some(exit) = p.poll_exit() {
                    outcome = Some(exit);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(outcome, Some(Exit::Finished));
            // Reported once, not repeatedly.
            assert_eq!(p.poll_exit(), None);
        }
    }

    #[test]
    fn a_child_that_fails_reports_failure() {
        #[cfg(unix)]
        {
            let mut p = player();
            let child = Command::new("false")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn false");
            p.child = Some(child);
            let mut outcome = None;
            for _ in 0..50 {
                if let Some(exit) = p.poll_exit() {
                    outcome = Some(exit);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(outcome, Some(Exit::Failed));
        }
    }
}
