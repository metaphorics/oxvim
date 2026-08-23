//! Non-interactive process entry points.

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::Command;
use std::cell::RefCell;
use std::rc::Rc;

use ox_editor::{
    Editor, EditorError, ExExecutor, ExecOutcome, Geometry, MessageRouting, OptionError, OptionValue,
};
use ox_eval::{Builtins, Scope};
use ox_eval::BuiltinHost as EvalBuiltins;
use ox_lua::{
    ApiDispatchContext, BuiltinHost, LuaHost, RuntimeRoot, Scheduler, Work, bind_api, bind_variables,
};
use ox_text::Buffer;
use ox_types::{BufHandle, Object, OxStr, Typval};

use crate::cli::{Cli, LuaScript, ShadaConfig, UserConfig, WindowLayout};
use crate::AppError;
use crate::messages::PrintfSink;
use crate::server::EditorVariables;
use crate::startuptime::StartupTimer;

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

/// Rebuilds the parsed command line for the embedded child process.
///
/// The child is the editor, so every option the scanner parsed has its
/// effect there: a flag missing here is a flag with no effect at all in the
/// default mode. `-` is deliberately absent, because the child's standard
/// input is the RPC channel and has no buffer text to read.
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
    if !cli.loadplugins {
        arguments.push("--noplugin".into());
    }
    for (requested, flag) in [
        (cli.readonly, "-R"),
        (cli.no_modifiable, "-M"),
        (cli.no_write && !cli.no_modifiable, "-m"),
        (cli.no_swap_file, "-n"),
        (cli.binary, "-b"),
    ] {
        if requested {
            arguments.push(flag.into());
        }
    }
    if let Some(height) = cli.window_height {
        arguments.push(format!("-w{height}"));
    }
    if let Some(flag) = match cli.window_layout {
        WindowLayout::Single => None,
        WindowLayout::Horizontal => Some("-o"),
        WindowLayout::Vertical => Some("-O"),
        WindowLayout::Tabs => Some("-p"),
    } {
        // An explicit count belongs to the flag; zero means one per file.
        if cli.window_count == 0 {
            arguments.push(flag.into());
        } else {
            arguments.push(format!("{flag}{}", cli.window_count));
        }
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

/// Seeds `$VIM` and `$VIMRUNTIME` from the resolved runtime tree (env.c
/// `vim_getenv`: the variables are derived from the executable location and
/// cached with `os_setenv`, so `expand()` and `:set` value expansion see them
/// on every later read). Explicitly exported values win, like upstream.
/// `$VIM` strips a trailing `runtime` component (`remove_tail` on
/// `RUNTIME_DIRNAME`); an unusual layout keeps the runtime path itself.
pub fn export_vim_environment() -> Result<(), AppError> {
    let runtime = runtime_root()?;
    if std::env::var_os("VIMRUNTIME").is_none() {
        ox_sys::set_env("VIMRUNTIME", &runtime);
    }
    if std::env::var_os("VIM").is_none() {
        let vim = if runtime.file_name().is_some_and(|name| name == "runtime") {
            runtime
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| runtime.clone())
        } else {
            runtime.clone()
        };
        ox_sys::set_env("VIM", vim);
    }
    Ok(())
}

/// Applies the startup option flags to a freshly created editor.
///
/// `main.c` `command_line_scan` sets these through `set_option_value` while
/// scanning, before any `--cmd` runs, so a startup command already observes
/// them. Buffer-scoped options land on the startup buffer, which is the only
/// buffer that exists at this point (upstream's `curbuf`).
pub fn apply_startup_options(editor: &mut Editor, cli: &Cli) -> Result<(), AppError> {
    let editor_error = |error: OptionError| AppError::Editor(error.to_string());
    // message.c msg_use_printf/msg_puts_printf read these process modes for
    // every message; main.c sets them while scanning the command line.
    editor.message_routing = MessageRouting {
        embedded: cli.embed,
        silent: cli.batch.is_some_and(|batch| batch.silent),
        ui_attached: false,
    };
    // "-V{level}" is 'verbose' (option.lua varname p_verbose), and a nonzero
    // 'verbose' is what keeps batch mode from dropping message output.
    if let Some(verbose) = &cli.verbose {
        editor
            .options_mut()
            .set_global("verbose", OptionValue::Number(i64::from(verbose.level)))
            .map_err(editor_error)?;
    }
    editor
        .options_mut()
        .set_global("loadplugins", OptionValue::Boolean(cli.loadplugins))
        .map_err(editor_error)?;
    if cli.no_write {
        editor.options_mut().set_global("write", OptionValue::Boolean(false)).map_err(editor_error)?;
    }
    // "-R" also slows the swap file down (`p_uc = 10000`); "-n" turns it off.
    if cli.readonly {
        editor
            .options_mut()
            .set_global("updatecount", OptionValue::Number(10_000))
            .map_err(editor_error)?;
    }
    if cli.no_swap_file {
        editor.options_mut().set_global("updatecount", OptionValue::Number(0)).map_err(editor_error)?;
    }
    if let Some(height) = cli.window_height {
        editor.options_mut().set_global("window", OptionValue::Number(height)).map_err(editor_error)?;
    }
    let Some(buffer) = editor.current_buffer() else { return Ok(()) };
    for (requested, name) in [
        (cli.readonly, "readonly"),
        (cli.no_modifiable, "modifiable"),
        (cli.binary, "binary"),
    ] {
        if requested {
            // 'modifiable' is the only one of the three that is reset.
            let value = OptionValue::Boolean(name != "modifiable");
            editor.options_mut().set_buffer(buffer, name, value).map_err(editor_error)?;
        }
    }
    Ok(())
}

