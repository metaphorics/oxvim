#![forbid(unsafe_code)]
//! Parsing and metadata for Neovim-compatible Ex command lines.
//!
//! This crate deliberately stops before command execution. It resolves names,
//! parses command structure, and exposes host seams for editor-owned values.

pub mod command;
pub mod expand;
pub mod parser;

pub use command::{
    AddrType, COMMANDS, CommandFlags, CommandSpec, NoUserCommands, ResolveError, ResolvedCommand,
    UserCommandInfo, UserCommandMatch, UserCommandProvider, command_spec, resolve_command,
};
pub use expand::{CmdlineContext, CmdlineSpecial, ExpansionPart, expand_with, scan_expansions};
pub use parser::{
    Address, AddressBase, CommandModifier, ErrorCode, ExCommand, ModifierKind, ParseError, Parser,
    Range, RangeKind, RangeSeparator, effective_addr_type, effective_flags,
};

impl ExCommand {
    /// Parses this command's raw argument tail as one Vimscript expression.
    ///
    /// Callers choose when a command's grammar is expression-shaped; the Ex
    /// parser itself preserves the raw tail instead of guessing per-command
    /// execution semantics.
    ///
    /// # Errors
    ///
    /// Returns [`ox_eval::EvalError`] when the tail does not lex or parse as a
    /// Vimscript expression, carrying the Vim error code, message, and the byte
    /// offset where parsing failed.
    pub fn parse_expression_args(&self) -> ox_eval::Result<ox_eval::Expr> {
        ox_eval::Parser::new(self.args.as_bytes()).parse()
    }
}

#[cfg(test)]
mod tests;
