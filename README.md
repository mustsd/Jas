# Jas

A local audio player for language learners, on macOS, Linux, and Windows.

Built for shadowing drills: mark a phrase, loop it, slow it down, repeat it five
times, and have it remember where you gave up yesterday. It is a command-line
program — no window, no TUI, no playlist library to maintain.

- Plays mp3 and other common formats through an external player.
- Loops the playlist by default.
- One command grammar, reachable three ways: single keys on a terminal, a `:`
  prompt, or a script on stdin.

## Install

Jas needs an external player. **ffmpeg (which ships `ffplay`) is enough**; mpv is
better if you have it.

| | macOS | Debian/Ubuntu | Windows |
|---|---|---|---|
| required | `brew install ffmpeg` | `sudo apt install ffmpeg` | `winget install Gyan.FFmpeg` |
| recommended | `brew install mpv` | `sudo apt install mpv` | `winget install mpv` |

Then build Jas itself:

```sh
cargo build --release      # target/release/jas
```

| What | Where |
|---|---|
| Architecture, rules, and known gaps | `PLANS.md` |
| Per-backend contract (capabilities, exit status) | `docs/backends.md` |
| Verification gate | `bash scripts/ainiux/check` |

`jas --doctor` prints what was found, the versions, and which backend it picked.
Run that first if anything seems wrong; it is the fastest way to see the problem.

### Why two backends

`mpv` can pause, seek, and change speed *live*, over a JSON socket. `ffplay` has no
control channel at all, so Jas restarts it for every pause or seek — a fraction of a
second of silence each time, and A-B loops are accurate to about ±150 ms instead of
exactly. Jas prefers mpv automatically and says so in `--doctor`.

## Quick start

```sh
jas lesson.mp3                  # play one file, keyboard control
jas ~/audio/chinese/            # a whole directory, sorted naturally (ch2 before ch10)
jas mix.m3u                     # a playlist file
jas --play mix.m3u < drill.txt  # scripted: no keyboard, commands from a file
```

Nothing plays until you ask. Press <kbd>Space</kbd> to start, or pass `--play`.

## The screen

There are no windows and no full-screen mode, but the prompt and the optional status
line are drawn in place, so the layout follows two rules a user will notice:

- **Every message starts at its own line.** Type a command, press Enter, and what you
typed stays visible with the result underneath it, rather than the result appearing
mid-line after the prompt.
- **A live line is never wider than the terminal.** At `--status-line`, the readout is
shortened to fit instead of wrapping: its head is kept, cut at a field boundary, so
`state=paused track=1/1` never ends mid-token. Type a command longer than the window
and the prompt keeps its *end* visible, marked with `…`, because that is where you are
typing.

The status line is repainted only when its text changes, so it does not flicker or
flood a slow connection.

## Keyboard (the default keymap)
| Key | Does |
|---|---|
| <kbd>Space</kbd> / <kbd>Enter</kbd> | play / pause |
| <kbd>←</kbd> <kbd>→</kbd> | seek ∓5 s |
| <kbd>Shift</kbd>+<kbd>←</kbd> <kbd>→</kbd> | seek ∓30 s |
| <kbd>n</kbd> / <kbd>p</kbd> | next / previous track |
| <kbd>[</kbd> / <kbd>]</kbd> | speed ∓0.05 (pitch preserved) |
| <kbd>\</kbd> | speed back to 1.0 |
| <kbd>a</kbd> / <kbd>b</kbd> | mark A / mark B at the current position |
| <kbd>c</kbd> | clear A-B |
| <kbd>r</kbd> | repeat count: off → 2 → 3 → 5 → 10 → off |
| <kbd>g</kbd> | add 250 ms of silence between repeats |
| <kbd>l</kbd> | loop mode: off → all → one |
| <kbd>:</kbd> | the command prompt (everything below works there) |
| <kbd>?</kbd> | the keymap |
| <kbd>q</kbd> / <kbd>Ctrl</kbd>+<kbd>D</kbd> | quit |
| <kbd>Ctrl</kbd>+<kbd>C</kbd> | quit (exit code 130) |

`--keys mpv` switches to the keys mpv users already know. `--no-keys` turns hotkeys
off and starts at the `:` prompt, which is what you want when scripting.

Every key is a shortcut for a command, and nothing is reachable *only* by a key, so
an unbound or misconfigured key never blocks a feature: type it after `:` instead.

## Commands

```text
play | pause | toggle            transport control
next | prev | goto <N>           playlist navigation (N is 1-based)
seek <T> | seek +T | seek -T     absolute, forward, backward
speed <X> | speed +X | speed -X  playback speed (0.25-4.0)
ab <A> <B> | ab <A> | ab clear   set or clear the A-B loop
ab-a | ab-b                      mark A and B at the current position
repeat <N|off|next|cycle>        segment repeat count
gap <MS> | gap +MS | gap -MS     pause between repeats
loop [off|all|one]               loop mode (no value: cycle)
shuffle [on|off]                 seeded shuffle (no value: toggle)
list | status | save             info and session control
backend [name] | keys [preset]   show or switch backend and keymap
help [keys] | quit               this list, the keymap, and exit
```

