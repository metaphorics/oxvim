//! Typed Vimscript evaluation failures.

use std::fmt;

/// A Vim-compatible expression error with its source byte offset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvalError {
    /// Vim error identifier, such as `E121`.
    pub code: &'static str,
    /// Byte offset in the expression where the error arose.
    pub offset: usize,
    /// Human-readable detail following the Vim error identifier.
    pub message: String,
}

impl EvalError {
    /// Construct an evaluation error.
    pub fn new(code: &'static str, offset: usize, message: impl Into<String>) -> Self {
        Self {
            code,
            offset,
            message: message.into(),
        }
    }
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} (byte {})", self.code, self.message, self.offset)
    }
}

impl std::error::Error for EvalError {}

/// Result type used throughout expression processing.
pub type Result<T> = std::result::Result<T, EvalError>;
