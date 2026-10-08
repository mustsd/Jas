//! RAII terminal guard: raw mode in, raw mode out on every path (R10).
//!
//! Requirement R10 says the terminal must never be left in raw mode on any exit
//! path: quit, error, panic, or signal. Three mechanisms cover it:
//!
//! 1. [`TerminalGuard`] is RAII: its `Drop` restores the terminal, and `Drop`
//!    runs during a panic unwind too.
//! 2. [`install_panic_hook`] wraps the previous hook so a panic restores first.
//! 3. [`install_signal_handlers`] restores from the signal handler before exiting.
//!
//! The full-screen TUI adds a second thing that must be handed back: the
//! **alternate screen** and the hidden cursor. [`ScreenGuard`] covers it with the
//! same three mechanisms -- RAII, the panic hook, and the signal handler -- so the
//! TUI cannot strand a user on a blank screen any more than it can leave echo off.
//!
//! A hard `SIGKILL` cannot be covered by any of these, so `--help` and the README
//! document `stty sane` as the recovery.
//!
//! The guard owns its [`RawMode`] implementation, so the object that enabled raw
//! mode is the same object that disables it. That matters more than it sounds:
//! a guard holding a *different* handle than the one that flipped termios would
//! report success while leaving the shell with echo off.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crossterm::cursor::{Hide, Show};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

use crate::error::{Error, Result};

const STATE_OFF: u8 = 0;
const STATE_RAW: u8 = 1;
const STATE_RESTORED: u8 = 2;

/// The alternate-screen state, in the same shape as `RAW_STATE`: a flag the panic
/// hook and the signal handler can read without holding the guard.
const ALT_OFF: u8 = 0;
const ALT_ON: u8 = 1;

static ALT_SCREEN: AtomicU8 = AtomicU8::new(ALT_OFF);

static RAW_STATE: AtomicU8 = AtomicU8::new(STATE_OFF);
static RESTORED_AT_LEAST_ONCE: AtomicBool = AtomicBool::new(false);

/// The minimum a terminal needs to support for Jas.
pub trait RawMode {
    fn enable(&mut self) -> io::Result<()>;
    fn disable(&mut self) -> io::Result<()>;
    /// True when this handle is attached to a real terminal.
    fn is_tty(&self) -> bool;
}

/// The real terminal, via `crossterm`.
pub struct RealTerminal;

impl RawMode for RealTerminal {
    fn enable(&mut self) -> io::Result<()> {
        crossterm::terminal::enable_raw_mode()
    }

    fn disable(&mut self) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }

    fn is_tty(&self) -> bool {
        io::stdin().is_terminal() || io::stdout().is_terminal()
    }
}

/// A stand-in used by tests. Its counters live behind `Rc<Cell<..>>`, so a clone
/// handed to a guard still reports into the original.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub struct FakeTerminal {
    raw: std::rc::Rc<std::cell::Cell<bool>>,
    enable_calls: std::rc::Rc<std::cell::Cell<usize>>,
    disable_calls: std::rc::Rc<std::cell::Cell<usize>>,
    tty: bool,
    fail_enable: bool,
    fail_disable: bool,
}

#[cfg(test)]
impl FakeTerminal {
    /// A fake that claims to be a TTY.
    pub fn tty() -> Self {
        Self {
            tty: true,
            ..Self::default()
        }
    }

    /// A fake that claims not to be a TTY.
    pub fn pipe() -> Self {
        Self::default()
    }

    pub fn failing_enable() -> Self {
        Self {
            tty: true,
            fail_enable: true,
            ..Self::default()
        }
    }

    pub fn failing_disable() -> Self {
        Self {
            tty: true,
            fail_disable: true,
            ..Self::default()
        }
    }

    pub fn is_raw(&self) -> bool {
        self.raw.get()
    }

    pub fn enable_calls(&self) -> usize {
        self.enable_calls.get()
    }

    pub fn disable_calls(&self) -> usize {
        self.disable_calls.get()
    }
}

#[cfg(test)]
impl RawMode for FakeTerminal {
    fn enable(&mut self) -> io::Result<()> {
        self.enable_calls.set(self.enable_calls.get() + 1);
        if self.fail_enable {
            return Err(io::Error::other("enable failed"));
        }
        self.raw.set(true);
        Ok(())
    }

    fn disable(&mut self) -> io::Result<()> {
        self.disable_calls.set(self.disable_calls.get() + 1);
        if self.fail_disable {
            return Err(io::Error::other("disable failed"));
        }
        self.raw.set(false);
        Ok(())
    }

    fn is_tty(&self) -> bool {
        self.tty
    }
}

/// Owns the raw-mode state for the whole interactive session.
pub struct TerminalGuard {
    mode: Box<dyn RawMode>,
    active: bool,
}

