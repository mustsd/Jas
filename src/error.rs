//! Error type and exit-code mapping.
//!
//! Exit codes are part of the CLI contract (PLANS.md section 5.6):
//!
//! | Code | Meaning |
//! |---|---|
//! | 0 | Success |
//! | 1 | Usage error |
//! | 2 | Runtime failure |
//! | 3 | No playable input |
//! | 130 | Interrupted |

use std::fmt;

pub const EXIT_OK: i32 = 0;
pub const EXIT_USAGE: i32 = 1;
pub const EXIT_RUNTIME: i32 = 2;
pub const EXIT_NO_INPUT: i32 = 3;
/// Matches the shell convention for a process killed by SIGINT.
pub const EXIT_INTERRUPTED: i32 = 130;

#[derive(Debug, Clone)]
pub enum Error {
    /// Bad flag, bad time literal, unknown command at startup.
    Usage(String),
    /// Backend crashed, IPC lost, decode error, I/O failure.
    Runtime(String),
    /// Nothing playable was found.
    NoInput(String),
}

impl Error {
    pub fn usage(msg: impl Into<String>) -> Self {
        Error::Usage(msg.into())
    }

    pub fn runtime(msg: impl Into<String>) -> Self {
        Error::Runtime(msg.into())
    }

    pub fn no_input(msg: impl Into<String>) -> Self {
        Error::NoInput(msg.into())
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => EXIT_USAGE,
            Error::Runtime(_) => EXIT_RUNTIME,
            Error::NoInput(_) => EXIT_NO_INPUT,
        }
    }

    /// Human text without the exit-code detail; used for the one-line error report.
    pub fn message(&self) -> String {
        match self {
            Error::Usage(m) | Error::Runtime(m) | Error::NoInput(m) => m.clone(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Runtime(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Runtime(format!("state file is not valid JSON: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_documented_table() {
        assert_eq!(Error::usage("x").exit_code(), 1);
        assert_eq!(Error::runtime("x").exit_code(), 2);
        assert_eq!(Error::no_input("x").exit_code(), 3);
        assert_eq!(EXIT_OK, 0);
        assert_eq!(EXIT_RUNTIME, 2);
        // 130 is not an `Error` variant: it comes from the signal handler or the
        // input layer, and is returned as a bare exit code.
        assert_eq!(EXIT_INTERRUPTED, 130);
    }

    #[test]
    fn messages_are_the_inner_text() {
        assert_eq!(Error::usage("bad flag").message(), "bad flag");
        assert_eq!(Error::usage("bad flag").to_string(), "bad flag");
        assert_eq!(Error::no_input("nothing").to_string(), "nothing");
    }

    #[test]
    fn io_errors_become_runtime_failures() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e: Error = io.into();
        assert_eq!(e.exit_code(), EXIT_RUNTIME);
        assert!(e.message().contains("missing"));
    }
}
