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
//! A hard `SIGKILL` cannot be covered by any of these, so `--help` and the README
//! document `stty sane` as the recovery.
//!
//! The guard owns its [`RawMode`] implementation, so the object that enabled raw
//! mode is the same object that disables it. That matters more than it sounds:
//! a guard holding a *different* handle than the one that flipped termios would
//! report success while leaving the shell with echo off.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::error::{Error, Result};

const STATE_OFF: u8 = 0;
const STATE_RAW: u8 = 1;
const STATE_RESTORED: u8 = 2;

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
    if RAW_STATE.load(Ordering::SeqCst) == STATE_RAW {
        let _ = crossterm::terminal::disable_raw_mode();
        RAW_STATE.store(STATE_RESTORED, Ordering::SeqCst);
        RESTORED_AT_LEAST_ONCE.store(true, Ordering::SeqCst);
        // A newline keeps the shell prompt off the line the status line used.
        let _ = io::stdout().flush();
        eprintln!();
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
}
