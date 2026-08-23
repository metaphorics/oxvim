//! Process builtins: job control, channel writes, and the shell-backed
//! `system`/`systemlist` (upstream `eval/funcs.c`, `channel.c`).

use std::ffi::OsString;
use std::path::PathBuf;
use std::cell::RefCell;
use std::rc::Rc;
use ox_eval::EvalError;
use ox_eval::Scope;
use ox_types::{OxStr, Special, Typval};
use crate::options::OptionValue;
use crate::script::FileIO;
use crate::{Editor, JobCallbacks, JobEvent, JobManager, JobStartOptions};

use crate::excmd_exec::{EvalHost, ExRuntime, LuaExec, call_user_function_with_self, flow_to_eval_error, replace_scope_pair, typval_to_object};

/// Routes one process builtin.
///
/// Every name [`super::route`] sends to [`super::Family::Process`] is served
/// here. There used to be a second dispatcher below this one -- serving five
/// of these eight names and ending in a bare `unreachable!()` -- which the
/// public `ExExecutor::call_builtin` entry point called directly, so
/// `jobstart`, `system` and `systemlist` (and every other builtin name a
/// caller passed it) panicked instead of running. One match over one name set
/// cannot drift that way.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let runtime = &mut *host.runtime;
    let editor = &mut *host.editor;
    let lua = host.lua;
    match name {
        "jobstop" => {
            let id = job_id(args.first())?;
            let Some(manager) = runtime.jobs.as_mut() else { return Ok(Typval::Number(0)); };
            manager.stop(id)
                .map(|stopped| Typval::Number(i64::from(stopped)))
                .map_err(|message| EvalError::new("E900", 0, message))
        }
        "jobpid" => {
            let id = job_id(args.first())?;
            Ok(Typval::Number(runtime.jobs.as_ref().and_then(|jobs| jobs.pid(id)).map_or(0, i64::from)))
        }
        "chansend" | "jobsend" => {
            let id = job_id(args.first())?;
            let data = channel_bytes(args.get(1))?;
            let Some(manager) = runtime.jobs.as_mut() else { return Ok(Typval::Number(0)); };
            manager.send(id, data)
                .map(|sent| Typval::Number(i64::from(sent)))
                .map_err(|message| EvalError::new("E900", 0, message))
        }
        "jobwait" => {
            let ids = job_ids(args.first())?;
            let timeout = match args.get(1) {
                Some(value) => value_number(value).ok_or_else(|| EvalError::new("E474", 0, "Invalid argument"))?,
                None => -1,
            };
            let Some(mut manager) = runtime.jobs.take() else {
                return Ok(Typval::list(ids.iter().map(|_| Typval::Number(-3)).collect()));
            };
            let waited = manager.wait(&ids, timeout);
            runtime.jobs = Some(manager);
            let (statuses, events) = waited.map_err(|message| EvalError::new("E900", 0, message))?;
            invoke_job_events(runtime, editor, scope, lua, events)?;
            Ok(Typval::list(statuses.into_iter().map(Typval::Number).collect()))
        }
        "jobstart" | "system" | "systemlist" => {
            let shell = shell_argv(editor);
            match name {
                "jobstart" => call_job_start(runtime, &shell, args),
                "system" => call_system_builtin(runtime, &shell, args, scope),
                _ => call_systemlist_builtin(runtime, &shell, args, scope),
            }
        }
        _ => unreachable!("process builtin route and dispatcher disagree"),
    }
}

/// The `'shell'` + `'shellcmdflag'` prefix a String command is executed
/// through, upstream `shell_build_argv` (`os/shell.c` 60-97).
///
/// Both options may carry arguments of their own, and
/// `set_init_default_shell` (`option.c` 182-199) double-quotes a `$SHELL`
/// holding a space, so a quoted first word is one word.
fn shell_argv(editor: &Editor) -> Vec<String> {
    let read = |name: &str, fallback: &str| match editor.options().get_global(name) {
        Ok(OptionValue::String(value)) if !value.is_empty() => value.clone(),
        _ => fallback.to_owned(),
    };
    let shell = read("shell", if cfg!(windows) { "cmd.exe" } else { "sh" });
    let mut argv = split_shell_words(&shell);
    argv.extend(split_shell_words(&read("shellcmdflag", if cfg!(windows) { "/c" } else { "-c" })));
    argv
}

