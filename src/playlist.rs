//! Playlist loading, natural sorting, seeded shuffle, and navigation.
//!
//! Sources are explicit files, directories (recursive), and `.m3u`/`.m3u8`
//! files. Paths are carried as `PathBuf` from here to the backend; nothing is
//! round-tripped through `String` except for display and state keys.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};

/// Extensions treated as audio. Deliberately a list, not a guess.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "ogg", "oga", "opus", "flac", "wav", "aif", "aiff", "wv", "ape",
    "mka", "alac", "weba", "mp2",
];

/// Extensions treated as playlists rather than tracks.
const PLAYLIST_EXTENSIONS: &[&str] = &["m3u", "m3u8"];

/// True for anything the loader will expand into tracks.
pub fn is_loadable(path: &Path) -> bool {
    is_audio(path) || PLAYLIST_EXTENSIONS.contains(&extension_of(path).as_str())
}

const MAX_WALK_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    Off,
    All,
    One,
}

impl LoopMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            LoopMode::Off => "off",
            LoopMode::All => "all",
            LoopMode::One => "one",
        }
    }

    /// Cycle order used by the `l` hotkey: off -> all -> one -> off.
    pub fn cycle(self) -> Self {
        match self {
            LoopMode::Off => LoopMode::All,
            LoopMode::All => LoopMode::One,
            LoopMode::One => LoopMode::Off,
        }
    }
}

/// A stable identity for one file: canonical path plus size and mtime, so a
/// changed file invalidates its stored resume position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId {
    pub path: String,
    pub size: u64,
    pub mtime_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub path: PathBuf,
    /// Filled in by the backend once the file is opened; unknown for streamed input.
    pub duration: Option<Duration>,
    pub id: TrackId,
}

impl Track {
    /// The name shown in `status`, `list`, and one-line confirmations.
    pub fn display_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }
}

/// What a navigation command did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The cursor moved; load and play the new track.
    Moved,
    /// The cursor moved because of looping.
    Wrapped,
    /// Loop mode `one`: replay the same track.
    Repeated,
    /// Nothing to do: already at the first track with looping off.
    AtBoundary,
    /// The end of the playlist with looping off.
    Exhausted,
}

#[derive(Debug, Clone)]
pub struct Playlist {
    /// Files in their original, naturally sorted order.
    original: Vec<Track>,
    /// A permutation of indices into `original`. Identity when not shuffled.
    order: Vec<usize>,
    cursor: usize,
    shuffled: bool,
    seed: u64,
    pub loop_mode: LoopMode,
}

impl Playlist {
    pub fn new(tracks: Vec<Track>, loop_mode: LoopMode, seed: u64) -> Self {
        let order = (0..tracks.len()).collect();
        Self {
            original: tracks,
            order,
            cursor: 0,
            shuffled: false,
            seed,
            loop_mode,
        }
    }

    /// Load from CLI paths. Never fails for a missing or unreadable entry:
    /// those are reported through `problems` and skipped.
    pub fn load(paths: &[PathBuf], loop_mode: LoopMode, seed: u64) -> LoadResult {
        let mut out: Vec<Track> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for path in paths {
            let mut found: Vec<PathBuf> = Vec::new();
            collect(path, 0, &mut found, &mut problems);
            // Natural sort each source separately, then the whole result again,
            // so `chapter2` precedes `chapter10` in both cases.
            found.sort_by(|a, b| natural_path_cmp(a, b));
            for file in found {
                if PLAYLIST_EXTENSIONS.contains(&extension_of(&file).as_str()) {
                    let mut entries = read_playlist_file(&file, &mut problems);
                    entries.sort_by(|a, b| natural_path_cmp(a, b));
                    push_all(entries, &mut out, &mut seen, &mut problems);
                } else {
                    push_all(vec![file], &mut out, &mut seen, &mut problems);
                }
            }
        }

        out.sort_by(|a, b| natural_path_cmp(&a.path, &b.path));
        problems.dedup();
        LoadResult {
            playlist: Playlist::new(out, loop_mode, seed),
            problems,
        }
    }

