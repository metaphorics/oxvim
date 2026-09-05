//! Typed decoder for Valgrind DHAT heap totals.

use std::{error::Error, fmt};

use serde::Deserialize;

const DHAT_FILE_VERSION: u64 = 2;
const HEAP_MODE: &str = "heap";

/// Whole-process allocation totals reported by Valgrind DHAT.
///
/// These totals cover the complete profiled process, not an individual timed
/// workload window. The caller is responsible for preserving the raw DHAT JSON
/// and Valgrind log alongside these derived totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DhatTotals {
    /// Operating-system process identifier recorded by DHAT.
    pub pid: u32,
    /// Sum of allocation counts (`pps[].tbk`) across all program points.
    pub total_blocks: u64,
    /// Sum of allocated bytes (`pps[].tb`) across all program points.
    pub total_bytes: u64,
}

/// Failure to decode or validate a DHAT heap document.
#[derive(Debug)]
#[non_exhaustive]
pub enum DhatError {
    /// Sonic-rs could not decode the typed DHAT schema.
    Decode {
        /// Schema region being decoded.
        context: &'static str,
        /// Original decoder error, including its JSON location.
        source: sonic_rs::Error,
    },
    /// A required top-level field was absent or null.
    MissingField { field: &'static str },
    /// A required program-point counter was absent or null.
    MissingProgramPointField { index: usize, field: &'static str },
    /// The document uses an unsupported DHAT schema version.
    UnsupportedVersion { expected: u64, actual: u64 },
    /// The document is not a DHAT heap profile.
    UnsupportedMode {
        expected: &'static str,
        actual: String,
    },
    /// The document belongs to a different process.
    PidMismatch { expected: u32, actual: u32 },
    /// Summing an allocation counter exceeded `u64`.
    Overflow { index: usize, field: &'static str },
}

impl fmt::Display for DhatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode { context, source } => write!(formatter, "invalid {context}: {source}"),
            Self::MissingField { field } => {
                write!(formatter, "missing required DHAT field `{field}`")
            }
            Self::MissingProgramPointField { index, field } => write!(
                formatter,
                "missing required DHAT field `pps[{index}].{field}`"
            ),
            Self::UnsupportedVersion { expected, actual } => write!(
                formatter,
                "unsupported DHAT `dhatFileVersion` {actual}; expected {expected}"
            ),
            Self::UnsupportedMode { expected, actual } => write!(
                formatter,
                "unsupported DHAT `mode` {actual:?}; expected {expected:?}"
            ),
            Self::PidMismatch { expected, actual } => write!(
                formatter,
                "DHAT `pid` {actual} does not match expected process {expected}"
            ),
            Self::Overflow { index, field } => write!(
                formatter,
                "DHAT counter `{field}` overflowed at program point {index}"
            ),
        }
    }
}

impl Error for DhatError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct DhatDocument {
    #[serde(rename = "dhatFileVersion")]
    dhat_file_version: Option<u64>,
    mode: Option<String>,
    pid: Option<u32>,
    pps: Option<Vec<ProgramPoint>>,
}

#[derive(Debug, Deserialize)]
struct ProgramPoint {
    tb: Option<u64>,
    tbk: Option<u64>,
}

