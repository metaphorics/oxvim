//! `--startuptime` log parser for both engine headers.
//!
//! Parses the millisecond timing log written by `--startuptime` into a
//! [`StartupLog`] of [`Mark`]s. It is strict: the two known header process
//! values (`Embedded` and `Primary (or UI client)`) are accepted, the fixed
//! banner/blank lines are ignored, every other non-empty line must be a
//! well-formed `clock  delta: label` mark, and a parse failure carries the
//! offending line number and content.
//!
//! Numeric tokens are fixed-point milliseconds. They are parsed as checked
//! `u64` microseconds so that every accepted value is deterministic and every
//! malformed or out-of-range token surfaces as a typed [`ParseError`] rather than
//! a silent NaN or a lossy cast.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Deref, DerefMut};

const HEADER_PREFIX: &str = "--- Startup times for process: ";
const HEADER_SUFFIX: &str = " ---";

/// Fixed banner lines (trimmed form) to ignore between the header and the first mark.
const BANNERS: &[&str] = &[
    "times in msec",
    "clock   self+sourced   self:  sourced script",
    "clock   elapsed:              other lines",
];

/// One startup milestone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    /// Human-readable milestone label.
    pub label: String,
    /// Cumulative time since process start, in microseconds.
    pub clock_us: u64,
    /// Time since the previous mark, in microseconds.
    pub delta_us: u64,
}

/// A parsed `--startuptime` log for a single process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartupLog {
    /// Process kind recorded in the header.
    pub process: String,
    /// Parsed milestone marks in order.
    pub marks: Vec<Mark>,
}

impl StartupLog {
    /// Build a log from a process header and a set of marks.
    pub fn new(process: impl Into<String>, marks: Vec<Mark>) -> Self {
        Self {
            process: process.into(),
            marks,
        }
    }
}

impl Deref for StartupLog {
    type Target = [Mark];

    fn deref(&self) -> &Self::Target {
        &self.marks
    }
}

impl DerefMut for StartupLog {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.marks
    }
}

impl IntoIterator for StartupLog {
    type Item = Mark;
    type IntoIter = std::vec::IntoIter<Mark>;

    fn into_iter(self) -> Self::IntoIter {
        self.marks.into_iter()
    }
}

impl<'a> IntoIterator for &'a StartupLog {
    type Item = &'a Mark;
    type IntoIter = std::slice::Iter<'a, Mark>;

    fn into_iter(self) -> Self::IntoIter {
        self.marks.iter()
    }
}