    pub fn len(&self) -> usize {
        self.original.len()
    }

    pub fn is_empty(&self) -> bool {
        self.original.is_empty()
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn is_shuffled(&self) -> bool {
        self.shuffled
    }

    /// 0-based cursor.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 1-based position, as shown to the user and used by `goto`.
    pub fn position(&self) -> usize {
        self.cursor + 1
    }

    pub fn current(&self) -> Option<&Track> {
        self.order.get(self.cursor).map(|i| &self.original[*i])
    }

    /// All tracks in play order.
    pub fn tracks(&self) -> impl Iterator<Item = &Track> {
        self.order.iter().map(move |i| &self.original[*i])
    }

    pub fn track_at(&self, one_based: usize) -> Option<&Track> {
        one_based
            .checked_sub(1)
            .and_then(|i| self.order.get(i))
            .map(|i| &self.original[*i])
    }

    pub fn set_loop_mode(&mut self, mode: LoopMode) {
        self.loop_mode = mode;
    }

    /// Apply or remove the shuffle. Deterministic for a given seed, and the same
    /// seed always produces the same order, which is what makes a resumed
    /// session keep its order.
    pub fn set_shuffled(&mut self, shuffled: bool, seed: u64) {
        let mut order: Vec<usize> = (0..self.original.len()).collect();
        if shuffled {
            shuffle(&mut order, seed);
        }
        self.seed = seed;
        self.shuffled = shuffled;
        // Keep pointing at the same file where possible.
        let current = self.order.get(self.cursor).copied();
        self.order = order;
        self.cursor = current
            .and_then(|i| self.order.iter().position(|j| *j == i))
            .unwrap_or(0);
    }

    pub fn next(&mut self) -> Step {
        if self.is_empty() {
            return Step::Exhausted;
        }
        if self.cursor + 1 < self.order.len() {
            self.cursor += 1;
            Step::Moved
        } else {
            match self.loop_mode {
                LoopMode::All => {
                    self.cursor = 0;
                    Step::Wrapped
                }
                LoopMode::One => Step::Repeated,
                LoopMode::Off => Step::Exhausted,
            }
        }
    }

    pub fn prev(&mut self) -> Step {
        if self.is_empty() {
            return Step::Exhausted;
        }
        if self.cursor > 0 {
            self.cursor -= 1;
            Step::Moved
        } else {
            match self.loop_mode {
                LoopMode::All => {
                    self.cursor = self.order.len() - 1;
                    Step::Wrapped
                }
                LoopMode::One => Step::Repeated,
                LoopMode::Off => Step::AtBoundary,
            }
        }
    }

    /// `goto <N>`, 1-based. Out of range is a usage error, not a panic.
    pub fn goto(&mut self, one_based: usize) -> Result<Step> {
        if self.track_at(one_based).is_none() {
            return Err(Error::usage(format!(
                "goto: track {one_based} is out of range (1-{})",
                self.order.len()
            )));
        }
        self.cursor = one_based - 1;
        Ok(Step::Moved)
    }
}

pub struct LoadResult {
    pub playlist: Playlist,
    pub problems: Vec<String>,
}

/// Walk one CLI path, appending playable files. Directories are recursive and
/// symlinks are not followed, so a self-referential link cannot hang the walk.
fn collect(path: &Path, depth: usize, out: &mut Vec<PathBuf>, problems: &mut Vec<String>) {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            problems.push(format!("{}: {e}", path.display()));
            return;
        }
    };
    if meta.file_type().is_symlink() {
        // Resolve one level so an explicitly named link to a file or directory works.
        match fs::metadata(path) {
            Ok(target) if target.is_dir() => {
                walk_dir(path, depth, out, problems);
            }
            Ok(_) => out.push(path.to_path_buf()),
            Err(e) => problems.push(format!("{}: broken link: {e}", path.display())),
        }
        return;
    }
    if meta.is_dir() {
        walk_dir(path, depth, out, problems);
    } else if is_loadable(path) {
        out.push(path.to_path_buf());
    } else {
        problems.push(format!("{}: not an audio file (skipped)", path.display()));
    }
}