/// Reads one startup file argument into buffer text.  A file that does not
/// exist yet still opens as a named empty buffer, like upstream's buffer
/// creation during argument-list setup; other read failures are `E484`,
/// matching `:edit`'s error for an unreadable file.
fn read_startup_file(file: &str) -> Result<Buffer, AppError> {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(AppError::Ex(format!("E484: Can't open file {file}: {error}"))),
    };
    Buffer::from_bytes(text.as_bytes()).map_err(|error| AppError::Ex(format!("E474: {error}")))
}

/// Turns the positional arguments into buffers and lays out the windows or
/// tab pages that `-o`, `-O` and `-p` asked for.
///
/// This is `main.c` `create_windows()` followed by `edit_buffers()`, and it
/// runs on both startup paths so a layout flag means the same thing in batch
/// mode as it does under a UI.
pub fn open_startup_buffers(editor: &mut Editor, cli: &Cli) -> Result<(), AppError> {
    if cli.stdin_file {
        open_stdin_buffer(editor)?;
    }
    let buffers = open_startup_files(editor, &cli.files)?;
    if cli.window_layout != WindowLayout::Single {
        create_startup_windows(editor, cli, &buffers)?;
    }
    Ok(())
}

/// Opens every positional file argument as a named buffer, mirroring
/// `main.c` `edit_buffers()`: `open_buffer(false, ...)` reads the first file
/// into the startup buffer itself, so buffer numbers match `nvim a b c`, and
/// each remaining file becomes a loaded buffer without stealing the current
/// window.  When a startup script has already replaced or modified the
/// startup buffer, every file, the first included, gets its own buffer.
fn open_startup_files(
    editor: &mut Editor,
    files: &[String],
) -> Result<Vec<BufHandle>, AppError> {
    let first_into_current = editor.current_buffer().is_some_and(|current| {
        editor
            .buffer(current)
            .is_ok_and(|state| state.name().as_bytes().is_empty() && !state.modified)
    });
    let mut handles = Vec::with_capacity(files.len());
    for (index, file) in files.iter().enumerate() {
        let text = read_startup_file(file)?;
        if index == 0 && first_into_current {
            let current = editor
                .current_buffer()
                .ok_or_else(|| AppError::Editor("no current buffer at startup".into()))?;
            if let Ok(state) = editor.buffer_mut(current) {
                state.load(text);
                state.set_name(OxStr::from(file.as_str()));
            }
            handles.push(current);
            continue;
        }
        let handle = editor
            .create_buffer_with(text, true)
            .map_err(|error| AppError::Editor(error.to_string()))?;
        if let Ok(state) = editor.buffer_mut(handle) {
            state.set_name(OxStr::from(file.as_str()));
            state.mark_saved();
        }
        handles.push(handle);
    }
    Ok(handles)
}

/// Reads standard input into the startup buffer, upstream's `EDIT_STDIN` for
/// a bare `-` argument. The buffer stays nameless, like upstream's.
fn open_stdin_buffer(editor: &mut Editor) -> Result<(), AppError> {
    let mut input = Vec::new();
    io::stdin().lock().read_to_end(&mut input).map_err(AppError::Io)?;
    let text = Buffer::from_bytes(&input).map_err(|error| AppError::Ex(format!("E474: {error}")))?;
    let current = editor
        .current_buffer()
        .ok_or_else(|| AppError::Editor("no current buffer at startup".into()))?;
    if let Ok(state) = editor.buffer_mut(current) {
        state.load(text);
    }
    Ok(())
}