/// A parse failure for `--startuptime` logs.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// The log did not contain a `--- Startup times for process: ... ---` header.
    MissingHeader,
    /// The header was present but was not followed by at least one mark.
    NoMarks,
    /// A non-banner, non-empty line was not a well-formed mark.
    MalformedLine {
        /// 1-indexed line number where the failure occurred.
        line_no: usize,
        /// Exact line content, preserved for diagnosis.
        content: String,
    },
    /// A mark line contained a number that failed strict integer parsing.
    InvalidNumber {
        /// 1-indexed line number where the failure occurred.
        line_no: usize,
        /// Exact line content, preserved for diagnosis.
        content: String,
        /// Which field failed (`clock` or `delta`).
        field: &'static str,
        /// Reason the token could not be parsed.
        reason: String,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader => {
                write!(f, "missing `--- Startup times for process: ... ---` header")
            }
            Self::NoMarks => write!(f, "no startup marks after header"),
            Self::MalformedLine { line_no, content } => {
                write!(f, "malformed startuptime line {line_no}: {content:?}")
            }
            Self::InvalidNumber {
                line_no,
                content,
                field,
                reason,
            } => write!(
                f,
                "invalid {field} on startuptime line {line_no}: {reason} ({content:?})"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a `--startuptime` log into a [`StartupLog`].
///
/// # Errors
///
/// Returns [`ParseError::MissingHeader`] if the log does not contain a header,
/// [`ParseError::NoMarks`] if the header is not followed by at least one mark,
/// and [`ParseError::MalformedLine`] or [`ParseError::InvalidNumber`] for any
/// other non-banner, non-empty line.
pub fn parse(log: &str) -> Result<StartupLog, ParseError> {
    let mut process = None;
    let mut marks = Vec::new();

    for (line_no, line) in log.lines().enumerate() {
        let line_no = line_no + 1;

        if process.is_none() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(value) = trimmed
                .strip_prefix(HEADER_PREFIX)
                .and_then(|s| s.strip_suffix(HEADER_SUFFIX))
            {
                process = Some(value.trim().to_string());
                continue;
            }

            return Err(ParseError::MalformedLine {
                line_no,
                content: line.to_string(),
            });
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if BANNERS.contains(&trimmed) {
            continue;
        }

        let (numbers, label) =
            trimmed
                .split_once(':')
                .ok_or_else(|| ParseError::MalformedLine {
                    line_no,
                    content: line.to_string(),
                })?;

        // Real logs carry two shapes: `(clock, self)` for most marks and
        // `(clock, self+sourced, self)` for `require(...)` marks. The stage's
        // own cost is the LAST numeric token in both shapes; anything other
        // than two or three numeric tokens is malformed.
        let mut tokens = numbers.split_whitespace();
        let clock_token = tokens.next().ok_or_else(|| ParseError::MalformedLine {
            line_no,
            content: line.to_string(),
        })?;
        let second_token = tokens.next().ok_or_else(|| ParseError::MalformedLine {
            line_no,
            content: line.to_string(),
        })?;
        let delta_token = match tokens.next() {
            Some(third_token) => {
                if tokens.next().is_some() {
                    return Err(ParseError::MalformedLine {
                        line_no,
                        content: line.to_string(),
                    });
                }
                third_token
            }
            None => second_token,
        };

        let clock_us = parse_millis_as_microseconds(clock_token).map_err(|reason| {
            ParseError::InvalidNumber {
                line_no,
                content: line.to_string(),
                field: "clock",
                reason,
            }
        })?;
        let delta_us = parse_millis_as_microseconds(delta_token).map_err(|reason| {
            ParseError::InvalidNumber {
                line_no,
                content: line.to_string(),
                field: "delta",
                reason,
            }
        })?;

        marks.push(Mark {
            label: label.trim().to_string(),
            clock_us,
            delta_us,
        });
    }

    let process = process.ok_or(ParseError::MissingHeader)?;
    if marks.is_empty() {
        return Err(ParseError::NoMarks);
    }

    Ok(StartupLog { process, marks })
}

/// Parse a fixed-point millisecond token into microseconds.
///
/// Accepts only the forms that `--startuptime` actually emits:
/// decimal digits with an optional fractional part (1–3 digits, decimal point,
/// 1–3 digits). Rejects signs, exponents, NaN, inf, absent/missing fractional
/// digits (bare `1.` or bare `1.0`), excess fractional precision, and values
/// that overflow `u64` when scaled by 1000.
fn parse_millis_as_microseconds(token: &str) -> Result<u64, String> {
    // Fast path for the overwhelmingly common case: a simple integer token.
    // This avoids allocation and the full scan for well-formed logs. The
    // digits-only gate is required because `u64::from_str` also accepts a
    // leading `+`, which `--startuptime` never emits.
    if !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit()) {
        // Gated on digits only, a `parse` failure can only be overflow.
        let n = token
            .parse::<u64>()
            .map_err(|_| format!("{token} overflows u64 microseconds"))?;
        return n
            .checked_mul(1000)
            .ok_or_else(|| format!("{token} × 1000 overflows u64"));
    }

    // General decimal parse: detect and reject non-numeric content.
    let bytes = token.as_bytes();
    let dot_pos = bytes.iter().position(|&b| b == b'.');

    let (int_part, frac_part) = match dot_pos {
        Some(pos) => (&bytes[..pos], &bytes[pos + 1..]),
        None => {
            return Err(format!(
                "expected decimal milliseconds, got non-numeric token: {token:?}"
            ));
        }
    };

    // Integer part must be non-empty digits.
    if int_part.is_empty() || !int_part.iter().all(|&b| b.is_ascii_digit()) {
        return Err(format!(
            "expected decimal milliseconds, got token with non-digit integer part: {token:?}"
        ));
    }

    // Fractional part must be 1–3 ASCII digits.
    let frac_len = frac_part.len();
    if frac_len == 0 || frac_len > 3 || !frac_part.iter().all(|&b| b.is_ascii_digit()) {
        return Err(format!(
            "expected 1–3 fractional digits (milliseconds), got {frac_len} in: {token:?}"
        ));
    }

    // Parse both components without allowing input length to trigger integer overflow.
    let int_val = int_part.iter().try_fold(0u64, |acc, &b| {
        acc.checked_mul(10)
            .and_then(|scaled| scaled.checked_add(u64::from(b - b'0')))
            .ok_or_else(|| format!("{token} overflows u64 microseconds"))
    })?;

    let frac_val = frac_part.iter().try_fold(0u64, |acc, &b| {
        acc.checked_mul(10)
            .and_then(|scaled| scaled.checked_add(u64::from(b - b'0')))
            .ok_or_else(|| format!("{token} overflows u64 microseconds"))
    })?;

    // Scale to microseconds: ms × 1000 + the fractional microseconds.
    let frac_scale = u32::try_from(frac_len)
        .map_err(|_| format!("{frac_len} fractional digits overflow u32 for: {token:?}"))?;
    let (frac_us, over) = frac_val.overflowing_mul(1000 / 10u64.pow(frac_scale));

    if over {
        return Err(format!(
            "overflow computing fractional microseconds for: {token:?}"
        ));
    }

    // Detect overflow: int_val × 1000 + frac_us must fit in u64.
    let (ms_scaled, overflow1) = int_val.overflowing_mul(1000);
    let (total_us, overflow2) = ms_scaled.overflowing_add(frac_us);
    if overflow1 || overflow2 {
        return Err(format!("{token} overflows u64 microseconds"));
    }

    Ok(total_us)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Maximum milliseconds that fit in `u64::MAX` without overflow on × 1000.
    const MAX_MILLIS: u64 = u64::MAX / 1000;

    #[test]
    fn parse_millis_integer_only() {
        assert_eq!(parse_millis_as_microseconds("0").unwrap(), 0);
        assert_eq!(parse_millis_as_microseconds("1").unwrap(), 1_000);
        assert_eq!(parse_millis_as_microseconds("100").unwrap(), 100_000);
        assert_eq!(parse_millis_as_microseconds("1234").unwrap(), 1_234_000);
    }

    #[test]
    fn parse_millis_one_digit_fraction() {
        assert_eq!(parse_millis_as_microseconds("0.1").unwrap(), 100);
        assert_eq!(parse_millis_as_microseconds("1.5").unwrap(), 1_500);
        assert_eq!(parse_millis_as_microseconds("100.9").unwrap(), 100_900);
    }

    #[test]
    fn parse_millis_two_digit_fraction() {
        assert_eq!(parse_millis_as_microseconds("0.01").unwrap(), 10);
        assert_eq!(parse_millis_as_microseconds("1.23").unwrap(), 1_230);
        assert_eq!(parse_millis_as_microseconds("99.99").unwrap(), 99_990);
    }

    #[test]
    fn parse_millis_three_digit_fraction() {
        assert_eq!(parse_millis_as_microseconds("0.001").unwrap(), 1);
        assert_eq!(parse_millis_as_microseconds("1.234").unwrap(), 1_234);
        assert_eq!(parse_millis_as_microseconds("500.999").unwrap(), 500_999);
    }

    #[test]
    fn parse_millis_rejects_sign() {
        parse_millis_as_microseconds("-1").expect_err("negative rejected");
        parse_millis_as_microseconds("+1").expect_err("plus rejected");
        parse_millis_as_microseconds("-0.001").expect_err("negative fraction rejected");
    }

    #[test]
    fn parse_millis_rejects_exponent() {
        parse_millis_as_microseconds("1e3").expect_err("exponent rejected");
        parse_millis_as_microseconds("1E+3").expect_err("exponent with sign rejected");
        parse_millis_as_microseconds("1.5e-3").expect_err("decimal exponent rejected");
    }

    #[test]
    fn parse_millis_rejects_nan_inf() {
        parse_millis_as_microseconds("NaN").expect_err("NaN rejected");
        parse_millis_as_microseconds("inf").expect_err("inf rejected");
        parse_millis_as_microseconds("Inf").expect_err("Inf rejected");
        parse_millis_as_microseconds("infinity").expect_err("infinity rejected");
        parse_millis_as_microseconds("Infinity").expect_err("Infinity rejected");
    }

    #[test]
    fn parse_millis_rejects_excess_precision() {
        parse_millis_as_microseconds("1.0000").expect_err("4-digit fraction rejected");
        parse_millis_as_microseconds("0.12345").expect_err("5-digit fraction rejected");
    }

    #[test]
    fn parse_millis_rejects_missing_fraction() {
        parse_millis_as_microseconds("1.").expect_err("bare dot rejected");
        parse_millis_as_microseconds("0.").expect_err("bare dot rejected");
    }

    #[test]
    fn parse_millis_rejects_non_numeric() {
        parse_millis_as_microseconds("abc").expect_err("letters rejected");
        parse_millis_as_microseconds("1a").expect_err("trailing letter rejected");
        parse_millis_as_microseconds("1_000").expect_err("underscore rejected");
        parse_millis_as_microseconds("").expect_err("empty rejected");
    }

    #[test]
    fn parse_millis_rejects_overflow() {
        // u64::MAX ms = 18446744073709551615 ms = 18446744073709551615000 µs — overflows.
        parse_millis_as_microseconds("18446744073709551615").expect_err("MAX u64 ms overflow");
    }

    #[test]
    fn parse_millis_allows_max_valid() {
        // (u64::MAX / 1000) ms = 18446744073709551 ms fits.
        let max_valid = format!("{MAX_MILLIS}");
        assert!(parse_millis_as_microseconds(&max_valid).is_ok());
        // Just over the limit.
        let over = format!("{}", MAX_MILLIS + 1);
        parse_millis_as_microseconds(&over).expect_err("just-over-max rejected");
    }

    #[test]
    fn parse_millis_allows_max_valid_with_fraction() {
        assert_eq!(
            parse_millis_as_microseconds("18446744073709551.615").unwrap(),
            u64::MAX
        );
        parse_millis_as_microseconds("18446744073709551.616")
            .expect_err("fraction above u64 maximum rejected");
        parse_millis_as_microseconds("999999999999999999999999999999999999999.0")
            .expect_err("overlong integer rejected");
    }

    #[test]
    fn parse_rejects_unknown_line_after_header() {
        let log = "--- Startup times for process: Embedded ---\nnot a mark line";
        parse(log).expect_err("unknown line rejected");
    }

    #[test]
    fn parse_rejects_missing_header() {
        let log = "0.001  0.001: some mark";
        parse(log).expect_err("missing header rejected");
    }

    #[test]
    fn parse_rejects_no_marks() {
        let log = "--- Startup times for process: Embedded ---\n";
        parse(log).expect_err("no marks rejected");
    }

    #[test]
    fn parse_ignores_banners() {
        let log = "--- Startup times for process: Embedded ---\ntimes in msec\nclock   self+sourced   self:  sourced script\nclock   elapsed:              other lines\n0.001  0.001: first mark";
        let result = parse(log).unwrap();
        assert_eq!(result.marks.len(), 1);
        assert_eq!(result.marks[0].label, "first mark");
    }

    #[test]
    fn parse_accepts_embedded_process() {
        let log = "--- Startup times for process: Embedded ---\n0.001  0.001: OXVIM STARTING";
        let result = parse(log).unwrap();
        assert_eq!(result.process, "Embedded");
        assert_eq!(result.marks.len(), 1);
    }

    #[test]
    fn parse_accepts_primary_process() {
        let log = "--- Startup times for process: Primary (or UI client) ---\n0.001  0.001: OXVIM STARTING";
        let result = parse(log).unwrap();
        assert_eq!(result.process, "Primary (or UI client)");
        assert_eq!(result.marks.len(), 1);
    }

    #[test]
    fn parse_rejects_malformed_mark_no_colon() {
        let log = "--- Startup times for process: Embedded ---\n0.001  0.001 no colon";
        parse(log).expect_err("no colon rejected");
    }

    #[test]
    fn parse_rejects_malformed_mark_missing_delta() {
        let log = "--- Startup times for process: Embedded ---\n0.001: label";
        parse(log).expect_err("missing delta rejected");
    }

    #[test]
    fn parse_rejects_extra_fields() {
        let log = "--- Startup times for process: Embedded ---\n0.001  0.001  extra: label";
        parse(log).expect_err("extra field rejected");
    }

    #[test]
    fn parse_rejects_invalid_clock_number() {
        let log = "--- Startup times for process: Embedded ---\nNaN  0.001: label";
        parse(log).expect_err("NaN clock rejected");
    }

    #[test]
    fn parse_rejects_invalid_delta_number() {
        let log = "--- Startup times for process: Embedded ---\n0.001  inf: label";
        parse(log).expect_err("inf delta rejected");
    }

    #[test]
    fn parse_rejects_overflow_clock() {
        // The integer part alone overflows when × 1000.
        let log = "--- Startup times for process: Embedded ---\n18446744073709551615  0.001: label"
            .to_string();
        parse(&log).expect_err("overflow clock rejected");
    }

    #[test]
    fn parse_full_log_smoke() {
        let log = "--- Startup times for process: Embedded ---\ntimes in msec\n\
            clock   self+sourced   self:  sourced script\n\
            clock   elapsed:              other lines\n\
            0.001  0.001: OXVIM STARTING\n\
            1.234  1.233: parsing arguments\n\
            2.100  0.866: opening buffers\n\
            2.493  0.108  0.059: require('vim._core.shared')";
        let result = parse(log).unwrap();
        assert_eq!(result.process, "Embedded");
        assert_eq!(result.marks.len(), 4);

        assert_eq!(result.marks[0].label, "OXVIM STARTING");
        assert_eq!(result.marks[0].clock_us, 1);
        assert_eq!(result.marks[0].delta_us, 1);

        assert_eq!(result.marks[1].label, "parsing arguments");
        assert_eq!(result.marks[1].clock_us, 1_234);
        assert_eq!(result.marks[1].delta_us, 1_233);

        assert_eq!(result.marks[2].label, "opening buffers");
        assert_eq!(result.marks[2].clock_us, 2_100);
        assert_eq!(result.marks[2].delta_us, 866);

        // Three-column `require` marks attribute the stage's own cost (the
        // last token), not `self+sourced`.
        assert_eq!(result.marks[3].label, "require('vim._core.shared')");
        assert_eq!(result.marks[3].clock_us, 2_493);
        assert_eq!(result.marks[3].delta_us, 59);
    }

    #[test]
    fn startuplog_deref() {
        let log = parse(
            "--- Startup times for process: Embedded ---\n\
            0.001  0.001: mark1\n\
            0.002  0.001: mark2",
        )
        .unwrap();
        // Deref to [Mark].
        let _: &[_] = &log;
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn startuplog_into_iter() {
        let log = parse(
            "--- Startup times for process: Embedded ---\n\
            0.001  0.001: mark1",
        )
        .unwrap();
        let marks: Vec<_> = log.into_iter().collect();
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn startuplog_owned_iter() {
        let log = parse(
            "--- Startup times for process: Embedded ---\n\
            0.001  0.001: mark1",
        )
        .unwrap();
        let marks: Vec<_> = log.iter().collect();
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn startuplog_deref_mut() {
        let mut log = parse(
            "--- Startup times for process: Embedded ---\n\
            0.001  0.001: mark1",
        )
        .unwrap();
        log.marks.push(Mark {
            label: "mark2".to_string(),
            clock_us: 2000,
            delta_us: 1000,
        });
        assert_eq!(log.len(), 2);
    }
}
