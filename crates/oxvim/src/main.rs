#![forbid(unsafe_code)]
//! Oxvim binary entry point: CLI parsing and process-mode dispatch.

mod api_info;
mod cli;
mod runtime;
mod server;

use std::fmt;
use std::io::{self, Write};
use std::process::ExitCode;

use cli::Cli;

/// Process startup failure with mode-specific classification.
#[derive(Debug)]
pub enum AppError {
    /// A command-line usage error.
    Usage(cli::UsageError),
    /// This valid process mode belongs to a later integration slice.
    NotWired(&'static str),
    /// Operating-system I/O failed.
    Io(io::Error),
    /// API registry assembly failed.
    Api(String),
    /// Editor initialization failed.
    Editor(String),
    /// Ex execution failed.
    Ex(String),
    /// Lua initialization or execution failed.
    Lua(String),
    /// Embedded RPC server initialization or dispatch failed.
    Server(String),
    /// Interactive terminal client failed.
    Tui(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(error) => error.fmt(formatter),
            Self::NotWired(mode) => write!(formatter, "oxvim: {mode} mode is not yet wired"),
            Self::Io(error) => write!(formatter, "oxvim: {error}"),
            Self::Api(error) => write!(formatter, "oxvim: cannot build API metadata: {error}"),
            Self::Editor(error) => write!(formatter, "oxvim: cannot initialize editor: {error}"),
            Self::Ex(error) => write!(formatter, "oxvim: Ex command failed: {error}"),
            Self::Lua(error) => write!(formatter, "oxvim: Lua script failed: {error}"),
            Self::Server(error) => write!(formatter, "oxvim: RPC server failed: {error}"),
            Self::Tui(error) => write!(formatter, "oxvim: terminal client failed: {error}"),
        }
    }
}

impl std::error::Error for AppError {}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ignored = writeln!(io::stderr().lock(), "{error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), AppError> {
    let cli = Cli::parse(std::env::args().skip(1)).map_err(AppError::Usage)?;
    if cli.api_info {
        let bytes = api_info::encoded().map_err(|error| AppError::Api(error.to_string()))?;
        io::stdout().lock().write_all(&bytes).map_err(AppError::Io)?;
        return Ok(());
    }
    if let Some(script) = &cli.lua_script {
        return runtime::run_lua(script);
    }
    if cli.scriptin.is_some() {
        return Err(AppError::NotWired("normal-mode script"));
    }
    if cli.batch.is_some() {
        return runtime::run_batch(&cli);
    }
    // A listening headless process must enter its network loop rather than the
    // stdio server; --listen is therefore selected before --headless/embed.
    if let Some(address) = &cli.listen {
        return server::run_listener(&cli, address);
    }
    if cli.embed || cli.headless {
        return server::run_stdio(&cli);
    }
    runtime::run_interactive(&cli)
}