fn split_shell_words(text: &str) -> Vec<String> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('"') {
        if let Some((quoted, tail)) = rest.split_once('"') {
            let mut argv = vec![quoted.to_owned()];
            argv.extend(tail.split_whitespace().map(str::to_owned));
            return argv;
        }
    }
    text.split_whitespace().map(str::to_owned).collect()
}

fn call_job_start<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    shell: &[String],
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    let options = normalize_job_options(shell, &args)?;
    let id = runtime.channel_ids.allocate();
    let mut manager = match runtime.jobs.take() {
        Some(manager) => manager,
        None => match JobManager::new() {
            Ok(manager) => manager,
            Err(_) => return Ok(Typval::Number(-1)),
        },
    };
    let started = manager.start(id, options);
    runtime.jobs = Some(manager);
    Ok(Typval::Number(if started.is_ok() { id as i64 } else { -1 }))
}

/// `f_system`/`f_systemlist` (`eval/funcs.c`) through `os_system`.
///
/// The optional second argument is the child's standard input, and upstream
/// closes that pipe once it has been written (`os/shell.c` `do_os_system`
/// shuts the input stream down before waiting). Without the close a child that
/// reads to EOF -- `system('cat', '123')` -- never finishes and the wait never
/// returns; that was the one census-3 timeout.
///
/// A shell that cannot be spawned is not an error upstream: `os_system` reports
/// it through `v:shell_error` and yields no output, which is what `nvim` does
/// with an unreachable `'shell'`. The `E677` this used to raise has no upstream
/// counterpart anywhere on this path, and being fatal it destroyed a whole test
/// file's record when `test_cmdline.vim` left `$PATH` poisoned.
fn run_shell_command<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    shell: &[String],
    args: &[Typval],
) -> ox_eval::Result<(i64, Vec<u8>)> {
    let (program, command_args) = job_command(shell, args.first())?;
    let input = args.get(1).map(|value| channel_bytes(Some(value))).transpose()?.unwrap_or_default();
    let Typval::Dict(options) = Typval::dict(Vec::new()) else { unreachable!() };
    let id = runtime.channel_ids.allocate();
    let start = JobStartOptions {
        program,
        args: command_args,
        environment: None,
        cwd: None,
        detached: false,
        pty: false,
        rpc: false,
        // Always a pipe, even for empty input: the child must see EOF on
        // standard input rather than inherit the parent's.
        stdin_pipe: true,
        stdout_buffered: true,
        stderr_buffered: true,
        callbacks: JobCallbacks { options, stdout: None, stderr: None, exit: None },
    };
    let Some(mut manager) = runtime.jobs.take().or_else(|| JobManager::new().ok()) else {
        return Ok((-1, Vec::new()));
    };
    if manager.start(id, start).is_err() {
        runtime.jobs = Some(manager);
        return Ok((-1, Vec::new()));
    }
    let sent = input.is_empty() || manager.send(id, input).is_ok();
    manager.close_input(id);
    let waited = manager.wait(&[id], -1);
    // `os_system` collects the child's standard error into the same buffer as
    // its standard output, which is why `system('nosuchcmd')` answers with the
    // shell's diagnostic rather than an empty string.
    let (mut stdout, stderr) = manager.take_buffered_output(id).unwrap_or_default();
    stdout.extend_from_slice(&stderr);
    runtime.jobs = Some(manager);
    let status = match waited {
        Ok((statuses, _)) if sent => statuses.first().copied().unwrap_or(-1),
        _ => -1,
    };
    Ok((status, stdout))
}

fn call_system_builtin<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    shell: &[String],
    args: Vec<Typval>,
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let (status, stdout) = run_shell_command(runtime, shell, &args)?;
    replace_scope_pair(&mut scope.vim, "shell_error", Typval::Number(status));
    Ok(Typval::String(OxStr(stdout)))
}

