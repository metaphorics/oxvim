#![forbid(unsafe_code)]
//! Byte-accurate Vimscript expression lexing, parsing, and evaluation.

pub mod builtins;
pub mod error;
pub mod eval;
mod find_file;
mod fuzzy;
pub mod lexer;
pub mod parser;
mod path_builtins;
pub mod scope;

pub use builtins::{
    BUILTINS, BuiltinSpec, Builtins, builtin_spec, call_buffer_builtin, call_higher_order_builtin,
    exists, float_as_string, is_buffer_builtin, is_builtin_implemented, is_higher_order_builtin,
    is_locked_value, lock_value,
};
pub use error::{EvalError, EvalErrorKind, Result};
pub use eval::{
    BufferHost, BuiltinHost, ClosureRegistry, Evaluator, NoBuiltins, NoRegex, RegexEngine,
    RegexMatch, closure_index, list_slice_bounds, normalize_list_index,
};
pub use parser::{Expr, Parser};
pub use path_builtins::apply_filename_modifiers;
pub use scope::{Scope, ScopeKind};

#[cfg(test)]
mod builtins_tests;
#[cfg(test)]
mod tests;
