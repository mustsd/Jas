//! Time literal parsing and formatting (pure).
//!
//! Accepted everywhere a `<T>` appears: `SS`, `MM:SS`, `HH:MM:SS`, each with an
//! optional `.ms` fraction. A leading `+` or `-` makes the value relative to the
//! current position, which the caller resolves against the transport clock.

use std::fmt;
use std::time::Duration;

/// Largest accepted value: 1000 hours. Guards against overflow and typos.
pub const MAX_MS: u64 = 1000 * 3600 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSpec {
    Absolute(Duration),
    /// Signed offset in milliseconds.
    Relative(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeError(String);

impl TimeError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl fmt::Display for TimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TimeError {}

/// Parse a time literal. See the module docs for the grammar.
pub fn parse_time(input: &str) -> Result<TimeSpec, TimeError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(TimeError::new("empty time value"));
    }
    let (sign, body) = match s.as_bytes()[0] {
        b'+' => (Some(1i64), &s[1..]),
        b'-' => (Some(-1i64), &s[1..]),
        _ => (None, s),
    };
    if body.is_empty() {
        return Err(TimeError::new(format!("`{s}` has a sign but no digits")));
    }

    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() > 3 {
        return Err(TimeError::new(format!(
            "`{s}` has too many `:` components (use H:MM:SS)"
        )));
    }
    let n = parts.len();
    let mut total_ms: i64 = 0;

    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == n;
        // Unit of this component: 1000, 60_000, or 3_600_000 ms.
        let unit_ms: i64 = 1_000 * 60i64.pow((n - 1 - i) as u32);
        let (digits, frac) = match part.split_once('.') {
            Some((d, f)) => (d, Some(f)),
            None => (*part, None),
        };
        if let Some(f) = frac {
            if !last {
                return Err(TimeError::new(format!(
                    "`{s}`: only the last component may carry a fraction"
                )));
            }
            if f.is_empty() || f.len() > 3 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return Err(TimeError::new(format!(
                    "`{s}`: fraction must be 1-3 digits (milliseconds)"
                )));
            }
        }
        if digits.is_empty() {
            return Err(TimeError::new(format!("`{s}`: `{part}` has no digits")));
        }
        if !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 9 {
            return Err(TimeError::new(format!("`{s}`: `{part}` is not a number")));
        }
        let value: i64 = match digits.parse() {
            Ok(v) => v,
            Err(_) => return Err(TimeError::new(format!("`{s}`: `{part}` is not a number"))),
        };
        // Everything except the leading component is a 0-59 sexagesimal field.
        if i > 0 && value >= 60 {
            return Err(TimeError::new(format!(
                "`{s}`: `{part}` must be 0-59 (use the next-larger component)"
            )));
        }
        if !last && value >= 10_000 {
            return Err(TimeError::new(format!("`{s}`: `{part}` is out of range")));
        }
        total_ms += value * unit_ms;
        if let Some(f) = frac {
            // Pad to milliseconds: "1.5" is 500 ms, "1.25" is 250 ms, "1.005" is 5 ms.
            let padded = format!("{f:0<3}");
            let ms: i64 = padded[..3].parse().unwrap_or(0);
            total_ms += ms;
        }
        if total_ms > MAX_MS as i64 {
            return Err(TimeError::new(format!(
                "`{s}` is too large (max 1000:00:00)"
            )));
        }
    }

    match sign {
        None => Ok(TimeSpec::Absolute(Duration::from_millis(total_ms as u64))),
        Some(s) => Ok(TimeSpec::Relative(s * total_ms)),
    }
}

/// `M:SS`, or `H:MM:SS` past the hour. Used for every position/duration display.
pub fn format_time(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The same as [`format_time`], but with a `.mmm` fraction when one is present.
///
/// This is the form that round-trips through [`parse_time`], so it is what any
/// generated command text must use; the plain form is for humans.
pub fn format_time_precise(d: Duration) -> String {
    let ms = d.subsec_millis();
    let base = format_time(d);
    if ms == 0 {
        base
    } else {
        // Only the seconds field carries the fraction, so trim and re-append.
        match base.rfind(':') {
            Some(i) => format!("{}:{:02}.{:03}", &base[..i], d.as_secs() % 60, ms),
            None => format!("{}.{ms:03}", d.as_secs()),
        }
    }
}

/// Seconds with one decimal, for `status` output and diagnostics.
pub fn format_seconds(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64())
}

/// Value for external player arguments (`-ss`, `start=`), millisecond precise.
pub fn arg_seconds(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64())
}