fn call_systemlist_builtin<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    shell: &[String],
    args: Vec<Typval>,
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let keep_empty = args.get(2).is_some_and(value_bool);
    let (status, stdout) = run_shell_command(runtime, shell, &args)?;
    replace_scope_pair(&mut scope.vim, "shell_error", Typval::Number(status));

    let mut lines = stdout
        .split(|byte| *byte == b'\n')
        .map(|line| Typval::String(OxStr(line.strip_suffix(b"\r").unwrap_or(line).to_vec())))
        .collect::<Vec<_>>();
    if !keep_empty && stdout.ends_with(b"\n") {
        lines.pop();
    }
    if stdout.is_empty() {
        lines.clear();
    }
    Ok(Typval::list(lines))
}

fn normalize_job_options(shell: &[String], args: &[Typval]) -> ox_eval::Result<JobStartOptions> {
    let (program, command_args) = job_command(shell, args.first())?;
    let options = match args.get(1) {
        None => Typval::dict(Vec::new()),
        Some(Typval::Dict(options)) => Typval::Dict(options.clone()),
        Some(_) => return Err(EvalError::new("E1206", 0, "Dictionary required")),
    };
    let Typval::Dict(options_ref) = options else { unreachable!() };
    let get = |key: &str| {
        options_ref.borrow().entries.iter().find(|(name, _)| name.as_bytes() == key.as_bytes()).map(|(_, value)| value.clone())
    };
    let callbacks = JobCallbacks {
        options: options_ref.clone(),
        stdout: callback_option(get("on_stdout"))?,
        stderr: callback_option(get("on_stderr"))?,
        exit: callback_option(get("on_exit"))?,
    };
    let environment = match get("env") {
        None => None,
        Some(Typval::Dict(values)) => {
            let mut environment = std::env::vars_os().collect::<Vec<_>>();
            for (name, value) in &values.borrow().entries {
                let value = value_text(value)?;
                let name = OsString::from(name.to_string_lossy().into_owned());
                if let Some((_, current)) = environment.iter_mut().find(|(current, _)| current == &name) {
                    *current = OsString::from(value);
                } else {
                    environment.push((name, OsString::from(value)));
                }
            }
            Some(environment)
        }
        Some(_) => return Err(EvalError::new("E1206", 0, "env must be a Dictionary")),
    };
    let cwd = get("cwd").map(|value| value_text(&value).map(PathBuf::from)).transpose()?;
    let stdin_pipe = match get("stdin") {
        Some(value) => value_text(&value)? != "null",
        None => true,
    };
    Ok(JobStartOptions {
        program,
        args: command_args,
        environment,
        cwd,
        detached: get("detach").is_some_and(|value| value_bool(&value)),
        pty: get("pty").is_some_and(|value| value_bool(&value)) || get("term").is_some_and(|value| value_bool(&value)),
        rpc: get("rpc").is_some_and(|value| value_bool(&value)),
        stdin_pipe,
        stdout_buffered: get("stdout_buffered").is_some_and(|value| value_bool(&value)),
        stderr_buffered: get("stderr_buffered").is_some_and(|value| value_bool(&value)),
        callbacks,
    })
}

/// `shell_build_argv` (`os/shell.c` 60-97): a String command runs through
/// `'shell'` + `'shellcmdflag'`, a List command is the argv itself.
///
/// `$SHELL` was read directly here before, which is not what upstream reads and
/// left `system()` (hardcoded `sh`) and `systemlist()` disagreeing about the
/// shell of the same editor.
fn job_command(shell: &[String], value: Option<&Typval>) -> ox_eval::Result<(PathBuf, Vec<OsString>)> {
    match value {
        Some(Typval::String(command)) if !command.as_bytes().is_empty() => {
            let (program, flags) = shell.split_first().ok_or_else(|| EvalError::new("E474", 0, "Invalid argument"))?;
            let mut args: Vec<OsString> = flags.iter().map(OsString::from).collect();
            args.push(OsString::from(command.to_string_lossy().into_owned()));
            Ok((PathBuf::from(program), args))
        }
        Some(Typval::List(items)) => {
            let items = items.borrow();
            let mut values = items.items.iter().map(value_text).collect::<ox_eval::Result<Vec<_>>>()?;
            if values.first().is_none_or(String::is_empty) {
                return Err(EvalError::new("E474", 0, "Invalid argument"));
            }
            let program = PathBuf::from(values.remove(0));
            Ok((program, values.into_iter().map(OsString::from).collect()))
        }
        _ => Err(EvalError::new("E474", 0, "Invalid argument")),
    }
}

