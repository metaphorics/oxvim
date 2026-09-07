//! Process builtins: job control, channel writes, and the shell-backed
//! `system`/`systemlist` (upstream `eval/funcs.c`, `channel.c`).

use crate::excmd_exec::ExEditorAccess;
use crate::job::DEFAULT_PTY_SIZE;
use crate::options::OptionValue;
use crate::script::FileIO;
use crate::{Editor, JobCallbacks, JobEvent, JobManager, JobStartOptions};
use ox_eval::EvalError;
use ox_eval::Scope;
use ox_types::{OxStr, Special, Typval};
use ox_uv::process::PtySize;
use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;

use crate::excmd_exec::{
    EvalHost, ExRuntime, LuaExec, call_user_function_with_self, flow_to_eval_error,
};

/// Routes one process builtin.
///
/// Every name [`super::route`] sends to [`super::Family::Process`] is served
/// here. There used to be a second dispatcher below this one -- serving five
/// of these eight names and ending in a bare `unreachable!()` -- which the
/// public `ExExecutor::call_builtin` entry point called directly, so
/// `jobstart`, `system` and `systemlist` (and every other builtin name a
/// caller passed it) panicked instead of running. One match over one name set
/// cannot drift that way.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let runtime = &mut *host.runtime;
    let access = host.access;
    let lua = host.lua;
    match name {
        "jobstop" => {
            let id = job_id(args.first())?;
            let Some(manager) = runtime.jobs.as_mut() else {
                return Ok(Typval::Number(0));
            };
            manager
                .stop(id)
                .map(|stopped| Typval::Number(i64::from(stopped)))
                .map_err(|message| EvalError::new("E900", 0, message))
        }
        "jobpid" => {
            let id = job_id(args.first())?;
            Ok(Typval::Number(
                runtime
                    .jobs
                    .as_ref()
                    .and_then(|jobs| jobs.pid(id))
                    .map_or(0, i64::from),
            ))
        }
        "chansend" | "jobsend" => {
            let id = job_id(args.first())?;
            let data = channel_bytes(args.get(1))?;
            let Some(mut manager) = runtime.jobs.take() else {
                return Ok(Typval::Number(0));
            };
            let sent = manager
                .send(id, data)
                .map_err(|message| EvalError::new("E900", 0, message))?;
            if access.with_ex_editor(|editor| editor.terminal_channel(id).is_some()) {
                let mut events = manager
                    .poll()
                    .map_err(|message| EvalError::new("E900", 0, message))?;
                // `f_chansend` only writes: `channel_send` ends in
                // `wstream_write` (channel.c:661) and no callback fires on
                // its stack. The polled events go back on the queue for the
                // main-loop turn -- the tick -- that delivers them
                // (`schedule_channel_event`, channel.c:729-737); invoking
                // them here would run Lua callbacks under the borrowed
                // executor, and their `vim.fn` re-entry would land on the
                // nested executor's never-pumped job manager.
                manager.defer_events(std::mem::take(&mut events));
                runtime.jobs = Some(manager);
                if let Some(bytes) = runtime
                    .jobs
                    .as_mut()
                    .and_then(|jobs| jobs.take_pty_output(id))
                {
                    access
                        .with_ex_editor(|editor| editor.append_terminal_buffer(id, &bytes))
                        .ok();
                }
            } else {
                runtime.jobs = Some(manager);
            }
            Ok(Typval::Number(i64::from(sent)))
        }
        "jobwait" => {
            let ids = job_ids(args.first())?;
            let timeout = match args.get(1) {
                Some(value) => value_number(value)
                    .ok_or_else(|| EvalError::new("E474", 0, "Invalid argument"))?,
                None => -1,
            };
            let Some(mut manager) = runtime.jobs.take() else {
                return Ok(Typval::list(
                    ids.iter().map(|_| Typval::Number(-3)).collect(),
                ));
            };
            let waited = manager.wait(&ids, timeout);
            for &id in &ids {
                if access.with_ex_editor(|editor| editor.terminal_channel(id).is_some())
                    && let Some(bytes) = manager.take_pty_output(id)
                {
                    access
                        .with_ex_editor(|editor| editor.append_terminal_buffer(id, &bytes))
                        .ok();
                }
            }
            runtime.jobs = Some(manager);
            let (statuses, mut events) =
                waited.map_err(|message| EvalError::new("E900", 0, message))?;
            invoke_job_events(runtime, access, scope, lua, &mut events)?;
            Ok(Typval::list(
                statuses.into_iter().map(Typval::Number).collect(),
            ))
        }
        "jobstart" | "system" | "systemlist" => {
            let shell = access.with_ex_editor(|editor| shell_argv(editor));
            match name {
                "jobstart" => call_job_start(runtime, access, &shell, args),
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
    argv.extend(split_shell_words(&read(
        "shellcmdflag",
        if cfg!(windows) { "/c" } else { "-c" },
    )));
    argv
}

fn split_shell_words(text: &str) -> Vec<String> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('"')
        && let Some((quoted, tail)) = rest.split_once('"')
    {
        let mut argv = vec![quoted.to_owned()];
        argv.extend(tail.split_whitespace().map(str::to_owned));
        return argv;
    }
    text.split_whitespace().map(str::to_owned).collect()
}

/// The current window's `(columns, rows)` a pty job inherits, `(0, 0)` when
/// there is no window. `f_jobstart` reads `curwin` for a `term` job
/// (eval/funcs.c:3505-3506): `w_view_width - win_col_off` by
/// `w_view_height`. This port models no `win_col_off`, so the frame width is
/// the text width.
fn current_window_pty_extent(editor: &Editor) -> (usize, usize) {
    editor.current_window().map_or((0, 0), |window| {
        (
            editor
                .window_geometry(window)
                .map_or(0, |geometry| geometry.width),
            editor.window_text_height(window).unwrap_or(0),
        )
    })
}

/// One pty dimension: `channel_job_start` keeps the `pty_proc_init` default
/// for a zero extent (channel.c:394-398).
fn pty_dimension(extent: usize, fallback: u16) -> u16 {
    if extent == 0 {
        fallback
    } else {
        u16::try_from(extent).unwrap_or(u16::MAX)
    }
}

fn call_job_start<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    shell: &[String],
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let mut options = normalize_job_options(shell, args)?;
    let id = runtime.channel_ids.allocate();
    let Ok(result_id) = i64::try_from(id) else {
        return Ok(Typval::Number(-1));
    };
    let wants_pty = options.pty;
    // A pty job inherits the current window's geometry: upstream sizes a
    // `term` job's pty from `curwin` (`f_jobstart`, eval/funcs.c:3505-3506)
    // and keeps the `pty_proc_init` default for each dimension left at zero
    // (channel.c:394-398). Every pty channel here owns a terminal buffer, so
    // a bare `jobstart(..., {'pty': v:true})` takes the same window default.
    let (window_rows, window_cols) = if wants_pty {
        let (columns, rows) = access.with_ex_editor(|editor| current_window_pty_extent(editor));
        options.pty_size = PtySize {
            columns: pty_dimension(columns, DEFAULT_PTY_SIZE.columns),
            rows: pty_dimension(rows, DEFAULT_PTY_SIZE.rows),
        };
        (rows, columns)
    } else {
        (0, 0)
    };
    let mut manager = match runtime.jobs.take() {
        Some(manager) => manager,
        None => match JobManager::new() {
            Ok(manager) => manager,
            Err(_) => return Ok(Typval::Number(-1)),
        },
    };
    let term = options.term;
    let started = manager.start(id, options);
    if let Ok(_pid) = started
        && wants_pty
    {
        // `jobstart({term=true})` attaches the terminal to the current
        // buffer (`f_jobstart`, eval/funcs.c:3529 `buf_T *const buf =
        // curbuf`); `:terminal` reaches here right after its own `enew`.
        // A bare pty job keeps a hidden single-row buffer. The emulator is
        // pre-sized to the viewport it opens in (`terminal.c` `topts`).
        let attach = if term {
            access.with_ex_editor(|editor| editor.current_buffer())
        } else {
            None
        };
        let rows = if term { window_rows.max(1) } else { 1 };
        let cols = if term {
            window_cols.max(1)
        } else {
            usize::from(DEFAULT_PTY_SIZE.columns)
        };
        let terminal = access.with_ex_editor(|editor| {
            match editor.allocate_terminal_buffer_rows(
                id,
                attach,
                crate::terminal_screen::ScreenSize::new(rows, cols),
            ) {
                Ok(buffer) => {
                    if let Some(pty) = manager.pty_slave(id).map(str::to_owned) {
                        editor.set_terminal_channel_pty(id, Some(pty));
                    }
                    Some(buffer)
                }
                Err(_) => None,
            }
        });
        if let Some(buffer) = terminal {
            manager.set_terminal_buffer(id, buffer);
        }
    }
    runtime.jobs = Some(manager);
    Ok(Typval::Number(if started.is_ok() { result_id } else { -1 }))
}

