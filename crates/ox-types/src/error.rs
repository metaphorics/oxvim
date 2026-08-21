//! API error type, mirroring upstream `Error`/`ErrorType`.
//!
//! Upstream (`api/private/defs.h`): `kErrorTypeException = 0` and
//! `kErrorTypeValidation = 1` (after `kErrorTypeNone = -1`).

use std::error::Error;
use std::fmt;

/// An error returned from a `nvim_*` API function.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ApiError {
    /// A runtime (execution) error — `kErrorTypeException` (type `0`).
    Exception(String),
    /// A validation error on the arguments/request — `kErrorTypeValidation`
    /// (type `1`).
    Validation(String),
}

impl ApiError {
    /// Construct a [`ApiError::Exception`].
    #[must_use]
    pub fn exception(message: impl Into<String>) -> Self {
        ApiError::Exception(message.into())
    }

    /// Construct a [`ApiError::Validation`].
    #[must_use]
    pub fn validation(message: impl Into<String>) -> Self {
        ApiError::Validation(message.into())
    }

    /// The numeric error type code on the wire: `0` for exception,
    /// `1` for validation.
    #[must_use]
    pub const fn error_type(&self) -> i64 {
        match self {
            ApiError::Exception(_) => 0,
            ApiError::Validation(_) => 1,
        }
    }

    /// The human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            ApiError::Exception(m) | ApiError::Validation(m) => m,
        }
    }
}

impl fmt::Display for ApiError {
    /// Renders the message only (the type code is conveyed separately on the
    /// wire).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.message(), f)
    }
}

impl Error for ApiError {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::ApiError;

    #[test]
    fn wire_type_codes_are_0_and_1() {
        assert_eq!(ApiError::exception("boom").error_type(), 0);
        assert_eq!(ApiError::validation("bad arg").error_type(), 1);
    }

    #[test]
    fn message_and_display() {
        let e = ApiError::exception("nope");
        assert_eq!(e.message(), "nope");
        assert_eq!(e.to_string(), "nope");
        let v = ApiError::validation("NaN");
        assert_eq!(v.message(), "NaN");
        assert_eq!(v.to_string(), "NaN");
    }

    #[test]
    fn error_trait_impl() {
        use std::error::Error;

        let e = ApiError::exception("x");
        assert!(Error::source(&e).is_none());
    }
}