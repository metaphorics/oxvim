//! Typed Vimscript evaluation failures.

use std::fmt;

use ox_types::OxStr;

/// Machine-readable evaluation failure category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvalErrorKind {
    /// A Vim-compatible `E` error.
    Vim,
    /// A known or unknown function that the current host cannot execute.
    NotImplemented(OxStr),
}

/// A Vim-compatible expression error with its source byte offset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvalError {
    /// Machine-readable failure category.
    pub kind: EvalErrorKind,
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
            kind: EvalErrorKind::Vim,
            code,
            offset,
            message: message.into(),
        }
    }

    /// Construct a typed unsupported-function failure.
    pub fn not_implemented(name: impl Into<OxStr>) -> Self {
        let name = name.into();
        Self {
            kind: EvalErrorKind::NotImplemented(name.clone()),
            code: "E117",
            offset: 0,
            message: format!("Function is not implemented: {}", name.to_string_lossy()),
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