pub(crate) fn start_terminal<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    command: &str,
) -> ox_eval::Result<(u64, ox_types::BufHandle)> {
    if command.trim().is_empty()
        && access.with_ex_editor(|editor| {
            matches!(
                editor.options().get_global("shell"),
                Ok(OptionValue::String(shell)) if shell.is_empty()
            )
        })
    {
        return Err(EvalError::new("E91", 0, "'shell' option is empty"));
    }
    let shell = access.with_ex_editor(|editor| shell_argv(editor));
    let job_command = if command.trim().is_empty() {
        Typval::List(Rc::new(RefCell::new(ox_types::ListData {
            items: shell
                .iter()
                .map(|part| Typval::String(OxStr::from(part.as_str())))
                .collect(),
            lock: ox_types::LockState::default(),
        })))
    } else {
        Typval::String(OxStr::from(command))
    };
    let options = Typval::dict(vec![(OxStr::from("term"), Typval::Bool(true))]);
    let Typval::Number(id) = call_job_start(runtime, access, &shell, &[job_command, options])?
    else {
        return Err(EvalError::new("E475", 0, "Invalid argument"));
    };
    let Ok(id) = u64::try_from(id) else {
        return Err(EvalError::new("E475", 0, "Invalid argument"));
    };
    let Some(buffer) =
        access.with_ex_editor(|editor| editor.terminal_channel(id).map(|terminal| terminal.buffer))
    else {
        return Err(EvalError::new("E475", 0, "Invalid argument"));
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let terminal_command = if command.trim().is_empty() {
        shell.join(" ")
    } else {
        command.trim().to_owned()
    };
    let name_text = format!("term://{}//{id}:{terminal_command}", cwd.display());
    let name = OxStr::from(name_text.as_str());
    access
        .with_ex_editor(|editor| editor.buffer_mut(buffer).map(|state| state.set_name(name)))
        .map_err(|error| EvalError::new("E475", 0, error.to_string()))?;
    if let Some(manager) = runtime.jobs.as_mut() {
        manager.set_terminal_exit_message(id, runtime.terminal_exit_message);
    }
    Ok((id, buffer))
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
    let input = args
        .get(1)
        .map(|value| channel_bytes(Some(value)))
        .transpose()?
        .unwrap_or_default();
    let Typval::Dict(options) = Typval::dict(Vec::new()) else {
        unreachable!()
    };
    let id = runtime.channel_ids.allocate();
    let start = JobStartOptions {
        program,
        args: command_args,
        environment: None,
        cwd: None,
        detached: false,
        pty: false,
        term: false,
        pty_size: DEFAULT_PTY_SIZE,
        rpc: false,
        stdin_pipe: true,
        stdout_buffered: true,
        stderr_buffered: true,
        // Always a pipe, even for empty input: the child must see EOF on
        // standard input rather than inherit the parent's.
        terminal_buffer: None,
        callbacks: JobCallbacks {
            options,
            stdout: None,
            stderr: None,
            exit: None,
        },
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
    // Events for OTHER jobs collected during the wait go back to the
    // deferred queue: system() must not destroy them, and invoking
    // callbacks here would reenter the executor mid-command (upstream
    // delivers them on the main loop, os/shell.c:957).
    if let Ok((_, events)) = &waited {
        manager.defer_events(events.clone());
    }
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
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let (status, stdout) = run_shell_command(runtime, shell, args)?;
    scope.replace_pair(
        ox_eval::ScopeKind::Vim,
        "shell_error",
        Typval::Number(status),
    );
    Ok(Typval::String(OxStr(stdout)))
}

fn call_systemlist_builtin<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    shell: &[String],
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let keep_empty = args.get(2).is_some_and(value_bool);
    let (status, stdout) = run_shell_command(runtime, shell, args)?;
    scope.replace_pair(
        ox_eval::ScopeKind::Vim,
        "shell_error",
        Typval::Number(status),
    );

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
    let Typval::Dict(options_ref) = options else {
        unreachable!()
    };
    let get = |key: &str| options_ref.borrow().get(key.as_bytes()).cloned();
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
            for entry in &values.borrow().entries {
                let value = value_text(&entry.value)?;
                let name = OsString::from(entry.key.to_string_lossy().into_owned());
                if let Some((_, current)) =
                    environment.iter_mut().find(|(current, _)| current == &name)
                {
                    *current = OsString::from(value);
                } else {
                    environment.push((name, OsString::from(value)));
                }
            }
            Some(environment)
        }
        Some(_) => return Err(EvalError::new("E1206", 0, "env must be a Dictionary")),
    };
    let cwd = get("cwd")
        .map(|value| value_text(&value).map(PathBuf::from))
        .transpose()?;
    let detached = get("detach").is_some_and(|value| value_bool(&value));
    let term = get("term").is_some_and(|value| value_bool(&value));
    let pty = get("pty").is_some_and(|value| value_bool(&value)) || term;
    let rpc = get("rpc").is_some_and(|value| value_bool(&value));
    // Always a pipe when unset, even for empty input: the child must see EOF
    // on standard input rather than inherit the parent's.
    let stdin_pipe = match get("stdin") {
        Some(value) => value_text(&value)? != "null",
        None => true,
    };
    let stdout_buffered = get("stdout_buffered").is_some_and(|value| value_bool(&value));
    let stderr_buffered = get("stderr_buffered").is_some_and(|value| value_bool(&value));
    Ok(JobStartOptions {
        program,
        args: command_args,
        environment,
        cwd,
        detached,
        pty,
        term,
        pty_size: DEFAULT_PTY_SIZE,
        rpc,
        stdin_pipe,
        stdout_buffered,
        stderr_buffered,
        terminal_buffer: None,
        callbacks,
    })
}