fn callback_option(value: Option<Typval>) -> ox_eval::Result<Option<Typval>> {
    match value {
        None | Some(Typval::Special(Special::Null)) => Ok(None),
        Some(value @ (Typval::String(_) | Typval::Funcref(_) | Typval::Partial(_))) => Ok(Some(value)),
        Some(_) => Err(EvalError::new("E921", 0, "Invalid callback argument")),
    }
}

fn invoke_job_events<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    events: Vec<JobEvent>,
) -> ox_eval::Result<()> {
    for event in events {
        let name = match event.callback {
            Typval::Funcref(funcref) | Typval::Partial(funcref) if funcref.registry.is_some() => {
                let reference = funcref.registry.expect("guarded Lua callback reference");
                let Some(lua) = lua else {
                    return Err(EvalError::new("E5108", 0, "Lua callback host is not installed"));
                };
                let args = event.args.iter().map(typval_to_object).collect();
                lua.borrow_mut()
                    .invoke_callback(editor, reference, args)
                    .map_err(|error| EvalError::new("E5108", 0, format!("{error:?}")))?;
                continue;
            }
            Typval::String(name) => name,
            Typval::Funcref(funcref) | Typval::Partial(funcref) => funcref.name,
            _ => continue,
        };
        call_user_function_with_self(
            runtime, editor, scope, lua, &name.to_string_lossy(), event.args, 1, 1,
            Some(event.receiver),
        )
        .map_err(|flow| flow_to_eval_error(flow, &name.to_string_lossy()))?;
    }
    Ok(())
}

fn job_id(value: Option<&Typval>) -> ox_eval::Result<u64> {
    let value = value.and_then(value_number).ok_or_else(|| EvalError::new("E475", 0, "Invalid argument: expected job id"))?;
    u64::try_from(value).map_err(|_| EvalError::new("E475", 0, "Invalid argument: expected job id"))
}

fn job_ids(value: Option<&Typval>) -> ox_eval::Result<Vec<u64>> {
    let Some(Typval::List(values)) = value else { return Err(EvalError::new("E714", 0, "List required")); };
    values.borrow().items.iter().map(|value| job_id(Some(value))).collect()
}

fn channel_bytes(value: Option<&Typval>) -> ox_eval::Result<Vec<u8>> {
    match value {
        Some(Typval::String(value)) => Ok(value.as_bytes().to_vec()),
        Some(Typval::Blob(value)) => Ok(value.clone()),
        Some(Typval::List(values)) => {
            let values = values.borrow();
            let mut bytes = Vec::new();
            for value in &values.items {
                bytes.extend_from_slice(value_text(value)?.as_bytes());
                bytes.push(b'\n');
            }
            Ok(bytes)
        }
        Some(value) => Ok(value_text(value)?.into_bytes()),
        None => Err(EvalError::new("E119", 0, "Not enough arguments")),
    }
}

fn value_text(value: &Typval) -> ox_eval::Result<String> {
    match value {
        Typval::String(value) => Ok(value.to_string_lossy().into_owned()),
        Typval::Number(value) => Ok(value.to_string()),
        Typval::Bool(value) => Ok(i64::from(*value).to_string()),
        _ => Err(EvalError::new("E730", 0, "Using a non-String as a String")),
    }
}

fn value_number(value: &Typval) -> Option<i64> {
    match value {
        Typval::Number(value) => Some(*value),
        Typval::Bool(value) => Some(i64::from(*value)),
        Typval::Job(value) | Typval::Channel(value) => i64::try_from(*value).ok(),
        _ => None,
    }
}

fn value_bool(value: &Typval) -> bool {
    value_number(value).is_some_and(|value| value != 0)
}