Times are `SS`, `MM:SS`, or `HH:MM:SS`, each with an optional `.ms`. A leading `+`
or `-` means "relative to here".

`status` is one line with a stable field order, meant for logging:

```text
state=playing track=1/12 name=ch1.mp3 pos=41.2 dur=192.0 speed=1 ab=12.0-31.0 repeat=2/3 gap=250 loop=all backend=mpv
```

## Drilling

```sh
jas --play lesson.mp3
# Space to start, then:
#   →  →  →        nudge to the phrase
#   a              mark A
#   ←              back up a touch
#   b              mark B
#   r  r           repeat 3 times
#   [              slow to 0.95
```

Or all at once, from the command line:

```sh
jas --play --ab-a 0:12 --ab-b 0:31 --repeat 3 --speed 0.75 lesson.mp3
```

Jas remembers, per file: the resume position, the A-B marks, the speed, the repeat
count, and the gap. It keys those on the canonical path plus the file's size and
modification time, so editing the audio correctly invalidates a stale resume point.
`--no-state` disables all of it.

## Scripting

The same grammar works line by line on stdin, so a drill can be a file:

```sh
printf 'ab 0:12 0:31\nrepeat 3\nspeed 0.75\nplay\nstatus\nquit\n' > drill.txt
jas --play lesson.mp3 < drill.txt
```

A line that does not parse is reported and the run continues, so one typo in a long
script does not lose the session. Exit codes: `0` success, `1` usage error, `2`
runtime failure (a backend died), `3` nothing playable, `130` interrupted.

`--list` prints the resolved playlist (natural sort applied) and `--list --json`
prints it as JSON — both work without any audio backend installed, which makes them
safe to use in CI.

## Configuration

Jas reads two files from the OS config directory (`~/.config/jas/` on Linux,
`~/Library/Application Support/jas/` on macOS, `%APPDATA%\jas\` on Windows). Use
`--config <dir>` to point somewhere else.

`state.json` is written by Jas. **`config.json` is read-only** — create it yourself:

```json
{
  "backend": "mpv",
  "speed": 0.9,
  "loop_mode": "one",
  "keys": "default",
  "status_line": true,
  "keymap": {
    "z": "toggle",
    "shift+space": "seek +15",
    "ctrl+r": "repeat cycle",
    "x": "quit"
  }
}
```

Precedence for `speed` and `loop_mode` is command line, then `config.json`, then the
built-in default. A keymap entry that does not parse is reported at startup with the
offending entry and then ignored, so a stale config can never stop playback; run
`keys` to see the effective map. Chords are `ctrl+`/`alt+`/`shift+` plus a key name
(`left`, `space`, `enter`, `esc`, `backspace`, `delete`, `tab`, `home`, `end`) or a
single character.

If `state.json` is ever corrupt it is moved aside as `state.json.corrupt-0` with a
warning rather than deleted or allowed to stop startup.

## Troubleshooting

**"no audio backend found"** — install ffmpeg or mpv (see above), then `jas --doctor`.

**My terminal has no echo left.** Only `SIGKILL` can do this; Jas restores the
terminal on quit, on error, on Ctrl+C, and on a panic.

```sh
stty sane
```

**Pauses click, and A-B loops are slightly loose.** You are on the `ffplay`
fallback. Install mpv for live control, or `--backend mpv` to insist on it.

**The wrong thing is playing / it did not start.** Without `--play`, Jas loads and
waits for input. On a pipe with no commands it exits silently — that is the design,
not a bug. Add `--play`.

## Status

This is an early implementation. What has actually been verified, and what has not:

- 294 unit tests and 21 integration tests, two pty layout checks (80 columns, and 40
  columns with a double-width Chinese filename), and a 20-check end-to-end smoke
  script, all passing on macOS with `ffplay` installed. `bash scripts/ainiux/check`
  runs them together with `cargo fmt --check` and `cargo clippy -D warnings`.
- The `ffplay` backend is exercised against real generated audio.
- **The `mpv` backend is not verified.** Its code and tests exist, but mpv is not
  installed on the development machine, so those tests skip. Treat live
  pause/seek/speed as unproven until `jas --doctor` says mpv and you try it.
- **Windows is unverified.** Key handling, Ctrl+C, and console restore have never
  been run there. mpv's named-pipe IPC is not implemented on Windows, so `auto`
  falls through to `ffplay` there.
- No pty test asserts that raw mode is left off after `q`, Ctrl+C, or a panic. The
  pty harness drives a real terminal but only reads output; the guard itself is tested
  against a fake terminal. The release-build and three-platform CI matrix in the plan
  is not set up either.
- Nobody has listened to the audio: quality claims (pitch preservation, seam
  smoothness) are design intent, not measurements.

There is no `config.json` writer: Jas reads that file but never creates it, so the
schema above is something you type. `state.json` is written for you.

`PLANS.md` is the working plan and carries the full list of gaps, including the
open questions; `docs/backends.md` is the per-backend reference.