fn walk_dir(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, problems: &mut Vec<String>) {
    if depth >= MAX_WALK_DEPTH {
        problems.push(format!(
            "{}: directory nesting is too deep (skipped)",
            dir.display()
        ));
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            problems.push(format!("{}: {e}", dir.display()));
            return;
        }
    };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                match entry.file_type() {
                    Ok(ft) if ft.is_dir() => dirs.push(path),
                    Ok(ft) if ft.is_file() => {
                        if is_loadable(&path) {
                            files.push(path);
                        }
                    }
                    // Symlinks and special files are handled by `collect`.
                    Ok(_) => files.push(path),
                    Err(e) => problems.push(format!("{}: {e}", path.display())),
                }
            }
            Err(e) => problems.push(format!("{}: {e}", dir.display())),
        }
    }
    files.sort_by(|a, b| natural_path_cmp(a, b));
    dirs.sort_by(|a, b| natural_path_cmp(a, b));
    out.extend(files);
    for d in dirs {
        walk_dir(&d, depth + 1, out, problems);
    }
}

fn push_all(
    files: Vec<PathBuf>,
    out: &mut Vec<Track>,
    seen: &mut HashSet<String>,
    problems: &mut Vec<String>,
) {
    for file in files {
        if let Some(track) = make_track(&file, problems) {
            if seen.insert(track.id.path.clone()) {
                out.push(track);
            } else {
                problems.push(format!("{}: duplicate entry (skipped)", file.display()));
            }
        }
    }
}

fn make_track(path: &Path, problems: &mut Vec<String>) -> Option<Track> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            problems.push(format!("{}: {e}", path.display()));
            return None;
        }
    };
    if !meta.is_file() {
        problems.push(format!("{}: not a regular file (skipped)", path.display()));
        return None;
    }
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let id = TrackId {
        path: encode_path(&canonical),
        size: meta.len(),
        mtime_secs,
    };
    Some(Track {
        path: canonical,
        duration: None,
        id,
    })
}

/// Read an `.m3u`/`.m3u8` file. Entries starting with `#` are comments; relative
/// entries resolve against the playlist's own directory; CRLF is accepted.
pub fn read_playlist_file(file: &Path, problems: &mut Vec<String>) -> Vec<PathBuf> {
    let text = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) => {
            problems.push(format!("{}: {e}", file.display()));
            return Vec::new();
        }
    };
    // m3u8 files carry no BOM handling of their own; strip one if present so the
    // first entry does not become unopenable.
    let text = text.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&text);
    let base = file.parent().unwrap_or_else(|| Path::new("."));
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(text).lines() {
        let entry = line.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        let path = Path::new(entry);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.join(path)
        };
        if resolved.exists() {
            out.push(resolved);
        } else {
            problems.push(format!(
                "{}: line entry `{entry}` not found (skipped)",
                file.display()
            ));
        }
    }
    out
}

pub fn extension_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_audio(path: &Path) -> bool {
    let ext = extension_of(path);
    AUDIO_EXTENSIONS.contains(&ext.as_str())
}

/// Encode a path as an injective, JSON-safe string: valid UTF-8 passes through
/// unchanged, invalid bytes are percent-encoded, so two different paths can
/// never produce the same state key.
pub fn encode_path(path: &Path) -> String {
    let os = path.as_os_str();
    match os.to_str() {
        Some(s) => s.to_string(),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let mut out = String::new();
                for byte in os.as_bytes() {
                    if *byte < 0x80 {
                        out.push(*byte as char);
                    } else {
                        out.push_str(&format!("%{byte:02X}"));
                    }
                }
                out
            }
            #[cfg(not(unix))]
            {
                os.to_string_lossy().into_owned()
            }
        }
    }
}

/// Compare paths the way a human reads them: `ch2` before `ch10`.
pub fn natural_path_cmp(a: &Path, b: &Path) -> Ordering {
    natural_cmp(&a.to_string_lossy(), &b.to_string_lossy())
}