/// `shell_build_argv` (`os/shell.c` 60-97): a String command runs through
/// `'shell'` + `'shellcmdflag'`, a List command is the argv itself.
///
/// `$SHELL` was read directly here before, which is not what upstream reads and
/// left `system()` (hardcoded `sh`) and `systemlist()` disagreeing about the
/// shell of the same editor.
fn job_command(
    shell: &[String],
    value: Option<&Typval>,
) -> ox_eval::Result<(PathBuf, Vec<OsString>)> {
    match value {
        Some(Typval::String(command)) if !command.as_bytes().is_empty() => {
            let (program, flags) = shell
                .split_first()
                .ok_or_else(|| EvalError::new("E474", 0, "Invalid argument"))?;
            let mut args: Vec<OsString> = flags.iter().map(OsString::from).collect();
            args.push(OsString::from(command.to_string_lossy().into_owned()));
            Ok((PathBuf::from(program), args))
        }
        Some(Typval::List(items)) => {
            let items = items.borrow();
            let mut values = items
                .items
                .iter()
                .map(value_text)
                .collect::<ox_eval::Result<Vec<_>>>()?;
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
        Some(value @ (Typval::String(_) | Typval::Funcref(_) | Typval::Partial(_))) => {
            Ok(Some(value))
        }
        Some(_) => Err(EvalError::new("E921", 0, "Invalid callback argument")),
    }
}

/// Delivers job events from the front of `events` on the executor this
/// runs under, and requeues what this stack must not deliver.
///
/// The split follows delivery capability. Vimscript callbacks execute on
/// this same stack -- no executor re-entry -- so the synchronous flush
/// semantics hold for them: `f_jobwait` processes each waited job's queue
/// on the main stack before returning statuses (funcs.c:3666-3670 and
/// 3721, `multiqueue_process_events`, multiqueue.c:153-162). A
/// Lua-registered callback instead goes through the Lua host and re-enters
/// the executor `RefCell` that the enclosing `call_builtin` frame holds;
/// that re-entry falls to the nested executor, whose separate job manager
/// nothing pumps, so any job the callback starts would strand. Those
/// events re-defer here -- upstream encodes the same non-recursion in
/// `on_channel_event`'s `callback_busy` re-enqueue (channel.c:758-762) --
/// and the borrow-free driver (the tick's `deliver_deferred_job_events`)
/// delivers them with no borrow live, so their re-entry lands on the
/// primary executor.
///
/// On a Vimscript handler failure, or when a Lua-registered event arrives
/// with no Lua host installed, everything not yet delivered is re-deferred
/// on the installed manager in its original relative order and the error
/// surfaces; a later delivery still serves it.
pub(crate) fn invoke_job_events<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    events: &mut Vec<JobEvent>,
) -> ox_eval::Result<()> {
    // Lua-registered events defer to the manager the moment they are
    // classified, not at the end: a Vimscript callback delivered in between
    // can enqueue newer events (a chansend sweep, a nested wait), and an
    // end-of-loop re-defer would land the older Lua events behind them -
    // upstream has one queue and its order is FIFO (multiqueue).
    let mut lua_deferred = false;
    let mut batch = std::mem::take(events).into_iter();
    while let Some(event) = batch.next() {
        if event_lua_reference(&event).is_some() {
            if lua.is_none() {
                redefer_job_events(runtime, vec![event]);
                redefer_job_events(runtime, batch.collect());
                return Err(EvalError::new(
                    "E5108",
                    0,
                    "Lua callback host is not installed",
                ));
            }
            if let Some(jobs) = runtime.jobs.as_mut() {
                jobs.defer_events(vec![event]);
            }
            lua_deferred = true;
            continue;
        }
        let name = match event.callback {
            Typval::String(name) => name,
            Typval::Funcref(funcref) | Typval::Partial(funcref) => funcref.name,
            _ => continue,
        };
        if let Err(flow) = call_user_function_with_self(
            runtime,
            access,
            scope,
            lua,
            &name.to_string_lossy(),
            event.args,
            1,
            1,
            Some(event.receiver),
        ) {
            redefer_job_events(runtime, batch.collect());
            return Err(flow_to_eval_error(flow, &name.to_string_lossy()));
        }
    }
    if lua_deferred {
        // The jobwait flush re-deferred Lua-registered callbacks; upstream
        // delivers them before `jobwait` returns (funcs.c:3668/3721), so
        // mark the manager for the first borrow-free boundary after the
        // builtin returns.
        if let Some(jobs) = runtime.jobs.as_mut() {
            jobs.set_lua_flush_pending();
        }
    }
    Ok(())
}

