//! The full-screen interface: the same session, drawn over the whole window.
//!
//! It is the default when stdin *and* stdout are both a terminal. `--no-tui`, a
//! pipe, or a redirect keeps the line-oriented interface from `repl.rs`; a run with
//! no terminal at all never sees a single escape sequence.
//!
//! What it inherits matters more than what it adds:
//!
//! - **One owner of the terminal.** Drawing goes through `ratatui`, whose backend is
//!   the same `crossterm` that `term.rs` uses for raw mode and `input.rs` for events,
//!   so no second library toggles termios. The alternate screen is owned by
//!   `term::ScreenGuard`: restored on drop, on a panic, and from the signal handler.
//! - **One command grammar.** Keys still go through [`InputLayer`], so a hotkey is
//!   the same [`Command`] the parser produces and nothing is reachable only by
//!   keypress; `:` opens the same prompt, drawn on the bottom row.
//! - **No colour.** The panes use attributes only (bold, reverse), never a colour,
//!   so `NO_COLOR` is satisfied by construction exactly as it is in line mode.
//! - **Messages are still messages.** The session keeps writing to a `Write`, but in
//!   the TUI that writer is a [`MessageSink`] instead of stdout: the tail of it is
//!   drawn in the message area, so a confirmation or an error is visible and nothing
//!   is interleaved into the middle of the frame.
//!
//! Everything the panes show is gathered into a plain [`View`], and drawing is a
//! function of it. That is what makes the layout testable against ratatui's
//! `TestBackend`: no terminal, no session, no timing.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

use crate::commands::{self, Command, HelpTopic};
use crate::error::{Error, Result};
use crate::input::{Input, InputLayer, Mode};
use crate::keys::{KeyCode, Keymap};
use crate::repl::{fit, Truncate, AUTOSAVE_INTERVAL, TICK_INTERVAL};
use crate::session::{Outcome, Session};
use crate::term;
use crate::time;
use crate::transport::DEFAULT_SPEED;

/// How long the screen may go unpainted while nothing is happening.
///
/// The transport is polled 25 times a second (`TICK_INTERVAL`) because the A-B
/// clock needs that resolution, but repainting at that rate is wasted work over a
/// slow connection: the only thing that moves is the progress bar, and ten frames a
/// second is smooth. Any input paints immediately.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// Message rows kept above the hint line.
const MESSAGE_ROWS: usize = 3;

/// How much session output is remembered. Bounded so a long drill cannot grow the
/// process without limit, and deep enough that the overlay has something to scroll.
const MESSAGE_HISTORY: usize = 200;

/// Wide enough for two panes side by side. Narrower than this, the player pane gets
/// the whole width: a list squeezed into ten columns is worth less than the time
/// readout it would displace.
const TWO_PANE_MIN_WIDTH: u16 = 60;

/// The narrowest window that can hold panes, a message, and a hint.
const MIN_USEFUL_WIDTH: u16 = 12;
const MIN_USEFUL_HEIGHT: u16 = 3;

// ---------------------------------------------------------------------------
// The message sink: the session's writer, with the far end redirected
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Sink {
    /// Completed lines, oldest first.
    lines: VecDeque<String>,
    /// The line being written right now: a `write!` without a newline is a real
    /// case (the status contract is one line per command, but nothing enforces it),
    /// and dropping it would lose the message entirely.
    partial: String,
}

/// A `Write` sink that keeps the session's output as lines instead of printing it.
///
/// The session's writer stays a `Write` in the TUI, deliberately: that is how every
/// other mode feeds it, and it is what keeps `session.rs` from knowing which
/// interface is in front of it. Only the far end changes.
///
/// Cloning shares one buffer, so `main` can seed startup messages and the session
/// can append to the same lines.
#[derive(Debug, Clone, Default)]
pub struct MessageSink {
    state: Arc<Mutex<Sink>>,
}

impl MessageSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// A poisoned lock means someone panicked while holding it. The lines are still
    /// fine, so recovering beats cascading into a second panic on the way out.
    fn lock(&self) -> MutexGuard<'_, Sink> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record a complete line, as if the session had printed it.
    pub fn push(&self, line: impl Into<String>) {
        let mut sink = self.lock();
        push_line(&mut sink, line.into());
    }

    /// The last `count` lines, oldest first. A line still being written counts as
    /// the newest, so a message is visible while it is being produced.
    ///
    /// Blank lines are dropped: a blank row in a three-row message area is a gap
    /// where a message could have been.
    pub fn tail(&self, count: usize) -> Vec<String> {
        if count == 0 {
            return Vec::new();
        }
        let sink = self.lock();
        let unfinished = !sink.partial.trim().is_empty();
        let wanted = count.saturating_sub(unfinished as usize);
        // Newest first while collecting, because that is the end that matters, then
        // flipped: a message area reads top to bottom like a transcript.
        let mut out: Vec<String> = sink
            .lines
            .iter()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(wanted)
            .cloned()
            .collect();
        out.reverse();
        if unfinished {
            out.push(sink.partial.clone());
        }
        out
    }
}

fn push_line(sink: &mut Sink, line: String) {
    sink.lines.push_back(line);
    while sink.lines.len() > MESSAGE_HISTORY {
        sink.lines.pop_front();
    }
}

impl Write for MessageSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Paths and messages are already `String`s, so a lossy decode cannot damage
        // anything that could have been written in the first place.
        let text = String::from_utf8_lossy(buf);
        let mut sink = self.lock();
        for piece in text.split_inclusive('\n') {
            match piece.strip_suffix('\n') {
                Some(line) => {
                    // A `\r\n` from a Windows-oriented path is still one line.
                    sink.partial
                        .push_str(line.strip_suffix('\r').unwrap_or(line));
                    let complete = std::mem::take(&mut sink.partial);
                    push_line(&mut sink, complete);
                }
                None => sink.partial.push_str(piece),
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Nothing is buffered on the way out: the lines are already in the sink.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The view model
// ---------------------------------------------------------------------------

/// One row of the playlist pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRow {
    /// 1-based, matching `goto`, `list`, and `track=3/12`.
    pub index: usize,
    pub name: String,
    /// The track the transport is on.
    pub current: bool,
}

/// The help overlay: the same text line mode prints, on top of the panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
}

impl Overlay {
    pub fn new(title: impl Into<String>, text: &str) -> Self {
        Self {
            title: title.into(),
            lines: text.lines().map(str::to_string).collect(),
            scroll: 0,
        }
    }