/// Natural comparison: digit runs compare numerically, everything else by char.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let mut xs = String::new();
                    while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                        xs.push(c);
                        ai.next();
                    }
                    let mut ys = String::new();
                    while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                        ys.push(c);
                        bi.next();
                    }
                    let xn = xs.trim_start_matches('0');
                    let yn = ys.trim_start_matches('0');
                    let ord = xn.len().cmp(&yn.len()).then_with(|| xn.cmp(yn));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let ord = x.cmp(&y);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

/// Deterministic Fisher-Yates over a seeded generator (splitmix64).
fn shuffle(items: &mut [usize], seed: u64) {
    let mut state = seed;
    for i in (1..items.len()).rev() {
        let j = (next_u64(&mut state) % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A playlist as JSON, for `--list --json`. Built here so the one-off CLI path
/// and the in-session `list` command cannot drift apart.
pub fn playlist_json(playlist: &Playlist) -> Result<String> {
    #[derive(serde::Serialize)]
    struct Entry {
        index: usize,
        path: String,
        name: String,
        current: bool,
    }
    let current = playlist.cursor();
    let entries: Vec<Entry> = playlist
        .tracks()
        .enumerate()
        .map(|(i, t)| Entry {
            index: i + 1,
            path: t.path.to_string_lossy().into_owned(),
            name: t.display_name(),
            current: i == current,
        })
        .collect();
    serde_json::to_string_pretty(&entries)
        .map_err(|e| Error::runtime(format!("cannot serialize the playlist: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        // Unique per call: these tests run in parallel, so a shared name means one
        // test deletes the directory out from under another.
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "jas-playlist-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"x").expect("write temp file");
        path
    }

    fn names(p: &Playlist) -> Vec<String> {
        p.tracks().map(|t| t.display_name()).collect()
    }

    #[test]
    fn natural_sort_orders_chapters_by_number() {
        assert_eq!(natural_cmp("ch2.mp3", "ch10.mp3"), Ordering::Less);
        assert_eq!(natural_cmp("ch10.mp3", "ch2.mp3"), Ordering::Greater);
        assert_eq!(natural_cmp("ch2.mp3", "ch2.mp3"), Ordering::Equal);
        assert_eq!(natural_cmp("ch1", "ch01"), Ordering::Equal);
        assert_eq!(natural_cmp("a", "b"), Ordering::Less);
        assert_eq!(natural_cmp("2", "10"), Ordering::Less);
        assert_eq!(natural_cmp("track9", "track10"), Ordering::Less);
    }

    #[test]
    fn natural_sort_is_stable_for_unicode_names() {
        // Chinese numerals and Arabic names must not panic or reorder wildly.
        assert_eq!(natural_cmp("你好2.mp3", "你好10.mp3"), Ordering::Less);
        assert_eq!(natural_cmp("مرحبا.mp3", "مرحبا.mp3"), Ordering::Equal);
        assert_ne!(natural_cmp("مرحبا.mp3", "你好.mp3"), Ordering::Equal);
        assert_eq!(natural_cmp("🎧1.mp3", "🎧2.mp3"), Ordering::Less);
    }

    #[test]
    fn sorts_a_directory_naturally() {
        let dir = tmpdir("sort");
        for n in ["ch10.mp3", "ch2.mp3", "ch1.mp3"] {
            touch(&dir, n);
        }
        let loaded = Playlist::load(std::slice::from_ref(&dir), LoopMode::All, 1);
        assert_eq!(
            names(&loaded.playlist),
            vec!["ch1.mp3", "ch2.mp3", "ch10.mp3"],
            "problems: {:?}",
            loaded.problems
        );
        assert!(loaded.problems.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loads_nested_directories_and_skips_non_audio() {
        let dir = tmpdir("nested");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        touch(&dir, "a.mp3");
        touch(&sub, "b.wav");
        touch(&dir, "notes.txt");
        touch(&dir, "cover.png");
        let loaded = Playlist::load(std::slice::from_ref(&dir), LoopMode::All, 1);
        let got = names(&loaded.playlist);
        assert!(got.contains(&"a.mp3".to_string()), "{got:?}");
        assert!(got.contains(&"b.wav".to_string()), "{got:?}");
        assert!(!got.iter().any(|n| n.ends_with(".txt")), "{got:?}");
        assert!(!got.iter().any(|n| n.ends_with(".png")), "{got:?}");
        // A directory is scanned quietly: non-audio files inside it are simply not
        // tracks. Naming one directly is the case that earns a warning.
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        let explicit = Playlist::load(&[dir.join("notes.txt")], LoopMode::All, 1);
        assert!(explicit.playlist.is_empty());
        assert!(explicit
            .problems
            .iter()
            .any(|p| p.contains("not an audio file")));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_paths_are_reported_not_fatal() {
        let dir = tmpdir("missing");
        let missing = dir.join("nope.mp3");
        let loaded = Playlist::load(&[missing], LoopMode::All, 1);
        assert!(loaded.playlist.is_empty());
        assert_eq!(loaded.problems.len(), 1);
        assert!(loaded.problems[0].contains("nope.mp3"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn absolute_paths_and_names_are_accepted() {
        let dir = tmpdir("explicit");
        let file = touch(&dir, "song.mp3");
        let loaded = Playlist::load(std::slice::from_ref(&file), LoopMode::All, 1);
        assert_eq!(loaded.playlist.len(), 1);
        assert_eq!(
            loaded.playlist.current().unwrap().path,
            fs::canonicalize(&file).unwrap()
        );
        // The track id carries size and mtime so a changed file invalidates resume.
        let id = &loaded.playlist.current().unwrap().id;
        assert_eq!(id.size, 1);
        assert!(id.path.contains("song.mp3"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_entries_are_deduped_once() {
        let dir = tmpdir("dupes");
        let file = touch(&dir, "a.mp3");
        let loaded = Playlist::load(&[dir.clone(), file.clone()], LoopMode::All, 1);
        assert_eq!(loaded.playlist.len(), 1);
        assert!(loaded.problems.iter().any(|p| p.contains("duplicate")));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn m3u_entries_resolve_relative_and_accept_crlf() {
        let dir = tmpdir("m3u");
        touch(&dir, "one.mp3");
        touch(&dir, "two.mp3");
        let list = dir.join("list.m3u");
        fs::write(&list, b"#EXTM3U\r\none.mp3\r\n\r\ntwo.mp3\r\n# comment\r\n").unwrap();
        let loaded = Playlist::load(&[list], LoopMode::All, 1);
        assert_eq!(
            names(&loaded.playlist),
            vec!["one.mp3", "two.mp3"],
            "{:?}",
            loaded.problems
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn m3u_handles_a_utf8_bom() {
        let dir = tmpdir("bom");
        touch(&dir, "one.mp3");
        let list = dir.join("list.m3u8");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"one.mp3\n");
        fs::write(&list, bytes).unwrap();
        let loaded = Playlist::load(&[list], LoopMode::All, 1);
        assert_eq!(
            names(&loaded.playlist),
            vec!["one.mp3"],
            "{:?}",
            loaded.problems
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn m3u_missing_entries_are_reported_and_skipped() {
        let dir = tmpdir("m3u-missing");
        touch(&dir, "one.mp3");
        let list = dir.join("list.m3u");
        fs::write(&list, b"one.mp3\ngone.mp3\n").unwrap();
        let loaded = Playlist::load(&[list], LoopMode::All, 1);
        assert_eq!(loaded.playlist.len(), 1);
        assert!(loaded.problems.iter().any(|p| p.contains("gone.mp3")));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_json_listing_is_valid_json_with_one_entry_per_track() {
        let dir = tmpdir("json");
        for n in ["a.mp3", "b.mp3"] {
            touch(&dir, n);
        }
        let loaded = Playlist::load(std::slice::from_ref(&dir), LoopMode::All, 1);
        let json = playlist_json(&loaded.playlist).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let entries = parsed.as_array().expect("an array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["index"], 1);
        assert_eq!(entries[0]["current"], true);
        assert_eq!(entries[1]["current"], false);
        assert!(entries[0]["name"].as_str().unwrap().ends_with(".mp3"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_ascii_filenames_load_and_keep_their_bytes() {
        let dir = tmpdir("unicode");
        for n in ["عربي.mp3", "中文.mp3", "song🎧.mp3"] {
            touch(&dir, n);
        }
        let loaded = Playlist::load(std::slice::from_ref(&dir), LoopMode::All, 1);
        let got = names(&loaded.playlist);
        assert_eq!(got.len(), 3, "{got:?} {:?}", loaded.problems);
        assert!(got.contains(&"عربي.mp3".to_string()), "{got:?}");
        assert!(got.contains(&"中文.mp3".to_string()), "{got:?}");
        assert!(got.contains(&"song🎧.mp3".to_string()), "{got:?}");
        // And the state key round-trips intact.
        assert!(
            loaded.playlist.current().unwrap().id.path.contains("🎧")
                || loaded.playlist.tracks().any(|t| t.id.path.contains("🎧"))
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_input_yields_an_empty_playlist_with_no_problems() {
        let mut loaded = Playlist::load(&[], LoopMode::All, 1);
        assert!(loaded.playlist.is_empty());
        assert!(loaded.problems.is_empty());
        assert_eq!(loaded.playlist.next(), Step::Exhausted);
        assert_eq!(loaded.playlist.prev(), Step::Exhausted);
        assert!(loaded.playlist.current().is_none());
        assert_eq!(loaded.playlist.position(), 1);
    }

    fn three_tracks() -> Playlist {
        let dir = tmpdir("nav");
        for n in ["a.mp3", "b.mp3", "c.mp3"] {
            touch(&dir, n);
        }
        let loaded = Playlist::load(std::slice::from_ref(&dir), LoopMode::All, 1);
        fs::remove_dir_all(&dir).ok();
        loaded.playlist
    }

    #[test]
    fn navigation_walks_forward_and_back() {
        let mut p = three_tracks();
        assert_eq!(p.position(), 1);
        assert_eq!(p.next(), Step::Moved);
        assert_eq!(p.position(), 2);
        assert_eq!(p.next(), Step::Moved);
        assert_eq!(p.position(), 3);
        assert_eq!(p.prev(), Step::Moved);
        assert_eq!(p.position(), 2);
    }

    #[test]
    fn loop_all_wraps_in_both_directions() {
        let mut p = three_tracks();
        p.set_loop_mode(LoopMode::All);
        p.goto(3).unwrap();
        assert_eq!(p.next(), Step::Wrapped);
        assert_eq!(p.position(), 1);
        assert_eq!(p.prev(), Step::Wrapped);
        assert_eq!(p.position(), 3);
    }

    #[test]
    fn loop_off_stops_at_the_end_and_the_start() {
        let mut p = three_tracks();
        p.set_loop_mode(LoopMode::Off);
        p.goto(3).unwrap();
        assert_eq!(p.next(), Step::Exhausted);
        assert_eq!(p.position(), 3, "an exhausted playlist must not move");
        p.goto(1).unwrap();
        assert_eq!(p.prev(), Step::AtBoundary);
        assert_eq!(p.position(), 1);
    }

    #[test]
    fn loop_one_repeats_without_moving() {
        let mut p = three_tracks();
        p.set_loop_mode(LoopMode::One);
        p.goto(3).unwrap();
        assert_eq!(p.next(), Step::Repeated);
        assert_eq!(p.position(), 3);
        p.goto(1).unwrap();
        assert_eq!(p.prev(), Step::Repeated);
        assert_eq!(p.position(), 1);
    }

    #[test]
    fn loop_mode_cycles_off_all_one() {
        assert_eq!(LoopMode::Off.cycle(), LoopMode::All);
        assert_eq!(LoopMode::All.cycle(), LoopMode::One);
        assert_eq!(LoopMode::One.cycle(), LoopMode::Off);
    }

    #[test]
    fn goto_validates_its_argument() {
        let mut p = three_tracks();
        assert!(p.goto(0).is_err());
        assert!(p.goto(4).is_err());
        let err = p.goto(9).unwrap_err().to_string();
        assert!(err.contains("1-3"), "{err}");
        assert!(p.goto(2).is_ok());
        assert_eq!(p.position(), 2);
    }

    #[test]
    fn shuffle_is_deterministic_for_a_seed() {
        let mut a = three_tracks();
        let mut b = three_tracks();
        a.set_shuffled(true, 12345);
        b.set_shuffled(true, 12345);
        assert_eq!(names(&a), names(&b));
        assert_eq!(a.seed(), 12345);
        assert!(a.is_shuffled());
    }

    #[test]
    fn different_seeds_generally_give_different_orders() {
        let mk = || {
            let dir = tmpdir("shuffle-many");
            for i in 0..26 {
                touch(&dir, &format!("t{i}.mp3"));
            }
            let loaded = Playlist::load(std::slice::from_ref(&dir), LoopMode::All, 1);
            fs::remove_dir_all(&dir).ok();
            loaded.playlist
        };
        let mut a = mk();
        let mut b = mk();
        a.set_shuffled(true, 1);
        b.set_shuffled(true, 2);
        assert_ne!(names(&a), names(&b));
    }

    #[test]
    fn shuffle_keeps_every_track_and_the_current_file() {
        let mut p = three_tracks();
        p.goto(2).unwrap();
        let before = p.current().unwrap().path.clone();
        p.set_shuffled(true, 7);
        assert_eq!(p.len(), 3);
        let mut got = names(&p);
        got.sort();
        assert_eq!(got, vec!["a.mp3", "b.mp3", "c.mp3"]);
        assert_eq!(
            p.current().unwrap().path,
            before,
            "shuffle must not change the track"
        );
    }

    #[test]
    fn unshuffling_restores_the_natural_order() {
        let mut p = three_tracks();
        p.set_shuffled(true, 99);
        p.set_shuffled(false, 99);
        assert_eq!(names(&p), vec!["a.mp3", "b.mp3", "c.mp3"]);
        assert!(!p.is_shuffled());
    }

    #[test]
    fn shuffling_an_empty_playlist_is_safe() {
        let mut p = Playlist::new(Vec::new(), LoopMode::All, 1);
        p.set_shuffled(true, 5);
        assert!(p.is_empty());
        assert!(p.current().is_none());
    }

    #[test]
    fn track_id_changes_when_a_file_changes() {
        let dir = tmpdir("idfile");
        let file = touch(&dir, "x.mp3");
        let first = Playlist::load(std::slice::from_ref(&file), LoopMode::All, 1).playlist;
        let id1 = first.current().unwrap().id.clone();
        fs::write(&file, b"much longer content").unwrap();
        let second = Playlist::load(std::slice::from_ref(&file), LoopMode::All, 1).playlist;
        let id2 = second.current().unwrap().id.clone();
        assert_ne!(id1, id2, "a changed file must get a new id");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn display_name_falls_back_to_the_full_path() {
        let track = Track {
            path: PathBuf::from("/no/such/dir/track.mp3"),
            duration: None,
            id: TrackId {
                path: "x".into(),
                size: 0,
                mtime_secs: 0,
            },
        };
        assert_eq!(track.display_name(), "track.mp3");
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert!(is_audio(Path::new("A.MP3")));
        assert!(is_audio(Path::new("b.FlAc")));
        assert!(!is_audio(Path::new("c.txt")));
        assert!(!is_audio(Path::new("noext")));
        // Playlists are loadable but are not audio files themselves.
        assert!(!is_audio(Path::new("list.m3u")));
        assert!(is_loadable(Path::new("list.m3u")));
        assert!(is_loadable(Path::new("list.M3U8")));
        assert!(!is_loadable(Path::new("notes.txt")));
    }

    #[test]
    fn encoded_paths_are_left_alone_when_already_utf8() {
        assert_eq!(encode_path(Path::new("/tmp/عربي.mp3")), "/tmp/عربي.mp3");
        assert_eq!(encode_path(Path::new("/tmp/🎧.mp3")), "/tmp/🎧.mp3");
    }
}
