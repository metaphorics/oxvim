#![forbid(unsafe_code)]
//! Byte-accurate Vimscript expression lexing, parsing, and evaluation.

pub mod error;
pub mod eval;
pub mod lexer;
pub mod parser;
pub mod scope;

pub use error::{EvalError, Result};
pub use eval::{BuiltinHost, Evaluator, NoBuiltins, NoRegex, RegexEngine};
pub use parser::{Expr, Parser};
pub use scope::{Scope, ScopeKind};

#[cfg(test)]
mod tests;