    /// The last scroll offset that still shows a line. The renderer clamps again to
    /// the rows it actually has, so this only keeps the offset meaningful.
    fn scroll_by(&mut self, delta: isize) -> bool {
        let last = self.lines.len().saturating_sub(1) as isize;
        let next = (self.scroll as isize + delta).clamp(0, last) as usize;
        let changed = next != self.scroll;
        self.scroll = next;
        changed
    }
}

/// Everything the panes show, gathered once per frame.
///
/// A plain struct on purpose: rendering is then a pure function of it, which is what
/// lets the layout be asserted against a `TestBackend` instead of a terminal.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub backend: String,
    /// How the backend is controlled, in two words: this is the distinction a user
    /// can hear (a live pause versus a restart). The full flag list is `jas doctor`'s
    /// job, and `:` `backend` prints it into the message area.
    pub control: &'static str,
    pub tracks: Vec<TrackRow>,
    /// 0-based position of the current track, for the list selection.
    pub cursor: usize,
    /// The current track's name, if a track is loaded.
    pub title: Option<String>,
    pub state: &'static str,
    pub position: Duration,
    pub duration: Option<Duration>,
    pub speed: f64,
    pub ab: Option<(Duration, Option<Duration>)>,
    /// `(current pass, requested passes)`, 1-based like `repeat=2/3` in `status`.
    pub repeats: Option<(u32, u32)>,
    pub gap: Duration,
    pub loop_mode: &'static str,
    pub seed: u64,
    pub shuffled: bool,
    /// Command mode: the bottom row is the `:` prompt and owns the cursor.
    pub command_mode: bool,
    pub prompt: String,
    pub hotkeys: bool,
    pub messages: Vec<String>,
    pub overlay: Option<Overlay>,
}

