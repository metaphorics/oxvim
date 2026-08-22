#![forbid(unsafe_code)]
//! Byte-accurate Vimscript expression lexing, parsing, and evaluation.

pub mod builtins;
pub mod error;
pub mod eval;
pub mod lexer;
pub mod parser;
pub mod scope;

pub use builtins::{
    builtin_spec, call_buffer_builtin, is_buffer_builtin, is_locked_value, lock_value, BuiltinSpec,
    Builtins, BUILTINS,
};
pub use error::{EvalError, EvalErrorKind, Result};
pub use eval::{BuiltinHost, BufferHost, Evaluator, NoBuiltins, NoRegex, RegexEngine};
pub use parser::{Expr, Parser};
pub use scope::{Scope, ScopeKind};

#[cfg(test)]
mod builtins_tests;
#[cfg(test)]
mod tests;
