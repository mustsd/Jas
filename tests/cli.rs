//! End-to-end tests that spawn the built binary and drive its real stdin/stdout.
//!
//! These are the layer below the smoke script: no shell, and they assert on exit
//! codes and output text exactly as a user or a script would. Most need no working
//! audio backend, because a session that never starts playback makes no sound;
//! anything requiring a real backend is gated and skips cleanly.
//!
//! Note on `#[ignore]`: nothing here is ignored. A test that cannot run prints why
//! and returns, so a skipped backend is visible in `--nocapture` output rather
//! than silently counted as a pass with no work done.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_jas");

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn tmpdir(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("jas-cli-test-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn touch(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"not real audio").expect("write temp file");
    path
}

/// Run the binary with `args`, feeding `stdin` when given.
fn run(args: &[&str], stdin: Option<&str>, cwd: Option<&Path>) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().expect("spawn jas");
    if let Some(text) = stdin {
        child
            .stdin
            .as_mut()
            .expect("stdin pipe")
            .write_all(text.as_bytes())
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait for jas");
    Output {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn version_and_help_are_well_formed() {
    let out = run(&["--version"], None, None);
    assert_eq!(out.code, Some(0));
    assert!(out.stdout.contains("jas"), "{}", out.stdout);

    let out = run(&["--help"], None, None);
    assert_eq!(out.code, Some(0));
    // R10's escape hatch must be discoverable from the help text.
    assert!(out.stdout.contains("stty sane"), "{}", out.stdout);
    // And the keys, so a user can start without the README.
    assert!(out.stdout.contains("space"), "{}", out.stdout);
    // The full-screen interface is the default on a terminal, so the way out of it
    // has to be in `--help` rather than only in the README.
    assert!(out.stdout.contains("--no-tui"), "{}", out.stdout);
}

#[test]
fn a_pipe_never_gets_the_full_screen_interface() {
    // The TUI is the default when both ends are a terminal. A pipe must stay plain
    // lines: a script reading stdout cannot be handed alternate-screen escapes.
    let dir = tmpdir("pipe-no-tui");
    let file = touch(&dir, "lesson.mp3");
    let out = run(
        &["--no-state", file.to_str().unwrap()],
        Some("status\nquit\n"),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        !out.stdout.contains('\x1b'),
        "an escape sequence reached a pipe: {:?}",
        out.stdout
    );
    assert!(out.stdout.contains("state=paused"), "{}", out.stdout);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_tui_keeps_the_line_oriented_interface() {
    // `--no-tui` includes `--status-line`, which is a line-mode feature: the two
    // together must behave exactly as they did before the TUI existed.
    let dir = tmpdir("no-tui");
    touch(&dir, "lesson.mp3");
    touch(&dir, "second.mp3");
    let out = run(
        &[
            "--no-state",
            "--no-tui",
            "--status-line",
            dir.to_str().unwrap(),
        ],
        Some("status\nnext\nstatus\nquit\n"),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("state=paused"), "{}", out.stdout);
    assert!(
        out.stdout.contains("track=2/2"),
        "`next` still moves through the playlist: {}",
        out.stdout
    );
    assert!(!out.stdout.contains('\x1b'), "{}", out.stdout);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unknown_flag_is_a_usage_error() {
    let out = run(&["--definitely-not-a-flag"], None, None);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("--definitely-not-a-flag"),
        "{}",
        out.stderr
    );
}

#[test]
fn list_resolves_a_directory_naturally_without_a_backend() {
    let dir = tmpdir("list");
    for name in ["ch10.mp3", "ch2.mp3", "ch1.mp3"] {
        touch(&dir, name);
    }
    let out = run(&["--list", dir.to_str().unwrap()], None, None);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let names: Vec<String> = out
        .stdout
        .lines()
        .map(|l| l.split_whitespace().last().unwrap_or("").to_string())
        .collect();
    assert_eq!(names.len(), 3, "{}", out.stdout);
    assert!(names[0].ends_with("ch1.mp3"), "{names:?}");
    assert!(names[1].ends_with("ch2.mp3"), "{names:?}");
    assert!(names[2].ends_with("ch10.mp3"), "{names:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_json_is_parseable_and_indexed() {
    let dir = tmpdir("list-json");
    touch(&dir, "a.mp3");
    touch(&dir, "b.mp3");
    let out = run(&["--list", "--json", dir.to_str().unwrap()], None, None);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", out.stdout));
    let entries = parsed.as_array().expect("an array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["index"], 1);
    assert!(entries[0]["path"].is_string());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_playable_input_exits_three() {
    let dir = tmpdir("none");
    let out = run(
        &["--list", dir.join("missing").to_str().unwrap()],
        None,
        None,
    );
    assert_eq!(out.code, Some(3), "stderr: {}", out.stderr);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_runs_without_a_backend_and_reports_the_choice() {
    let out = run(&["--doctor"], None, None);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("backends found"), "{}", out.stdout);
    assert!(out.stdout.contains("resolved:"), "{}", out.stdout);
}

/// A scripted session that never starts audio, so it needs no working backend and
/// makes no sound: the commands exercise parsing, dispatch, and output only.#[test]
fn cfg_dir_with(name: &str, config_json: &str) -> (PathBuf, PathBuf) {
    let dir = tmpdir(name);
    let cfg = dir.join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.json"), config_json).unwrap();
    (dir, cfg)
}

#[test]
fn config_defaults_are_applied_when_the_flags_are_absent() {
    // `loop_mode` and `speed` come from config.json only because the CLI leaves
    // them unset; this is the precedence rule in one assertion.
    let (dir, cfg) = cfg_dir_with("cfg-defaults", r#"{"loop_mode":"one","speed":0.6}"#);
    let file = touch(&dir, "lesson.mp3");

    let out = run(
        &[
            "--no-keys",
            "--config",
            cfg.to_str().unwrap(),
            file.to_str().unwrap(),
        ],
        Some(
            "status
quit
",
        ),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("loop=one"), "{}", out.stdout);
    assert!(out.stdout.contains("speed=0.6"), "{}", out.stdout);

    // And the flag beats the file.
    let out = run(
        &[
            "--no-keys",
            "--loop",
            "off",
            "--speed",
            "1.25",
            "--config",
            cfg.to_str().unwrap(),
            file.to_str().unwrap(),
        ],
        Some(
            "status
quit
",
        ),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("loop=off"), "{}", out.stdout);
    assert!(out.stdout.contains("speed=1.25"), "{}", out.stdout);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn config_backend_is_honoured_and_still_reports_a_missing_backend() {
    // `native` is deliberately not built in, so a config that asks for it must fail
    // the same way `--backend native` does. If config were ignored, this would exit
    // 0 and play through the auto-detected backend instead.
    let (dir, cfg) = cfg_dir_with("cfg-backend", r#"{"backend":"native"}"#);
    let file = touch(&dir, "lesson.mp3");
    let out = run(
        &["--config", cfg.to_str().unwrap(), file.to_str().unwrap()],
        Some(
            "quit
",
        ),
        None,
    );
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("native"), "{}", out.stderr);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unknown_backend_in_config_is_a_usage_error() {
    let (dir, cfg) = cfg_dir_with("cfg-backend-bad", r#"{"backend":"vlc"}"#);
    let file = touch(&dir, "lesson.mp3");
    let out = run(
        &["--config", cfg.to_str().unwrap(), file.to_str().unwrap()],
        Some(
            "quit
",
        ),
        None,
    );
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("vlc"), "{}", out.stderr);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_piped_script_is_executed_line_by_line() {
    let dir = tmpdir("script");
    let file = touch(&dir, "lesson.mp3");
    let script = "status\nlist\nsave\nseek +5\nspeed 0.75\nloop one\nhelp\nquit\n";
    let out = run(
        &["--no-state", "--no-keys", file.to_str().unwrap()],
        Some(script),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    // A loaded, waiting track reads as `paused`, not `idle`: something is loaded.
    assert!(out.stdout.contains("state=paused"), "{}", out.stdout);
    assert!(out.stdout.contains("speed 0.75"), "{}", out.stdout);
    assert!(out.stdout.contains("loop one"), "{}", out.stdout);
    assert!(out.stdout.contains("lesson.mp3"), "{}", out.stdout);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_bad_script_line_is_reported_and_the_rest_still_runs() {
    let dir = tmpdir("bad-script");
    let file = touch(&dir, "lesson.mp3");
    let out = run(
        &["--no-state", file.to_str().unwrap()],
        Some("seek nonsense\nstatus\nquit\n"),
        None,
    );
    assert_eq!(
        out.code,
        Some(0),
        "a bad line must not abort the run: {}",
        out.stderr
    );
    assert!(out.stdout.contains("seek:"), "{}", out.stdout);
    // The line after the bad one still ran.
    assert!(out.stdout.contains("state="), "{}", out.stdout);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_relative_seek_clamps_at_zero_and_is_reported() {
    let dir = tmpdir("clamp");
    let file = touch(&dir, "lesson.mp3");
    let out = run(
        &["--no-state", file.to_str().unwrap()],
        Some("seek -30\nstatus\nquit\n"),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("seek -0:30"), "{}", out.stdout);
    assert!(out.stdout.contains("pos=0.0"), "{}", out.stdout);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn non_ascii_paths_work_end_to_end() {
    let dir = tmpdir("unicode");
    for name in ["عربي.mp3", "中文.mp3", "song🎧.mp3"] {
        touch(&dir, name);
    }
    let out = run(&["--list", dir.to_str().unwrap()], None, None);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 3, "{}", out.stdout);
    // The bytes survive: each name appears intact in the listing.
    for name in ["عربي.mp3", "中文.mp3", "song🎧.mp3"] {
        assert!(out.stdout.contains(name), "lost {name} in: {}", out.stdout);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_empty_playlist_with_no_paths_and_no_state_exits_three() {
    let dir = tmpdir("empty");
    // `--no-state` means no saved playlist either, so there is nothing to play.
    let out = run(
        &["--no-state", "--config", dir.to_str().unwrap()],
        Some("status\n"),
        None,
    );
    assert_eq!(
        out.code,
        Some(3),
        "stdout: {} stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stderr.contains("no playable files") || out.stderr.contains("nothing to play"),
        "{}",
        out.stderr
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn state_is_written_atomically_and_reloaded() {
    let dir = tmpdir("state");
    let cfg = dir.join("cfg");
    let file = touch(&dir, "lesson.mp3");

    let out = run(
        &[
            "--no-keys",
            "--config",
            cfg.to_str().unwrap(),
            file.to_str().unwrap(),
        ],
        Some("ab 0:00.500 0:01.200\nspeed 0.8\nsave\nquit\n"),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    let state = std::fs::read_to_string(cfg.join("state.json")).expect("state.json written");
    let parsed: serde_json::Value = serde_json::from_str(&state).expect("valid JSON");
    assert_eq!(parsed["loop_mode"], "all");
    let tracks = parsed["tracks"].as_object().expect("tracks object");
    assert_eq!(tracks.len(), 1);
    let entry = tracks.values().next().unwrap();
    assert_eq!(entry["ab_a_ms"], 500);
    assert_eq!(entry["ab_b_ms"], 1200);
    assert_eq!(entry["speed"], 0.8);

    // A second run with no paths restores that playlist and its marks.
    let out = run(
        &["--no-keys", "--config", cfg.to_str().unwrap()],
        Some("status\nquit\n"),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("lesson.mp3"), "{}", out.stdout);
    assert!(out.stdout.contains("ab=0.5-1.2"), "{}", out.stdout);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_corrupt_state_file_is_reported_but_not_fatal() {
    let dir = tmpdir("corrupt");
    let cfg = dir.join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("state.json"), b"{ not json at all").unwrap();
    let file = touch(&dir, "lesson.mp3");

    let out = run(
        &[
            "--no-keys",
            "--config",
            cfg.to_str().unwrap(),
            file.to_str().unwrap(),
        ],
        Some("quit\n"),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("corrupt"), "{}", out.stderr);
    // Moved aside rather than deleted, so the user can inspect it.
    assert!(cfg.join("state.json.corrupt-0").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_bad_keymap_entry_is_reported_and_the_rest_of_the_map_survives() {
    let dir = tmpdir("keymap");
    let cfg = dir.join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("config.json"),
        br#"{"keymap":{"z":"toggle","y":"not-a-command"}}"#,
    )
    .unwrap();
    let file = touch(&dir, "lesson.mp3");

    let out = run(
        &[
            "--no-keys",
            "--config",
            cfg.to_str().unwrap(),
            file.to_str().unwrap(),
        ],
        Some("keys\nquit\n"),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("not-a-command"), "{}", out.stderr);
    // The valid override is in the effective map, and the defaults are still there.
    assert!(out.stdout.contains("toggle"), "{}", out.stdout);
    assert!(out.stdout.contains("next"), "{}", out.stdout);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn m3u_playlists_are_resolved_relative_to_the_file() {
    let dir = tmpdir("m3u");
    touch(&dir, "one.mp3");
    touch(&dir, "two.mp3");
    let list = dir.join("drill.m3u");
    std::fs::write(&list, b"#EXTM3U\r\none.mp3\r\ntwo.mp3\r\n").unwrap();
    // Run from a different directory: relative entries must resolve against the
    // playlist file, not the process working directory.
    let elsewhere = tmpdir("m3u-cwd");
    let out = run(&["--list", list.to_str().unwrap()], None, Some(&elsewhere));
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 2, "{}", out.stdout);
    assert!(out.stdout.contains("one.mp3"), "{}", out.stdout);
    assert!(out.stdout.contains("two.mp3"), "{}", out.stdout);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&elsewhere).ok();
}

#[test]
fn a_missing_m3u_entry_is_skipped_with_a_warning() {
    let dir = tmpdir("m3u-missing");
    touch(&dir, "one.mp3");
    let list = dir.join("drill.m3u");
    std::fs::write(&list, b"one.mp3\ngone.mp3\n").unwrap();
    let out = run(&["--list", list.to_str().unwrap()], None, None);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 1);
    assert!(out.stderr.contains("gone.mp3"), "{}", out.stderr);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn contradictory_flags_are_rejected_before_any_backend_is_opened() {
    let dir = tmpdir("flags");
    let file = touch(&dir, "lesson.mp3");
    for args in [
        vec!["--ab-b", "0:10", file.to_str().unwrap()],
        vec!["--ab-a", "0:20", "--ab-b", "0:10", file.to_str().unwrap()],
        vec!["--speed", "99", file.to_str().unwrap()],
        vec!["--play", "--list", file.to_str().unwrap()],
        vec!["--json", file.to_str().unwrap()],
    ] {
        let out = run(&args, None, None);
        assert_eq!(
            out.code,
            Some(1),
            "expected a usage error for {args:?}, got {} / {}",
            out.stdout,
            out.stderr
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_backend_that_cannot_be_found_is_a_usage_error_naming_it() {
    // `native` is never silently substituted, even when other backends exist.
    let dir = tmpdir("backend");
    let file = touch(&dir, "lesson.mp3");
    let out = run(
        &["--backend", "native", "--play", file.to_str().unwrap()],
        Some("quit\n"),
        None,
    );
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("native"), "{}", out.stderr);
    std::fs::remove_dir_all(&dir).ok();
}