/// Apply a signed offset, clamping at zero.
pub fn offset(base: Duration, delta_ms: i64) -> Duration {
    let base_ms = base.as_millis() as i64;
    let out = base_ms.saturating_add(delta_ms).max(0);
    Duration::from_millis(out as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(s: &str) -> Duration {
        match parse_time(s) {
            Ok(TimeSpec::Absolute(d)) => d,
            other => panic!("expected absolute time for {s}, got {other:?}"),
        }
    }

    fn rel(s: &str) -> i64 {
        match parse_time(s) {
            Ok(TimeSpec::Relative(ms)) => ms,
            other => panic!("expected relative time for {s}, got {other:?}"),
        }
    }

    #[test]
    fn parses_all_documented_shapes() {
        assert_eq!(abs("0"), Duration::ZERO);
        assert_eq!(abs("5"), Duration::from_secs(5));
        assert_eq!(abs("90"), Duration::from_secs(90));
        assert_eq!(abs("1:30"), Duration::from_secs(90));
        assert_eq!(abs("01:30"), Duration::from_secs(90));
        assert_eq!(abs("0:00"), Duration::ZERO);
        assert_eq!(abs("1:02:03"), Duration::from_secs(3723));
        assert_eq!(abs("0.5"), Duration::from_millis(500));
        assert_eq!(abs("0:00.250"), Duration::from_millis(250));
        assert_eq!(abs("1.005"), Duration::from_millis(1005));
        assert_eq!(abs(" 1:00 "), Duration::from_secs(60));
    }

    #[test]
    fn parses_relative_specs() {
        assert_eq!(rel("+5"), 5_000);
        assert_eq!(rel("-5"), -5_000);
        assert_eq!(rel("+0:01.500"), 1_500);
        assert_eq!(rel("-1:00"), -60_000);
        assert_eq!(rel("+0"), 0);
    }

    #[test]
    fn rejects_invalid_input() {
        for bad in [
            "",
            "  ",
            "+",
            "-",
            "abc",
            "1:2:3:4",
            "1:",
            ":1",
            "1:60",
            "1:99",
            "2:60:00",
            "1.2345",
            "1.",
            ".5",
            "-",
            "1:2.5:3",
            "1e3",
            "0x10",
            "1001:00:00",
            "1:0000000000",
        ] {
            assert!(parse_time(bad).is_err(), "expected `{bad}` to be rejected");
        }
    }

    #[test]
    fn error_messages_are_actionable() {
        let e = parse_time("1:60").unwrap_err().to_string();
        assert!(e.contains("0-59"), "unexpected message: {e}");
        let e = parse_time("").unwrap_err().to_string();
        assert!(e.contains("empty"), "unexpected message: {e}");
    }

    #[test]
    fn formats_human_readable_positions() {
        assert_eq!(format_time(Duration::ZERO), "0:00");
        assert_eq!(format_time(Duration::from_secs(41)), "0:41");
        assert_eq!(format_time(Duration::from_secs(192)), "3:12");
        assert_eq!(format_time(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(format_time(Duration::from_millis(1999)), "0:01");
    }

    #[test]
    fn formats_external_player_arguments() {
        assert_eq!(arg_seconds(Duration::from_millis(1500)), "1.500");
        assert_eq!(format_seconds(Duration::from_millis(12400)), "12.4");
    }

    #[test]
    fn offsets_clamp_at_zero() {
        assert_eq!(offset(Duration::from_secs(5), -10_000), Duration::ZERO);
        assert_eq!(
            offset(Duration::from_secs(5), 500),
            Duration::from_millis(5_500)
        );
        assert_eq!(
            offset(Duration::from_secs(5), -500),
            Duration::from_millis(4_500)
        );
    }

    #[test]
    fn precise_formatting_round_trips_through_the_parser() {
        for ms in [
            0u64, 1, 999, 1000, 1500, 5_999, 60_000, 90_500, 3_600_000, 3_723_456,
        ] {
            let d = Duration::from_millis(ms);
            let text = format_time_precise(d);
            match parse_time(&text) {
                Ok(TimeSpec::Absolute(back)) => {
                    assert_eq!(back, d, "`{text}` did not round-trip {d:?}")
                }
                other => panic!("`{text}` did not parse as absolute: {other:?}"),
            }
            // And with a sign, for relative seeks.
            let signed = format!("+{text}");
            match parse_time(&signed) {
                Ok(TimeSpec::Relative(back)) => assert_eq!(back, ms as i64),
                other => panic!("`{signed}` did not parse as relative: {other:?}"),
            }
        }
    }

    #[test]
    fn precise_formatting_omits_a_zero_fraction() {
        assert_eq!(format_time_precise(Duration::from_secs(90)), "1:30");
        assert_eq!(format_time_precise(Duration::from_millis(1500)), "0:01.500");
        assert_eq!(format_time_precise(Duration::from_millis(250)), "0:00.250");
        assert_eq!(
            format_time_precise(Duration::from_millis(3_723_456)),
            "1:02:03.456"
        );
    }

    #[test]
    fn formatting_is_stable_at_component_boundaries() {
        // Guards the ms math for values that would round oddly if formatted in f32.
        assert_eq!(abs("0:00.001"), Duration::from_millis(1));
        assert_eq!(format_time(Duration::from_millis(3_599_999)), "59:59");
        assert_eq!(format_time(Duration::from_secs(3_600)), "1:00:00");
        assert_eq!(abs("100:00:00"), Duration::from_secs(360_000));
    }
}
