//! Non-interactive process entry points.

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::rc::Rc;

use ox_editor::{Editor, ExExecutor, ExecOutcome, Geometry, MessageKind};
use ox_lua::{BuiltinHost, LuaHost, RuntimeRoot, Scheduler, Work};
use ox_types::{Object, OxStr, Typval};

use crate::cli::LuaScript;
use crate::AppError;

/// Resolve the runtime root in process startup order.
pub fn runtime_root() -> Result<PathBuf, AppError> {
    if let Some(path) = std::env::var_os("OXVIM_RUNTIME") {
        return Ok(PathBuf::from(path));
    }
    let executable = std::env::current_exe().map_err(AppError::Io)?;
    if let Some(directory) = executable.parent() {
        let relative = directory.join("../../runtime");
        if relative.is_dir() {
            return Ok(relative);
        }
    }
    Ok(PathBuf::from("./runtime"))
}

/// Execute Ex source read from stdin and write command messages.
pub fn run_batch() -> Result<(), AppError> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).map_err(AppError::Io)?;

    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).map_err(|error| AppError::Editor(error.to_string()))?;
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).map_err(|error| AppError::Editor(error.to_string()))?)
        .map_err(|error| AppError::Editor(error.to_string()))?;
    let mut executor = ExExecutor::new();
    'input: for line in input.lines() {
        for command in split_commands(line) {
            let outcome = executor
                .execute_line(&mut editor, command)
                .map_err(|error| AppError::Ex(error.to_string()))?;
            if outcome == ExecOutcome::Quit {
                break 'input;
            }
        }
    }

    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    for message in editor.messages() {
        let destination: &mut dyn Write = if message.kind == MessageKind::Error { &mut err } else { &mut out };
        match &message.content {
            Object::String(text) => destination.write_all(text.as_bytes()).map_err(AppError::Io)?,
            value => write!(destination, "{value:?}").map_err(AppError::Io)?,
        }
        destination.write_all(b"\n").map_err(AppError::Io)?;
    }
    Ok(())
}

fn split_commands(line: &str) -> Vec<&str> {
    let mut commands = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = if quote == Some(character) {
                None
            } else if quote.is_none() {
                Some(character)
            } else {
                quote
            };
        } else if character == '|' && quote.is_none() {
            let command = line[start..index].trim();
            if !command.is_empty() {
                commands.push(command);
            }
            start = index + 1;
        }
    }
    let command = line[start..].trim();
    if !command.is_empty() {
        commands.push(command);
    }
    commands
}

/// Run a Lua file with its trailing argv exposed in `_G.arg`.
pub fn run_lua(script: &LuaScript) -> Result<(), AppError> {
    let source = if script.path == "-" {
        let mut source = Vec::new();
        io::stdin().read_to_end(&mut source).map_err(AppError::Io)?;
        source
    } else {
        fs::read(&script.path).map_err(AppError::Io)?
    };
    let host = LuaHost::new(
        RuntimeRoot::new(runtime_root()?),
        Rc::new(NoBuiltins),
        Rc::new(ImmediateScheduler),
    )
    .map_err(|error| AppError::Lua(error.to_string()))?;
    let lua = host.lua();
    let arguments = lua.create_table().map_err(|error| AppError::Lua(error.to_string()))?;
    arguments.set(0, script.path.as_str()).map_err(|error| AppError::Lua(error.to_string()))?;
    for (index, argument) in script.args.iter().enumerate() {
        arguments
            .set(index + 1, argument.as_str())
            .map_err(|error| AppError::Lua(error.to_string()))?;
    }
    lua.globals().set("arg", arguments).map_err(|error| AppError::Lua(error.to_string()))?;
    lua.load(&source)
        .set_name(&script.path)
        .exec()
        .map_err(|error| AppError::Lua(error.to_string()))
}

struct NoBuiltins;

impl BuiltinHost for NoBuiltins {
    fn call(&self, name: &OxStr, _args: Vec<Typval>) -> Result<Typval, String> {
        Err(format!("Vimscript builtin unavailable: {}", name.to_string_lossy()))
    }
}

struct ImmediateScheduler;

impl Scheduler for ImmediateScheduler {
    fn schedule_deferred(&self, work: Work) -> Result<(), String> {
        work().map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::split_commands;

    #[test]
    fn splits_only_unquoted_unescaped_bars() {
        assert_eq!(split_commands("echo 'a|b' | echo \"c|d\" | echo e\\|f"), ["echo 'a|b'", "echo \"c|d\"", "echo e\\|f"]);
    }
}
