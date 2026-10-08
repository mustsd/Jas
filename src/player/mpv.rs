//! `mpv` backend: live control over JSON IPC.
//!
//! mpv is driven through `--input-ipc-server`, a Unix domain socket carrying
//! newline-delimited JSON. Because mpv can genuinely pause, seek, and change
//! speed, this backend reports `Capabilities::LIVE` and the transport uses it for
//! drift correction instead of emulation.
//!
//! Two details are load-bearing:
//!
//! - **Terminal input is disabled** (`--no-terminal`). Otherwise mpv would read
//!   the same keystrokes as Jas and `q` would quit the player, not the session.
//! - **One long-lived instance** (`--idle=yes`, then `loadfile` over IPC). Spawning
//!   mpv per track would reintroduce the ffplay restart problem.
//!
//! Only the Unix socket transport is implemented. On Windows the named-pipe
//! client is not written yet, so `detect` reports mpv but does not offer it, and
//! the chain falls through to ffplay. That limitation is stated rather than
//! hidden.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{Capabilities, Exit, Player};
use crate::error::{Error, Result};
use crate::time::arg_seconds;

/// How long to wait for the IPC socket to appear after spawning mpv.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-read timeout. A busy mpv must not freeze the session loop.
const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// One JSON command, as mpv expects it.
pub fn command_line(request_id: u64, args: &[String]) -> String {
    let mut out = String::from("{\"command\":[");
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(a));
    }
    out.push_str(&format!("],\"request_id\":{request_id}}}\n"));
    out
}

/// Minimal JSON string escaping: mpv commands can carry file paths, which may
/// contain quotes, backslashes, and arbitrary non-ASCII text.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A parsed reply from mpv.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// A reply to our request: the JSON `data` field, when present.
    Reply {
        request_id: u64,
        ok: bool,
        data: Option<serde_json::Value>,
        error: Option<String>,
    },
    /// An asynchronous event, e.g. `end-file`.
    Event { name: String },
    /// A line we could not use. Never fatal: mpv is allowed to add fields.
    Unknown(String),
}

/// Parse one newline-delimited reply. Unknown shapes are ignored rather than
/// treated as errors, so a newer mpv cannot break the session.
pub fn parse_response(line: &str) -> Response {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Response::Unknown(String::new());
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Response::Unknown(trimmed.to_string());
    };
    if let Some(event) = value.get("event").and_then(|e| e.as_str()) {
        return Response::Event {
            name: event.to_string(),
        };
    }
    let Some(request_id) = value.get("request_id").and_then(|v| v.as_u64()) else {
        return Response::Unknown(trimmed.to_string());
    };
    let error = value.get("error").and_then(|e| e.as_str());
    let ok = error.is_none() || error == Some("success");
    Response::Reply {
        request_id,
        ok,
        data: value.get("data").cloned(),
        error: error.filter(|e| *e != "success").map(str::to_string),
    }
}

/// Extract a numeric property (seconds) from a reply. Values that are absent,
/// negative, non-numeric, or too large to be a `Duration` all yield `None`
/// rather than panicking.
pub fn value_as_seconds(data: &serde_json::Value) -> Option<Duration> {
    let secs = data.as_f64()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(secs).ok()
}

struct Ipc {
    stream: BufReader<UnixStream>,
    request_id: u64,
}

