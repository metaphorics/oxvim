//! Non-interactive process entry points.

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::cell::RefCell;
use std::rc::Rc;

use ox_editor::{Editor, ExExecutor, ExecOutcome, Geometry, MessageKind};
use ox_eval::{Builtins, Scope};
use ox_eval::BuiltinHost as EvalBuiltins;
use ox_lua::{
    ApiDispatchContext, BuiltinHost, LuaHost, RuntimeRoot, Scheduler, Work, bind_api, bind_variables,
};
use ox_types::{Object, OxStr, Typval};

use crate::cli::{Cli, LuaScript, ShadaConfig, UserConfig};
use crate::AppError;
use crate::server::EditorVariables;

/// Start the terminal client against a child copy of this executable in embed mode.
pub fn run_interactive(cli: &Cli) -> Result<(), AppError> {
    let executable = std::env::current_exe().map_err(AppError::Io)?;
    let mut command = Command::new(executable);
    command.arg("--embed");
    for argument in interactive_child_arguments(cli) {
        command.arg(argument);
    }
    ox_tui::run_command(command).map_err(|error| AppError::Tui(error.to_string()))
}

fn interactive_child_arguments(cli: &Cli) -> Vec<String> {
    let mut arguments = Vec::new();
    if cli.clean {
        arguments.push("--clean".into());
    } else {
        match &cli.user_config {
            UserConfig::Default => {}
            UserConfig::None => arguments.extend(["-u".into(), "NONE".into()]),
            UserConfig::NoRc => arguments.extend(["-u".into(), "NORC".into()]),
            UserConfig::File(path) => arguments.extend(["-u".into(), path.clone()]),
        }
    }
    match &cli.shada {
        ShadaConfig::Default => {}
        ShadaConfig::None => arguments.extend(["-i".into(), "NONE".into()]),
        ShadaConfig::File(path) => arguments.extend(["-i".into(), path.clone()]),
    }
    for pre_command in &cli.pre_commands {
        arguments.extend(["--cmd".into(), pre_command.clone()]);
    }
    for command in &cli.commands {
        arguments.push(format!("+{command}"));
    }
    if let Some(verbose) = &cli.verbose {
        let suffix = verbose.file.as_deref().unwrap_or_default();
        arguments.push(format!("-V{}{suffix}", verbose.level));
    }
    if !cli.files.is_empty() {
        arguments.push("--".into());
        arguments.extend(cli.files.iter().cloned());
    }
    arguments
}

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

/// Execute Ex source read from stdin, with `--cmd` before and `+cmd` after.
pub fn run_batch(pre_commands: &[String], commands: &[String]) -> Result<(), AppError> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).map_err(AppError::Io)?;

    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).map_err(|error| AppError::Editor(error.to_string()))?;
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).map_err(|error| AppError::Editor(error.to_string()))?)
        .map_err(|error| AppError::Editor(error.to_string()))?;
    let mut executor = ExExecutor::new();

    execute_lines(&mut executor, &mut editor, pre_commands)?;
    execute_lines(&mut executor, &mut editor, input.lines().collect::<Vec<_>>().as_slice())?;
    execute_lines(&mut executor, &mut editor, commands)?;

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

fn execute_lines<S: AsRef<str>>(
    executor: &mut ExExecutor,
    editor: &mut Editor,
    lines: &[S],
) -> Result<(), AppError> {
    for line in lines {
        for command in split_commands(line.as_ref()) {
            let outcome = executor
                .execute_line(editor, command)
                .map_err(|error| AppError::Ex(error.to_string()))?;
            if outcome == ExecOutcome::Quit {
                return Ok(());
            }
        }
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
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer(true)
        .map_err(|error| AppError::Editor(error.to_string()))?;
    editor
        .create_tabpage(
            buffer,
            Geometry::new(0, 0, 80, 24)
                .map_err(|error| AppError::Editor(error.to_string()))?,
        )
        .map_err(|error| AppError::Editor(error.to_string()))?;
    editor.vvars_mut().insert(
        OxStr::from("servername"),
        Object::String(OxStr::from("")),
    );
    let editor = Rc::new(RefCell::new(editor));
    let registry = ox_api::core().map_err(|error| AppError::Api(error.to_string()))?;
    let host = LuaHost::new(
        RuntimeRoot::new(runtime_root()?),
        Rc::new(ScriptBuiltins),
        Rc::new(ImmediateScheduler),
    )
    .map_err(|error| AppError::Lua(error.to_string()))?;
    bind_api(
        host.lua(),
        &registry,
        ApiDispatchContext::new(editor.clone()),
        host.fast_callbacks(),
    )
    .map_err(|error| AppError::Lua(error.to_string()))?;
    bind_variables(host.lua(), Rc::new(EditorVariables { editor }))
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
        .set_name(&format!("@{}", script.path))
        .exec()
        .map_err(|error| AppError::Lua(error.to_string()))
}

struct ScriptBuiltins;
impl BuiltinHost for ScriptBuiltins {
    fn call(&self, name: &OxStr, args: Vec<Typval>) -> Result<Typval, String> {
        // Pure-eval vimscript builtins with no editor state: the runtime
        // prelude probes has('win32') during host init
        // (runtime/lua/vim/_core/system.lua), and `-l` scripts may call any
        // stateless builtin.
        let mut builtins = Builtins::without_regex();
        let mut scope = Scope::new();
        EvalBuiltins::call(&mut builtins, name, args, &mut scope)
            .map_err(|error| error.to_string())
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
