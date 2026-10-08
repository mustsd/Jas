# Jas

Jas is a local audio player primarily for language learners on macOS, Linux, and Windows.

- Plays mp3 and other common audio formats.
- Playlist loops by default.
- CLI first: it runs on the command line, and a pipe or a redirect gets line-oriented
  output and nothing else. On a terminal it draws a full-screen interface over the same
  session, which `--no-tui` turns off.

This document is the working plan. It records the decisions that are locked, the
architecture they imply, and the milestones with acceptance criteria. Anything not
listed under "Open questions" is considered settled until someone reopens it.

## 0. Status (what exists today)

Written after the first implementation pass, so it reflects the code, not just the
intent. Everything below is checked by `bash scripts/ainiux/check`, which runs
`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and
an end-to-end smoke script against the built binary.

| Milestone | State |
|---|---|
| M0 skeleton, CLI surface, `--list`, exit codes | done |
| M1 ffplay respawn backend, transport clock, playlist, hotkeys | done |
| M2 mpv JSON IPC backend, capability negotiation, `jas doctor` | done for Unix; **unverified on this machine (mpv not installed)** |
| M3 A-B, repeat N, gap, per-track state and resume | done |
| M4 interactive mode, `:` prompt, line editor, `--status-line`, non-TTY mode | done |
| M5 optional `native` backend | not started, still optional |
| M6 packaging, README, `docs/backends.md` | README done; `docs/backends.md` not written |
| M7 full-screen interface (`tui.rs`), default on a terminal, `--no-tui` to opt out | done on macOS; **unverified on Windows** |

M7 is new: the first version of this plan listed a TUI as a non-goal (section 2), and
the reversal is recorded there and in section 5.2 rather than being quietly dropped.

Verification status, stated precisely because it matters:

- `cargo test`: 342 unit tests + 23 integration tests (`tests/cli.rs`), all passing on
  macOS with `ffplay` installed. These include a real `ffplay` playback test that
  spawns the player against generated audio and asserts the respawn contract.
- `bash scripts/ainiux/smoke`: 20 end-to-end checks against the built binary.
- `python3 scripts/ainiux/pty`: drives the real binary on a pty and asserts the
  layout it drew. Line mode (`--expect line`) is checked at 80 columns and at 40
  columns with a double-width CJK filename; the full-screen interface
  (`--expect tui`, no `--no-tui`, so it is also the check that the TUI is the
  default) is checked at 100x30 by freezing the frame it drew when it left the
  alternate screen. A pipe cannot exercise any of that drawing, so this is the only
  automated check of the interactive layout.
- `bash scripts/ainiux/check` runs all of the above.
- `docs/backends.md` documents the per-backend contract.
- **Not verified**: the mpv path (mpv is not installed on the development machine,
  so those two integration tests print `skipping` and return); the full-screen
  interface on Windows, where the alternate screen goes through the Win32 console
  API that has never been run here; the "TTY stdin, redirected stdout" case, which
  takes the line-oriented path by construction but has no check of its own; Windows
  behaviour of any kind (including Ctrl+C,
  console restore, and named-pipe IPC); the termios round-trip of terminal restore
  (the pty harness drives a real terminal but reads output, it does not inspect
  termios, so neither raw mode nor the alternate screen is checked as a *state*);
  and audio *quality* claims, which no test can assert.

## 1. Stack (locked)

- Language: Rust, edition 2021, MSRV pinned in `Cargo.toml`.
- Distribution: one self-contained binary per platform, produced by CI.
- Audio: external players, discovered at runtime (see section 4.3). No bundling of
  codecs or players.
- Interaction: one command grammar reachable three ways -- single-key hotkeys on a
  TTY, a `:` command prompt, and piped/flags-only scripting. See section 5.2.
- Interface: a full-screen TUI on a terminal (default; `--no-tui` opts out) and a
  line-oriented one everywhere else. Both drive the same `Session`, so the interface
  changes what you see, never what the transport does.

Crate choices (small, widely used, no heavyweight runtime):
| Need | Crate |
|---|---|
| Argument parsing | `clap` (derive) |
| Serialization of state and `--json` output | `serde`, `serde_json` |
| Error types | `thiserror` for library errors, `anyhow` at the binary edge |
| Config/state directory | `directories` |
| Terminal raw mode + cross-platform key events | `crossterm` |
| Full-screen rendering | `ratatui` (0.27, `default-features = false, features = ["crossterm"]`) |
| Line editing for the `:` prompt | in-house, on `crossterm` events (see 5.2) |
| Display width of a line (CJK is two columns) | `unicode-width` |
| Ctrl+C / console events | `ctrlc` |
| Unix socket + Windows named pipe | `interprocess` |
| Native audio output (optional feature) | `cpal` |

Honesty note on "single static binary": on Linux the target is
`x86_64-unknown-linux-musl`, but the optional `native` feature links ALSA and
therefore glibc. The default build (external backends only) is genuinely static.
`ratatui` is pinned to the version whose crossterm requirement is the 0.27 the rest
of the program uses, so the dependency tree holds **one** crossterm: a second copy
would be a second terminal library to disagree with about termios, which is exactly
the failure the in-house line editor exists to avoid (5.2). It is built with only the
crossterm backend and draws no colour, so the feature set is smaller than the default
one.

## 2. Non-goals (v1)

- No GUI, no web UI.
- **No TUI was the original v1 non-goal, and it has been reversed deliberately.** The
  plan said "no cursor addressing, no screen layout, no redraw loop"; the full-screen
  interface (section 5.2, milestone M7) is now the default on a terminal. What the
  non-goal was protecting is still protected: a pipe, a file, or a redirect gets
  line-oriented output with no escape sequences at all, `--no-tui` gives a terminal
  the old interface, and the TUI is a *view* over the one session -- it adds no
  second command grammar, no second input layer, and no second state store.
- No library management (tag editing, artwork, ratings, scanning).
- No streaming sources (URLs, podcasts, internet radio).
- No mobile targets, no recorder, no transcription, no speech analysis.
- No bundled ffmpeg/mpv. If the user's machine has no backend, Jas explains
  exactly what to install instead of failing silently.

## 3. Product requirements

The three bullets in the brief expand into these v1 requirements:

| ID | Requirement |
|---|---|
| R1 | Play mp3, m4a/aac, ogg/opus, flac, wav, and anything else the chosen backend decodes. |
| R2 | Accept a playlist from explicit files, directories, and `.m3u`/`.m3u8` files. |
| R3 | Loop the playlist by default; support `off`, `all` (default), `one`. |
| R4 | Work as a pure CLI: no window, no required TTY, scriptable, sensible exit codes. |
| R5 | Support language-learner drills: A-B segment loop, segment repeat count, inter-repeat gap, and speed change with preserved pitch. |
| R6 | Persist resume position, A-B marks, and speed per track. |
| R7 | Run on macOS, Linux, and Windows with the same commands and flags. |
| R8 | Handle non-ASCII paths (Arabic, Chinese, emoji) without corruption on every platform. |
| R9 | Provide interactive control: single-key hotkeys on a TTY plus a `:` command prompt covering the whole command grammar. |
| R10 | Never leave the terminal in raw mode, on the alternate screen, or with the cursor hidden on any exit path: quit, error, panic, or signal. |
| R11 | On a terminal, provide a full-screen interface over the same session and the same command grammar, with `--no-tui` as the way out; a pipe, a file, or a redirect never sees a single escape sequence. |

## 4. Architecture

### 4.1 Module layout

```
src/
  main.rs          entry point: parse args, resolve config, dispatch, exit codes
  cli.rs           clap definitions, flag validation
  commands.rs      command grammar: parse line -> Command enum (pure)
  keys.rs          key event -> Command mapping, default keymap (pure)
  lineedit.rs      line buffer + history: (buf, key) -> (buf, effect) (pure)
  input.rs         the single input layer: TTY events or stdin lines; mode switch
  term.rs          RAII terminal guard: restore on drop, panic, and signal
  repl.rs          line-mode loop, screen discipline (ScreenWriter), status line
  tui.rs           full-screen loop: panes, message sink, help overlay (ratatui)
  time.rs          time literal parsing and formatting (pure)
  playlist.rs      loaders, natural sort, seeded shuffle, loop modes, navigation
  transport.rs     logical position clock and play state machine (pure)
  session.rs       orchestration: playlist + transport + player + state
  state.rs         config/state file, atomic write, resume lookup
  error.rs         error enum and exit-code mapping
  player/
    mod.rs         Player trait, Capabilities, Exit, FakePlayer
    detect.rs      backend discovery and auto-ordering
    mpv.rs         mpv JSON IPC backend (live control) -- Unix only, see 9
    ffplay.rs      ffplay respawn backend (emulated control)
    native.rs      optional feature: ffmpeg decode -> PCM -> cpal
                   (not written; `--backend native` reports that it is absent
                    instead of failing obscurely)
