# Backends

Jas has no audio code of its own. It drives an external player and owns the
*logical* position itself, so the same commands behave the same way regardless of
what is installed. This document is the reference for what each backend can and
cannot do, and how the difference is hidden (or not).

## Choosing one

| | `mpv` | `ffplay` | OS opener |
|---|---|---|---|
| ships with | mpv | ffmpeg | the OS |
| control channel | JSON IPC over a Unix socket | none | none |
| live pause | yes | no — the process is killed | no |
| live seek | yes | no — restart with `-ss` | no |
| live speed | yes | no — restart with `-af atempo=` | no |
| A-B loop | native (`ab-loop-a`/`ab-loop-b`) | emulated by polling, ±150 ms | not supported |
| reports duration | yes | no | no |
| reports position | yes | no | no |
| auto-order | 1st | 2nd | 3rd (open and forget) |

`--backend auto` (the default) probes in that order and takes the first hit.
`--backend mpv|ffplay|native` forces one, and an explicitly requested but missing
backend is an error rather than a silent fallback — otherwise `--backend mpv`
would quietly do something else, which is worse than failing.

`jas doctor` prints exactly what was found, its version, and the capabilities Jas
resolved. Ask for that first in a bug report.

### Why mpv is preferred over ffplay

The initial survey answer was "ffplay first, then mpv". That is reversed on
purpose: `ffplay -nodisp` has no control channel, so with it as the primary backend
every pause, seek, and speed change costs a process restart — an audible gap of
roughly 100–300 ms, and an A-B loop that can only be as accurate as the polling
interval. mpv is one optional install that makes the drill features precise. The
fallback chain still contains ffplay, so "works with just ffmpeg" is preserved.

## Capabilities, not names

Nothing in the session asks "is this mpv?". It asks what the backend can do:

```rust
struct Capabilities {
    live_pause: bool,
    live_seek: bool,
    live_speed: bool,
    native_ab_loop: bool,
    position_feedback: bool,
    duration_feedback: bool,
}
```

The transport decides *what* should happen (an `Effect`); the session decides *how*
based on those flags:

| Effect | `live_seek` set | otherwise |
|---|---|---|
| `Play { at }` | unpause; the offset is already correct | `load(at)` then play |
| `Pause` | set the pause property | kill the child |
| `RespawnAt { at }` | `seek(at)` | `load(at)` then play |
| `Stop` | stop the file | kill the child |

So a backend with live seek but not live speed still works: the speed change
respawns at the held position. Adding a third backend means filling in those six
flags honestly, not touching the session.

## How a track ends

A backend must report a child that ended on its own through `poll_exit()`, which
returns `Finished` or `Failed` and reports each exit **once**:

```rust
fn poll_exit(&mut self) -> Option<Exit>;   // None = still running, or not applicable
```

This is deliberately not an "is it alive?" boolean, and the difference is not
cosmetic. A backend with no `duration_feedback` signals the end of a track *by
exiting*: `ffplay -autoexit` returns 0 when the file ends. Conflating "exited" with
"died" makes every finished track look like a crash, which stops the playlist
instead of advancing it -- exactly what happened before this distinction existed.

So the contract is:

- **`Finished`** (exit status success): the track played to its end. The session
  advances to the next track per the loop mode. This is the *only* end-of-track
  signal when the duration is unknown, because the transport has nothing to compare
  its clock against.
- **`Failed`** (nonzero exit, or a failed wait): a decode error or a crashed player.
  The session stops, reports it with `diagnostics()`, and exits 2.
- **`None`**: still running, or the backend has no per-track child at all. mpv is one
  long-lived idle process, so it returns `None` until it really dies.

Pausing kills the child for the respawn strategy, so the child handle is gone before
anyone polls it; the session only asks while sound is expected
(`!transport.is_silent()`), which is what stops a deliberate pause from being read
as a failure.

## Position is computed, never accumulated

```
position(t) = clamp(anchor_pos + (t - anchor_t0) * speed, 0, duration)
```

Every state change re-anchors `(anchor_pos, anchor_t0)`. Monotonic time only, so an
NTP step or a DST change cannot make the position jump. This is why the ffplay
backend needs no timing state of its own: it only ever answers "spawn at offset T".

When `position_feedback` is set, a backend-reported position corrects drift — but
only when the disagreement exceeds 300 ms (`DRIFT_TOLERANCE`), because the computed
clock is steadier than a report stream and correcting jitter would make the
position jitter too.

## mpv specifics

```
mpv --idle=yes --no-video --really-quiet --no-terminal \
    --no-input-default-bindings --keep-open=no \
    --input-ipc-server=<tmpdir>/jas-mpv-<pid>.sock
```

Two flags are load-bearing:

- **`--no-terminal`** — without it mpv reads the same keystrokes as Jas, so `q`
  would quit the player instead of the session.
- **`--idle=yes`** plus `loadfile` over IPC — one long-lived process. Spawning mpv
  per track would reintroduce exactly the restart cost that made it worth preferring.

Protocol notes:

- Newline-delimited JSON, one request per line, with a `request_id` so replies can
  be matched. Replies that do not match are stale and are skipped.
- A 500 ms per-read timeout keeps a busy mpv from freezing the session loop.
- Unknown fields and asynchronous events (`end-file`, `idle`) are ignored rather
  than treated as errors, so a newer mpv cannot break the session.
- Paths are JSON-escaped, so quotes, backslashes, control characters, and non-ASCII
  text are all safe; a newline in a path cannot terminate the message early.
- The socket lives in the temp dir, is unlinked before spawning (a stale socket
  would stop mpv binding) and removed on exit, including after a failed handshake.

**Not implemented: Windows.** The named-pipe client does not exist, so on Windows
`jas doctor` reports mpv as found-but-unusable and `auto` falls through to ffplay.

## ffplay specifics

```
ffplay -nodisp -autoexit -hide_banner -nostats -loglevel error \
       [-ss <seconds>] [-af <atempo chain>] -- <path>
```

- `-nodisp` means no window, but it also means **no control channel at all**. That
  is the whole reason for the respawn strategy.
- stdin is `Stdio::null()`. Without that, `ffplay` eats the keystrokes Jas needs.
- The `--` separator keeps a file named like a flag (`-weird.mp3`) from being parsed
  as an option.
- `-ss` takes milliseconds-as-seconds (`1.500`), so a seek is not rounded to whole
  seconds.
- Speed uses `-af atempo=X`. A single `atempo` only accepts 0.5–2.0, so values
  outside that are chained: `atempo=0.5,atempo=0.5,atempo=0.8000` for 0.1x.
  `t` (pitch-preserving) is left at its default.
- stderr is piped and drained on a thread, and the last line is reported when the
  backend dies unexpectedly. A pipe nobody reads would block the child.
- The child is killed and reaped on pause, stop, and drop, so a dropped player
  cannot leave audio playing. **Job objects / process groups are not used**, so a
  child that forks would outlive Jas; ffplay does not fork.

## What is *not* hidden

Being honest about the seams matters more than pretending they are not there:

- An emulated A-B wrap is accurate to about ±150 ms, i.e. one polling interval.
  A shadowing drill will not notice; a musician might. It is named as emulated in
  the docs rather than described as exact.
- A pause on ffplay is a process kill, so there is a short gap before the sound
  stops and a short silence before it resumes.
- Speed changes on ffplay restart the file at the held offset, so there is a
  discontinuity in the audio even though the position is preserved.
- `--backend auto` picks the first *usable* backend, so installing mpv later changes
  behaviour between runs. `jas doctor` and the startup banner both state which one
  was chosen.
