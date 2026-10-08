//! Config and resume state, with atomic writes.
//!
//! Two files live in the OS config directory (or wherever `--config` points):
//!
//! - `config.json`  -- backend preference, defaults, keymap overrides
//! - `state.json`   -- playlist, index, seed, and per-track resume entries
//!
//! A corrupt state file is moved aside with a warning, never fatal. Writes are
//! atomic: write a temp file in the same directory, `fsync`, then rename.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::playlist::{LoopMode, TrackId};
use crate::transport::{clamp_speed, DEFAULT_SPEED};

/// Per-track memory: resume position, A-B marks, speed, and repeat count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TrackState {
    /// Resume position in milliseconds.
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub ab_a_ms: Option<u64>,
    #[serde(default)]
    pub ab_b_ms: Option<u64>,
    #[serde(default)]
    pub speed: Option<f64>,
    #[serde(default)]
    pub repeats: Option<u32>,
    #[serde(default)]
    pub gap_ms: Option<u64>,
}

impl TrackState {
    pub fn position(&self) -> Duration {
        Duration::from_millis(self.position_ms)
    }

    pub fn ab(&self) -> Option<(Duration, Option<Duration>)> {
        self.ab_a_ms.map(|a| {
            (
                Duration::from_millis(a),
                self.ab_b_ms.map(Duration::from_millis),
            )
        })
    }

    pub fn speed(&self) -> f64 {
        self.speed.map(clamp_speed).unwrap_or(DEFAULT_SPEED)
    }

    pub fn gap(&self) -> Duration {
        Duration::from_millis(self.gap_ms.unwrap_or(0))
    }
}

/// `state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub version: u32,
    /// Paths of the last playlist, in the order they were loaded.
    #[serde(default)]
    pub playlist: Vec<String>,
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub seed: u64,
    #[serde(default)]
    pub shuffled: bool,
    #[serde(default)]
    pub loop_mode: Option<String>,
    /// Per-track entries, keyed by the encoded id from `playlist::encode_path`
    /// plus size and mtime.
    #[serde(default)]
    pub tracks: BTreeMap<String, TrackState>,
}

/// The key under which a track's state is stored.
pub fn track_key(id: &TrackId) -> String {
    format!("{}|{}|{}", id.path, id.size, id.mtime_secs)
}

impl State {
    pub const VERSION: u32 = 1;

    pub fn new() -> Self {
        Self {
            version: Self::VERSION,
            ..Self::default()
        }
    }

    pub fn get(&self, id: &TrackId) -> Option<&TrackState> {
        self.tracks.get(&track_key(id))
    }

    pub fn set(&mut self, id: &TrackId, entry: TrackState) {
        self.tracks.insert(track_key(id), entry);
    }

    /// Forget entries whose files are gone, so the file cannot grow forever.
    pub fn prune(&mut self, known: &[TrackId]) {
        let live: std::collections::HashSet<String> = known.iter().map(track_key).collect();
        self.tracks.retain(|k, _| live.contains(k));
    }
}

/// `config.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub speed: Option<f64>,
    #[serde(default)]
    pub loop_mode: Option<String>,
    #[serde(default)]
    pub keys: Option<String>,
    #[serde(default)]
    pub status_line: Option<bool>,
    /// chord -> command text. Parsed by `keys::build`; bad entries are reported
    /// at startup and ignored.
    #[serde(default)]
    pub keymap: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            backend: None,
            speed: None,
            loop_mode: None,
            keys: None,
            status_line: None,
            keymap: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn loop_mode(&self) -> Option<LoopMode> {
        match self.loop_mode.as_deref() {
            Some("off") => Some(LoopMode::Off),
            Some("all") => Some(LoopMode::All),
            Some("one") => Some(LoopMode::One),
            _ => None,
        }
    }

    pub fn speed(&self) -> Option<f64> {
        self.speed.map(clamp_speed)
    }
}

/// Where the two files live. `--config <PATH>` overrides the directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    enabled: bool,
}

