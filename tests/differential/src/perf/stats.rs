//! Nearest-rank percentiles and in-place summarization.
//!
//! Every reported percentile is an actually observed sample. The rank is
//! computed with checked integer arithmetic and is clamped to the valid
//! range, so callers cannot silently produce an interpolated or out-of-bounds
//! value.

use serde::{Deserialize, Serialize};

/// A percentile summary over one sample distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Percentiles {
    /// Number of samples summarized.
    pub n: usize,
    /// Observed 50th percentile (nearest-rank).
    pub p50: u64,
    /// Observed 95th percentile (nearest-rank).
    pub p95: u64,
    /// Observed 99th percentile (nearest-rank).
    pub p99: u64,
    /// Maximum observed sample.
    pub max: u64,
}

/// Why a percentile or summary could not be computed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatsError {
    /// The input sample set was empty.
    Empty,
    /// The percentile denominator was zero.
    ZeroDenominator,
    /// The percentile numerator was greater than the denominator.
    NumeratorExceedsDenominator {
        /// Requested numerator.
        numerator: usize,
        /// Requested denominator.
        denominator: usize,
    },
    /// Integer arithmetic for the rank calculation overflowed.
    Overflow,
}

impl std::fmt::Display for StatsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "cannot compute percentile of an empty sample"),
            Self::ZeroDenominator => write!(f, "percentile denominator must not be zero"),
            Self::NumeratorExceedsDenominator {
                numerator,
                denominator,
            } => write!(
                f,
                "percentile numerator {numerator} exceeds denominator {denominator}"
            ),
            Self::Overflow => write!(f, "percentile rank arithmetic overflow"),
        }
    }
}

impl std::error::Error for StatsError {}

/// Compute the nearest-rank percentile on an ascending-sorted slice.
///
/// The quantile is `numerator / denominator`. The rank is
/// `ceil(numerator * n / denominator)`, clamped to at least `1`, and the
/// returned value is the sample at `rank - 1`.
///
/// # Errors
///
/// - [`StatsError::Empty`] if `sorted` is empty.
/// - [`StatsError::ZeroDenominator`] if `denominator` is `0`.
/// - [`StatsError::NumeratorExceedsDenominator`] if `numerator > denominator`.
/// - [`StatsError::Overflow`] if the rank arithmetic overflows `usize`.
pub fn percentile(sorted: &[u64], numerator: usize, denominator: usize) -> Result<u64, StatsError> {
    if sorted.is_empty() {
        return Err(StatsError::Empty);
    }
    if denominator == 0 {
        return Err(StatsError::ZeroDenominator);
    }
    if numerator > denominator {
        return Err(StatsError::NumeratorExceedsDenominator {
            numerator,
            denominator,
        });
    }

    let n = sorted.len();
    let offset = denominator - 1;
    let product = numerator.checked_mul(n).ok_or(StatsError::Overflow)?;
    let sum = product.checked_add(offset).ok_or(StatsError::Overflow)?;
    let rank = sum / denominator;
    let rank = rank.max(1);
    let idx = rank.checked_sub(1).ok_or(StatsError::Overflow)?;

    sorted.get(idx).copied().ok_or(StatsError::Overflow)
}

/// Sort samples in place and return p50/p95/p99/max as observed values.
///
/// # Errors
///
/// - [`StatsError::Empty`] if `samples` is empty.
/// - [`StatsError::Overflow`] only if an internal arithmetic check fails; this
///   cannot happen for valid p50/p95/p99 ranks.
pub fn summarize(samples: &mut [u64]) -> Result<Percentiles, StatsError> {
    if samples.is_empty() {
        return Err(StatsError::Empty);
    }

    samples.sort_unstable();

    let n = samples.len();
    let p50 = percentile(samples, 50, 100)?;
    let p95 = percentile(samples, 95, 100)?;
    let p99 = percentile(samples, 99, 100)?;
    let max = samples.last().copied().ok_or(StatsError::Empty)?;

    Ok(Percentiles {
        n,
        p50,
        p95,
        p99,
        max,
    })
}
