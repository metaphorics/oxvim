//! Editor handle types: buffers, windows, tabpages.
//!
//! Handles are 1-based identifiers. `0` (see [`Handle::CURRENT`]) means "the
//! current" object of that kind at an API call site, per upstream
//! `handle_T`/`api/private/defs.h` semantics.

use std::error::Error;
use std::fmt;

/// An error converting an integer into an editor handle.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HandleError {
    /// Handles are 1-based, `0` meaning "current"; negative values are never
    /// valid.
    Negative(i64),
    /// The value cannot be represented in the `i32` handle field.
    OutOfRange(i64),
}

impl fmt::Display for HandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandleError::Negative(v) => write!(f, "negative handle value {v}"),
            HandleError::OutOfRange(v) => write!(f, "handle value {v} out of i32 range"),
        }
    }
}

impl Error for HandleError {}

macro_rules! handle_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub struct $name(i32);

        impl $name {
            /// `0`: "the current" object of this kind at an API call site.
            pub const CURRENT: Self = Self(0);

            /// Whether this handle is the "current" sentinel (`0`).
            #[must_use]
            pub const fn is_current(self) -> bool {
                self.0 == 0
            }

            /// The raw 1-based id (`0` when current).
            #[must_use]
            pub const fn raw(self) -> i32 {
                self.0
            }
        }

        impl From<$name> for i64 {
            fn from(h: $name) -> i64 {
                i64::from(h.0)
            }
        }

        impl TryFrom<i64> for $name {
            type Error = HandleError;

            fn try_from(value: i64) -> Result<Self, HandleError> {
                if value < 0 {
                    return Err(HandleError::Negative(value));
                }
                let narrowed: i32 = value.try_into().map_err(|_| HandleError::OutOfRange(value))?;
                Ok(Self(narrowed))
            }
        }
    };
}

handle_type! {
    /// A buffer handle (`handle_T` in upstream Neovim).
    BufHandle
}

handle_type! {
    /// A window handle.
    WinHandle
}

handle_type! {
    /// A tabpage handle.
    TabHandle
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{BufHandle, HandleError, TabHandle, WinHandle};

    #[test]
    fn current_sentinel() {
        assert!(BufHandle::CURRENT.is_current());
        assert_eq!(BufHandle::CURRENT.raw(), 0);
        assert!(!BufHandle::try_from(3).unwrap().is_current());
    }

    #[test]
    fn all_kinds_convert_round_trip() {
        let b = BufHandle::try_from(7).unwrap();
        let w = WinHandle::try_from(2).unwrap();
        let t = TabHandle::try_from(1).unwrap();
        assert!(!b.is_current() && !w.is_current() && !t.is_current());
        assert_eq!(i64::from(BufHandle::CURRENT), 0);
        assert_eq!(i64::from(WinHandle::CURRENT), 0);
        assert_eq!(i64::from(TabHandle::CURRENT), 0);
    }

    #[test]
    fn rejects_negative() {
        let err = BufHandle::try_from(-1).unwrap_err();
        assert_eq!(err, HandleError::Negative(-1));
        assert!(BufHandle::try_from(-5).is_err());
    }

    #[test]
    fn rejects_above_i32_range() {
        let err = BufHandle::try_from(i64::from(i32::MAX) + 1).unwrap_err();
        assert_eq!(err, HandleError::OutOfRange(i64::from(i32::MAX) + 1));
    }

    #[test]
    fn converts_to_i64() {
        let h = BufHandle::try_from(42).unwrap();
        assert_eq!(i64::from(h), 42);
        assert_eq!(i64::from(BufHandle::CURRENT), 0);
    }
}