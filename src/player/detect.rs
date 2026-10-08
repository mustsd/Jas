//! Backend discovery and auto-ordering.
//!
//! `auto` probes in this order and takes the first hit:
//!
//! 1. `mpv`    -- full live control over JSON IPC. Preferred.
//! 2. `ffplay` -- respawn control.
//! 3. OS-native opener -- open and forget.
//!
//! This deliberately puts `mpv` ahead of `ffplay`, a deviation from the initial
//! survey answer of "ffplay first, then mpv" (PLANS.md section 4.3): `ffplay`
//! has no control channel in `-nodisp` mode, so with it as the primary backend
//! every pause costs an audible process restart. The fallback chain still
//! contains `ffplay`, so the "works with just ffmpeg" story survives, and
//! `--backend` overrides the choice. An explicitly requested but missing backend
//! is an error, not a silent fallback.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::player::{ffplay::FfplayPlayer, Capabilities, Player};

#[cfg(unix)]
use crate::player::mpv::MpvPlayer;

/// A backend that was found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: &'static str,
    pub program: PathBuf,
    pub version: String,
    pub capabilities: Capabilities,
    pub notes: String,
}

/// What `--backend` / `config.json` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preference {
    Auto,
    Named(String),
}

impl Preference {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Preference::Auto),
            "mpv" | "ffplay" | "native" => Ok(Preference::Named(raw.trim().to_ascii_lowercase())),
            other => Err(Error::usage(format!(
                "unknown backend `{other}` (expected auto, mpv, ffplay, or native)"
            ))),
        }
    }
}