/// Decodes and validates whole-process allocation totals from DHAT heap JSON.
///
/// Unknown fields are ignored because DHAT includes unrelated metadata and may
/// add such metadata without changing the counters consumed here.
///
/// # Errors
///
/// Returns [`DhatError`] for malformed JSON, missing or invalid required fields,
/// a version other than 2, a mode other than `heap`, a PID other than
/// `expected_pid`, or overflow while summing either counter.
pub fn decode_dhat(bytes: &[u8], expected_pid: u32) -> Result<DhatTotals, DhatError> {
    let document: DhatDocument =
        sonic_rs::from_slice(bytes).map_err(|source| DhatError::Decode {
            context: "DHAT heap JSON",
            source,
        })?;

    let version = document.dhat_file_version.ok_or(DhatError::MissingField {
        field: "dhatFileVersion",
    })?;
    if version != DHAT_FILE_VERSION {
        return Err(DhatError::UnsupportedVersion {
            expected: DHAT_FILE_VERSION,
            actual: version,
        });
    }

    let mode = document
        .mode
        .ok_or(DhatError::MissingField { field: "mode" })?;
    if mode != HEAP_MODE {
        return Err(DhatError::UnsupportedMode {
            expected: HEAP_MODE,
            actual: mode,
        });
    }

    let pid = document
        .pid
        .ok_or(DhatError::MissingField { field: "pid" })?;
    if pid != expected_pid {
        return Err(DhatError::PidMismatch {
            expected: expected_pid,
            actual: pid,
        });
    }

    let program_points = document
        .pps
        .ok_or(DhatError::MissingField { field: "pps" })?;
    let mut total_blocks = 0_u64;
    let mut total_bytes = 0_u64;
    for (index, point) in program_points.into_iter().enumerate() {
        let bytes = point
            .tb
            .ok_or(DhatError::MissingProgramPointField { index, field: "tb" })?;
        let blocks = point.tbk.ok_or(DhatError::MissingProgramPointField {
            index,
            field: "tbk",
        })?;
        total_bytes = total_bytes.checked_add(bytes).ok_or(DhatError::Overflow {
            index,
            field: "pps[].tb",
        })?;
        total_blocks = total_blocks
            .checked_add(blocks)
            .ok_or(DhatError::Overflow {
                index,
                field: "pps[].tbk",
            })?;
    }

    Ok(DhatTotals {
        pid,
        total_blocks,
        total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{DhatError, DhatTotals, decode_dhat};

    const PID: u32 = 42;

    #[test]
    fn empty_program_points_have_zero_totals() {
        let totals = decode_dhat(
            br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"pps":[]}"#,
            PID,
        );
        assert!(matches!(
            totals,
            Ok(DhatTotals {
                pid: PID,
                total_blocks: 0,
                total_bytes: 0,
            })
        ));
    }

    #[test]
    fn sums_each_counter_once_and_ignores_unrelated_fields() {
        let totals = decode_dhat(
            br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"cmd":"nvim","pps":[{"tb":7,"tbk":2,"extra":true},{"tb":11,"tbk":3}]}"#,
            PID,
        );
        assert!(matches!(
            totals,
            Ok(DhatTotals {
                pid: PID,
                total_blocks: 5,
                total_bytes: 18,
            })
        ));
    }

    #[test]
    fn rejects_malformed_json_with_source_and_context() {
        let result = decode_dhat(br#"{"dhatFileVersion":2,"pps":[}"#, PID);
        match result {
            Err(error @ DhatError::Decode { context, .. }) => {
                assert_eq!(context, "DHAT heap JSON");
                assert!(error.source().is_some());
            }
            other => assert!(matches!(other, Err(DhatError::Decode { .. }))),
        }
    }

    #[test]
    fn rejects_missing_required_top_level_fields() {
        let cases = [
            (
                br#"{"mode":"heap","pid":42,"pps":[]}"#.as_slice(),
                "dhatFileVersion",
            ),
            (
                br#"{"dhatFileVersion":2,"pid":42,"pps":[]}"#.as_slice(),
                "mode",
            ),
            (
                br#"{"dhatFileVersion":2,"mode":"heap","pps":[]}"#.as_slice(),
                "pid",
            ),
            (
                br#"{"dhatFileVersion":2,"mode":"heap","pid":42}"#.as_slice(),
                "pps",
            ),
        ];
        for (input, field) in cases {
            assert!(matches!(
                decode_dhat(input, PID),
                Err(DhatError::MissingField { field: actual }) if actual == field
            ));
        }
    }

    #[test]
    fn rejects_missing_program_point_counters() {
        let cases = [
            (
                br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"pps":[{"tbk":1}]}"#.as_slice(),
                "tb",
            ),
            (
                br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"pps":[{"tb":1}]}"#.as_slice(),
                "tbk",
            ),
        ];
        for (input, field) in cases {
            assert!(matches!(
                decode_dhat(input, PID),
                Err(DhatError::MissingProgramPointField { index: 0, field: actual }) if actual == field
            ));
        }
    }

    #[test]
    fn rejects_wrong_version_mode_and_pid() {
        assert!(matches!(
            decode_dhat(
                br#"{"dhatFileVersion":3,"mode":"heap","pid":42,"pps":[]}"#,
                PID
            ),
            Err(DhatError::UnsupportedVersion {
                expected: 2,
                actual: 3
            })
        ));
        assert!(matches!(
            decode_dhat(br#"{"dhatFileVersion":2,"mode":"copy","pid":42,"pps":[]}"#, PID),
            Err(DhatError::UnsupportedMode { expected: "heap", actual }) if actual == "copy"
        ));
        assert!(matches!(
            decode_dhat(
                br#"{"dhatFileVersion":2,"mode":"heap","pid":7,"pps":[]}"#,
                PID
            ),
            Err(DhatError::PidMismatch {
                expected: PID,
                actual: 7
            })
        ));
    }

    #[test]
    fn rejects_schema_range_and_type_defects() {
        let cases = [
            br#"{"dhatFileVersion":-1,"mode":"heap","pid":42,"pps":[]}"#.as_slice(),
            br#"{"dhatFileVersion":2.5,"mode":"heap","pid":42,"pps":[]}"#.as_slice(),
            br#"{"dhatFileVersion":2,"mode":7,"pid":42,"pps":[]}"#.as_slice(),
            br#"{"dhatFileVersion":2,"mode":"heap","pid":-1,"pps":[]}"#.as_slice(),
            br#"{"dhatFileVersion":2,"mode":"heap","pid":42.5,"pps":[]}"#.as_slice(),
            br#"{"dhatFileVersion":2,"mode":"heap","pid":4294967296,"pps":[]}"#.as_slice(),
            br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"pps":{}}"#.as_slice(),
            br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"pps":[{"tb":-1,"tbk":1}]}"#.as_slice(),
            br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"pps":[{"tb":1.5,"tbk":1}]}"#
                .as_slice(),
            br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"pps":[{"tb":1,"tbk":-1}]}"#.as_slice(),
            br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"pps":[{"tb":1,"tbk":1.5}]}"#
                .as_slice(),
        ];
        for input in cases {
            assert!(matches!(
                decode_dhat(input, PID),
                Err(DhatError::Decode { .. })
            ));
        }
    }

    #[test]
    fn rejects_both_counter_overflows() {
        assert!(matches!(
            decode_dhat(
                br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"pps":[{"tb":18446744073709551615,"tbk":0},{"tb":1,"tbk":0}]}"#,
                PID,
            ),
            Err(DhatError::Overflow { index: 1, field: "pps[].tb" })
        ));
        assert!(matches!(
            decode_dhat(
                br#"{"dhatFileVersion":2,"mode":"heap","pid":42,"pps":[{"tb":0,"tbk":18446744073709551615},{"tb":0,"tbk":1}]}"#,
                PID,
            ),
            Err(DhatError::Overflow { index: 1, field: "pps[].tbk" })
        ));
    }
}