tests/
  cli.rs           end-to-end: spawn the binary, drive stdin, assert stdout/codes
scripts/ainiux/
  cargo test build check   run cargo with the toolchain on PATH
  test one                 test-suite helpers (tail output / single exact test)
  smoke                    end-to-end checks against the built binary
  pty                      drive the real binary on a pty and assert the layout
                            (`--expect line` and `--expect tui`)
  pty-narrow.txt           the 40-column, CJK-filename pty scenario
  pty-tui.txt              the 100x30 full-screen pty scenario
  backends                 report which backend-dependent tests ran vs. skipped
```

Separation of concerns: `player/*` does audio I/O and nothing else; `transport.rs`
owns time and play state; `session.rs` wires everything; `repl.rs`, `tui.rs`,
`input.rs`, and `main.rs` do I/O only. `commands.rs`, `keys.rs`, `lineedit.rs`,
`time.rs`, `transport.rs`, and `playlist.rs` are pure and account for most of the
unit tests.

The one place that is *not* purely separated is `session.rs`, whose writer is a
`ScreenWriter` sharing an occupancy flag with the loop's. That is deliberate: the
session prints confirmations and errors, and it cannot know whether the loop has an
in-place line on screen. See the layout rules in section 5.2.

`tui.rs` is the second consumer of that writer, and keeps the same discipline: the
session's output goes to a `MessageSink` (a `Write` that keeps lines instead of
printing them) and the TUI draws the tail of it. So the session still does not know
which interface is in front of it, and the TUI adds no behaviour to it.

Two decisions in `tui.rs` are worth stating because they are the ones a reader would
try to "improve" back:

- **The playlist pane has no cursor of its own.** Its selection is the transport's
  current track, and navigation stays `next`/`prev`/`goto`. A second cursor would
  need keeping in step with the transport, and the two would disagree the first time
  a track ended on its own.
- **The two panes are rectangles computed by hand, not a `Layout`.** The split is one
  expression (`38%` of the width, or nothing below 60 columns) and the degenerate
  sizes are easier to test as arithmetic than to reason about through a constraint
  solver. The one piece of layout that does use percentages -- the overlay -- is a
  pure `centered_rect` function with its own tests.

One deviation from the plan as first written: `keys.rs` defines its own
`Chord { code, mods }` rather than taking `crossterm::KeyEvent`. `input.rs` is then
the only module that mentions the terminal library, which keeps the whole keymap
pure and table-testable with no TTY. That is what makes the §10 requirement ("a
keymap typo fails the build") achievable rather than aspirational.

### 4.2 The Player trait

```rust
trait Player {
    fn capabilities(&self) -> Capabilities;
    fn load(&mut self, track: &Track, at: Duration) -> Result<()>;
    fn play(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn seek(&mut self, to: Duration) -> Result<()>;
    fn set_speed(&mut self, speed: f64) -> Result<()>;
    fn set_ab_loop(&mut self, ab: Option<(Duration, Duration)>) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
}

struct Capabilities {
    live_pause: bool,     // pause without restarting audio
    live_seek: bool,      // seek without restarting audio
    live_speed: bool,     // change speed mid-track
    native_ab_loop: bool, // backend loops a segment itself
    position_feedback: bool, // backend reports true position
}
```

Every capability is negotiated, never assumed. When a command needs a capability
the backend lacks, Jas either emulates it (section 4.4) or answers
`seek is not supported by backend ffplay` and keeps running. No command may crash
or silently no-op.

### 4.3 Backends and auto-ordering

`--backend auto` (default) probes in this order and takes the first hit:

1. `mpv` -- full live control over JSON IPC. Preferred.
2. `ffplay` (ships with `ffmpeg`) -- respawn control, see 4.4.
3. OS-native opener (`afplay` on macOS, `open`/`start`) -- open and forget.

Ordering note: this deliberately puts `mpv` ahead of `ffplay`, which is a
deviation from the initial survey answer of "ffplay first, then mpv". Rationale:
`ffplay` has no control channel in `-nodisp` mode, so with it as the primary
backend every pause/seek costs a process restart (audible, ~100-300 ms). `mpv`
is a single optional install that makes R5 (A-B drill, live speed) precise. The
fallback chain still contains ffplay, so the "works with just ffmpeg" story is
preserved. `--backend mpv|ffplay|native` overrides detection; an explicitly
requested but missing backend is an error, not a silent fallback.

`jas doctor` prints which backends were found, their versions, and the resolved
capabilities. This is the first thing to ask for in a bug report.

### 4.4 Two control strategies, one state machine

The hard part of this project is that "pause/seek/speed" must behave identically
whether the backend can be controlled or not. The solution: position lives in
`transport.rs`, never in the backend.

**Live control (mpv).** Connect to `--input-ipc-server`. Use `time-pos`, `pause`,
`speed`, `ab-loop-a`/`ab-loop-b` properties, and `loadfile`/`seek` commands.
Position is anchored to `time-pos` observations to correct drift.

**Respawn control (ffplay, OS openers).** Spawn `ffplay -nodisp -autoexit -ss <T>`
for the current position. Then:

- Seek = kill the child, respawn at the new offset.
- Pause = kill the child, freeze the transport clock (resume respawns).
- Speed = respawn with `-af atempo=<X>` (chained `atempo` filters for X outside 0.5-2.0).
- A-B loop = poll the transport clock and respawn at A when it passes B.
  Accuracy is one polling interval; the target tolerance is +/-150 ms, which is
  fine for shadowing drills but documented as emulated.
- Position is derived from the wall clock, not from the child process.

**The transport clock.** Position is computed, never accumulated:

```
position(t) = clamp(anchor_pos + (t - anchor_t0) * speed, 0, duration)
```

Every state change (play, pause, seek, speed change, respawn, track change)
re-anchors `(anchor_pos, anchor_t0)`. The clock is an injected `Clock` trait so
tests drive time by hand and assert on exact positions. `Instant`/`MONOTONIC` only
-- never `SystemTime` -- so NTP steps and DST cannot make position jump.

### 4.5 Playback state machine

States: `Idle -> Playing <-> Paused -> Finished -> (next track) | Stopped`.
`Finished` is decided by the transport clock crossing `duration` when duration is
known, or by the child process exiting cleanly when it is not.

## 5. CLI surface

### 5.1 Invocation

```
jas [OPTIONS] [PATH]...
```

Paths may be files, directories (recursive), or m3u/m3u8. With no paths, Jas
restores the last playlist from state. With no TTY, flags alone drive playback;
section 5.2 has the table that decides which input mode is used.

| Flag | Meaning |
|---|---|
| `--loop <off\|all\|one>` | Loop mode, default `all`. |
| `--shuffle` / `--seed <N>` | Shuffle; the seed makes order reproducible and testable. |
| `--speed <0.25-4.0>` | Playback speed, default `1.0`. |
| `--ab-a <T>` / `--ab-b <T>` | Set A-B marks at startup. |
| `--repeat <N\|off>` | Repeat the A-B segment (or the whole track) N times. |
| `--gap <MS>` | Pause inserted between repeats, default `0`. |
| `--play` | Start playing immediately without an interactive prompt. |
| `--track <N>` | Start at track `N` (1-based). |
| `--play` | Start playing immediately without waiting for a keypress. |
| `--list [--json]` | Print the resolved playlist and exit. Works without a backend. |
| `--doctor` | Print the backends found, their versions, and the resolved choice. |
| `--backend <auto\|mpv\|ffplay\|native>` | Force a backend. `native` is not built into this binary. |
| `--keys <default\|mpv\|off>` | Keymap preset for hotkey mode; `off` is the same as `--no-keys`. |
| `--no-keys` | Start in command mode with hotkeys disabled. |
| `--status-line` | Keep one status line updated in place with `\r` (line-mode interface, TTY only). |
| `--no-tui` | Start in the line-oriented interface instead of the full-screen one. |
| `--no-state` | Do not read or write resume state. |
| `--config <PATH>` | Use an alternate config/state **directory**. |
| `-q` / `-v` / `--version` / `-h` | Quiet, verbose, version, help. |

Without `--play`, a session loads the track at its resume point and waits: on a TTY
for a keypress, on a pipe for the script's first `play`. That is deliberate -- a bare
`jas lesson.mp3` should not start talking at you -- but it does mean that
`jas lesson.mp3 < /dev/null` loads, finds its input exhausted, and exits 0 without
making a sound. Add `--play` for the scripted case.
| `-q` / `-v` / `--version` / `-h` | Quiet, verbose, version, help. |

### 5.2 Interactive mode

Two things are decided here: *which interface* draws the session, and *which input
mode* feeds it. They are separate questions, and both answer themselves rather than
needing a flag.

| stdin | stdout | interface |
|---|---|---|
| TTY | TTY | **full-screen TUI** (the default); `--no-tui` gives the line-oriented one |
| TTY | not a TTY (`> log`) | line-oriented: cursor addressing cannot go into a file |
| pipe or file | either | line-oriented; a script's output stays parseable |

There is exactly one input layer, and two ways to feed it. Which one you get depends
on stdin, not on a flag you have to remember:

| stdin | mode | notes |
|---|---|---|
| TTY | **hotkey mode**, with `:` opening the command prompt | the normal interactive case; `--no-keys` starts in command mode instead |
| pipe or file | **command mode**, one line at a time | hotkeys are off; this is the scriptable mode |
| neither (`--play`, no stdin) | no input at all | playback runs to completion or until signalled; flags only |

Both TTY modes speak one grammar (5.4): every hotkey is a shortcut for a command, and
nothing is reachable only by keypress. `keys.rs` maps a key event to the same
`Command` value the parser produces, so the two paths cannot drift apart.

Mode state machine -- one thread owns the terminal, so there is never more than one
consumer of raw mode:

```
          ':'                        Enter  (Ctrl+C / Esc cancels)
  Hotkey  ------>  Command  -------------------------------->  Hotkey
```

- **Hotkey mode**: raw mode, one keypress per action, no Enter required. Each action
  prints a one-line confirmation (`paused  0:41 / 3:12`) so the scrollback still reads
  as a transcript.
- **Command mode**: a small in-house line editor over the same `crossterm` event
  stream -- insert, backspace, left/right, home/end, up/down history (in memory, last
  100 lines, never persisted), Enter to run, Esc or Ctrl+C to return to hotkeys.
- **Why in-house rather than `rustyline`**: line editing needs raw mode, and so do
  hotkeys. Two libraries toggling termios on the same fd is the standard way to end up
  with a broken terminal, and on Windows it means two console-mode managers. One owner,
  one event stream, no fight. The cost is roughly 150 lines of editing logic; the
  mitigation is that the buffer logic is pure and table-tested (section 10), and
  `rustyline` stays a documented fallback (open question 6).
- **The backend never sees the keyboard**: `mpv` gets its terminal input disabled and
  `ffplay` still reads keys even under `-nodisp`, so every child is spawned with
  `Stdio::null()` on stdin. Otherwise one keypress would hit both processes and `q`
  would kill the player instead of Jas.
- **The hotkey map is not the only way in**: a command that needs an argument is
  reachable by typing it after `:`, so an unbound or misconfigured key never blocks a
  feature.
- **Terminal restore is guaranteed** (R10): `term.rs` holds an RAII guard that leaves
  raw mode on drop, plus a panic hook and a SIGINT/SIGTERM handler that restore before
  `stty sane` as the recovery. The guard is unit-tested against a fake terminal,
  including its behaviour during a panic unwind. What is still missing is a check of
  the termios round-trip itself: the pty harness drives a real terminal, but it reads
  *output*, so it would not notice a terminal left with echo off.

#### Layout rules

An in-place line (the `:` prompt, or the `--status-line` readout) is painted with no
newline, so it *owns the cursor row*. Two rules follow from that, and both exist
because getting them wrong produced a real bug: the prompt was painted, and the very
next message was appended to it, so output appeared to start in the middle of a line
rather than at the beginning.

1. **Exactly one thing owns the row, and every message begins at column 0.**
   `repl::ScreenWriter` enforces this. It erases the in-place line *lazily*, on the
   first byte of the next message, so a message always starts a fresh row. Lazy
   matters: erasing eagerly each loop iteration would blank and repaint 25 times a
   second and flicker over a slow link. The occupancy flag is shared with the
   *session's* writer, because confirmations and errors come from there and cannot
   know about the prompt.
2. **An in-place line is never wider than the terminal.** A wrapped in-place line is
   worse than a truncated one: `\r\x1b[2K` only erases the row the cursor is on, so
   the row above keeps the tail of the previous line and every later line looks
   misplaced. In-place text is fitted to `width - 1` columns, measured in *display*
   columns (`unicode-width`), because a CJK character occupies two and counting
   characters would let a Chinese filename overflow. The status line keeps its head
   (cut at a field boundary, so it reads as an abbreviation rather than a glitch) and
   the prompt keeps its tail, marked with `…`, because the cursor is at the end.
   The width is re-read each iteration, so a resize does not reintroduce the problem.

Three smaller rules fall out of the same reasoning:

- **Echo is mandatory in command mode.** A raw-mode terminal does not echo, so each
  keystroke asks for a repaint (`Input::Redraw`). Reporting `Idle` for buffer changes
  was a bug: the user typed an entire command into a screen that never changed.
- **Command mode is always marked.** The prompt is `:` even under `--no-keys`, where
  there is no hotkey mode to return to: without a marker the user is looking at a
  blank row with no cue that it accepts input.
- **The banner is the first thing on screen.** Startup used to print a confirmation
  *above* it, because the session reported a state change before `main` printed the
  banner. Startup now resolves everything silently and prints the banner first, so
  the transcript reads in the order things happened. `--play` and the `--ab-a`-style
  overrides report *below* it.
- **`--no-keys` at a terminal still gets a prompt.** "Is there a human at a terminal?"
  and "are hotkeys on?" are different questions; conflating them left `--no-keys`
  typing blind.

#### The full-screen interface

The TUI is the default on a terminal and is a *view* of the same session: keys still
go through `input.rs`, so a hotkey is still the same `Command` the parser produces,
and `:` opens the same prompt, on the bottom row. What it adds:

- A playlist pane (selection = the transport's current track; no second cursor), a
  now-playing pane (name, a progress bar with the A and B marks drawn in it, position
  and duration, speed, repeat, gap, loop mode, backend), a three-row message area
  holding the tail of the session's output, and a bottom row that is the `:` prompt in
  command mode and the key reminder otherwise.
- A help overlay for `?`, `help`, `help keys`, and `keys`. It shows the text those
  commands print -- `commands::HELP` and `Keymap::render()` -- so the overlay cannot
  disagree with the CLI. Esc (either spelling), `?`, space, Enter or `q` close it;
  `↑`/`↓`, `j`/`k`, or `n`/`p` scroll it.
- **No colour**, only bold and reverse, so `NO_COLOR` stays satisfied by construction
  rather than by a check.
- Messages are lines rather than bytes on stdout: the session keeps writing to a
  `Write`, but in the TUI that writer is a sink and the message area draws its tail.
  A confirmation or an error is therefore visible instead of landing in the middle of
  the frame.
- One line is left on the normal screen when the TUI exits (state and position),
  because the alternate screen takes the transcript with it.

Degradation is explicit rather than emergent: below 60 columns the playlist pane is
dropped rather than squeezed, and below 12x3 the frame becomes a single state line.
The layout is a pure function of a `View` struct, which is what lets six window
sizes -- down to 1x1 -- be asserted in unit tests instead of by hand.

Two rules carry over from the line-oriented interface unchanged: the message area
keeps the *tail* of the output (three rows cannot show a transcript), and no line is
ever drawn wider than the space it has, so a pane border cannot be pushed onto the
next row. The single-row behaviours above still matter for line mode, and the
`--status-line` readout is line-mode only.

### 5.3 Default key bindings

The one-key layer, aimed at the drill loop: play/pause, nudge, mark the phrase, repeat
it.

| Key | Command | Notes |
|---|---|---|
| `Space` / `Enter` | `toggle` | pause/resume |
| `→` / `←` | `seek +5` / `seek -5` | |
| `Shift+→` / `Shift+←` | `seek +30` / `seek -30` | |
| `n` / `p` | `next` / `prev` | |
| `[` / `]` | `speed -0.05` / `speed +0.05` | clamps to 0.25-4.0 |
| `\` | `speed 1.0` | reset |
| `a` / `b` | `ab-a` / `ab-b` | mark the phrase at the current position |
| `c` | `ab clear` | |
| `r` | `repeat cycle` | cycles off -> 2 -> 3 -> 5 -> 10 -> off |
| `g` | `gap +250` | inter-repeat gap, 250 ms per press |
| `l` | `loop` | cycles off -> all -> one |
| `:` | *(mode switch)* | enters the command prompt; not a bindable key |
| `?` | `help keys` | keymap + command list (the help overlay in the TUI) |
| `Esc` / `↑` `↓` | *(overlay)* | close / scroll the help overlay; TUI only |
| `q` / `Ctrl+D` | `quit` | exit code 0, like `quit` |
| `Ctrl+C` | *(interrupt)* | exit code 130, terminal restored first |

- Keys are declared in `config.json` under `"keymap"` as chord -> command string, so
  the table above is a default, not a hard-coded surface.
- `--keys mpv` accepts the key names mpv uses, for people who already have that muscle
  memory.
- A keymap entry that does not parse is reported at startup with the offending entry
  and then ignored, so a stale config can never stop playback; `keys` and
  `--keys <preset>` show the effective map, so the mismatch is visible.
- Keys are matched on the event, not on raw bytes, so `→` works identically on all
  three platforms instead of depending on escape-sequence timing.
- `:` and `Ctrl+C` are deliberately *not* keymap entries. They are mode transitions
  handled by the input layer, so no config can make the full command grammar
  unreachable or make Ctrl+C stop meaning "get me out".
- **The overlay is not a second keymap.** In the TUI, `?`, `help`, `help keys`, and
  `keys` open an overlay showing the same text those commands print, from the same
  source (`commands::HELP`, `Keymap::render()`), so the two interfaces cannot disagree
  about what a key does. While it is open it takes every key except Ctrl+C; Esc (`?`,
  space, Enter and `q` also work) closes it, and the arrows scroll it. A key it does
  not know is swallowed rather than passed to the session, because a stray `n` while
  reading the help must not skip a track behind it.
- Control keys are normalized before lookup: terminals report `Ctrl+C` either as
  `CONTROL + 'c'` or as the raw code point `U+0003`, and both must be the same
  chord, or the quit key would only sometimes work.

### 5.4 Command grammar

Every hotkey in 5.3 maps to one of these. The grammar is identical on a TTY, on a pipe,
and in a file, so a script can be:

```
jas --play mix.m3u < drill.txt
```

| Command | Aliases | Effect |
|---|---|---|
| `repeat <N\|off\|next\|cycle>` | | Segment repeat count; `next` shifts the segment by its own length after N repeats; `cycle` steps off -> 2 -> 3 -> 5 -> 10 -> off. |
| `gap <MS>` / `gap +MS` / `gap -MS` | | Inter-repeat gap, absolute or relative. |
| `loop [off\|all\|one]` | | Loop mode; with no value it cycles. |
| `shuffle [on\|off]` | | Reshuffle; with no value it toggles. |
| `list` / `status` / `save` / `backend` / `keys` | `ls`, `st` | Info and session control; `keys` prints the active keymap (and opens the overlay in the TUI). |
| `help [keys]` / `quit` | `?`, `q`, `x` | Help (keymap and commands) and exit. |

Two grammar notes worth stating because they were wrong in an earlier draft of this
document: `repeat next` and `repeat cycle` are different actions ("shift the segment
forward" versus "step the repeat count"), and bare `loop` / `shuffle` mean "cycle" /
"toggle". Without those, the `r` and `l` hotkeys would have had to either lie about
their name or share a command with a different meaning.

Ranges are validated, not silently clamped, wherever the user typed a number:
`jas --speed 99` is a usage error (exit 1), while `speed +10` at the prompt clamps
to 4.0 and says so. Clamping a flag would hide a typo; clamping a relative nudge is
the intuitive behaviour.
| `repeat <N\|off\|next>` | | Segment repeat count; `next` shifts the segment by its own length after N repeats. |
| `gap <MS>` / `gap +MS` | | Inter-repeat gap, absolute or relative. |
| `loop <off\|all\|one>` | | Loop mode. |
| `shuffle <on\|off>` | | Reshuffle (`on` keeps the seed for reproducibility). |
| `list` / `status` / `save` / `backend` / `keys` | `ls`, `st` | Info and session control; `keys` prints the active keymap. |
| `help [keys]` / `quit` | `?`, `q`, `x` | Help (keymap and commands) and exit. |

`status` is one machine-readable line (track index, name, position, duration,
state, speed, A-B, loop) so users can log drill sessions. In the TUI it lands in the
message area instead of on stdout; the *text* is the same, so a drill log can be built
from either interface.

`help`, `help keys`, and `keys` are the one place the interface changes the shape of
the answer: line mode prints the text as rows, the TUI shows it in a scrollable
overlay. The text itself comes from one place in both cases (`commands::HELP` and
`Keymap::render()`), which is what keeps the shortcut from becoming a second copy.

The **Aliases** column is empty wherever a hotkey uses the same letter for a
*different* action, and that is deliberate: the keymap (5.3) is the one-key layer, so a
bare `p` typed at the prompt is an error rather than a second meaning for the key `p`
(which means `prev`). The short forms that remain -- `q`/`x` (quit), `?` (help), `ls`,
`st` -- agree with the keymap, so no letter means two different things anywhere.

### 5.5 Time syntax

Accepted everywhere a `<T>` appears: `SS`, `MM:SS`, `HH:MM:SS`, each with an
optional `.ms` fraction. A leading `+` or `-` makes it relative to the current
position. Negative results clamp to `0`; past the end they clamp to the duration
(or advance to the next track with a clear message). `time.rs` owns this grammar,
including the error messages, and is exhaustively unit-tested.

### 5.6 Exit codes

| 130 | SIGINT on a pipe or file, or the `Ctrl+C` key in hotkey mode. In command mode Ctrl+C cancels the typed line instead of exiting, so the drill is not lost to a mistyped command. Terminal state is restored first. |

A runtime failure that ends a headless session yields 2; an explicit `quit` yields 0
even after a recovered error, because the user asked for it. 130 is not an `Error`
variant: it comes from the signal handler or the input layer, and is returned as a
bare code.

## 6. Learner features (the reason this exists)

- **A-B loop** for shadowing: mark a phrase, loop it, optionally with a gap and a
  repeat count. Backed by mpv's native loop when available, emulated otherwise.
- **Segment repeat with auto-advance**: after `N` repeats, either stop, advance to
  the next track, or shift the segment forward by one segment length (`repeat
  next`) -- the standard drill loop.
- **Speed with preserved pitch** at 0.5x-2.0x, one keystroke to nudge
  (`speed +0.05`) and one to reset. Score the quality per backend.
- **Per-track memory**: resume position, A-B marks, speed, and repeat count are
  stored per track so a second session on the same file starts where it stopped.
- **Planned, not v1**: side-by-side original/translation text display. Keep the
  `status` contract stable so a companion tool can consume it later.

## 7. Playlists

- Sources: explicit files, directories (recursive, natural sort so `ch2` precedes
  `ch10`), and `.m3u`/`.m3u8` (relative entries resolved against the playlist file).
  A BOM and CRLF in a playlist are handled.
- **Not implemented**: stdin line lists. An earlier draft listed them as a source;
  they are not, because stdin is already the command channel (5.2), and overloading
  it would make `jas - < list.txt` ambiguous. Use a `.m3u` or an argument list.
- Navigation: `next`, `prev`, `goto`, with `off`/`all`/`one` loop semantics.
  In `one` mode with A-B active, the A-B loop wins inside the track.
- Shuffle uses a seeded PRNG so `--seed` reproduces the exact order; the seed is
  printed at startup and saved so a resumed session keeps its order.
- Duplicate and missing entries: missing files are reported once, skipped, and do
  not abort the session; if nothing is playable, exit code 3.

## 8. State and persistence

- Location: the OS config dir via `directories` (`~/.config/jas/`, Library on
  macOS, `%APPDATA%` on Windows), with `--config` to override.
- Two files: `config.json` (backend preference, defaults, keymap overrides) and
  `state.json` (playlist, index, seed, loop mode, and per-track entries keyed by
  canonical path plus size and mtime; a changed file invalidates its resume
  position).
- Writes are atomic: write to a temp file in the same directory, `fsync`, rename.
  A corrupt file is moved aside as `<name>.corrupt-<n>` with a warning, never fatal.
  Nothing is deleted, so the user can inspect it.
- Opt-out everywhere: `--no-state`.
- `--config` takes a *directory*, not a file, so both files move together.
- **`config.json` is read-only in this version.** Jas reads `backend`, `speed`,
  `loop_mode`, `keys`, `status_line`, and `keymap`, and reports entries it cannot
  use. It never writes the file, so the README shows the schema rather than Jas
  seeding one. An earlier draft implied Jas would create it, which is not
  implemented.
- A configured `speed` or `loop_mode` sits between the command line and the built-in
  default: `--speed` beats `config.json` beats 1.0. That is why the CLI flags are
  "unset" rather than pre-filled with a default. Config resolution happens once, in
  `main`, so there is exactly one precedence rule and one read per file.
## 9. Cross-platform notes

- **Paths**: `PathBuf`/`OsString` end to end. Never round-trip a path through
  `String` except for display, and never assume UTF-8 on Windows. Tests cover
  Arabic, Chinese, and emoji filenames on all three platforms.
- **Process control**: no SIGTERM on Windows; use the platform kill API. Install a
  Ctrl+C handler that stops the child before exit, so no orphan keeps playing.
  **State of this today**: `ffplay` children are killed directly and reaped, and a
  dropped player kills its child; **job objects / process groups are not
  implemented**, so a child that forks would outlive Jas. `ffplay` does not fork.
- **mpv IPC**: Unix domain socket on macOS/Linux. **Not implemented on Windows**:
  the named-pipe client (`\\.\pipe\jas-<pid>`) is not written, so `jas doctor`
  reports mpv as found-but-unusable there and `auto` falls through to `ffplay`.
  This weakens R7 in the narrow sense that the *backend set* differs by platform,
  while the commands and flags stay identical. The socket path lives in the temp
  dir and is removed on exit, including on a failed handshake.
- **Terminal modes**: on a TTY, raw mode is entered once for the whole interactive
  session -- both hotkey and command mode consume `crossterm` events. Non-TTY stdin
  gets line mode with no prompts and never touches terminal settings. Output stays
  newline-delimited (machine-parseable) except the opt-in `--status-line`, which is
  TTY-only, uses `\r`, and is cleared on exit. In the TUI, raw mode is joined by the
  alternate screen and a hidden cursor, both owned by `term::ScreenGuard` and both
  handed back on the same paths (see R10 below); a redirect of stdout disables the TUI
  even when stdin is a terminal, so no cursor addressing can be written into a file.
- **Colour**: no colour is emitted at all in this version, so `NO_COLOR` has nothing
  to suppress. The requirement is met by construction rather than by a check.
- **Terminal restore** (R10): `term.rs` leaves raw mode via RAII on drop, on panic, and
  from the signal handler, so Ctrl+C, an error, or a panic cannot leave a shell with
  echo off. The guard *owns* the object that enabled raw mode, so it cannot report
  success while restoring something else. The TUI adds a second guard in the same
  shape -- alternate screen and cursor -- for the same reason: a panic inside the TUI
  used to be able to leave a blank screen with no cursor and no way back. The two share
  one `emergency_restore` (screen first, then raw mode, so a message printed during
  recovery is not thrown away with the screen). `SIGKILL` is explicitly out of scope
  and `stty sane` is documented in `--help`.
- **Line endings**: emit `\n`; accept `\r\n` in command input and m3u files.
- **Keyboard on Windows**: `crossterm` covers console mode and key decoding, so there
  is no `termios` code path, but Ctrl+C arrives as a key event rather than a signal.
  Key *release* events are ignored, because Windows reports both press and release
  and acting on both would run every action twice. **Unverified on Windows**:
  Ctrl+C, Ctrl+D, and console-restore behaviour have never been run there, and neither
  has the full-screen interface, whose alternate screen goes through the Win32 console
  API rather than an escape sequence.

## 10. Testing strategy

1. **Pure unit tests** (the bulk): `time.rs` parsing/formatting round-trips,
   `commands.rs` grammar and error messages, `transport.rs` with a fake clock (pause,
   seek, speed change, re-anchor, clamping, overshoot), `playlist.rs` sort/loop/shuffle
   determinism.
2. **Input layer tests**: `keys.rs` is table-driven -- every key documented in 5.3 must
   map to a command, and every mapped command must be accepted by the parser, so a
   keymap typo fails the build instead of doing nothing at runtime. `lineedit.rs` is
   tested as a pure buffer: insert, backspace, left/right, home/end, history recall and
   wrap-around, and the invariant that the buffer does not grow unboundedly on
   paste-like input. `input.rs` asserts that a buffer change reports `Redraw` (the
   echo) and that `:` is a mode switch rather than a binding.
3. **Screen-discipline tests** (`repl.rs`): `ScreenWriter` is tested through a
   *wrap-aware* terminal renderer that replays the byte stream onto a grid, so an
   assertion can be "this line starts at column 0" rather than "these bytes were
   written". It models wrapping deliberately: ignoring it would hide the exact bug the
   tests exist to catch. The renderer itself is tested against a known erase sequence,
   because a renderer that quietly ignores `ESC[2K` would make every layout assertion
   pass for the wrong reason -- which is precisely what happened during development.
   Covered: a message replaces rather than joins an in-place line; one erase per
   occupied row, not one per write; an empty write does not clear; shrinking text
   leaves no tail; `keep_line` keeps a submitted line as a row; fitting to the width
   including double-width text; and the loop's own behaviour end to end.
4. **FakePlayer**: a `Player` implementation that records commands, so session and
   command-dispatch logic are tested with no audio device, no backend, and no timing
   flakiness in CI. It reports child exits through the same `poll_exit` path the real
   backends use, so the `Finished`-versus-`Failed` distinction is tested without
   spawning anything.
5. **Backend contract tests**: each real backend is run against generated audio
4. **Backend contract tests**: each real backend is run against generated audio
   (`ffmpeg -f lavfi -i "sine=frequency=440:duration=1"`) and must satisfy the same
   assertions for load/play/pause/seek. A backend that is not installed prints why
   and returns, so a skip is visible instead of silently counted as a pass.
   - `ffplay`: **runs here** -- spawns the real player, plays a generated tone,
     asserts pause kills the child, that a resume respawns at the held offset, and
     that `-autoexit` ends the process on its own.
   - `mpv`: written, but **skipped here** because mpv is not installed on the
     development machine. The IPC path has therefore never been executed against a
     live mpv. Treat it as unverified until someone runs it where mpv exists.
6. **End-to-end CLI tests** (`tests/cli.rs`, 21 cases): spawn the built binary, feed
   commands on stdin, assert stdout/exit codes. Covers non-ASCII paths, a missing
   playlist entry, a corrupt state file, a bad keymap entry, relative seek clamping,
   contradictory flags, and m3u resolution from a different working directory.
   `scripts/ainiux/smoke` adds 20 shell-level checks including real audio generation.
7. **pty layout harness** (`scripts/ainiux/pty`): spawns the real binary attached to a
   real pty, sends keystrokes from a small scenario DSL, and replays the output into a
   screen model. `--expect line` asserts the line-mode invariants -- no message glued to
   the prompt, no escape sequence other than `ESC[2K`, clean exit -- at two sizes, one
   of them 40 columns with a double-width CJK filename. `--expect tui` asserts the
   full-screen interface instead, by modelling the alternate screen: the frame is
   captured at `ESC[?1049l` (after which a real terminal has already thrown it away),
   the sequences emitted are checked against an allowlist, and the one line left on the
   normal screen is checked. This is the only automated check of the interactive
   drawing: on a pipe there are no panes, no prompt, and no status line at all, so every
   layout bug is invisible to the tests above.
   **Still not done**: asserting the termios round-trip (raw mode restored after `q`,
   Ctrl+C, and a forced panic) or the terminal *state* after the alternate screen is
   left. The guards' logic is unit-tested against a fake terminal including a panic
   unwind; this harness could be extended to check the real one, and that is the next
   thing to add here.
8. **Full-screen interface tests** (`tui.rs` + the pty harness): the layout is a pure
    function of a `View` struct, so it is rendered into ratatui's `TestBackend` -- no
    terminal, no session, no timing -- and asserted on the rows a user would see: the
    panes' content, the message area's bottom alignment, the overlay's opacity and
    scroll extent, the cursor's column in command mode, and six window sizes down to
    1x1. The pure parts have their own tests: the progress bar is exactly the width it
    is given (a bar one column too wide pushes a pane border onto the next row), `A`
    and `B` land where the fractions say, the overlay rectangle never escapes the
    window, and the message sink keeps lines, caps its history, and shows a line that
    has not ended yet. On a real terminal, `scripts/ainiux/pty --expect tui` freezes the
    frame drawn when the alternate screen is left and asserts the panes, the message
    area, the overlay, the escape sequences used (cursor addressing and attributes
    only, no colour), and that the screen and cursor were returned. **Not done**: the
    same check on Windows, and asserting the termios/console *state* after exit rather
    than the sequences that should have produced it.
9. **Edge cases required by policy**: empty playlist, one very long track, boundary
   times (`00:00`, exactly the duration, past the end), invalid input (`seek abc`,
   `speed 0`, negative values, an unparseable keymap entry), and a backend that dies
   mid-track. A permission-denied file is reported by the loader; there is no
   dedicated test for it. A zero- and one-column terminal is covered by the fitting
   tests.
10. **CI**: `bash scripts/ainiux/check` runs `cargo fmt --check`,
    `cargo clippy --all-targets -- -D warnings`, `cargo test`, the two pty checks in
    line mode, the pty check of the full-screen interface, and the smoke script.
   **Not done**: the three-platform matrix and release builds on tags. Everything
   recorded here was produced on macOS.

## 11. Milestones

| M | Scope | Done when |
|---|---|---|
| M0 | Crate skeleton, clap surface, `--list`, exit codes, CI. | `jas --version` and `jas --list <dir>` work; `check` is green. |
| M1 | `ffplay` respawn backend + transport clock + playlist + loop `all` default + minimal hotkey mode (space, arrows, n/p, q). | Play/pause/seek/next/prev work on a generated WAV and the hotkeys drive them; transport tests are deterministic under a fake clock. |
| M2 | mpv JSON IPC backend, capability negotiation, `jas doctor`. | Live pause/seek/speed with no audible restart; unsupported commands degrade with a clear message. |
| M3 | Learner features: A-B, repeat N, gap, per-track state and resume. | A drill session (`ab 0:12 0:31`, `repeat 3`, `speed 0.75`) survives a restart and resumes correctly. |
| M4 | Interactive mode complete: `:` prompt, in-house line editor and history, full keymap and config overrides, `--keys`/`--no-keys`, `--status-line`, non-TTY stdin mode, `status` contract, Unicode path tests, and the layout rules in 5.2. | Every key in 5.3 is exercised by a test and maps to a parser-accepted command; the prompt echoes and no message is glued to it; `jas --play list.m3u < drill.txt` runs headless. |
| M5 | Optional `native` feature: ffmpeg decode -> PCM -> cpal. | Sample-accurate A-B loop with no external player process; feature is off by default. |
| M6 | Packaging: release builds, README/`--help` accuracy, `docs/backends.md`. | A user with only ffmpeg installed can play, drill, and resume by following the README alone. |
| M7 | Full-screen interface over the same session: panes, message area, help overlay, `--no-tui`, and R10 extended to the alternate screen and the cursor. | It is the default when stdin and stdout are terminals; a pipe or a redirect stays free of escape sequences; the pty check freezes the frame it drew and proves the screen and the cursor came back. |

(Per-milestone status is in section 0.)

M1 and M2 are the risky ones; M5 is deliberately optional and can be dropped
without affecting R1-R10.

What the milestones still owe, stated plainly rather than implied:

- M1's "the pty test proves terminal restore" is not met. The pty harness now exists
  and drives a real terminal, but it reads output; it does not check termios, so a
  terminal left in raw mode would still pass.
- M2 is met in code and unmet in evidence: no verifier has watched mpv seek without
  an audible restart on this machine.
- M4's "Arabic and Chinese filenames verified on all three platforms" is met on one
  platform.
- M6 is met except for the release-build check.
- M7 is met on one platform and in one direction: the pty check asserts the
  *sequences* that leave the alternate screen and show the cursor, not the terminal
  state they produce, and nothing here has been run on Windows, where the alternate
  screen is a console-API call rather than an escape sequence.

## 12. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| `ffplay` has no control channel in `-nodisp` mode. | Every pause/seek restarts audio. | Respawn strategy in 4.4, documented +/-150 ms A-B tolerance, mpv preferred in auto-order. |
| Two input layers fighting over terminal raw mode. | Broken terminal, dropped keystrokes, bugs that are hard to reproduce. | One input thread owns the terminal (5.2); `crossterm` events feed both modes; `rustyline` deliberately unused; RAII guard + panic hook + signal handler restore state (R10). |
| A backend consumes the same keystrokes as Jas. | `q` kills the player instead of Jas; keys handled twice. | Every child gets `Stdio::null()` on stdin; mpv terminal input disabled. |
| The in-house line editor is weaker than a library. | Missing editing features, wide-character or paste bugs. | Pure buffer logic with table-driven tests; `rustyline` remains the documented fallback (open question 6). |
| Pitch-preserving speed quality varies by backend. | Drill audio may sound metallic. | `atempo` chaining outside 0.5-2.0 is unit-tested; mpv's `scaletempo` is the reference. **The audio itself has not been judged by a listener.** |
| "Single static binary" is false for the `native` feature on Linux. | Misleading docs and support load. | Feature off by default; the limitation is stated in the README and in 4.3. |
| Windows process tree and Ctrl+C handling. | Orphan players keep making noise after exit. | A `ctrlc` handler stops the child first, and a dropped player kills its child. **Job objects are not implemented**, and no test asserts no leftover process on Windows. |
| Non-UTF-8 or RTL paths. | Corrupt or unopenable paths. | `OsString`/`PathBuf` end to end; `encode_path` percent-encodes invalid UTF-8 so two distinct paths cannot collide as state keys. Unicode tests exist on one platform, not three. |
| The TUI and the line-oriented interface drift apart (different key hints, different help text, a feature only reachable in one of them). | A user in one interface cannot do what the README describes. | Both drive the same `Session` and the same `InputLayer`, so a hotkey is one `Command` and nothing is reachable only by a keypress; the overlay renders `commands::HELP` and `Keymap::render()`, the same text line mode prints; and both interfaces have assertions on the same content (the TUI's via `TestBackend`, line mode's via the pty checks). |
| A second terminal owner (a rendering library that also toggles termios or decodes keys). | Broken terminal, dropped keystrokes, double-handled keys. | `ratatui` is pinned to the release whose crossterm requirement is the 0.27 already in use, so the tree holds one crossterm; raw mode and the alternate screen are owned by `term.rs`, not by the renderer; and the renderer is given a `Write`, so it never opens a descriptor of its own. |
| Three message rows hide an error that has already scrolled past. | The user does not see why playback stopped. | The message area always shows the *newest* output, and an error is written through the same sink as everything else; `status` is one `:` away for the full readout. Still a real limitation: a long transcript is not readable in the TUI (open question 15). |
| Timing-sensitive tests become flaky. | CI noise, ignored failures. | `FakePlayer` plus an injected clock by default; real-backend tests are gated and tolerance-based. The one loop test that must end uses a clock that advances on every read rather than sleeping. |
| Corrupt state file. | Start-up failure. | Atomic writes, move-aside on parse failure, `--no-state` escape hatch. Unknown JSON fields are ignored so an older Jas can read a newer file. |
| A backend that dies mid-track leaves the session spinning. | The error is re-reported forever and no input ends the run. | **Found and fixed**: the session stops the transport when the child is gone, so the error is reported once and the loop exits with code 2. |
| A normal track end is reported as a crash. | Every finished `ffplay` track printed "stopped unexpectedly" and the playlist stopped instead of advancing. | **Found by the pty harness and fixed**: liveness is not the question. `poll_exit()` reports `Finished` vs `Failed` from the child's exit status, and a clean exit advances the playlist, because for a backend with no duration feedback that *is* the end-of-track signal. |
| An in-place line wider than the terminal. | The cursor lands on a later row, `\r\x1b[2K` clears only that one, and the row above keeps the old tail -- so every later line looks misplaced. | **Found by the pty harness at 60 columns and fixed**: in-place text is fitted to `width - 1` display columns, re-read every iteration, so a resize cannot reintroduce it. |
| The tests' own terminal renderer disagrees with a terminal. | Layout assertions pass for the wrong reason. | The renderer is modelled on wrapping and is itself tested against a known erase sequence. It had exactly this bug (the CSI parser excluded the final letter, so `ESC[2K` never matched), which the assertions only caught once a case depended on the erase. |

## 13. Open questions

1. Should `--ab-a`/`--ab-b` also accept percentages of track duration (`--ab-b 80%`)?
2. Should the session support playlist mutation (`add <path>`, `remove <N>`)?
3. Central `state.json` versus a per-file `.jas.json` sidecar for portability across
   machines? Current decision is central, keyed by path + size + mtime.
4. `README.md` is now a real usage guide. Closed.
5. Is the `native` backend (M5) wanted at all, or is mpv an acceptable hard
   requirement that would simplify 4.4?
6. If the in-house line editor proves too weak (wide-character editing, bracketed
   paste, kill-ring expectations), do we fall back to `rustyline` for command mode
   only? That reintroduces two terminal owners, so decide by the end of M4. It has
   not proved too weak yet, and the buffer is tested for multibyte editing and paste
   bombs, so the answer is "not yet".
7. Ship a `vi`-style keymap preset next to `default` and `mpv`, or keep two presets?
8. Should `--status-line` default to on when stdout is a TTY, or stay opt-in?
9. The mpv path is unverified without mpv installed. Do we add a CI job that installs
   mpv, or accept mpv as a manual pre-release check?
10. A cue beyond SIGKILL: should `--help` and the panic hook also print the `stty
    sane` hint, or is documenting it once enough? Currently it is in `--help` and in
    the raw-mode failure path.
11. Should the A-B drift tolerance (300 ms) or the ffplay A-B wrap tolerance (+/-150
    ms) be exposed as flags for users who want tighter or looser loops?
12. The pty harness verifies output but not termios. Should it also assert that raw
    mode is off after `q`, Ctrl+C, and a forced panic? That would close M1's last gap.
13. With `--status-line` and a terminal narrow enough that the line must be cut, is
    the field-boundary truncation the right call, or should the readout switch to a
    short form (position and state only) below some width?
14. Should the TUI's message area be a scrollable log pane instead of three rows? It
    would make `list`, `help`, and a long run of confirmations readable, at the cost of
    a pane the transport does not need. The chosen layout keeps the frame simple and
    accepts that only the newest messages are visible.
15. Should the playlist pane be navigable (select a track, Enter to play it)? Today it
    is a view of the transport, which keeps one cursor; a second cursor would need
    keeping in step with `next`/`prev` and with a track ending on its own.
16. Should `config.json` gain a `tui` key, so a user who prefers the line interface
    does not have to remember `--no-tui` every time? The flag is the whole surface
    today, and the interface does not change what the session does.