impl View {
    /// Read the session into the shape the panes want. No I/O, no pulling.
    pub fn new(
        session: &Session,
        layer: &InputLayer,
        messages: Vec<String>,
        overlay: Option<Overlay>,
    ) -> Self {
        let now = session.now();
        let cursor = session.playlist.cursor();
        let tracks = session
            .playlist
            .tracks()
            .enumerate()
            .map(|(i, track)| TrackRow {
                index: i + 1,
                name: track.display_name(),
                current: i == cursor,
            })
            .collect();
        let ab = session.transport.ab().map(|ab| (ab.a, ab.b));
        let repeats = session
            .transport
            .ab()
            .and_then(|ab| ab.repeats)
            .map(|n| (session.transport.repeats_done() + 1, n));
        let gap = session.transport.ab().map(|ab| ab.gap).unwrap_or_default();
        let caps = session.capabilities();
        Self {
            backend: session.player_name().to_string(),
            control: if caps.live_pause || caps.live_seek || caps.live_speed {
                "live control"
            } else {
                "emulated control"
            },
            tracks,
            cursor,
            title: session.playlist.current().map(|t| t.display_name()),
            state: session.transport.state().as_str(),
            position: session.transport.position(now),
            duration: session.transport.duration(),
            speed: session.transport.speed(),
            ab,
            repeats,
            gap,
            loop_mode: session.playlist.loop_mode.as_str(),
            seed: session.playlist.seed(),
            shuffled: session.playlist.is_shuffled(),
            command_mode: layer.mode == Mode::Command,
            prompt: layer.prompt(),
            hotkeys: layer.hotkeys_on(),
            messages,
            overlay,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure drawing helpers
// ---------------------------------------------------------------------------

/// Draw a progress bar of exactly `width` columns, with the A and B marks in it.
///
/// Pure, and exact about its width, because the pane it sits in is a fixed number of
/// columns: a bar one column too wide would push the pane's border onto the next row
/// and make the whole frame look broken. `None` means there is no room for a bar at
/// all, and the caller shows the times instead.
///
/// A mark replaces a bar cell, but never the play head: the position is the one
/// thing that must stay readable. The consequence is that a B at the very end of the
/// track is invisible, which is the same information -- the bar is full.
pub fn render_bar(
    width: usize,
    position: Duration,
    duration: Option<Duration>,
    ab: Option<(Duration, Option<Duration>)>,
) -> Option<String> {
    let interior = width.checked_sub(2)?;
    if interior == 0 {
        return None;
    }
    let fraction = |t: Duration| -> Option<f64> {
        let total = duration?;
        if total.is_zero() {
            return None;
        }
        Some((t.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0))
    };
    let mut cells = vec!['-'; interior];
    if let Some(f) = fraction(position) {
        let head = ((f * interior as f64) as usize).min(interior - 1);
        for cell in cells.iter_mut().take(head) {
            *cell = '=';
        }
        cells[head] = if f >= 1.0 { '=' } else { '>' };
    }
    let marks = [
        ('A', ab.map(|(a, _)| a).and_then(fraction)),
        ('B', ab.and_then(|(_, b)| b).and_then(fraction)),
    ];
    for (marker, at) in marks {
        if let Some(f) = at {
            let index = ((f * interior as f64) as usize).min(interior - 1);
            if cells[index] != '>' {
                cells[index] = marker;
            }
        }
    }
    Some(format!("[{}]", cells.into_iter().collect::<String>()))
}

/// A rectangle centred in `area`, `percent_x`/`percent_y` of its size, clamped so it
/// can never leave `area`.
///
/// Plain arithmetic rather than a `Layout`: the overlay is the only thing here that
/// wants a percentage of the frame, and an expression is easier to test at the
/// degenerate sizes (one row, one column) than a constraint solver is to reason about.
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let width = ((area.width as u32 * percent_x as u32 / 100) as u16).min(area.width);
    let height = ((area.height as u32 * percent_y as u32 / 100) as u16).min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// The one-row key reminder. Short on purpose: `?` is the complete answer, and a
/// legend cut off at the right edge is worth less than a short one that fits.
pub fn hint_line(view: &View) -> String {
    if view.overlay.is_some() {
        return "esc closes this  |  ↑ / ↓ scroll".to_string();
    }
    if !view.hotkeys {
        return "hotkeys are off: type a command".to_string();
    }
    "space play/pause   ←/→ seek   a/b mark   r repeat   ? help   : command   q quit".to_string()
}

/// The transport readout, in the order that matters to someone drilling: where we
/// are, how fast, how many passes, the loop mode, then the gap. The tail is what gets
/// cut on a narrow pane, so the drill controls come before the fine print.
fn transport_line(view: &View) -> String {
    let position = time::format_time(view.position);
    let mut parts = vec![match view.duration {
        Some(d) => format!("{position} / {}", time::format_time(d)),
        None => format!("{position} / ?"),
    }];
    if view.speed != DEFAULT_SPEED {
        parts.push(format!("speed {}", view.speed));
    }
    match view.repeats {
        Some((pass, total)) => parts.push(format!("repeat {pass}/{total}")),
        None if view.ab.is_some() => parts.push("repeat off".to_string()),
        None => {}
    }
    parts.push(format!("loop {}", view.loop_mode));
    if view.gap > Duration::ZERO {
        parts.push(format!("gap {} ms", view.gap.as_millis()));
    }
    parts.join("  ")
}

fn ab_line(view: &View) -> String {
    match view.ab {
        None => "A-B none".to_string(),
        Some((a, Some(b))) => format!("A-B {} - {}", time::format_time(a), time::format_time(b)),
        Some((a, None)) => format!("A-B {} - end", time::format_time(a)),
    }
}

fn backend_line(view: &View) -> String {
    format!("backend {}   {}", view.backend, view.control)
}

/// The shuffle seed, when there is one. Its own line because it is three words longer
/// than the backend line and would push that one's tail off a narrow pane.
fn shuffle_line(view: &View) -> Option<String> {
    if view.shuffled {
        Some(format!("shuffle on (seed {})", view.seed))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Paint one frame. Never panics on a small window: below the useful size it shows
/// the one line that still carries information.
pub fn draw(frame: &mut Frame, view: &View, list: &mut ListState) {
    let area = frame.size();
    if area.width < MIN_USEFUL_WIDTH || area.height < MIN_USEFUL_HEIGHT {
        let line = format!("{} {}", view.state, time::format_time(view.position));
        frame.render_widget(
            Paragraph::new(fit(&line, area.width as usize, Truncate::Head)),
            area,
        );
        return;
    }

    let hint_height = 1;
    // Two rows of borders plus something to draw in them are what the panes need
    // before a message area is worth having.
    let message_height = (MESSAGE_ROWS as u16).min(area.height.saturating_sub(4));
    let panes_height = area.height - hint_height - message_height;
    let panes = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: panes_height,
    };
    let messages = Rect {
        x: area.x,
        y: area.y + panes_height,
        width: area.width,
        height: message_height,
    };
    let hint = Rect {
        x: area.x,
        y: area.y + panes_height + message_height,
        width: area.width,
        height: hint_height,
    };

    let playlist_width = if panes.width >= TWO_PANE_MIN_WIDTH {
        ((panes.width as usize * 38 / 100).max(20)) as u16
    } else {
        0
    };
    if playlist_width > 0 {
        draw_playlist(
            frame,
            Rect {
                x: panes.x,
                y: panes.y,
                width: playlist_width,
                height: panes.height,
            },
            view,
            list,
        );
    }
    draw_player(
        frame,
        Rect {
            x: panes.x + playlist_width,
            y: panes.y,
            width: panes.width - playlist_width,
            height: panes.height,
        },
        view,
    );
    if message_height > 0 {
        draw_messages(frame, messages, view);
    }
    draw_bottom(frame, hint, view);
    if let Some(overlay) = &view.overlay {
        draw_overlay(frame, area, overlay);
    }
}

/// The playlist pane.
///
/// The selection is the transport's current track, not a cursor of its own: two
/// cursors would need keeping in step, and `next`/`prev` already exist. So this pane
/// is a view of where the session is, and nothing here is navigable on its own.
fn draw_playlist(frame: &mut Frame, area: Rect, view: &View, list: &mut ListState) {
    let title = format!(" playlist: {} track(s) ", view.tracks.len());
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = area.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = view
        .tracks
        .iter()
        .map(|track| {
            let text = fit(
                &format!("{:>4}  {}", track.index, track.name),
                inner,
                Truncate::Head,
            );
            ListItem::new(Line::from(text))
        })
        .collect();
    list.select(Some(view.cursor));
    let widget = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("> ");
    frame.render_stateful_widget(widget, area, list);
}

/// The now-playing pane: what is playing, how far in, and how it is set up.
fn draw_player(frame: &mut Frame, area: Rect, view: &View) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let inner = area.width.saturating_sub(2) as usize;
    let title = view
        .title
        .clone()
        .unwrap_or_else(|| "nothing loaded".to_string());
    let mut lines = vec![
        Line::from(fit(&title, inner, Truncate::Head))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        Line::from(render_bar(inner, view.position, view.duration, view.ab).unwrap_or_default()),
        Line::from(fit(&transport_line(view), inner, Truncate::Head)),
        Line::from(fit(&ab_line(view), inner, Truncate::Head)),
    ];
    // The backend line is the first thing to go on a short pane: the drill settings
    // above it are what change during a session, and which backend is playing is in
    // `--doctor` and in the header of `backend` output.
    if area.height > 6 {
        lines.push(Line::from(fit(&backend_line(view), inner, Truncate::Head)));
        if let Some(shuffle) = shuffle_line(view) {
            lines.push(Line::from(fit(&shuffle, inner, Truncate::Head)));
        }
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(player_title(view));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The player pane's title: the state, and where this track sits in the playlist.
/// The counter lives here rather than on the transport row because that row is the
/// one that gets cut on a narrow window.
fn player_title(view: &View) -> String {
    if view.tracks.is_empty() {
        format!(" {} ", view.state)
    } else {
        format!(
            " {} · track {}/{} ",
            view.state,
            view.cursor + 1,
            view.tracks.len()
        )
    }
}

/// The message area: the tail of whatever the session printed, newest at the bottom
/// so it sits next to the hint line rather than drifting up and away.
fn draw_messages(frame: &mut Frame, area: Rect, view: &View) {
    let rows = area.height as usize;
    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for _ in view.messages.len()..rows {
        lines.push(Line::default());
    }
    lines.extend(
        view.messages
            .iter()
            .rev()
            .take(rows)
            .rev()
            .map(|message| Line::from(fit(message, width, Truncate::Head))),
    );
    frame.render_widget(Paragraph::new(lines), area);
}

/// The bottom row: the `:` prompt in command mode, the key reminder otherwise.
fn draw_bottom(frame: &mut Frame, area: Rect, view: &View) {
    let width = area.width as usize;
    if view.command_mode {
        // The prompt keeps its end, exactly like the line-mode prompt: that is where
        // the cursor is and where the user is typing.
        let text = fit(&view.prompt, width, Truncate::Tail);
        let column = text.width().min(width.saturating_sub(1));
        frame.render_widget(Paragraph::new(text), area);
        frame.set_cursor(area.x + column as u16, area.y);
    } else {
        frame.render_widget(
            Paragraph::new(fit(&hint_line(view), width, Truncate::Head)),
            area,
        );
    }
}

/// The help overlay. Content is whatever `help`/`?` prints in line mode, scrolled to
/// fit -- the same text, so the two interfaces cannot drift.
/// When the overlay is scrolled, the title says which slice of the text is on
/// screen, so a reader can tell there is more rather than assuming the keymap ends
/// where the window does.
fn draw_overlay(frame: &mut Frame, area: Rect, overlay: &Overlay) {
    let rect = centered_rect(80, 80, area);
    if rect.width < 6 || rect.height < 3 || overlay.lines.is_empty() {
        return;
    }
    frame.render_widget(Clear, rect);
    let rows = rect.height.saturating_sub(2) as usize;
    let width = rect.width.saturating_sub(2) as usize;
    let scroll = overlay.scroll.min(overlay.lines.len().saturating_sub(rows));
    let lines: Vec<Line> = overlay
        .lines
        .iter()
        .skip(scroll)
        .take(rows)
        .map(|line| Line::from(fit(line, width, Truncate::Head)))
        .collect();
    let last = (scroll + rows).min(overlay.lines.len());
    let title = format!(
        " {} ({}-{} of {}) ",
        overlay.title,
        scroll + 1,
        last,
        overlay.lines.len()
    );
    let paragraph =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(paragraph, rect);
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// The TUI's own state: the few things the session has no opinion about.
#[derive(Debug, Default)]
struct UiState {
    overlay: Option<Overlay>,
    /// Kept between frames so the playlist list scrolls to follow the transport.
    list: ListState,
}

/// What the UI did with one input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handled {
    /// The UI took it: redraw, and do not run it as a command.
    ByUi,
    /// Nothing in the UI wanted it; the session gets it.
    BySession,
}

impl UiState {
    fn new() -> Self {
        Self::default()
    }

    /// Give the event to the UI first.
    ///
    /// While the overlay is open it takes every key except Ctrl+C, which the loop
    /// handles before calling this because "get me out" must always work. A key it
    /// does not know is swallowed rather than passed on: otherwise a stray `n` while
    /// reading the help would skip a track behind it.
    fn step(&mut self, event: &Input, keymap: &Keymap) -> Handled {
        if self.overlay.is_some() {
            self.overlay_event(event);
            return Handled::ByUi;
        }
        if let Input::Command(cmd) = event {
            if let Some(overlay) = overlay_for(cmd, keymap) {
                self.overlay = Some(overlay);
                return Handled::ByUi;
            }
        }
        Handled::BySession
    }

    /// Apply one event to the overlay.
    ///
    /// Closing keys: Esc (either spelling), `?`, space, Enter, and `q` -- so the
    /// overlay behaves like a help window the obvious keys dismiss, and `q` twice
    /// quits. Anything else scrolls it or is swallowed; the frame is repainted after
    /// every UI event, so a scroll is on screen as soon as the key is pressed.
    fn overlay_event(&mut self, event: &Input) {
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        let close = match event {
            Input::CancelLine => true,
            // Esc arrives as a chord in hotkey mode (it is not a binding) and as
            // `CancelLine` in command mode, so both spellings close the overlay.
            Input::Chord(chord) => chord.mods.is_none() && chord.code == KeyCode::Esc,
            Input::Command(cmd) => {
                matches!(cmd, Command::Help(_) | Command::Quit | Command::Toggle)
            }
            _ => false,
        };
        if close {
            self.overlay = None;
            return;
        }
        match event {
            // Arrow keys and their vi-like neighbours, as reported by the input
            // layer when no binding claims them (see `Input::Chord`).
            Input::Chord(chord) => match (chord.mods.is_none(), chord.code) {
                (true, KeyCode::Up) | (true, KeyCode::Char('k')) => {
                    overlay.scroll_by(-1);
                }
                (true, KeyCode::Down) | (true, KeyCode::Char('j')) => {
                    overlay.scroll_by(1);
                }
                _ => {}
            },
            // `n`/`p` scroll too, so a keymap that spends the arrows on seeking
            // (the `mpv` preset) can still read the help.
            Input::Command(Command::Next) => {
                overlay.scroll_by(1);
            }
            Input::Command(Command::Prev) => {
                overlay.scroll_by(-1);
            }
            _ => {}
        }
    }
}

/// The overlay a command opens, if it is one of the two that print reference text.
///
/// The text is taken from the same source line mode prints -- `commands::HELP` and
/// `Keymap::render` -- so the TUI cannot show a help that disagrees with the CLI.
fn overlay_for(cmd: &Command, keymap: &Keymap) -> Option<Overlay> {
    match cmd {
        Command::Help(HelpTopic::General) => Some(Overlay::new("commands", commands::HELP)),
        Command::Help(HelpTopic::Keys) | Command::Keys(None) => {
            Some(Overlay::new("keys", &keymap.render()))
        }
        _ => None,
    }
}

/// Run the TUI until the user quits. Returns the process exit code.
///
/// Every message the session produces lands in `sink`; `out` is the terminal ratatui
/// draws on. The screen is handed back explicitly on both normal exits, and by `Drop`
/// on every other path -- including a panic unwind -- so `main` can print its final
/// line onto a normal screen.
pub fn run<W: Write>(
    session: &mut Session,
    layer: &mut InputLayer,
    sink: &MessageSink,
    out: W,
) -> Result<i32> {
    let mut screen = term::ScreenGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))
        .map_err(|e| Error::runtime(format!("cannot start the full-screen interface: {e}")))?;

    let mut ui = UiState::new();
    let mut last_draw = Instant::now();
    let mut last_save = Instant::now();

    loop {
        // Advance the A-B clock and react, exactly as the line-mode loop does: the
        // TUI changes what is on screen, not what the transport is doing.
        session.tick()?;
        if let Some(err) = session.last_error.take() {
            // Line mode turns this into the exit code once its input runs out. The TUI
            // cannot run out of input, so the error is shown and the session keeps
            // going; the exit code is whatever the user's exit is, and a clean `quit`
            // stays 0 even after a recovered failure.
            sink.push(format!("error: {}", err.message()));
        }

        let event = layer.poll(&session.keymap, TICK_INTERVAL)?;
        let mut repaint = false;

        match &event {
            // Before the UI and before the session: a backend failure is reported in
            // the message area, and Ctrl+C always means "get me out".
            Input::Interrupt => {
                session.save_progress();
                screen.restore();
                return Ok(crate::error::EXIT_INTERRUPTED);
            }
            Input::Idle => {}
            Input::Redraw => repaint = true,
            Input::ParseError(message) => {
                sink.push(message.clone());
                repaint = true;
            }
            Input::CancelLine => repaint = true,
            Input::Chord(_) => {}
            Input::Command(_) => repaint = true,
        }

        // The UI gets first refusal: the overlay is the only thing that takes an
        // event away from the session, and what it did (if anything) is on screen
        // immediately.
        match ui.step(&event, &session.keymap) {
            Handled::ByUi => repaint = true,
            Handled::BySession => {
                if let Input::Command(cmd) = event {
                    let outcome = session.run(cmd);
                    layer.after_submit();
                    if outcome == Outcome::Quit {
                        session.save_progress();
                        screen.restore();
                        return Ok(crate::error::EXIT_OK);
                    }
                }
            }
        }

        if repaint || last_draw.elapsed() >= REDRAW_INTERVAL {
            let messages = sink.tail(MESSAGE_ROWS);
            let view = View::new(session, layer, messages, ui.overlay.clone());
            terminal
                .draw(|frame| draw(frame, &view, &mut ui.list))
                .map_err(|e| Error::runtime(format!("cannot draw the screen: {e}")))?;
            last_draw = Instant::now();
        }

        if last_save.elapsed() >= AUTOSAVE_INTERVAL {
            session.save_progress();
            last_save = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Chord;
    use crate::player::{Capabilities, FakePlayer};
    use crate::playlist::LoopMode;
    use crate::session::SessionOptions;
    use crate::state::Store;
    use crate::transport::FakeClock;
    use ratatui::backend::TestBackend;

    // ---- The message sink ----

    #[test]
    fn the_sink_splits_output_into_lines() {
        let sink = MessageSink::new();
        write!(sink.clone(), "one\ntwo\n").unwrap();
        assert_eq!(sink.tail(5), vec!["one", "two"]);
    }

    #[test]
    fn the_sink_shows_a_line_that_has_not_ended_yet() {
        // `write!` without a newline is legal, and losing it would lose a message.
        let sink = MessageSink::new();
        write!(sink.clone(), "speed 0.75").unwrap();
        assert_eq!(sink.tail(5), vec!["speed 0.75"]);
        // And it is finished off by the next newline rather than duplicated.
        writeln!(sink.clone(), " done").unwrap();
        assert_eq!(sink.tail(5), vec!["speed 0.75 done"]);
    }

    #[test]
    fn the_sink_keeps_writes_that_arrive_in_arbitrary_chunks() {
        let sink = MessageSink::new();
        let mut writer = sink.clone();
        writer.write_all(b"par").unwrap();
        writer.write_all(b"tial\nnext").unwrap();
        writer.flush().unwrap();
        assert_eq!(sink.tail(5), vec!["partial", "next"]);
    }

    #[test]
    fn the_sink_accepts_crlf_and_keeps_the_text() {
        // A path or a redirected stream can carry CRLF; the `\r` is not a character
        // the user typed.
        let sink = MessageSink::new();
        write!(sink.clone(), "status\r\nquit\r\n").unwrap();
        assert_eq!(sink.tail(5), vec!["status", "quit"]);
    }

    #[test]
    fn the_sink_caps_its_history() {
        let sink = MessageSink::new();
        for i in 0..MESSAGE_HISTORY + 50 {
            sink.push(format!("line {i}"));
        }
        let tail = sink.tail(3);
        assert_eq!(
            tail,
            vec![
                format!("line {}", MESSAGE_HISTORY + 47),
                format!("line {}", MESSAGE_HISTORY + 48),
                format!("line {}", MESSAGE_HISTORY + 49),
            ]
        );
    }

    #[test]
    fn the_sink_tail_is_oldest_first_and_never_blank() {
        let sink = MessageSink::new();
        write!(sink.clone(), "first\n\nsecond\n   \nthird\n").unwrap();
        assert_eq!(sink.tail(3), vec!["first", "second", "third"]);
        assert_eq!(sink.tail(2), vec!["second", "third"]);
        assert!(sink.tail(0).is_empty());
    }

    #[test]
    fn clones_of_the_sink_share_one_buffer() {
        // This is what lets `main` seed startup messages and the session append to
        // the same lines.
        let sink = MessageSink::new();
        let session_side = sink.clone();
        sink.push("from main");
        writeln!(session_side.clone(), "from the session").unwrap();
        assert_eq!(sink.tail(5), vec!["from main", "from the session"]);
    }

    #[test]
    fn the_sink_keeps_non_ascii_lines_intact() {
        let sink = MessageSink::new();
        writeln!(sink.clone(), "state=paused name=第二课.wav ab=0.5-1.2 درس").unwrap();
        assert_eq!(
            sink.tail(1),
            vec!["state=paused name=第二课.wav ab=0.5-1.2 درس"]
        );
    }

    // ---- The progress bar ----

    #[test]
    fn the_bar_fills_towards_the_position() {
        let total = Some(Duration::from_secs(100));
        assert_eq!(
            render_bar(12, Duration::ZERO, total, None).unwrap(),
            "[>---------]"
        );
        assert_eq!(
            render_bar(12, Duration::from_secs(50), total, None).unwrap(),
            "[=====>----]"
        );
    }

    #[test]
    fn the_bar_is_exactly_the_width_it_was_given() {
        // One column too wide would push the pane's border onto the next row.
        for width in 3..40 {
            let bar = render_bar(
                width,
                Duration::from_secs(1),
                Some(Duration::from_secs(2)),
                None,
            )
            .unwrap_or_else(|| panic!("width {width} should fit a bar"));
            assert_eq!(bar.width(), width, "{bar:?} is not {width} wide");
        }
        assert_eq!(render_bar(2, Duration::ZERO, None, None), None);
        assert_eq!(render_bar(0, Duration::ZERO, None, None), None);
    }

    #[test]
    fn the_bar_shows_a_full_track_as_full() {
        let bar = render_bar(
            8,
            Duration::from_secs(10),
            Some(Duration::from_secs(10)),
            None,
        );
        assert_eq!(
            bar.unwrap(),
            "[======]",
            "a finished track has no head left"
        );
    }

    #[test]
    fn the_bar_says_nothing_about_an_unknown_length() {
        let bar = render_bar(8, Duration::from_secs(30), None, None).unwrap();
        assert_eq!(bar, "[------]", "no duration, no honest fraction");
        // And a zero-length file is not a division by zero.
        let bar = render_bar(8, Duration::from_secs(1), Some(Duration::ZERO), None).unwrap();
        assert_eq!(bar, "[------]");
    }

    #[test]
    fn the_bar_marks_a_and_b() {
        let ab = Some((Duration::from_secs(20), Some(Duration::from_secs(60))));
        let bar = render_bar(12, Duration::ZERO, Some(Duration::from_secs(100)), ab).unwrap();
        let cells: Vec<char> = bar.trim_matches(['[', ']']).chars().collect();
        // Ten interior columns: 20% and 60% land on 2 and 6.
        assert_eq!(cells[2], 'A', "{bar}");
        assert_eq!(cells[6], 'B', "{bar}");
        assert_eq!(bar.width(), 12);
    }

    #[test]
    fn the_bar_never_hides_the_play_head_behind_a_mark() {
        let ab = Some((Duration::from_secs(50), Some(Duration::from_secs(90))));
        let bar = render_bar(
            12,
            Duration::from_secs(50),
            Some(Duration::from_secs(100)),
            ab,
        )
        .expect("a bar");
        assert!(bar.contains('>'), "{bar:?}");
    }

    #[test]
    fn a_mark_past_the_end_is_clamped_inside_the_bar() {
        // The marks come from the transport, which clamps to the duration, but the
        // bar is the last line of defence: a mark off the end must not widen it.
        let ab = Some((Duration::from_secs(500), Some(Duration::from_secs(900))));
        let bar = render_bar(10, Duration::ZERO, Some(Duration::from_secs(100)), ab).unwrap();
        assert_eq!(bar.width(), 10, "{bar}");
    }

    // ---- The overlay rectangle ----

    #[test]
    fn the_overlay_stays_inside_the_window() {
        for (width, height) in [(80, 24), (40, 10), (12, 3), (1, 1), (200, 60)] {
            let area = Rect::new(0, 0, width, height);
            let rect = centered_rect(80, 80, area);
            assert!(
                rect.x + rect.width <= area.width && rect.y + rect.height <= area.height,
                "{rect:?} escapes {area:?}"
            );
            assert!(rect.width <= area.width && rect.height <= area.height);
        }
    }

    #[test]
    fn the_overlay_is_centred() {
        let area = Rect::new(0, 0, 100, 20);
        let rect = centered_rect(50, 50, area);
        assert_eq!((rect.width, rect.height), (50, 10));
        assert_eq!((rect.x, rect.y), (25, 5));
    }

    #[test]
    fn a_full_size_overlay_is_the_whole_window() {
        let area = Rect::new(2, 3, 40, 12);
        assert_eq!(centered_rect(100, 100, area), area);
    }

    // ---- The view ----

    fn harness(name: &str, tracks: usize) -> (Session, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "jas-tui-test-{name}-{}-{tracks}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for i in 0..tracks {
            let path = dir.join(format!("track{i}.mp3"));
            std::fs::write(&path, b"x").unwrap();
            paths.push(path);
        }
        let player = FakePlayer::with_caps(Capabilities::LIVE);
        let (session, _) = Session::new(
            SessionOptions {
                paths,
                loop_mode: Some(LoopMode::All),
                seed: 1,
                ..SessionOptions::default()
            },
            Box::new(player),
            Store::disabled(Some(dir.join("cfg"))),
            Box::new(FakeClock::new()),
            Box::new(std::io::sink()),
        );
        (session, dir)
    }

    #[test]
    fn the_view_reads_the_session() {
        let (mut session, dir) = harness("view", 3);
        session.start(Duration::ZERO).unwrap();
        session.run(Command::Goto(2));
        session.run(Command::Ab {
            a: time::TimeSpec::Absolute(Duration::from_secs(12)),
            b: Some(time::TimeSpec::Absolute(Duration::from_secs(31))),
        });
        session.run(Command::Repeat(commands::RepeatArg::Count(3)));
        session.run(Command::Gap(commands::GapArg::Set(Duration::from_millis(
            250,
        ))));
        session.run(Command::Speed(commands::SpeedArg::Set(0.75)));

        let layer = InputLayer::detached();
        let view = View::new(&session, &layer, vec!["a message".to_string()], None);

        assert_eq!(view.tracks.len(), 3);
        assert_eq!(view.cursor, 1, "goto 2 made the second track current");
        assert_eq!(view.tracks[1].index, 2);
        assert!(view.tracks[1].current);
        assert!(!view.tracks[0].current);
        assert_eq!(view.title.as_deref(), Some("track1.mp3"));
        assert_eq!(view.state, "playing");
        assert_eq!(view.speed, 0.75);
        assert_eq!(
            view.ab,
            Some((Duration::from_secs(12), Some(Duration::from_secs(31))))
        );
        assert_eq!(view.repeats, Some((1, 3)));
        assert_eq!(view.gap, Duration::from_millis(250));
        assert_eq!(view.loop_mode, "all");
        assert_eq!(view.backend, "fake");
        assert_eq!(view.control, "live control");
        assert!(!view.shuffled);
        assert_eq!(view.messages, vec!["a message".to_string()]);
        assert!(view.overlay.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_view_reports_command_mode_and_the_prompt() {
        let (session, dir) = harness("view-mode", 1);
        let mut layer = InputLayer::detached();
        layer.mode = Mode::Command;
        for c in "seek +5".chars() {
            layer.handle_chord_for_test(Chord::char(c));
        }
        let view = View::new(&session, &layer, Vec::new(), None);
        assert!(view.command_mode);
        assert_eq!(view.prompt, ":seek +5");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- Rendering, through ratatui's test backend ----

    /// Render `view` and return the rows a terminal of that size would show.
    ///
    /// The buffer is replayed the way a terminal draws it: a double-width glyph
    /// covers the cell after it, so that cell is stepped over rather than printed.
    /// Skipping it is what makes a CJK assertion below mean something -- joining the
    /// placeholder in would make every wide character look like it had a space after
    /// it.
    fn screen(view: &View, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut list = ListState::default();
        terminal
            .draw(|frame| draw(frame, view, &mut list))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut rows = Vec::new();
        let mut index = 0;
        while index < buffer.content.len() {
            let mut row = String::new();
            let mut column = 0;
            while column < width && index < buffer.content.len() {
                let symbol = buffer.content[index].symbol();
                let cell_width = symbol.width().max(1);
                row.push_str(symbol);
                index += cell_width;
                column += cell_width as u16;
            }
            rows.push(row.trim_end().to_string());
        }
        rows
    }

    fn sample() -> View {
        View {
            backend: "mpv".to_string(),
            control: "live control",
            tracks: (1..=4)
                .map(|index| TrackRow {
                    index,
                    name: format!("chapter-{index}.mp3"),
                    current: index == 2,
                })
                .collect(),
            cursor: 1,
            title: Some("chapter-2.mp3".to_string()),
            state: "playing",
            position: Duration::from_secs(41),
            duration: Some(Duration::from_secs(192)),
            speed: 0.75,
            ab: Some((Duration::from_secs(12), Some(Duration::from_secs(31)))),
            repeats: Some((2, 3)),
            gap: Duration::from_millis(250),
            loop_mode: "all",
            seed: 7,
            shuffled: false,
            command_mode: false,
            prompt: String::new(),
            hotkeys: true,
            messages: vec!["seek +5 -> 0:41 / 3:12".to_string()],
            overlay: None,
        }
    }

    fn joined(view: &View, width: u16, height: u16) -> String {
        screen(view, width, height).join("\n")
    }

    #[test]
    fn the_frame_shows_the_playlist_the_transport_and_the_backend() {
        let view = sample();
        let text = joined(&view, 100, 20);
        assert!(text.contains("playlist: 4 track(s)"), "{text}");
        assert!(text.contains("chapter-2.mp3"), "{text}");
        assert!(text.contains("0:41 / 3:12"), "{text}");
        assert!(text.contains("speed 0.75"), "{text}");
        assert!(text.contains("repeat 2/3"), "{text}");
        assert!(text.contains("gap 250 ms"), "{text}");
        assert!(text.contains("loop all"), "{text}");
        assert!(text.contains("track 2/4"), "{text}");
        assert!(text.contains("A-B 0:12 - 0:31"), "{text}");
        assert!(text.contains("backend mpv"), "{text}");
        assert!(text.contains("live control"), "{text}");
        assert!(text.contains("> "), "the current track is marked: {text}");
        assert!(
            text.contains("seek +5 -> 0:41 / 3:12"),
            "the session's last message is visible: {text}"
        );
        assert!(
            text.contains("q quit"),
            "the key reminder is on the last row: {text}"
        );
    }

    #[test]
    fn the_shuffle_seed_has_a_line_of_its_own() {
        let mut view = sample();
        view.shuffled = true;
        assert_eq!(shuffle_line(&view).as_deref(), Some("shuffle on (seed 7)"));
        let text = joined(&view, 100, 20);
        assert!(text.contains("shuffle on (seed 7)"), "{text}");
        view.shuffled = false;
        assert_eq!(shuffle_line(&view), None);
        assert!(
            !joined(&view, 100, 20).contains("shuffle"),
            "no shuffle line when the playlist is in order"
        );
    }

    #[test]
    fn the_panes_are_side_by_side_when_there_is_room_and_stacked_when_there_is_not() {
        let view = sample();
        let wide = screen(&view, 100, 20);
        // The playlist pane's title is on the first row, next to the player pane's.
        assert!(wide[0].contains("playlist: 4 track(s)"), "{:?}", wide[0]);
        assert!(
            wide[0].contains("playing"),
            "the player pane shares the row: {:?}",
            wide[0]
        );
        let narrow = screen(&view, 40, 20);
        assert!(
            !narrow[0].contains("playlist: 4"),
            "no playlist pane below the two-pane width: {:?}",
            narrow[0]
        );
        assert!(narrow[0].contains("playing"), "{:?}", narrow[0]);
        assert!(
            narrow.join("\n").contains("0:41 / 3:12"),
            "the transport is still there"
        );
    }

    #[test]
    fn command_mode_puts_the_prompt_on_the_bottom_row_and_the_cursor_after_it() {
        let mut view = sample();
        view.command_mode = true;
        view.prompt = ":seek +5".to_string();
        let rows = screen(&view, 60, 12);
        let last = rows.last().expect("a bottom row").clone();
        assert_eq!(last.trim_end(), ":seek +5", "{rows:?}");

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut list = ListState::default();
        terminal
            .draw(|frame| draw(frame, &view, &mut list))
            .unwrap();
        assert_eq!(
            terminal.get_cursor().ok(),
            Some((8, 11)),
            "the cursor sits after the typed text on the last row"
        );
    }

    #[test]
    fn an_over_long_prompt_keeps_its_end_visible() {
        let mut view = sample();
        view.command_mode = true;
        view.prompt = format!(":ab {}", "x".repeat(200));
        let rows = screen(&view, 30, 10);
        let last = rows.last().expect("a bottom row").clone();
        assert!(last.starts_with('…'), "{last:?}");
        assert!(last.ends_with("xxxx"), "{last:?}");
        assert!(last.width() <= 30, "{last:?} is {} wide", last.width());
    }

    #[test]
    fn hotkeys_off_says_so_instead_of_showing_a_key_legend() {
        let mut view = sample();
        view.hotkeys = false;
        let text = joined(&view, 80, 12);
        assert!(text.contains("hotkeys are off"), "{text}");
    }

    #[test]
    fn a_wide_name_is_drawn_whole_when_it_fits() {
        // Double-width characters are the reason the fitting is measured in columns.
        let mut view = sample();
        view.title = Some("第二课.wav".to_string());
        view.tracks[1].name = "第二课.wav".to_string();
        let text = joined(&view, 100, 20);
        assert!(text.contains("第二课.wav"), "{text}");
    }

    #[test]
    fn a_name_too_long_for_its_pane_is_cut_rather_than_wrapped() {
        // A wrapped row would push the pane's border and make the frame ragged.
        let mut view = sample();
        view.title = Some("ا".repeat(400));
        let rows = screen(&view, 60, 16);
        assert_eq!(rows.len(), 16, "the frame keeps its height: {rows:?}");
        for row in &rows {
            assert!(
                row.width() <= 60,
                "a row overflowed its window: {row:?} is {} wide",
                row.width()
            );
        }
    }

    #[test]
    fn the_messages_sit_at_the_bottom_of_their_area() {
        let mut view = sample();
        view.messages = vec!["first".to_string(), "second".to_string()];
        let rows = screen(&view, 80, 16);
        let first = rows.iter().position(|r| r.contains("first"));
        let second = rows.iter().position(|r| r.contains("second"));
        assert!(first.is_some() && second.is_some(), "{rows:?}");
        assert!(first.unwrap() < second.unwrap(), "oldest first: {rows:?}");
        assert_eq!(
            second.unwrap(),
            rows.len() - 2,
            "the newest message is next to the hint row: {rows:?}"
        );
    }

    #[test]
    fn the_help_overlay_lists_the_keymap_and_its_extent() {
        let mut view = sample();
        let map = crate::keys::build("default", crate::keys::DEFAULT_BINDINGS).0;
        view.overlay = Some(Overlay::new("keys", &map.render()));
        let text = joined(&view, 80, 24);
        assert!(text.contains("keys"), "{text}");
        assert!(text.contains("space"), "{text}");
        assert!(text.contains("ab-a"), "{text}");
        assert!(
            text.contains(&format!("of {}", map.render().lines().count())),
            "the overlay says how much there is: {text}"
        );
        // And it covers the panes: the now-playing row is gone from the middle.
        assert!(
            !text.contains("backend mpv"),
            "the overlay is opaque: {text}"
        );
    }

    #[test]
    fn the_overlay_scrolls_to_the_last_line() {
        let mut view = sample();
        let map = crate::keys::build("default", crate::keys::DEFAULT_BINDINGS).0;
        let mut overlay = Overlay::new("keys", &map.render());
        overlay.scroll = overlay.lines.len() - 1;
        let last = overlay.lines.last().unwrap().trim().to_string();
        view.overlay = Some(overlay);
        let text = joined(&view, 80, 24);
        assert!(text.contains(&last), "{text}");
    }

    #[test]
    fn a_window_too_small_for_panes_still_says_what_is_playing() {
        let view = sample();
        for (width, height) in [(1u16, 1u16), (5, 2), (11, 3), (12, 3), (20, 6), (30, 8)] {
            let rows = screen(&view, width, height);
            assert_eq!(rows.len(), height as usize, "{width}x{height}: {rows:?}");
            assert!(
                rows.iter().any(|row| !row.trim().is_empty()),
                "{width}x{height} rendered nothing at all"
            );
        }
    }

    // ---- What the keys do ----

    fn keymap() -> Keymap {
        crate::keys::build("default", crate::keys::DEFAULT_BINDINGS).0
    }

    #[test]
    fn question_mark_opens_the_keymap_overlay() {
        let mut ui = UiState::new();
        let event = Input::Command(Command::Help(HelpTopic::Keys));
        assert_eq!(ui.step(&event, &keymap()), Handled::ByUi);
        let overlay = ui.overlay.expect("the overlay is open");
        assert_eq!(
            overlay.lines,
            keymap()
                .render()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>(),
            "the TUI shows exactly what line mode prints"
        );
    }

    #[test]
    fn the_typed_help_command_opens_the_same_overlay() {
        let mut ui = UiState::new();
        assert_eq!(
            ui.step(
                &Input::Command(Command::Help(HelpTopic::General)),
                &keymap()
            ),
            Handled::ByUi
        );
        let overlay = ui.overlay.expect("the overlay is open");
        assert_eq!(overlay.lines, commands::HELP.lines().collect::<Vec<_>>());
        assert_eq!(overlay.title, "commands");
    }

    #[test]
    fn a_command_the_overlay_does_not_handle_goes_to_the_session() {
        let mut ui = UiState::new();
        assert_eq!(
            ui.step(&Input::Command(Command::Toggle), &keymap()),
            Handled::BySession
        );
        assert!(ui.overlay.is_none());
    }

    #[test]
    fn escape_space_and_question_mark_all_close_the_overlay() {
        for closer in [
            Input::CancelLine,
            Input::Chord(Chord::new(KeyCode::Esc)),
            Input::Command(Command::Toggle),
            Input::Command(Command::Help(HelpTopic::Keys)),
            Input::Command(Command::Quit),
        ] {
            let mut ui = UiState::new();
            ui.step(&Input::Command(Command::Help(HelpTopic::Keys)), &keymap());
            assert_eq!(ui.step(&closer, &keymap()), Handled::ByUi);
            assert!(ui.overlay.is_none(), "{closer:?} should have closed it");
        }
    }

    #[test]
    fn a_shifted_escape_is_not_a_close_key() {
        // Only the plain key closes: a modified Esc is a different keystroke, and
        // guessing that it also means "close" is how a keymap stops meaning what it
        // says.
        let mut ui = UiState::new();
        ui.overlay = Some(Overlay::new("x", "one\ntwo"));
        ui.overlay_event(&Input::Chord(Chord::with(
            KeyCode::Esc,
            crate::keys::Mods {
                ctrl: false,
                alt: false,
                shift: true,
            },
        )));
        assert!(ui.overlay.is_some());
    }

    #[test]
    fn arrows_and_j_k_scroll_the_overlay_within_its_bounds() {
        let mut ui = UiState::new();
        let long = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        ui.overlay = Some(Overlay::new("long", &long));
        let lines = ui.overlay.as_ref().expect("still open").lines.len();
        // Scrolling up at the top does nothing, and does not underflow.
        ui.overlay_event(&Input::Chord(Chord::new(KeyCode::Up)));
        assert_eq!(ui.overlay.as_ref().unwrap().scroll, 0);
        for _ in 0..200 {
            ui.overlay_event(&Input::Chord(Chord::new(KeyCode::Down)));
        }
        assert_eq!(
            ui.overlay.as_ref().unwrap().scroll,
            lines - 1,
            "clamped at the end"
        );
        ui.overlay_event(&Input::Chord(Chord::char('k')));
        assert_eq!(ui.overlay.as_ref().unwrap().scroll, lines - 2);
        ui.overlay_event(&Input::Chord(Chord::char('j')));
        assert_eq!(ui.overlay.as_ref().unwrap().scroll, lines - 1);
    }

    #[test]
    fn a_key_the_overlay_does_not_know_is_swallowed() {
        // Otherwise `n` while reading the help would skip a track behind it.
        let mut ui = UiState::new();
        ui.step(&Input::Command(Command::Help(HelpTopic::Keys)), &keymap());
        assert_eq!(
            ui.step(&Input::Command(Command::Next), &keymap()),
            Handled::ByUi,
            "Next scrolls rather than navigating"
        );
        assert!(ui.overlay.is_some());
        assert_eq!(
            ui.step(
                &Input::Command(Command::Speed(commands::SpeedArg::Nudge(0.05))),
                &keymap()
            ),
            Handled::ByUi
        );
        assert!(ui.overlay.is_some(), "the overlay is still open");
    }

    #[test]
    fn n_and_p_scroll_so_the_mpv_preset_can_read_the_help() {
        let mut ui = UiState::new();
        ui.overlay = Some(Overlay::new("x", "a\nb\nc\nd\ne"));
        ui.overlay_event(&Input::Command(Command::Prev));
        assert_eq!(ui.overlay.as_ref().unwrap().scroll, 0, "already at the top");
        ui.overlay_event(&Input::Command(Command::Next));
        assert_eq!(ui.overlay.as_ref().unwrap().scroll, 1);
    }

    #[test]
    fn overlay_for_leaves_every_other_command_alone() {
        let map = keymap();
        assert!(overlay_for(&Command::Toggle, &map).is_none());
        assert!(overlay_for(&Command::Status, &map).is_none());
        assert!(overlay_for(&Command::Keys(Some("mpv".to_string())), &map).is_none());
        assert!(overlay_for(&Command::Keys(None), &map).is_some());
    }
}