/// Look for a program on `PATH` ourselves rather than shelling out to `which`,
/// so a missing program is never an error path of its own.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_string())
            .split(';')
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if cfg!(windows) {
            for ext in &exts {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        } else {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `--version` output, first line, for `jas doctor`. Never fatal.
fn version_of(program: &Path) -> String {
    let output = Command::new(program).arg("--version").output();
    match output {
        Ok(out) => {
            let text = if out.stdout.is_empty() {
                out.stderr
            } else {
                out.stdout
            };
            let line = String::from_utf8_lossy(&text)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if line.is_empty() {
                "version unknown".to_string()
            } else {
                line
            }
        }
        Err(_) => "version unknown".to_string(),
    }
}

/// Probe everything found, in preference order.
pub fn probe() -> Vec<Found> {
    let mut found = Vec::new();

    // mpv: only usable where we can speak its IPC. On Windows the named-pipe
    // client is not implemented, so mpv is reported but not offered.
    if let Some(program) = find_program("mpv") {
        #[cfg(unix)]
        found.push(Found {
            name: "mpv",
            program: program.clone(),
            version: version_of(&program),
            capabilities: Capabilities::LIVE,
            notes: "live control over JSON IPC".to_string(),
        });
        #[cfg(not(unix))]
        found.push(Found {
            name: "mpv",
            program: program.clone(),
            version: version_of(&program),
            capabilities: Capabilities::EMULATED,
            notes: "found, but its IPC client is not implemented on this platform yet".to_string(),
        });
    }

    if let Some(program) = find_program("ffplay") {
        found.push(Found {
            name: "ffplay",
            program: program.clone(),
            version: version_of(&program),
            capabilities: Capabilities::EMULATED,
            notes: "respawn control; +/-150 ms A-B accuracy".to_string(),
        });
    }

    if let Some(program) = os_opener() {
        found.push(Found {
            name: "os",
            program: program.clone(),
            version: "n/a".to_string(),
            capabilities: Capabilities::EMULATED,
            notes: "open and forget; `quit` may not stop audio".to_string(),
        });
    }

    found
}

/// The OS-native opener, last in the chain because it cannot be controlled.
pub fn os_opener() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        find_program("afplay").or_else(|| find_program("open"))
    }
    #[cfg(windows)]
    {
        find_program("powershell")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        find_program("ffplay").or_else(|| find_program("aplay"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Resolve a preference into a backend, or explain what to install.
pub fn open(preference: &Preference) -> Result<Box<dyn Player>> {
    let found = probe();
    match preference {
        Preference::Auto => {
            for candidate in &found {
                if let Some(player) = instantiate(candidate) {
                    return Ok(player);
                }
            }
            Err(Error::no_input(no_backend_message(&found)))
        }
        Preference::Named(name) => {
            if name == "native" {
                return Err(Error::usage(
                    "the `native` backend is not built into this binary (it is an optional feature and is not part of v1)",
                ));
            }
            if name == "os" {
                return Err(Error::usage(
                    "`os` is only used as the last step of `--backend auto`; it cannot be selected",
                ));
            }
            let candidate = found
                .iter()
                .find(|f| f.name == name.as_str())
                .ok_or_else(|| {
                    Error::usage(format!(
                        "backend `{name}` was requested but not found on PATH{}",
                        install_hint(name)
                    ))
                })?;
            instantiate(candidate).ok_or_else(|| {
                Error::usage(format!(
                    "backend `{name}` is present but unusable here: {}",
                    candidate.notes
                ))
            })
        }
    }
}

fn instantiate(candidate: &Found) -> Option<Box<dyn Player>> {
    match candidate.name {
        #[cfg(unix)]
        "mpv" => MpvPlayer::new(candidate.program.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn Player>),
        "ffplay" => Some(Box::new(FfplayPlayer::new(candidate.program.clone()))),
        _ => None,
    }
}

fn install_hint(name: &str) -> String {
    match name {
        "mpv" => ". Install it with `brew install mpv` (macOS), `apt install mpv` (Debian), or from mpv.io.".to_string(),
        "ffplay" => ". It ships with ffmpeg: `brew install ffmpeg` or `apt install ffmpeg`.".to_string(),
        _ => String::new(),
    }
}

fn no_backend_message(found: &[Found]) -> String {
    let mut msg = String::from(
        "no audio backend found. Jas plays through an external player. Install one of:\n\
         \x20 mpv    - best experience (live pause/seek/speed): https://mpv.io\n\
         \x20 ffplay - ships with ffmpeg: https://ffmpeg.org",
    );
    if !found.is_empty() {
        msg.push_str("\nFound but unusable:");
        for f in found {
            msg.push_str(&format!(
                "\n  {} ({}): {}",
                f.name,
                f.program.display(),
                f.notes
            ));
        }
    }
    msg
}

/// The text of `jas doctor`.
pub fn doctor_report(preference: &Preference) -> String {
    let found = probe();
    let mut out = String::from("backends found:\n");
    if found.is_empty() {
        out.push_str("  (none)\n");
    }
    for f in &found {
        out.push_str(&format!(
            "  {:<7} {}\n          version: {}\n          capabilities: {}\n          {}\n",
            f.name,
            f.program.display(),
            f.version,
            f.capabilities.summary(),
            f.notes
        ));
    }
    match open(preference) {
        Ok(player) => out.push_str(&format!(
            "\nresolved: {} ({})\n",
            player.name(),
            player.capabilities().summary()
        )),
        Err(e) => out.push_str(&format!("\nresolved: none -- {e}\n")),
    }
    if found.iter().all(|f| f.name != "ffplay") {
        out.push_str("\nhint: install ffmpeg for the ffplay fallback.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Box<dyn Player>` is not `Debug`, so `unwrap_err` is unavailable here.
    fn expect_err(result: Result<Box<dyn Player>>) -> Error {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
    }

    #[test]
    fn preference_parsing_accepts_the_documented_values() {
        assert_eq!(Preference::parse("auto").unwrap(), Preference::Auto);
        assert_eq!(
            Preference::parse("MPV").unwrap(),
            Preference::Named("mpv".into())
        );
        assert_eq!(
            Preference::parse(" ffplay ").unwrap(),
            Preference::Named("ffplay".into())
        );
        assert_eq!(
            Preference::parse("native").unwrap(),
            Preference::Named("native".into())
        );
        let err = Preference::parse("vlc").unwrap_err();
        assert_eq!(err.exit_code(), crate::error::EXIT_USAGE);
        assert!(err.message().contains("vlc"));
    }

    #[test]
    fn finding_a_program_that_cannot_exist_returns_none() {
        assert_eq!(
            find_program("jas-definitely-not-a-real-program-xyzzy"),
            None
        );
    }

    #[test]
    fn finding_a_program_that_exists_returns_a_path() {
        // `PATH` always has a shell or a core utility on every supported platform.
        #[cfg(unix)]
        let name = "sh";
        #[cfg(windows)]
        let name = "cmd";
        let path = find_program(name).expect("a basic shell should be on PATH");
        assert!(path.is_absolute(), "{}", path.display());
    }

    #[test]
    fn an_explicitly_named_missing_backend_is_an_error_not_a_fallback() {
        // `native` is never silently replaced by something else.
        let err = expect_err(open(&Preference::Named("native".into())));
        assert_eq!(err.exit_code(), crate::error::EXIT_USAGE);
        assert!(err.message().contains("native"));
    }

    #[test]
    fn the_os_opener_cannot_be_selected_directly() {
        let err = expect_err(open(&Preference::Named("os".into())));
        assert!(err.message().contains("auto"));
    }

    #[test]
    fn a_missing_named_backend_explains_how_to_install_it() {
        let err = match open(&Preference::Named("mpv".into())) {
            Ok(_) => return, // mpv is installed here; nothing to assert
            Err(e) => e,
        };
        assert_eq!(err.exit_code(), crate::error::EXIT_USAGE);
        assert!(err.message().contains("mpv"));
        assert!(
            err.message().contains("mpv.io") || err.message().contains("install"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn the_no_backend_message_names_both_alternatives() {
        let msg = no_backend_message(&[]);
        assert!(msg.contains("mpv"));
        assert!(msg.contains("ffplay"));
        assert!(msg.contains("ffmpeg"));
    }

    #[test]
    fn probing_is_never_fatal_and_reports_capabilities() {
        let found = probe();
        for f in &found {
            assert!(!f.program.as_os_str().is_empty());
            assert!(!f.notes.is_empty());
            if f.name == "mpv" {
                #[cfg(unix)]
                assert_eq!(f.capabilities, Capabilities::LIVE);
            }
            if f.name == "ffplay" {
                assert_eq!(f.capabilities, Capabilities::EMULATED);
            }
        }
    }

    #[test]
    fn doctor_output_is_readable_even_with_no_backends() {
        let report = doctor_report(&Preference::Auto);
        assert!(report.contains("backends found"));
        assert!(report.contains("resolved:"));
    }
}