impl Store {
    /// The default location: the OS config dir via `directories`, falling back to
    /// a dot-directory in `$HOME` when the platform has no config dir.
    pub fn default_location() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "jas")
            .map(|d| d.config_dir().to_path_buf())
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".jas")))
    }

    /// Build a store. `override_path` is used as the directory when given, so a
    /// single `--config /tmp/jas` keeps everything in `/tmp/jas`.
    pub fn new(override_path: Option<PathBuf>) -> Self {
        let dir = override_path
            .or_else(Self::default_location)
            .unwrap_or_else(|| PathBuf::from("."));
        Self { dir, enabled: true }
    }

    pub fn disabled(override_path: Option<PathBuf>) -> Self {
        let mut store = Self::new(override_path);
        store.enabled = false;
        store
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    /// Load `state.json`. A missing file is an empty state; a corrupt file is
    /// moved aside as `state.json.corrupt-<n>` and reported, never fatal.
    pub fn load_state(&self) -> (State, Vec<String>) {
        if !self.enabled {
            return (State::new(), Vec::new());
        }
        let path = self.state_path();
        let mut notes = Vec::new();
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (State::new(), notes),
            Err(e) => {
                notes.push(format!("cannot read {}: {e}", path.display()));
                return (State::new(), notes);
            }
        };
        match serde_json::from_slice::<State>(&bytes) {
            Ok(state) => (state, notes),
            Err(e) => {
                let aside = self.quarantine_path("state.json");
                match fs::rename(&path, &aside) {
                    Ok(()) => notes.push(format!(
                        "{} is corrupt ({e}); moved it to {}",
                        path.display(),
                        aside.display()
                    )),
                    Err(rename_err) => notes.push(format!(
                        "{} is corrupt ({e}) and could not be moved aside ({rename_err}); ignoring it",
                        path.display()
                    )),
                }
                (State::new(), notes)
            }
        }
    }

    /// Load `config.json`. Same policy as the state file.
    pub fn load_config(&self) -> (Config, Vec<String>) {
        if !self.enabled {
            return (Config::default(), Vec::new());
        }
        let path = self.config_path();
        let mut notes = Vec::new();
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (Config::default(), notes)
            }
            Err(e) => {
                notes.push(format!("cannot read {}: {e}", path.display()));
                return (Config::default(), notes);
            }
        };
        match serde_json::from_slice::<Config>(&bytes) {
            Ok(config) => (config, notes),
            Err(e) => {
                let aside = self.quarantine_path("config.json");
                match fs::rename(&path, &aside) {
                    Ok(()) => notes.push(format!(
                        "{} is corrupt ({e}); moved it to {}",
                        path.display(),
                        aside.display()
                    )),
                    Err(rename_err) => notes.push(format!(
                        "{} is corrupt ({e}) and could not be moved aside ({rename_err}); ignoring it",
                        path.display()
                    )),
                }
                (Config::default(), notes)
            }
        }
    }

    pub fn save_state(&self, state: &State) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.save_json(&self.state_path(), state)
    }

    fn quarantine_path(&self, name: &str) -> PathBuf {
        for n in 0..100 {
            let candidate = self.dir.join(format!("{name}.corrupt-{n}"));
            if !candidate.exists() {
                return candidate;
            }
        }
        self.dir.join(format!("{name}.corrupt"))
    }

    /// Write JSON atomically: temp file in the same directory, fsync, rename.
    fn save_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .map_err(|e| Error::runtime(format!("cannot create {}: {e}", self.dir.display())))?;
        let body = serde_json::to_vec_pretty(value)
            .map_err(|e| Error::runtime(format!("cannot serialize {}: {e}", path.display())))?;
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        {
            let mut file = fs::File::create(&tmp)
                .map_err(|e| Error::runtime(format!("cannot write {}: {e}", tmp.display())))?;
            file.write_all(&body)
                .map_err(|e| Error::runtime(format!("cannot write {}: {e}", tmp.display())))?;
            // fsync before rename, so a crash cannot leave a renamed but empty file.
            file.sync_all()
                .map_err(|e| Error::runtime(format!("cannot flush {}: {e}", tmp.display())))?;
        }
        fs::rename(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            Error::runtime(format!("cannot replace {}: {e}", path.display()))
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("jas-state-test-{name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn id(path: &str, size: u64, mtime: u64) -> TrackId {
        TrackId {
            path: path.into(),
            size,
            mtime_secs: mtime,
        }
    }

    #[test]
    fn state_round_trips_through_a_file() {
        let dir = tmpdir("roundtrip");
        let store = Store::new(Some(dir.clone()));
        let mut state = State::new();
        state.playlist = vec!["/a/b.mp3".into()];
        state.index = 2;
        state.seed = 42;
        state.set(
            &id("/a/b.mp3", 10, 20),
            TrackState {
                position_ms: 1500,
                ab_a_ms: Some(1000),
                ab_b_ms: Some(2000),
                speed: Some(0.75),
                repeats: Some(3),
                gap_ms: Some(250),
            },
        );
        store.save_state(&state).unwrap();

        let (loaded, notes) = store.load_state();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(loaded, state);
        let entry = loaded.get(&id("/a/b.mp3", 10, 20)).unwrap();
        assert_eq!(entry.position(), Duration::from_millis(1500));
        assert_eq!(
            entry.ab(),
            Some((Duration::from_secs(1), Some(Duration::from_secs(2))))
        );
        assert_eq!(entry.speed(), 0.75);
        assert_eq!(entry.repeats, Some(3));
        assert_eq!(entry.gap(), Duration::from_millis(250));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_scalar_change_invalidates_the_entry() {
        let dir = tmpdir("invalidate");
        let store = Store::new(Some(dir.clone()));
        let mut state = State::new();
        state.set(
            &id("/a/b.mp3", 10, 20),
            TrackState {
                position_ms: 500,
                ..Default::default()
            },
        );
        store.save_state(&state).unwrap();
        let (loaded, _) = store.load_state();
        assert!(loaded.get(&id("/a/b.mp3", 10, 20)).is_some());
        // Same path, different size: the file changed, so the resume point is void.
        assert!(loaded.get(&id("/a/b.mp3", 11, 20)).is_none());
        assert!(loaded.get(&id("/a/b.mp3", 10, 21)).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_state_file_is_an_empty_state() {
        let dir = tmpdir("missing");
        let store = Store::new(Some(dir.clone()));
        let (state, notes) = store.load_state();
        assert_eq!(state, State::new());
        assert!(notes.is_empty());
        let (config, notes) = store.load_config();
        assert_eq!(config, Config::default());
        assert!(notes.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_state_file_is_moved_aside_not_fatal() {
        let dir = tmpdir("corrupt");
        let store = Store::new(Some(dir.clone()));
        fs::write(store.state_path(), b"{ this is not json").unwrap();
        let (state, notes) = store.load_state();
        assert_eq!(state, State::new(), "a corrupt file must not stop startup");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("corrupt"), "{notes:?}");
        assert!(!store.state_path().exists());
        assert!(dir.join("state.json.corrupt-0").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repeated_corruption_does_not_overwrite_the_first_quarantine() {
        let dir = tmpdir("corrupt-twice");
        let store = Store::new(Some(dir.clone()));
        fs::write(store.state_path(), b"nope").unwrap();
        store.load_state();
        fs::write(store.state_path(), b"nope again").unwrap();
        store.load_state();
        assert!(dir.join("state.json.corrupt-0").exists());
        assert!(dir.join("state.json.corrupt-1").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_config_file_is_moved_aside_not_fatal() {
        let dir = tmpdir("corrupt-config");
        let store = Store::new(Some(dir.clone()));
        fs::write(store.config_path(), b"]not json[").unwrap();
        let (config, notes) = store.load_config();
        assert_eq!(config, Config::default());
        assert!(notes.iter().any(|n| n.contains("corrupt")));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writing_leaves_no_temp_file_behind() {
        let dir = tmpdir("atomic");
        let store = Store::new(Some(dir.clone()));
        store.save_state(&State::new()).unwrap();
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_disabled_store_never_touches_the_disk() {
        let dir = tmpdir("disabled");
        let store = Store::disabled(Some(dir.clone()));
        store.save_state(&State::new()).unwrap();
        assert!(!store.state_path().exists());
        assert!(!store.config_path().exists());
        let (state, notes) = store.load_state();
        assert_eq!(state, State::new());
        assert!(notes.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_store_creates_its_directory_on_demand() {
        let dir = tmpdir("created").join("nested").join("deeper");
        let store = Store::new(Some(dir.clone()));
        store.save_state(&State::new()).unwrap();
        assert!(store.state_path().exists());
        fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn partial_state_files_load_with_defaults() {
        // Forward compatibility: a file from an older build must not fail.
        let dir = tmpdir("partial");
        let store = Store::new(Some(dir.clone()));
        fs::write(store.state_path(), br#"{"playlist":["/x.mp3"]}"#).unwrap();
        let (state, notes) = store.load_state();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(state.playlist, vec!["/x.mp3".to_string()]);
        assert_eq!(state.index, 0);
        assert_eq!(state.seed, 0);
        assert!(state.tracks.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_fields_are_ignored_rather_than_fatal() {
        let dir = tmpdir("unknown-fields");
        let store = Store::new(Some(dir.clone()));
        fs::write(
            store.state_path(),
            br#"{"version":1,"future_field":{"a":1}}"#,
        )
        .unwrap();
        let (_, notes) = store.load_state();
        assert!(notes.is_empty(), "{notes:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_parses_loop_modes_and_speeds() {
        let config = Config {
            loop_mode: Some("one".into()),
            speed: Some(99.0),
            ..Config::default()
        };
        assert_eq!(config.loop_mode(), Some(LoopMode::One));
        assert_eq!(config.speed(), Some(crate::transport::MAX_SPEED));
        let bad = Config {
            loop_mode: Some("sometimes".into()),
            ..Config::default()
        };
        assert_eq!(bad.loop_mode(), None);
    }

    #[test]
    fn track_state_defaults_are_sane() {
        let entry = TrackState::default();
        assert_eq!(entry.position(), Duration::ZERO);
        assert_eq!(entry.ab(), None);
        assert_eq!(entry.speed(), DEFAULT_SPEED);
        assert_eq!(entry.gap(), Duration::ZERO);
    }

    #[test]
    fn a_stored_speed_outside_the_range_is_clamped_on_read() {
        let entry = TrackState {
            speed: Some(100.0),
            ..Default::default()
        };
        assert_eq!(entry.speed(), crate::transport::MAX_SPEED);
        let entry = TrackState {
            speed: Some(0.0),
            ..Default::default()
        };
        assert_eq!(entry.speed(), crate::transport::MIN_SPEED);
    }

    #[test]
    fn pruning_drops_tracks_that_are_no_longer_in_the_playlist() {
        let mut state = State::new();
        let keep = id("/keep.mp3", 1, 1);
        let drop = id("/drop.mp3", 1, 1);
        state.set(
            &keep,
            TrackState {
                position_ms: 10,
                ..Default::default()
            },
        );
        state.set(
            &drop,
            TrackState {
                position_ms: 20,
                ..Default::default()
            },
        );
        state.prune(std::slice::from_ref(&keep));
        assert!(state.get(&keep).is_some());
        assert!(state.get(&drop).is_none());
    }

    #[test]
    fn track_keys_distinguish_same_path_with_different_scalars() {
        assert_ne!(track_key(&id("/a", 1, 1)), track_key(&id("/a", 2, 1)));
        assert_ne!(track_key(&id("/a", 1, 1)), track_key(&id("/a", 1, 2)));
    }

    #[test]
    fn non_ascii_paths_survive_a_state_round_trip() {
        let dir = tmpdir("unicode");
        let store = Store::new(Some(dir.clone()));
        let mut state = State::new();
        for p in ["/موسيقى/درس.mp3", "/音乐/第二课.mp3", "/clips/🎧.mp3"] {
            state.set(
                &id(p, 5, 5),
                TrackState {
                    position_ms: 1234,
                    ..Default::default()
                },
            );
        }
        store.save_state(&state).unwrap();
        let (loaded, notes) = store.load_state();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(loaded.tracks.len(), 3);
        for p in ["/موسيقى/درس.mp3", "/音乐/第二课.mp3", "/clips/🎧.mp3"] {
            assert_eq!(
                loaded.get(&id(p, 5, 5)).map(|e| e.position()),
                Some(Duration::from_millis(1234)),
                "lost state for {p}"
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_config_file_is_read_the_same_way_state_is() {
        let dir = tmpdir("config-read");
        let store = Store::new(Some(dir.clone()));
        // Written by hand, the way a user would: Jas reads config.json, so the
        // test must not rely on a writer that only exists inside Jas.
        fs::write(
            store.config_path(),
            br#"{"backend":"mpv","status_line":true,"keymap":{"z":"toggle"}}"#,
        )
        .unwrap();
        let (loaded, notes) = store.load_config();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(loaded.keymap.get("z").map(String::as_str), Some("toggle"));
        assert_eq!(loaded.backend.as_deref(), Some("mpv"));
        assert_eq!(loaded.status_line, Some(true));
        fs::remove_dir_all(&dir).ok();
    }
}