impl TerminalGuard {
    /// Enter raw mode if we really have a terminal. Returns a guard either way, so
    /// callers never branch on TTY-ness themselves.
    pub fn enter(enabled: bool) -> Result<Self> {
        Self::enter_with(Box::new(RealTerminal), enabled)
    }

    /// The testable form: any [`RawMode`] implementation, taken by value so the
    /// guard disables exactly what it enabled.
    pub fn enter_with(mut mode: Box<dyn RawMode>, enabled: bool) -> Result<Self> {
        if !enabled || !mode.is_tty() {
            return Ok(Self {
                mode,
                active: false,
            });
        }
        match mode.enable() {
            Ok(()) => {
                RAW_STATE.store(STATE_RAW, Ordering::SeqCst);
                Ok(Self { mode, active: true })
            }
            Err(e) => Err(Error::runtime(format!(
                "cannot switch the terminal to raw mode: {e}"
            ))),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Restore now. Called by `Drop`; also callable directly when the order
    /// matters, e.g. before printing a final message.
    pub fn restore(&mut self) {
        if self.active {
            self.active = false;
            if let Err(e) = self.mode.disable() {
                // Never fail the exit path because restore failed; report it.
                eprintln!("jas: warning: could not restore the terminal: {e}");
            }
            RAW_STATE.store(STATE_RESTORED, Ordering::SeqCst);
            RESTORED_AT_LEAST_ONCE.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// The terminal width in columns, or `None` when there is no terminal. Used to keep
/// in-place lines from wrapping.
pub fn terminal_width() -> Option<usize> {
    crossterm::terminal::size()
        .ok()
        .map(|(cols, _)| cols as usize)
}

/// True while raw mode is believed to be on. Used by the pty test.
#[cfg(test)]
pub fn is_raw() -> bool {
    RAW_STATE.load(Ordering::SeqCst) == STATE_RAW
}

/// True once a restore has happened at least once. Used by the pty test.
#[cfg(test)]
pub fn was_restored() -> bool {
    RESTORED_AT_LEAST_ONCE.load(Ordering::SeqCst)
}

/// Restore the terminal from a context where no guard is reachable: a panic hook
/// or a signal handler. Safe to call when nothing is raw.
pub fn emergency_restore() {
    // The alternate screen is left first: anything printed while still on it would
    // be thrown away when the screen is switched back.
    leave_alt_screen();
    if RAW_STATE.load(Ordering::SeqCst) == STATE_RAW {
        let _ = crossterm::terminal::disable_raw_mode();
        RAW_STATE.store(STATE_RESTORED, Ordering::SeqCst);
        RESTORED_AT_LEAST_ONCE.store(true, Ordering::SeqCst);
        // A newline keeps the shell prompt off the line the status line used.
        let _ = io::stdout().flush();
        eprintln!();
    }
}
/// The screen switches the TUI needs that raw mode does not cover.
pub trait ScreenMode {
    /// Take over the whole window and hide the cursor.
    fn enter(&mut self) -> io::Result<()>;
    /// Hand the window back and show the cursor again.
    fn leave(&mut self) -> io::Result<()>;
}

/// The real screen, via `crossterm` on stdout.
pub struct RealScreen;

impl ScreenMode for RealScreen {
    fn enter(&mut self) -> io::Result<()> {
        // `execute!` flushes, so the switch is not left sitting in a buffer.
        crossterm::execute!(io::stdout(), EnterAlternateScreen, Hide)
    }

    fn leave(&mut self) -> io::Result<()> {
        crossterm::execute!(io::stdout(), LeaveAlternateScreen, Show)
    }
}

/// A stand-in used by tests, with the same shared-counter shape as [`FakeTerminal`].
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub struct FakeScreen {
    on_alt: std::rc::Rc<std::cell::Cell<bool>>,
    enter_calls: std::rc::Rc<std::cell::Cell<usize>>,
    leave_calls: std::rc::Rc<std::cell::Cell<usize>>,
    fail_enter: bool,
}

#[cfg(test)]
impl FakeScreen {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn failing_enter() -> Self {
        Self {
            fail_enter: true,
            ..Self::default()
        }
    }

    pub fn is_on_alt(&self) -> bool {
        self.on_alt.get()
    }

    pub fn enter_calls(&self) -> usize {
        self.enter_calls.get()
    }

    pub fn leave_calls(&self) -> usize {
        self.leave_calls.get()
    }
}

#[cfg(test)]
impl ScreenMode for FakeScreen {
    fn enter(&mut self) -> io::Result<()> {
        self.enter_calls.set(self.enter_calls.get() + 1);
        if self.fail_enter {
            return Err(io::Error::other("enter failed"));
        }
        self.on_alt.set(true);
        Ok(())
    }

    fn leave(&mut self) -> io::Result<()> {
        self.leave_calls.set(self.leave_calls.get() + 1);
        self.on_alt.set(false);
        Ok(())
    }
}

/// Owns the alternate screen for as long as the TUI runs.
///
/// The same discipline as [`TerminalGuard`]: the object that entered is the object
/// that leaves, and `Drop` runs it on every path, including a panic unwind. The
/// global flag is what the panic hook and the signal handler consult, because
/// neither can reach this guard's owner.
pub struct ScreenGuard {
    mode: Box<dyn ScreenMode>,
    active: bool,
}

impl ScreenGuard {
    /// Take over the alternate screen.
    pub fn enter() -> Result<Self> {
        Self::enter_with(Box::new(RealScreen))
    }

    /// The testable form: any [`ScreenMode`], taken by value.
    pub fn enter_with(mut mode: Box<dyn ScreenMode>) -> Result<Self> {
        match mode.enter() {
            Ok(()) => {
                ALT_SCREEN.store(ALT_ON, Ordering::SeqCst);
                Ok(Self { mode, active: true })
            }
            Err(e) => Err(Error::runtime(format!(
                "cannot switch to the alternate screen: {e}"
            ))),
        }
    }

    /// Hand the screen back now. Called by `Drop`; also callable directly when the
    /// order matters, e.g. before printing a final message.
    pub fn restore(&mut self) {
        if self.active {
            self.active = false;
            if let Err(e) = self.mode.leave() {
                eprintln!("jas: warning: could not restore the screen: {e}");
            }
            ALT_SCREEN.store(ALT_OFF, Ordering::SeqCst);
        }
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// True while the alternate screen is believed to be in use. Used by the pty test.
#[cfg(test)]
pub fn is_on_alt_screen() -> bool {
    ALT_SCREEN.load(Ordering::SeqCst) == ALT_ON
}

/// Leave the alternate screen and show the cursor, from a context that cannot
/// reach a [`ScreenGuard`]. Safe to call when the TUI never started.
fn leave_alt_screen() {
    if ALT_SCREEN.load(Ordering::SeqCst) == ALT_ON {
        ALT_SCREEN.store(ALT_OFF, Ordering::SeqCst);
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

/// Wrap the current panic hook so a panic restores the terminal first.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        emergency_restore();
        previous(info);
    }));
}

/// Restore the terminal on SIGINT/SIGTERM, then exit with the shell's convention
/// for an interrupted process. On Windows this covers Ctrl+C and console close.
pub fn install_signal_handlers() -> Result<()> {
    ctrlc::set_handler(|| {
        emergency_restore();
        std::process::exit(crate::error::EXIT_INTERRUPTED);
    })
    .map_err(|e| Error::runtime(format!("cannot install the signal handler: {e}")))
}

/// The message shown when someone's terminal is left broken by a `SIGKILL`.
pub const RAW_MODE_RECOVERY_HINT: &str = "If your terminal is left without echo, run: stty sane";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_guard_never_touches_the_terminal() {
        let fake = FakeTerminal::tty();
        {
            let guard = TerminalGuard::enter_with(Box::new(fake.clone()), false).unwrap();
            assert!(!guard.is_active());
        }
        assert_eq!(fake.enable_calls(), 0);
        assert_eq!(fake.disable_calls(), 0);
    }

    #[test]
    fn a_non_tty_is_never_switched_to_raw_mode() {
        let fake = FakeTerminal::pipe();
        let guard = TerminalGuard::enter_with(Box::new(fake.clone()), true).unwrap();
        assert!(!guard.is_active());
        drop(guard);
        assert_eq!(fake.enable_calls(), 0);
        assert_eq!(fake.disable_calls(), 0);
        assert!(!fake.is_raw());
    }

    #[test]
    fn the_guard_restores_the_terminal_it_enabled() {
        let fake = FakeTerminal::tty();
        {
            let guard = TerminalGuard::enter_with(Box::new(fake.clone()), true).unwrap();
            assert!(guard.is_active());
            assert!(fake.is_raw());
        }
        assert_eq!(fake.enable_calls(), 1);
        assert_eq!(fake.disable_calls(), 1);
        assert!(!fake.is_raw(), "raw mode survived the guard");
    }

    #[test]
    fn the_guard_restores_while_unwinding_a_panic() {
        let fake = FakeTerminal::tty();
        let observed = fake.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = TerminalGuard::enter_with(Box::new(observed.clone()), true).unwrap();
            assert!(observed.is_raw());
            panic!("deliberate panic inside the session");
        }));
        assert!(result.is_err(), "the panic should have propagated");
        assert_eq!(
            fake.disable_calls(),
            1,
            "the guard must run during unwinding"
        );
        assert!(!fake.is_raw(), "raw mode survived the panic");
    }

    #[test]
    fn restore_is_idempotent() {
        let fake = FakeTerminal::tty();
        let mut guard = TerminalGuard::enter_with(Box::new(fake.clone()), true).unwrap();
        guard.restore();
        guard.restore();
        drop(guard);
        assert_eq!(fake.disable_calls(), 1, "restore must not run twice");
    }

    #[test]
    fn a_failing_enable_is_a_runtime_error_not_a_panic() {
        let result = TerminalGuard::enter_with(Box::new(FakeTerminal::failing_enable()), true);
        let err = match result {
            Ok(_) => panic!("a failing enable must not produce a guard"),
            Err(e) => e,
        };
        assert_eq!(err.exit_code(), crate::error::EXIT_RUNTIME);
        assert!(err.message().contains("raw mode"));
    }

    #[test]
    fn a_failing_restore_does_not_panic_and_does_not_retry() {
        let fake = FakeTerminal::failing_disable();
        let mut guard = TerminalGuard::enter_with(Box::new(fake.clone()), true).unwrap();
        guard.restore();
        assert!(!guard.is_active());
        drop(guard);
        assert_eq!(fake.disable_calls(), 1);
    }

    #[test]
    fn the_recovery_hint_names_stty_sane() {
        assert!(RAW_MODE_RECOVERY_HINT.contains("stty sane"));
    }

    #[test]
    fn raw_state_tracking_follows_the_guard() {
        // The global flag is what a signal handler can consult, so it must not
        // claim raw mode is on after the guard has restored.
        let fake = FakeTerminal::tty();
        let guard = TerminalGuard::enter_with(Box::new(fake.clone()), true).unwrap();
        assert!(is_raw());
        drop(guard);
        assert!(!is_raw());
        assert!(was_restored());
    }

    // ---- The alternate screen: the TUI's other promise (R10) ----

    #[test]
    fn the_screen_guard_gives_the_window_back() {
        let fake = FakeScreen::new();
        {
            let _guard = ScreenGuard::enter_with(Box::new(fake.clone())).unwrap();
            assert!(fake.is_on_alt());
        }
        assert_eq!(fake.enter_calls(), 1);
        assert_eq!(fake.leave_calls(), 1);
        assert!(!fake.is_on_alt(), "the alternate screen survived the guard");
    }

    #[test]
    fn the_screen_guard_restores_while_unwinding_a_panic() {
        // The user-visible bug this prevents: a panic inside the TUI leaving a
        // blank alternate screen with no cursor and no way back.
        let fake = FakeScreen::new();
        let observed = fake.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ScreenGuard::enter_with(Box::new(observed.clone())).unwrap();
            assert!(observed.is_on_alt());
            panic!("deliberate panic inside the TUI");
        }));
        assert!(result.is_err(), "the panic should have propagated");
        assert_eq!(fake.leave_calls(), 1, "the guard must run during unwinding");
        assert!(!fake.is_on_alt(), "the screen survived the panic");
    }

    #[test]
    fn restoring_the_screen_twice_leaves_once() {
        let fake = FakeScreen::new();
        let mut guard = ScreenGuard::enter_with(Box::new(fake.clone())).unwrap();
        guard.restore();
        guard.restore();
        drop(guard);
        assert_eq!(fake.leave_calls(), 1, "restore must not run twice");
    }

    #[test]
    fn a_failing_enter_is_a_runtime_error_not_a_panic() {
        let result = ScreenGuard::enter_with(Box::new(FakeScreen::failing_enter()));
        let err = match result {
            Ok(_) => panic!("a failing enter must not produce a guard"),
            Err(e) => e,
        };
        assert_eq!(err.exit_code(), crate::error::EXIT_RUNTIME);
        assert!(
            err.message().contains("alternate screen"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn alt_screen_tracking_follows_the_guard() {
        // `emergency_restore` and the signal handler both read this flag, so it must
        // not claim the alternate screen is in use after the guard has left it.
        assert!(!is_on_alt_screen());
        let fake = FakeScreen::new();
        let guard = ScreenGuard::enter_with(Box::new(fake.clone())).unwrap();
        assert!(is_on_alt_screen());
        drop(guard);
        assert!(!is_on_alt_screen());
    }

    #[test]
    fn an_emergency_restore_is_safe_without_a_screen_guard() {
        // The panic hook can fire before the TUI ever starts, and on Windows a
        // console event can arrive at any time; neither may panic here.
        emergency_restore();
        emergency_restore();
        assert!(!is_on_alt_screen());
        assert!(!is_raw());
    }
}