impl Ipc {
    fn connect(path: &Path) -> Result<Self> {
        let deadline = Instant::now() + SOCKET_TIMEOUT;
        loop {
            match UnixStream::connect(path) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
                    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
                    return Ok(Self {
                        stream: BufReader::new(stream),
                        request_id: 1,
                    });
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(Error::runtime(format!(
                            "cannot connect to mpv at {}: {e}",
                            path.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }

    /// Send one command and wait for its reply.
    fn request(&mut self, args: &[String]) -> Result<Option<serde_json::Value>> {
        let id = self.request_id;
        self.request_id += 1;
        let line = command_line(id, args);
        self.stream
            .get_mut()
            .write_all(line.as_bytes())
            .and_then(|()| self.stream.get_mut().flush())
            .map_err(|e| Error::runtime(format!("mpv IPC write failed: {e}")))?;

        let deadline = Instant::now() + IO_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                return Err(Error::runtime(format!(
                    "mpv did not answer `{}` within {} ms",
                    args.first().cloned().unwrap_or_default(),
                    IO_TIMEOUT.as_millis()
                )));
            }
            let mut buf = String::new();
            match self.stream.read_line(&mut buf) {
                Ok(0) => {
                    return Err(Error::runtime(
                        "mpv closed the IPC connection; the player is gone".to_string(),
                    ))
                }
                Ok(_) => match parse_response(&buf) {
                    Response::Reply {
                        request_id,
                        ok,
                        data,
                        error,
                    } => {
                        if request_id != id {
                            // A stale reply from an earlier timed-out request.
                            continue;
                        }
                        if ok {
                            return Ok(data);
                        }
                        return Err(Error::runtime(format!(
                            "mpv rejected `{}`: {}",
                            args.join(" "),
                            error.unwrap_or_else(|| "unknown error".into())
                        )));
                    }
                    // `end-file` and friends are events, not replies. With
                    // `duration_feedback` set the transport already knows when the
                    // track ends, so they need no bookkeeping.
                    Response::Event { .. } => continue,
                    Response::Unknown(_) => continue,
                },
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue
                }
                Err(e) => return Err(Error::runtime(format!("mpv IPC read failed: {e}"))),
            }
        }
    }
}

pub struct MpvPlayer {
    child: Option<Child>,
    ipc: RefCell<Option<Ipc>>,
    socket_path: PathBuf,
    loaded: Option<PathBuf>,
    duration: Option<Duration>,
    speed: f64,
    paused: bool,
}

impl MpvPlayer {
    /// Spawn mpv with an idle IPC server. Fails if mpv cannot start or the socket
    /// never appears, so an unusable backend is reported at startup, not on the
    /// first keypress.
    pub fn new(program: PathBuf) -> Result<Self> {
        let socket_path = std::env::temp_dir().join(format!("jas-mpv-{}.sock", std::process::id()));
        // A stale socket from a crashed run would make mpv refuse to bind.
        let _ = std::fs::remove_file(&socket_path);

        let child = Command::new(&program)
            .args([
                "--idle=yes",
                "--no-video",
                "--really-quiet",
                // Load-bearing: without this mpv reads our keystrokes.
                "--no-terminal",
                "--no-input-default-bindings",
                "--keep-open=no",
                &format!("--input-ipc-server={}", socket_path.display()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::runtime(format!("cannot start {}: {e}", program.display())))?;

        let mut player = Self {
            child: Some(child),
            ipc: RefCell::new(None),
            socket_path,
            loaded: None,
            duration: None,
            speed: 1.0,
            paused: true,
        };
        match Ipc::connect(&player.socket_path) {
            Ok(ipc) => {
                *player.ipc.borrow_mut() = Some(ipc);
                Ok(player)
            }
            Err(e) => {
                // Do not leave an orphaned mpv behind on a failed handshake.
                player.kill();
                Err(e)
            }
        }
    }

    fn request(&self, args: &[String]) -> Result<Option<serde_json::Value>> {
        let mut borrow = self.ipc.borrow_mut();
        let ipc = borrow
            .as_mut()
            .ok_or_else(|| Error::runtime("mpv is not connected"))?;
        ipc.request(args)
    }

    fn set_property(&self, name: &str, value: &str) -> Result<()> {
        self.request(&[
            "set_property".to_string(),
            name.to_string(),
            value.to_string(),
        ])?;
        Ok(())
    }

    fn get_property(&self, name: &str) -> Result<Option<serde_json::Value>> {
        self.request(&["get_property".to_string(), name.to_string()])
    }

    fn kill(&mut self) {
        *self.ipc.borrow_mut() = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }

    /// Events mpv queued while we were talking to it are not collected: with
    /// `duration_feedback` set, the transport already knows when a track ends.
    /// Refresh the cached duration, used by the transport for clamping.
    fn refresh_duration(&mut self) -> Option<Duration> {
        if let Ok(Some(value)) = self.get_property("duration") {
            self.duration = value_as_seconds(&value);
        }
        self.duration
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        self.kill();
    }
}

impl Player for MpvPlayer {
    fn name(&self) -> &'static str {
        "mpv"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::LIVE
    }

    fn load(&mut self, track: &Path, at: Duration) -> Result<Option<Duration>> {
        if !track.exists() {
            return Err(Error::runtime(format!(
                "{}: file not found",
                track.display()
            )));
        }
        let path = track.to_string_lossy().into_owned();
        // `loadfile` replaces the current file; the start offset and the paused
        // state ride along as per-file options, so there is no window where the
        // wrong audio plays.
        self.request(&[
            "loadfile".to_string(),
            path,
            "replace".to_string(),
            format!("start={},pause=yes", arg_seconds(at)),
        ])?;
        self.loaded = Some(track.to_path_buf());
        self.paused = true;
        self.duration = None;
        // mpv needs a moment to probe the file before `duration` is meaningful.
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(d) = self.refresh_duration() {
                return Ok(Some(d));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn play(&mut self) -> Result<()> {
        self.set_property("pause", "no")?;
        self.paused = false;
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        self.set_property("pause", "yes")?;
        self.paused = true;
        Ok(())
    }

    fn seek(&mut self, to: Duration) -> Result<()> {
        self.request(&["seek".to_string(), arg_seconds(to), "absolute".to_string()])?;
        Ok(())
    }

    fn set_speed(&mut self, speed: f64) -> Result<()> {
        self.set_property("speed", &format!("{speed}"))?;
        self.speed = speed;
        Ok(())
    }

    fn set_ab_loop(&mut self, ab: Option<(Duration, Option<Duration>)>) -> Result<()> {
        match ab {
            None => {
                // `no` clears the property.
                self.request(&["set_property".into(), "ab-loop-a".into(), "no".into()])?;
                self.request(&["set_property".into(), "ab-loop-b".into(), "no".into()])?;
            }
            Some((a, b)) => {
                self.request(&["set_property".into(), "ab-loop-a".into(), arg_seconds(a)])?;
                // A missing B means "the end of the file" for the transport; mpv's
                // `no` means exactly that.
                let b = match b {
                    Some(b) => arg_seconds(b),
                    None => "no".to_string(),
                };
                self.request(&["set_property".into(), "ab-loop-b".into(), b])?;
            }
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        // Clear the file so nothing keeps playing; mpv stays alive and idle.
        self.request(&["stop".to_string()])?;
        self.loaded = None;
        self.paused = true;
        Ok(())
    }

    fn position(&self) -> Option<Duration> {
        let value = self.get_property("time-pos").ok().flatten()?;
        value_as_seconds(&value)
    }

    /// mpv is one long-lived idle process, so it does not exit per track. The only
    /// time it reports an exit is when it really died, which is always a failure.
    fn poll_exit(&mut self) -> Option<Exit> {
        let status = self.child.as_mut()?.try_wait().ok()??;
        self.child = None;
        Some(if status.success() {
            Exit::Finished
        } else {
            Exit::Failed
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_are_valid_json_with_a_request_id() {
        let line = command_line(7, &["set_property".into(), "pause".into(), "yes".into()]);
        assert!(line.ends_with('\n'));
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).expect("valid JSON");
        assert_eq!(parsed["request_id"], 7);
        assert_eq!(parsed["command"][0], "set_property");
        assert_eq!(parsed["command"][2], "yes");
    }

    #[test]
    fn paths_with_special_characters_are_escaped() {
        let line = command_line(1, &["loadfile".into(), "/a/\"quo\\te\".mp3".into()]);
        let parsed: serde_json::Value =
            serde_json::from_str(line.trim()).expect("escaping must produce valid JSON");
        assert_eq!(parsed["command"][1], "/a/\"quo\\te\".mp3");
    }

    #[test]
    fn non_ascii_paths_are_sent_verbatim() {
        for name in ["/موسيقى/درس.mp3", "/音乐/第二课.mp3", "/x/🎧.mp3"] {
            let line = command_line(2, &["loadfile".into(), name.into()]);
            let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(parsed["command"][1], name);
        }
    }

    #[test]
    fn control_characters_cannot_break_the_protocol() {
        // A newline inside a path must not terminate the JSON line early.
        let line = command_line(3, &["loadfile".into(), "/a\nb.mp3".into()]);
        assert_eq!(line.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["command"][1], "/a\nb.mp3");
    }

    #[test]
    fn a_successful_reply_is_parsed() {
        let r = parse_response(r#"{"request_id":4,"error":"success","data":12.5}"#);
        match r {
            Response::Reply {
                request_id,
                ok,
                data,
                error,
            } => {
                assert_eq!(request_id, 4);
                assert!(ok);
                assert_eq!(error, None);
                assert_eq!(data.and_then(|d| d.as_f64()), Some(12.5));
            }
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn an_error_reply_is_reported_not_swallowed() {
        let r = parse_response(r#"{"request_id":4,"error":"property not found"}"#);
        match r {
            Response::Reply { ok, error, .. } => {
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("property not found"));
            }
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn asynchronous_events_are_recognized() {
        assert_eq!(
            parse_response(r#"{"event":"end-file","reason":"eof"}"#),
            Response::Event {
                name: "end-file".into()
            }
        );
        assert_eq!(
            parse_response(r#"{"event":"idle"}"#),
            Response::Event {
                name: "idle".into()
            }
        );
    }

    #[test]
    fn unknown_lines_are_ignored_rather_than_fatal() {
        assert_eq!(parse_response("").get_unknown(), Some(""));
        assert!(matches!(parse_response("not json"), Response::Unknown(_)));
        assert!(matches!(
            parse_response(r#"{"weird":true}"#),
            Response::Unknown(_)
        ));
    }

    #[test]
    fn seconds_are_extracted_safely() {
        let value = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(
            value_as_seconds(&value("12.5")),
            Some(Duration::from_millis(12_500))
        );
        assert_eq!(value_as_seconds(&value("0")), Some(Duration::ZERO));
        // mpv reports `null` while a property is unavailable.
        assert_eq!(value_as_seconds(&value("null")), None);
        assert_eq!(value_as_seconds(&value("\"yes\"")), None);
        assert_eq!(value_as_seconds(&value("-1")), None);
        // A value too large for a `Duration` must not panic the session loop.
        assert_eq!(value_as_seconds(&value("1e308")), None);
        assert_eq!(value_as_seconds(&value("{\"a\":1}")), None);
    }

    #[test]
    fn capabilities_advertise_live_control() {
        // Constructing an MpvPlayer requires mpv, so check the negotiated profile
        // that the backend reports, field by field.
        assert_eq!(
            Capabilities::LIVE,
            Capabilities {
                live_pause: true,
                live_seek: true,
                live_speed: true,
                native_ab_loop: true,
                position_feedback: true,
                duration_feedback: true,
            }
        );
        assert_eq!(Capabilities::EMULATED, Capabilities::default());
    }

    /// The real handshake, skipped when mpv is not installed.
    #[test]
    fn mpv_can_be_started_when_it_is_installed() {
        let Some(program) = crate::player::detect::find_program("mpv") else {
            eprintln!("skipping: mpv is not installed");
            return;
        };
        let player = MpvPlayer::new(program);
        match player {
            Ok(p) => assert_eq!(p.name(), "mpv"),
            Err(e) => panic!("mpv is installed but the handshake failed: {e}"),
        }
    }

    /// End-to-end against real audio, skipped when ffmpeg/mpv are absent.
    #[test]
    fn mpv_loads_generated_audio_when_available() {
        use crate::player::detect::find_program;
        let (Some(mpv), Some(ffmpeg)) = (find_program("mpv"), find_program("ffmpeg")) else {
            eprintln!("skipping: mpv and/or ffmpeg is not installed");
            return;
        };
        let dir = std::env::temp_dir().join(format!("jas-mpv-it-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("sine.wav");
        let status = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
            ])
            .arg(&wav)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            eprintln!("skipping: could not generate test audio");
            return;
        }

        let mut player = MpvPlayer::new(mpv).expect("mpv should start");
        let duration = player
            .load(&wav, Duration::ZERO)
            .expect("load should succeed");
        assert!(
            matches!(duration, Some(d) if d > Duration::from_millis(1500) && d < Duration::from_millis(2500)),
            "expected roughly 2 s of duration, got {duration:?}"
        );
        player
            .set_speed(0.75)
            .expect("speed should be settable live");
        player
            .set_ab_loop(Some((
                Duration::from_millis(200),
                Some(Duration::from_millis(800)),
            )))
            .expect("A-B should be settable live");
        player
            .seek(Duration::from_millis(500))
            .expect("seek should be live");
        player.set_ab_loop(None).expect("A-B should clear");
        player.stop().expect("stop should succeed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Small helper so the `Unknown` tests read cleanly.
    impl Response {
        fn get_unknown(&self) -> Option<&str> {
            match self {
                Response::Unknown(s) => Some(s.as_str()),
                _ => None,
            }
        }
    }
}