/// Extracts a Lua registry reference from a job callback, returning `None`
/// for Vimscript funcrefs and plain string callbacks. A registry reference
/// re-enters the executor through the Lua host, which is what the
/// borrow-held delivery must not run; the server's borrow-free driver
/// keeps the same classification in `lua_job_reference`.
fn event_lua_reference(event: &JobEvent) -> Option<usize> {
    match &event.callback {
        Typval::Funcref(funcref) | Typval::Partial(funcref) => funcref.registry,
        _ => None,
    }
}

/// Puts events this stack must not deliver back on the installed manager's
/// deferred queue; without a manager there is nothing to serve them from.
fn redefer_job_events<F: FileIO>(runtime: &mut ExRuntime<F>, events: Vec<JobEvent>) {
    if events.is_empty() {
        return;
    }
    if let Some(jobs) = runtime.jobs.as_mut() {
        jobs.defer_events(events);
    }
}

fn job_id(value: Option<&Typval>) -> ox_eval::Result<u64> {
    let value = value
        .and_then(value_number)
        .ok_or_else(|| EvalError::new("E475", 0, "Invalid argument: expected job id"))?;
    u64::try_from(value).map_err(|_| EvalError::new("E475", 0, "Invalid argument: expected job id"))
}

fn job_ids(value: Option<&Typval>) -> ox_eval::Result<Vec<u64>> {
    let Some(Typval::List(values)) = value else {
        return Err(EvalError::new("E714", 0, "List required"));
    };
    values
        .borrow()
        .items
        .iter()
        .map(|value| job_id(Some(value)))
        .collect()
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

#[cfg(all(test, unix))]
mod tests {

    use crate::excmd_exec::{LuaExec, LuaExecError};
    use crate::{Editor, ExExecutor, Geometry, JobEvent, TestEditorAccess};
    use ox_eval::Scope;
    use ox_types::{Funcref, Object, OxStr, Typval};
    use std::cell::{Cell, RefCell};
    use std::path::Path;
    use std::rc::Rc;

    fn global(scope: &Scope, name: &str) -> Option<Typval> {
        scope
            .global
            .iter()
            .find(|(key, _)| key.to_string_lossy() == name)
            .map(|(_, value)| value.clone())
    }

    fn global_number(scope: &Scope, name: &str) -> Option<i64> {
        match global(scope, name)? {
            Typval::Number(value) => Some(value),
            Typval::Bool(value) => Some(i64::from(value)),
            _ => None,
        }
    }

    fn global_flag(scope: &Scope, name: &str) -> bool {
        global_number(scope, name) == Some(1)
    }

    #[test]
    fn chansend_to_pty_delivers_stdout_and_exit_events_without_dropping() {
        let _guard = crate::PROCESS_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let source = r#"
            let s:logger = {'events': []}
            function! s:logger.on_stdout(id, data, event)
                let g:stdout_seen = 1
                call add(self.events, a:data)
            endfunction
            function! s:logger.on_exit(id, status, event)
                let g:exit_seen = 1
            endfunction
            let g:job = jobstart(['sh', '-c', 'cat'], s:logger)
            for i in range(30)
                call chansend(g:job, "hello\n")
                if exists('g:stdout_seen')
                    break
                endif
                sleep 20m
            endfor
            call jobstop(g:job)
            for i in range(30)
                call chansend(g:job, "x")
                if exists('g:exit_seen')
                    break
                endif
                sleep 20m
            endfor
            call jobwait([g:job], 2000)
        "#;
        exec.execute_script(&editor, "<test>", source).unwrap();
        assert!(
            global_flag(exec.scope(), "stdout_seen"),
            "chansend poll must deliver on_stdout event"
        );
        assert!(
            global_flag(exec.scope(), "exit_seen"),
            "chansend poll must deliver on_exit event"
        );
    }

    /// Polls the job's pty until the child's `stty size` answer arrives.
    /// `stty size` prints "rows columns" of its controlling terminal — the
    /// winsize the pty was spawned with, read back from inside the child.
    /// The terminal exit message shares the stream, so the answer is the
    /// one line that parses as two numbers.
    fn pty_size_seen_by_child(exec: &mut ExExecutor, job: i64) -> Option<(u16, u16)> {
        let mut output = Vec::new();
        for _ in 0..100 {
            if let Ok(bytes) = exec.take_pty_output(u64::try_from(job).unwrap()) {
                output.extend_from_slice(&bytes);
            }
            for line in String::from_utf8_lossy(&output).split(['\r', '\n']) {
                let mut parts = line.split(' ');
                if let (Some(rows), Some(columns), None) = (
                    parts.next().and_then(|part| part.parse::<u16>().ok()),
                    parts.next().and_then(|part| part.parse::<u16>().ok()),
                    parts.next(),
                ) {
                    return Some((rows, columns));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    // `f_jobstart` sizes a `term` job's pty from the current window
    // (eval/funcs.c:3505-3506): a 120x40 window has 39 text rows (the
    // message row is reserved), so the child must see "39 120".
    #[test]
    fn terminal_job_pty_matches_the_window_geometry() {
        let _guard = crate::PROCESS_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 120, 40).unwrap())
            .unwrap();
        let editor = TestEditorAccess::new(editor);
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "<test>",
            "let g:job = jobstart(['sh', '-c', 'stty size'], {'term': v:true})",
        )
        .unwrap();
        let job = global_number(exec.scope(), "job").unwrap();
        assert_eq!(pty_size_seen_by_child(&mut exec, job), Some((39, 120)));
    }

    // A bare `jobstart(..., {'pty': v:true})` takes the same window default:
    // every pty channel here owns a terminal buffer, so the pty is sized to
    // the window it can be displayed in.
    #[test]
    fn bare_pty_jobstart_uses_the_window_geometry() {
        let _guard = crate::PROCESS_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 120, 40).unwrap())
            .unwrap();
        let editor = TestEditorAccess::new(editor);
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "<test>",
            "let g:job = jobstart(['sh', '-c', 'stty size'], {'pty': v:true})",
        )
        .unwrap();
        let job = global_number(exec.scope(), "job").unwrap();
        assert_eq!(pty_size_seen_by_child(&mut exec, job), Some((39, 120)));
    }

    // With no window to inherit from, each dimension keeps the
    // `pty_proc_init` 80x24 default (channel.c:394-398,
    // os/pty_proc_unix.c:460-461).
    #[test]
    fn pty_jobstart_without_a_window_keeps_the_default_size() {
        let _guard = crate::PROCESS_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "<test>",
            "let g:job = jobstart(['sh', '-c', 'stty size'], {'pty': v:true})",
        )
        .unwrap();
        let job = global_number(exec.scope(), "job").unwrap();
        assert_eq!(pty_size_seen_by_child(&mut exec, job), Some((24, 80)));
    }

    // chansend's poll exists to collect the pty echo, and the job events it
    // sweeps up with it must not run on chansend's stack. Upstream's
    // `f_chansend` only writes (funcs.c:649-694; `channel_send` ends in
    // `wstream_write`, channel.c:661) and delivery stays on the main loop;
    // here the swept events re-defer and a later drain -- jobwait here, the
    // tick in interactive mode -- delivers them. Delivering them inline
    // under the borrowed executor is what sent a Lua callback's `vim.fn`
    // re-entry to the nested executor's never-pumped manager.
    #[test]
    fn chansend_poll_defers_swept_events_for_a_later_drain() {
        let _guard = crate::PROCESS_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let editor = TestEditorAccess::new(editor);
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "<test>",
            "function! Bcb(id, data, event)\nlet g:seen = 1\nendfunction\nlet g:seen = 0\nlet g:writer = jobstart(['sh', '-c', 'echo ready'], {'on_stdout': 'Bcb'})\nlet g:term = jobstart(['cat'], {'term': v:true})\nsleep 250m\ncall chansend(g:term, \"go\\n\")\nlet g:seen_after_chansend = g:seen\ncall jobwait([g:writer], 2000)\nlet g:seen_after_drain = g:seen",
        )
        .unwrap();
        assert!(
            !global_flag(exec.scope(), "seen_after_chansend"),
            "chansend must not deliver swept job events on its own stack"
        );
        assert!(
            global_flag(exec.scope(), "seen_after_drain"),
            "the swept event must surface on the next drain, not be dropped"
        );
    }

    // The delivery split under the executor's borrow: Vimscript callbacks
    // run on this stack -- upstream's flush processes each waited job's
    // queue before returning statuses (funcs.c:3721) -- while a
    // Lua-registered callback, whose invocation re-enters the executor
    // `RefCell` the enclosing `call_builtin` frame holds and whose `vim.fn`
    // work would land on the nested executor's never-pumped manager,
    // re-defers for the borrow-free driver. The Lua host must never be
    // entered from this stack.
    #[test]
    fn job_event_delivery_runs_vimscript_and_defers_lua_registered_events() {
        struct ProbingLua(Cell<usize>);
        impl LuaExec for ProbingLua {
            fn execute_chunk(&mut self, _: &str, _: Vec<Object>) -> Result<Object, LuaExecError> {
                Err(LuaExecError::Load("unused".to_owned()))
            }
            fn execute_file(&mut self, _: &Path) -> Result<(), LuaExecError> {
                Err(LuaExecError::Load("unused".to_owned()))
            }
            fn invoke_callback(
                &mut self,
                _: usize,
                _: Vec<Object>,
            ) -> Result<Object, LuaExecError> {
                self.0.set(self.0.get() + 1);
                Err(LuaExecError::Runtime("must not run here".to_owned()))
            }
        }
        let _guard = crate::PROCESS_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let host = Rc::new(RefCell::new(ProbingLua(Cell::new(0))));
        exec.set_lua_exec(host.clone());
        exec.execute_script(
            &editor,
            "<test>",
            "function! VimCb(id, data, event)\nlet g:delivered = a:event\nendfunction\nlet g:probe = jobstart(['sh', '-c', 'exit 0'])",
        )
        .unwrap();
        let Typval::Dict(receiver) = Typval::dict(Vec::new()) else {
            unreachable!("Typval::dict builds a dict")
        };
        let args = |event: &str| {
            vec![
                Typval::Number(1),
                Typval::list(Vec::new()),
                Typval::String(OxStr::from(event)),
            ]
        };
        let lua_event = JobEvent {
            callback: Typval::Funcref(Funcref {
                name: OxStr::from("probe"),
                args: Vec::new(),
                dict: None,
                registry: Some(42),
            }),
            receiver: receiver.clone(),
            args: args("exit"),
        };
        let vim_event = JobEvent {
            callback: Typval::String(OxStr::from("VimCb")),
            receiver,
            args: args("stdout"),
        };
        exec.defer_job_events(vec![lua_event, vim_event]);
        // The borrow-held seam's split: Vimscript events invoke on this
        // stack; Lua-registered ones stay undelivered for the borrow-free
        // driver, exactly as `invoke_job_events` classifies them.
        let mut redeferred = Vec::new();
        for event in exec.take_deferred_job_events() {
            if super::event_lua_reference(&event).is_some() {
                redeferred.push(event);
                continue;
            }
            exec.invoke_vimscript_job_callback(&editor, event).unwrap();
        }
        assert_eq!(
            global(exec.scope(), "delivered"),
            Some(Typval::String(OxStr::from("stdout"))),
            "the Vimscript callback must run on this stack"
        );
        assert_eq!(
            host.borrow().0.get(),
            0,
            "the Lua host must not be entered from the Vimscript seam"
        );
        exec.defer_job_events(redeferred);
        let requeued = exec.take_deferred_job_events();
        assert_eq!(
            requeued.len(),
            1,
            "the Lua-registered event must re-defer, not drop"
        );
        assert!(
            matches!(&requeued[0].callback, Typval::Funcref(funcref) if funcref.registry == Some(42)),
            "the re-deferred event must be the Lua-registered one"
        );
    }

    #[test]
    fn job_event_burst_delivers_each_event_once_in_order() {
        let _guard = crate::PROCESS_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "<test>",
            "let g:seen = []\nfunction! Burst(id)\ncall add(g:seen, a:id)\nendfunction\nlet g:probe = jobstart(['sh', '-c', 'exit 0'])",
        )
        .unwrap();
        let Typval::Dict(receiver) = Typval::dict(Vec::new()) else {
            unreachable!("dictionary fixture")
        };
        exec.defer_job_events(
            (0..1000)
                .map(|id| JobEvent {
                    callback: Typval::String(OxStr::from("Burst")),
                    receiver: receiver.clone(),
                    args: vec![Typval::Number(id)],
                })
                .collect(),
        );
        exec.evaluate_expression(&editor, "jobwait([g:probe], 0)")
            .unwrap();
        assert_eq!(
            global(exec.scope(), "seen"),
            Some(Typval::list((0..1000).map(Typval::Number).collect()))
        );
        exec.evaluate_expression(&editor, "jobwait([g:probe], 0)")
            .unwrap();
        assert_eq!(
            global(exec.scope(), "seen"),
            Some(Typval::list((0..1000).map(Typval::Number).collect()))
        );
    }

    // A Lua-registered event arriving with no Lua host installed keeps the
    // loud E5108 through the jobwait flush (`invoke_job_events`), and the
    // batch that never reached a handler re-defers in its original order --
    // the old delivery dropped the offending event itself, so a later host
    // could never serve it.
    #[test]
    fn lua_event_without_a_host_reports_e5108_and_requeues_the_batch() {
        let _guard = crate::PROCESS_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "<test>",
            "function! VimCb(id, data, event)\nlet g:seen = a:event\nendfunction\nlet g:probe = jobstart(['sh', '-c', 'exit 0'])",
        )
        .unwrap();
        let Typval::Dict(receiver) = Typval::dict(Vec::new()) else {
            unreachable!("Typval::dict builds a dict")
        };
        let event = |callback| JobEvent {
            callback,
            receiver: receiver.clone(),
            args: vec![
                Typval::Number(1),
                Typval::list(Vec::new()),
                Typval::String(OxStr::from("exit")),
            ],
        };
        exec.defer_job_events(vec![
            event(Typval::String(OxStr::from("VimCb"))),
            event(Typval::Funcref(Funcref {
                name: OxStr::from("probe"),
                args: Vec::new(),
                dict: None,
                registry: Some(7),
            })),
            event(Typval::String(OxStr::from("VimCb"))),
        ]);
        // `jobwait`'s flush is `invoke_job_events`' only caller: it runs the
        // leading Vimscript event, then reports E5108 on the hostless
        // Lua-registered one and requeues the undelivered tail.
        let error = exec
            .evaluate_expression(&editor, "jobwait([g:probe], 0)")
            .expect_err("a Lua-registered event with no host must raise E5108");
        assert!(
            error.to_string().contains("E5108"),
            "unexpected error: {error}"
        );
        assert_eq!(
            global(exec.scope(), "seen"),
            Some(Typval::String(OxStr::from("exit"))),
            "events before the Lua-registered one must still deliver"
        );
        let requeued = exec.take_deferred_job_events();
        assert_eq!(
            requeued.len(),
            2,
            "the Lua-registered event and the tail must survive the error"
        );
        assert!(
            requeued.iter().any(
                |event| matches!(&event.callback, Typval::Funcref(funcref) if funcref.registry == Some(7))
            ),
            "the offending Lua-registered event itself must be requeued"
        );
    }
}