/// Builds the `-o`/`-O`/`-p` layout, upstream `main.c` `create_windows()`.
///
/// The startup window or tab page already shows the first buffer, so this
/// adds `count - 1` more. Each new one shows the next startup buffer;
/// windows past the last file get a fresh empty buffer, matching upstream,
/// where the extra split windows are never edited into. The first window of
/// the first tab page stays current.
fn create_startup_windows(
    editor: &mut Editor,
    cli: &Cli,
    buffers: &[BufHandle],
) -> Result<(), AppError> {
    let editor_error = |error: EditorError| AppError::Editor(error.to_string());
    let first_window = editor
        .current_window()
        .ok_or_else(|| AppError::Editor("no current window at startup".into()))?;
    let first_tab = editor
        .current_tabpage()
        .ok_or_else(|| AppError::Editor("no current tabpage at startup".into()))?;
    let mut previous = first_window;
    for index in 1..cli.startup_window_count() {
        let buffer = match buffers.get(index) {
            Some(buffer) => *buffer,
            None => editor.create_buffer(true).map_err(editor_error)?,
        };
        if cli.window_layout == WindowLayout::Tabs {
            let geometry = Geometry::new(0, 0, 80, 24)
                .map_err(|error| AppError::Editor(error.to_string()))?;
            editor.create_tabpage(buffer, geometry).map_err(editor_error)?;
            continue;
        }
        previous = if cli.window_layout == WindowLayout::Vertical {
            editor.split_vertical(first_tab, previous, buffer).map_err(editor_error)?
        } else {
            editor.split_horizontal(first_tab, previous, buffer).map_err(editor_error)?
        };
    }
    editor.set_current_tabpage(first_tab).map_err(editor_error)?;
    editor.set_current_window(first_window).map_err(editor_error)
}

/// Execute Ex source read from stdin, with `--cmd` before startup and `+cmd`
/// after it.
///
/// `main.c` finishes startup (`--cmd`, config, files, `-c`/`+cmd`,
/// `VimEnter`) and only then enters the Ex command loop that reads standard
/// input, so every startup command is already done by the first input line.
/// `-E`/`-Es` set upstream's `input_istext`, which instead reads standard
/// input as buffer text during startup, before the `-c`/`+cmd` arguments.
pub fn run_batch(cli: &Cli, timer: &mut StartupTimer) -> Result<(), AppError> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).map_err(AppError::Io)?;

    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).map_err(|error| AppError::Editor(error.to_string()))?;
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).map_err(|error| AppError::Editor(error.to_string()))?)
        .map_err(|error| AppError::Editor(error.to_string()))?;
    apply_startup_options(&mut editor, cli)?;
    // option.c set_init_default for 'runtimepath'/'packpath' before any
    // user command runs (option.c runtimepath_default layout).
    let default_rtp = ox_editor::default_runtimepath(cli.clean, &runtime_root()?);
    editor
        .options_mut()
        .set_global("runtimepath", OptionValue::String(default_rtp.clone()))
        .map_err(|error| AppError::Editor(error.to_string()))?;
    editor
        .options_mut()
        .set_global("packpath", OptionValue::String(default_rtp.clone()))
        .map_err(|error| AppError::Editor(error.to_string()))?;
    let mut executor = ExExecutor::new();
    executor
        .scripts_mut()
        .set_runtime_roots_from_rtp(&default_rtp);
    executor.set_channel_ids(editor.channel_ids());

    execute_lines(&mut executor, &mut editor, &cli.pre_commands)?;
    timer.mark("sourcing vimrc file(s)");
    let input_is_text = cli.batch.is_some_and(|batch| batch.input_is_text);
    if input_is_text {
        let text = Buffer::from_bytes(input.as_bytes())
            .map_err(|error| AppError::Ex(format!("E474: {error}")))?;
        editor
            .buffer_mut(buffer)
            .map_err(|error| AppError::Editor(error.to_string()))?
            .load(text);
    } else {
        open_startup_buffers(&mut editor, cli)?;
    }
    timer.mark("opening buffers");
    execute_lines(&mut executor, &mut editor, &cli.commands)?;
    if !input_is_text {
        execute_lines(&mut executor, &mut editor, input.lines().collect::<Vec<_>>().as_slice())?;
    }

    let mut sink = PrintfSink::default();
    for (message, destination) in editor.messages().iter().zip(editor.message_destinations()) {
        sink.write(*destination, message).map_err(AppError::Io)?;
    }
    sink.finish(editor.message_routing).map_err(AppError::Io)?;
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
            if let ExecOutcome::Quit(_) = outcome {
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
