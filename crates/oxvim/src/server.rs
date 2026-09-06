//! Embedded stdio and listening RPC servers.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;

use mlua::{Function, Lua, MultiValue, Table, Value, Variadic};
use ox_api::{
    ApiSession, AutocmdExecution, AutocmdExecutor, ChannelInfo, CommandExecutor, LuaExecutor,
    Registry, close_channel, register_channel,
};
use ox_editor::job::JobEvent;
use ox_editor::{
    AutocmdAction, AutocmdContext, AutocmdKind, ChannelIds, CmdlineKind, Editor, Event, ExExecutor,
    ExecError, ExecOutcome, Geometry, Keys, LuaExec, LuaExecError, MessageDestination, MessageKind,
    Mode, ModeMachine, OptionValue, PendingEditMode, ServerHost, TypeaheadFlags, UserCommand,
    VisualKind, vim_variable_is_writable,
};
use ox_lua::{
    ApiDispatchContext, BuiltinHost, EventLoopPump, LuaHost, RuntimeRoot as LuaRuntimeRoot,
    Scheduler, VariableHost, VariableScope, Work, bind_api, bind_variables, bind_with,
    call_with_traceback, collect_typval_refs, error_shim, free_lua_ref, free_typval_refs,
    lua_to_object, lua_to_object_ref, lua_to_typval, object_to_lua, typval_to_lua,
};
use ox_rpc::{CHAN_STDIO, ChannelId, IncrementalDecoder, Message};
use ox_types::{ApiError, BufHandle, Dict, Object, OxStr, TabHandle, Typval, WinHandle};
use ox_ui::{
    CmdlineState as UiCmdlineState, Compositor, ContentChunk, Emitter, Highlight, HlAttrs,
    MessageState, RedrawOutput, UiOptions,
};
#[cfg(unix)]
use ox_uv::dns;
#[cfg(unix)]
use ox_uv::net::Pipe;
use ox_uv::{Handle, HandleId, NetEvent, RunMode, Tcp, UvLoop};
#[cfg(unix)]
use ox_uv::{Poll, PollEvents};

use crate::AppError;
use crate::cli::{Cli, UserConfig};
use crate::messages::PrintfSink;
use crate::runtime::{apply_startup_options, open_startup_buffers, runtime_root};
use crate::startuptime::StartupTimer;

#[derive(Default)]
struct TerminalChannelSink {
    output: BTreeMap<u64, Vec<u8>>,
}

impl ox_api::ChannelSink for TerminalChannelSink {
    fn send(&mut self, channel: u64, bytes: &[u8]) -> Result<(), String> {
        self.output
            .entry(channel)
            .or_default()
            .extend_from_slice(bytes);
        Ok(())
    }
}

struct JobChannelSink {
    session: Rc<ApiSession>,
    ex: Rc<RefCell<ExExecutor>>,
    queue: Rc<RefCell<VecDeque<Work>>>,
    /// Number of deferred sends; when > 0 the borrow is held by an outer
    /// RPC handler and new sends must be queued to preserve order.
    deferred: Rc<Cell<usize>>,
}
impl JobChannelSink {
    fn run_send(&self, channel: u64, bytes: Vec<u8>) {
        if let Ok(mut ex) = self.ex.try_borrow_mut() {
            let sent = ex.job_send(channel, &bytes);
            report_job_send(&self.session, sent, channel);
            return;
        }
        let ex = self.ex.clone();
        let session = self.session.clone();
        let queue = self.queue.clone();
        let deferred = self.deferred.clone();
        deferred.set(deferred.get().saturating_add(1));
        queue.borrow_mut().push_back(Box::new(move || {
            let sent = ex.borrow_mut().job_send(channel, &bytes);
            report_job_send(&session, sent, channel);
            deferred.set(deferred.get().saturating_sub(1));
            Ok(())
        }));
    }
}

/// A failed or missing job-channel write reports through the message system
/// instead of vanishing: queued sends run after `send` returned, so there is
/// no caller left to receive the result (upstream logs channel write
/// failures). `Ok(false)` means the channel no longer exists, e.g. the job
/// exited while terminal metadata is still alive.
fn report_job_send(session: &Rc<ApiSession>, sent: Result<bool, String>, channel: u64) {
    let message = match sent {
        Ok(true) => return,
        Ok(false) => format!("channel {channel} write failed: channel does not exist"),
        Err(error) => format!("channel {channel} write failed: {error}"),
    };
    session.with_editor_mut(|editor| {
        ox_editor::excmd_exec::push_text_message(editor, message, true, true);
    });
}

/// A failing job-callback delivery reports through the message system the
/// way upstream treats callback failures (`emsg` in `invoke_callback`,
/// eval/funcs.c): the loop keeps running, the user sees why the event did
/// not reach its handler.
fn report_job_callback_error(session: &ApiSession, error: &str) {
    session.with_editor_mut(|editor| {
        ox_editor::excmd_exec::push_text_message(
            editor,
            format!("job callback delivery failed: {error}"),
            true,
            true,
        );
    });
}
/// Extracts a Lua registry reference from a deferred job callback, returning
/// `None` for Vimscript funcrefs and plain string callbacks.
fn lua_job_reference(event: &JobEvent) -> Option<usize> {
    match &event.callback {
        Typval::Funcref(funcref) | Typval::Partial(funcref) => funcref.registry,
        _ => None,
    }
}

/// Delivers one batch of deferred job events without holding the executor
/// [`RefCell`] across callbacks (upstream `process_events` → `channel_write` →
/// `invoke_callback`, event/loop.c, runs all callbacks on the main stack).
///
/// Phase A drains the batch with a short `ex` borrow. Phase B invokes each
/// callback with the borrow dropped -- Lua callbacks go straight to the Lua
/// host, Vimscript callbacks reborrow `ex` for one event. Phase C requeues
/// the unconsumed tail on handler failure and updates the delivered flag.
fn deliver_deferred_job_events(
    session: &ApiSession,
    ex: &Rc<RefCell<ExExecutor>>,
) -> Result<bool, String> {
    let lua = ex.borrow().lua_host();
    let mut batch: VecDeque<JobEvent> = ex.borrow_mut().take_deferred_job_events().into();
    if batch.is_empty() {
        return Ok(false);
    }
    let batch_len = batch.len();
    let mut error = None;
    // A hostless Lua event is recoverable - a later host could serve it -
    // so it parks outside the drain and the loop continues past it: the
    // report fires once per pass, the rest of the batch still delivers,
    // and the event re-defers for a later host (the old head-requeue
    // starved the tail forever; pushing it back into this batch would
    // re-pop it forever). Script errors keep the flush contract: the
    // failing event is consumed, the tail re-defers in order, the error
    // returns.
    let mut parked: Vec<JobEvent> = Vec::new();
    while let Some(event) = batch.pop_front() {
        if let Some(reference) = lua_job_reference(&event) {
            let Some(lua) = lua.as_ref() else {
                report_job_callback_error(session, "E5108: Lua callback host is not installed");
                error = Some("E5108: Lua callback host is not installed".to_owned());
                parked.push(event);
                continue;
            };
            let args = event.args.iter().map(ox_rpc::typval_to_object).collect();
            if let Err(lua_error) = lua.borrow_mut().invoke_callback(reference, args) {
                // The event reached its handler and the handler failed:
                // consumed, like upstream's per-event multiqueue processing;
                // requeueing it would retry every drain.
                error = Some(format!("E5108: {lua_error}"));
                break;
            }
        } else {
            let Ok(mut guard) = ex.try_borrow_mut() else {
                batch.push_front(event);
                break;
            };
            if let Err(invoke_error) = guard.invoke_vimscript_job_callback(session, event) {
                error = Some(invoke_error);
                break;
            }
        }
    }
    parked.extend(batch);
    if !parked.is_empty() {
        ex.borrow_mut().defer_job_events(parked);
    }
    match error {
        Some(error) => Err(error),
        None => Ok(batch_len > 0),
    }
}

impl ox_api::ChannelSink for JobChannelSink {
    fn send(&mut self, channel: u64, bytes: &[u8]) -> Result<(), String> {
        if self.deferred.get() > 0 {
            let ex = self.ex.clone();
            let session = self.session.clone();
            let queue = self.queue.clone();
            let deferred = self.deferred.clone();
            let bytes = bytes.to_vec();
            deferred.set(deferred.get().saturating_add(1));
            queue.borrow_mut().push_back(Box::new(move || {
                let sent = ex.borrow_mut().job_send(channel, &bytes);
                report_job_send(&session, sent, channel);
                deferred.set(deferred.get().saturating_sub(1));
                Ok(())
            }));
            return Ok(());
        }
        self.run_send(channel, bytes.to_vec());
        Ok(())
    }

    fn take_pty_output(&mut self, channel: u64) -> Result<Vec<u8>, String> {
        match self.ex.try_borrow_mut() {
            Ok(mut ex) => ex.take_pty_output(channel).map_err(|error| error.clone()),
            Err(_) => Ok(Vec::new()),
        }
    }
}
/// Every `plugin/**/*.vim` then every `plugin/**/*.lua` under one
/// `'runtimepath'` entry, in the order `load_plugins` sources them.
///
/// `gen_expand_wildcards` sorts each pattern's matches, and
/// `source_callback_vim_lua` (runtime.c:371-396) then walks the whole match
/// list twice -- `.vim` first, `.lua` second -- so a `plugin/a.lua` is sourced
/// after a `plugin/z.vim`. Files with any other extension are not sourced by
/// this path at all.
fn plugin_scripts(root: &Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, found);
            } else {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(&root.join("plugin"), &mut found);
    let extension =
        |path: &Path, wanted: &str| path.extension().is_some_and(|value| value == wanted);
    let mut ordered: Vec<_> = found
        .iter()
        .filter(|path| extension(path, "vim"))
        .cloned()
        .collect();
    ordered.extend(found.iter().filter(|path| extension(path, "lua")).cloned());
    ordered
}

/// All mutable state shared by every RPC transport.
pub struct AppState {
    session: Rc<ApiSession>,
    lua: Rc<RefCell<LuaHost>>,
    registry: Rc<Registry>,
    ex: Rc<RefCell<ExExecutor>>,
    mode: Rc<RefCell<ModeMachine>>,
    exiting: bool,
    /// Process exit code requested by `:cquit` (0 for plain quits).
    exit_code: i64,
    rendered_messages: usize,
    /// Stdout/stderr message output for the modes with no attached UI.
    printf: PrintfSink,
    lua_work: Rc<RefCell<VecDeque<Work>>>,
    emitter: Emitter,
    /// Long-lived render state: the layer stack and its grid buffers, rebuilt
    /// in place on each redraw rather than reconstructed.
    compositor: Compositor,
}

/// The editor/Lua/Ex triangle every process mode shares: one editor, one
/// Lua state with the API and variables bound over it, and the Ex
/// executors its Lua builtins re-enter. The embed/RPC server and the
/// batch (`-e -s`) runner both build this once, so `:lua`, `luaeval()`,
/// and Lua-side API dispatch behave identically in every mode.
pub(crate) struct EmbeddedCore {
    pub(crate) session: Rc<ApiSession>,
    pub(crate) lua: Rc<RefCell<LuaHost>>,
    pub(crate) registry: Rc<Registry>,
    pub(crate) ex: Rc<RefCell<ExExecutor>>,
    pub(crate) nested_ex: Rc<RefCell<ExExecutor>>,
    pub(crate) lua_work: Rc<RefCell<VecDeque<Work>>>,
}

/// Wire one editor into the Lua host, the API registry, and the Ex
/// executors, mirroring `AppState` startup for every process mode:
/// runtimepath defaults, channel/job/command sinks, the embedded core
/// prelude, the `LuaExec` host the Ex executor calls for `:lua`,
/// `:luafile`, `:luado`, and `luaeval()`, and the API-level Lua and
/// autocmd hosts nested `nvim_exec_lua`/`nvim_exec_autocmds` re-enter.
///
/// The primary and nested `ExExecutor` pair is the Rust borrow-reentry
/// mechanism: nested Ex work runs on `nested_ex` against the live editor
/// once `ex` is already borrowed by the enclosing command.
#[expect(
    clippy::too_many_lines,
    reason = "startup host wiring order preserves shared callback and executor lifetimes"
)]
pub(crate) fn build_embedded_core(
    mut editor: Editor,
    clean: bool,
) -> Result<EmbeddedCore, AppError> {
    // Stale callbacks from a previous core on this thread must not fire on
    // the new Lua state (see ox_lua::ui_events::reset).
    ox_lua::reset_ui_events();
    // option.c set_init_default for 'runtimepath'/'packpath': the
    // runtimepath_default layout over the resolved runtime tree,
    // before any user startup command runs.
    // Sourcing-relevant roots never fall back to the launch directory
    // (upstream os/env.c:884-936): an unresolved tree contributes no entry
    // instead of auto-sourcing a planted `./runtime`.
    let default_rtp = ox_editor::default_runtimepath(clean, &runtime_root().unwrap_or_default());
    editor
        .options_mut()
        .set_global("runtimepath", OptionValue::String(default_rtp.clone()))
        .map_err(|error| AppError::Editor(error.to_string()))?;
    editor
        .options_mut()
        .set_global("packpath", OptionValue::String(default_rtp.clone()))
        .map_err(|error| AppError::Editor(error.to_string()))?;
    // The session is the sole editor carrier: everything below reaches the
    // editor through `session.with_editor*`, never a parallel `Rc`.
    let session = Rc::new(ApiSession::new(Rc::new(RefCell::new(editor))));
    let registry = Rc::new(ox_api::core().map_err(|error| AppError::Api(error.to_string()))?);
    let lua_work = Rc::new(RefCell::new(VecDeque::new()));
    let ex = Rc::new(RefCell::new(ExExecutor::new()));
    let nested_ex = Rc::new(RefCell::new(ExExecutor::new()));
    // Runtime searches follow &runtimepath (the seeded default includes
    // the runtime tree, matching the previous single-root setup).
    ex.borrow_mut()
        .scripts_mut()
        .set_runtime_roots_from_rtp(&default_rtp);
    nested_ex
        .borrow_mut()
        .scripts_mut()
        .set_runtime_roots_from_rtp(&default_rtp);
    let channel_ids = session.with_editor(Editor::channel_ids);
    ex.borrow_mut().set_channel_ids(channel_ids.clone());
    nested_ex.borrow_mut().set_channel_ids(channel_ids.clone());
    // The nested executor shares durable definitions with the primary so
    // reentrant API paths observe the same user commands and functions.
    nested_ex
        .borrow_mut()
        .share_user_commands_from(&ex.borrow());
    nested_ex
        .borrow_mut()
        .share_user_functions_from(&ex.borrow());
    let mut lua = LuaHost::new(
        LuaRuntimeRoot::new(runtime_root().unwrap_or_default()),
        Rc::new(EditorBuiltins {
            session: session.clone(),
            ex: ex.clone(),
        }),
        Rc::new(LuaScheduler {
            queue: lua_work.clone(),
        }),
    )
    .map_err(|error| AppError::Lua(error.to_string()))?;
    bind_api(
        lua.lua(),
        &registry,
        ApiDispatchContext::new(session.clone()),
        lua.fast_callbacks(),
    )
    .map_err(|error| AppError::Lua(error.to_string()))?;
    bind_with(
        lua.lua(),
        ApiDispatchContext::new(session.clone()),
        lua.fast_callbacks(),
    )
    .map_err(|error| AppError::Lua(error.to_string()))?;
    let ui_context = ApiDispatchContext::new(session.clone());
    let ui_fast = lua.fast_callbacks();
    ox_lua::bind_ui_events(lua.lua(), &ui_context, &ui_fast)
        .map_err(|error| AppError::Lua(error.to_string()))?;
    bind_variables(
        lua.lua(),
        Rc::new(EditorVariables {
            session: session.clone(),
        }),
    )
    .map_err(|error| AppError::Lua(error.to_string()))?;
    ox_api::set_channel_sink(&session, Box::new(TerminalChannelSink::default()));
    ox_api::set_job_sink(
        &session,
        Box::new(JobChannelSink {
            session: session.clone(),
            ex: ex.clone(),
            queue: lua_work.clone(),
            deferred: Rc::new(Cell::new(0)),
        }),
    );
    let event_loop = lua.event_loop_pump();
    ox_api::set_command_executor(
        &session,
        Box::new(ServerCommandHost {
            session: session.clone(),
            ex: ex.clone(),
            nested_ex: nested_ex.clone(),
            lua: lua.lua().clone(),
            registry: registry.clone(),
            channel_ids: channel_ids.clone(),
            event_loop: event_loop.clone(),
        }),
        Box::new(ServerCommandHost {
            session: session.clone(),
            ex: ex.clone(),
            nested_ex: nested_ex.clone(),
            lua: lua.lua().clone(),
            registry: registry.clone(),
            channel_ids: channel_ids.clone(),
            event_loop: event_loop.clone(),
        }),
    );
    ox_api::set_lua_executor(
        &session,
        Box::new(ApiLuaExecutor {
            session: session.clone(),
            lua: lua.lua().clone(),
            registry: registry.clone(),
            ex: ex.clone(),
            nested_ex: nested_ex.clone(),
            channel_ids: channel_ids.clone(),
            event_loop: event_loop.clone(),
        }),
        Box::new(ApiLuaExecutor {
            session: session.clone(),
            lua: lua.lua().clone(),
            registry: registry.clone(),
            ex: ex.clone(),
            nested_ex: nested_ex.clone(),
            channel_ids: channel_ids.clone(),
            event_loop: event_loop.clone(),
        }),
    );
    ox_api::set_autocmd_executor(
        &session,
        Box::new(ServerAutocmdHost {
            session: session.clone(),
            ex: ex.clone(),
            nested_ex: nested_ex.clone(),
            lua: lua.lua().clone(),
            registry: registry.clone(),
            channel_ids: channel_ids.clone(),
            event_loop: event_loop.clone(),
        }),
        Box::new(ServerAutocmdHost {
            session: session.clone(),
            ex: ex.clone(),
            nested_ex: nested_ex.clone(),
            lua: lua.lua().clone(),
            registry: registry.clone(),
            channel_ids: channel_ids.clone(),
            event_loop: event_loop.clone(),
        }),
    );
    // Load the reachable embedded core prelude before user-controlled Ex startup commands.
    lua.exec("require('vim._core.shared')", Vec::new())
        .map_err(|error| AppError::Lua(error.to_string()))?;
    let callback_lua = lua.lua().clone();
    let lua = Rc::new(RefCell::new(lua));
    let callback_host = || {
        Rc::new(RefCell::new(ServerLuaExec {
            session: session.clone(),
            lua: callback_lua.clone(),
            registry: registry.clone(),
            ex: ex.clone(),
            nested_ex: nested_ex.clone(),
            event_loop: event_loop.clone(),
        }))
    };
    ex.borrow_mut().set_lua_exec(callback_host());
    nested_ex.borrow_mut().set_lua_exec(callback_host());
    Ok(EmbeddedCore {
        session,
        lua,
        registry,
        ex,
        nested_ex,
        lua_work,
    })
}

impl AppState {
    /// Build one editor/Lua/API instance and execute process startup.
    pub fn new(cli: &Cli, timer: &mut StartupTimer) -> Result<Self, AppError> {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer(true)
            .map_err(|error| AppError::Editor(error.to_string()))?;
        editor
            .create_tabpage(
                buffer,
                Geometry::new(0, 0, 80, 24).map_err(|error| AppError::Editor(error.to_string()))?,
            )
            .map_err(|error| AppError::Editor(error.to_string()))?;
        editor
            .vvars_mut()
            .insert(OxStr::from("servername"), Object::String(OxStr::from("")));
        crate::runtime::seed_argv(&mut editor);
        apply_startup_options(&mut editor, cli)?;
        let EmbeddedCore {
            session,
            lua,
            registry,
            ex,
            nested_ex,
            lua_work,
        } = build_embedded_core(editor, cli.clean)?;

        let mode = Rc::new(RefCell::new(ModeMachine::default()));
        ex.borrow_mut().set_mode_machine(mode.clone());
        nested_ex.borrow_mut().set_mode_machine(mode.clone());
        ox_api::set_mode_machine(&session, mode.clone());
        let mut state = Self {
            session,
            lua,
            registry,
            ex,
            mode,
            exiting: false,
            exit_code: 0,
            rendered_messages: 0,
            printf: PrintfSink::default(),
            lua_work,
            emitter: Emitter::new(),
            compositor: Compositor::new(1, 1),
        };
        state.run_startup(cli, timer)?;
        // main.c writes startup message output before the process waits on
        // its input, and --headless/-es exit without ever attaching a UI.
        state.publish_messages()?;
        Ok(state)
    }

    fn run_startup(&mut self, cli: &Cli, timer: &mut StartupTimer) -> Result<(), AppError> {
        // main.c fills the global argument list from the command line
        // before any startup command runs, so argc()/argv() observe the
        // pending files even inside --cmd and -S scripts.
        if !cli.files.is_empty() {
            self.session.with_editor_mut(|editor| {
                editor.arglist_mut().set(
                    cli.files
                        .iter()
                        .map(|file| OxStr::from(file.as_str()))
                        .collect(),
                );
            });
        }
        for command in &cli.pre_commands {
            self.execute_ex(command)?;
            if self.exiting {
                return Ok(());
            }
        }

        // main.c `source_startup_scripts` (2229-2249): an explicit `-u` file
        // replaces discovery entirely, `NONE` and `NORC` source nothing at
        // all, and otherwise the user's config is discovered. `-es` skips the
        // whole step upstream (`else if (!silent_mode)`).
        //
        // `--clean` is not a separate case: it *is* `-u NONE`
        // (main.c:1193-1197), so a later `-u <file>` on the same command line
        // overwrites it and is honoured. Gating this on `!cli.clean` made
        // `--clean -u file` ignore the file, which the oracle sources.
        match &cli.user_config {
            UserConfig::File(path) => self.source_config_file(Path::new(path))?,
            UserConfig::None | UserConfig::NoRc => {}
            UserConfig::Default => {
                if !cli.batch.is_some_and(|batch| batch.silent) {
                    self.discover_user_config()?;
                }
            }
        }
        timer.mark("sourcing vimrc file(s)");
        if self.exiting {
            return Ok(());
        }

        // main.c:489 `load_plugins`, after the user config and before the
        // window layout. 'loadplugins' already carries the `--noplugin` and
        // `-u NONE`-unless-`--clean` rules from cli.rs.
        if cli.loadplugins {
            self.load_plugins();
        }
        timer.mark("loading plugins");

        // main.c create_windows()/edit_buffers(): the requested window or
        // tab-page layout is built first, then every positional file becomes
        // a named buffer loaded from disk (upstream also names a buffer when
        // the file does not exist yet) and fills one window in argv order.
        if self.exiting {
            return Ok(());
        }
        self.session
            .with_editor_mut(|editor| open_startup_buffers(editor, cli))?;
        timer.mark("opening buffers");
        for command in &cli.commands {
            self.execute_ex(command)?;
            if self.exiting {
                return Ok(());
            }
        }
        self.fire_vim_enter()
    }

    /// Sources one config file, choosing the host by extension the way
    /// `do_source` picks between `nlua_exec_file` and the Ex parser.
    fn source_config_file(&mut self, path: &Path) -> Result<(), AppError> {
        if path.extension().is_some_and(|extension| extension == "lua") {
            return self
                .lua
                .borrow_mut()
                .exec_file(path)
                .map_err(|error| AppError::Lua(error.to_string()));
        }
        let source = fs::read_to_string(path).map_err(AppError::Io)?;
        let name = path.to_string_lossy().into_owned();
        self.ex
            .borrow_mut()
            .execute_script_core(&*self.session, &name, &source)
            .map_err(|error| AppError::Ex(error.to_string()))?;
        Ok(())
    }

    /// `do_user_initialization` (main.c:2108-2210), in its order:
    ///
    /// 1. `$VIMINIT` as Ex commands, and nothing else if it ran.
    /// 2. `stdpath('config')/init.lua`, then `init.vim`. Only one is sourced;
    ///    when the Lua one wins and the Vim one also exists, upstream reports
    ///    `E5422: Conflicting configs` (errors.h:233) and keeps going.
    /// 3. The same pair under each `stdpath('config_dirs')` entry, in order.
    /// 4. `$EXINIT` as Ex commands.
    ///
    /// This is the step whose absence meant nothing a user wrote ever ran:
    /// before it, only an explicit `-u` was read.
    fn discover_user_config(&mut self) -> Result<(), AppError> {
        if self.execute_env("VIMINIT")? {
            return Ok(());
        }
        let mut bases = ox_editor::stdpath(ox_editor::StdPath::Config);
        bases.extend(ox_editor::stdpath(ox_editor::StdPath::ConfigDirs));
        for base in bases {
            let lua = Path::new(&base).join("init.lua");
            let vim = Path::new(&base).join("init.vim");
            if lua.is_file() {
                self.source_config_file(&lua)?;
                if vim.is_file() {
                    self.session.with_editor_mut(|editor| {
                        editor.push_message(ox_editor::Message {
                            kind: MessageKind::Error,
                            content: Object::String(OxStr::from(
                                format!(
                                    "E5422: Conflicting configs: \"{}\" \"{}\"",
                                    lua.display(),
                                    vim.display()
                                )
                                .as_str(),
                            )),
                            history: true,
                            leading_newline: true,
                        });
                    });
                }
                return Ok(());
            }
            if vim.is_file() {
                return self.source_config_file(&vim);
            }
        }
        self.execute_env("EXINIT").map(|_| ())
    }

    /// `execute_env` (main.c:2257-...): a non-empty environment variable is run
    /// as Ex command lines. Reports whether it ran.
    fn execute_env(&mut self, name: &str) -> Result<bool, AppError> {
        let Some(value) = std::env::var_os(name) else {
            return Ok(false);
        };
        let value = value.to_string_lossy().into_owned();
        if value.is_empty() {
            return Ok(false);
        }
        self.execute_ex(&value)?;
        Ok(true)
    }

    /// `load_plugins` (runtime.c:1397-1424): `plugin/**/*` under every
    /// `'runtimepath'` entry, with the `after/` entries held back to the end
    /// (`DIP_NOAFTER` then `DIP_AFTER`).
    ///
    /// Within one entry every `.vim` file is sourced before every `.lua` file,
    /// which is `source_callback_vim_lua`'s two passes (runtime.c:371-396) and
    /// not an accident of directory order.
    ///
    /// Packages (`pack/*/start/*`) are not sourced here: upstream's
    /// `add_pack_start_dirs`/`load_start_packages` also rewrite
    /// `'runtimepath'`, which is a separate mechanism from this one.
    fn load_plugins(&mut self) {
        let Ok(OptionValue::String(value)) = self
            .session
            .with_editor(|editor| editor.options().get_global("runtimepath").cloned())
        else {
            return;
        };
        let (after, plain): (Vec<&str>, Vec<&str>) = value
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .partition(|entry| {
                Path::new(entry)
                    .file_name()
                    .is_some_and(|name| name == "after")
            });
        for entry in plain.into_iter().chain(after) {
            for script in plugin_scripts(Path::new(entry)) {
                // `source_callback_vim_lua` (runtime.c:371-396) discards
                // `do_source`'s result and sources the next file, so an error
                // inside one plugin ends that plugin and nothing else. One
                // broken plugin must not be able to stop startup -- with the
                // error propagated instead, `runtime/plugin/gzip.vim` took the
                // whole editor down on every plain startup.
                if let Err(error) = self.source_config_file(&script) {
                    self.session.with_editor_mut(|editor| {
                        editor.push_message(ox_editor::Message {
                            kind: MessageKind::Error,
                            content: Object::String(OxStr::from(
                                format!("{}: {error}", script.display()).as_str(),
                            )),
                            history: true,
                            leading_newline: true,
                        });
                    });
                }
                if self.exiting {
                    return;
                }
            }
        }
    }

    fn execute_ex(&mut self, command: &str) -> Result<(), AppError> {
        let outcome = self
            .ex
            .borrow_mut()
            .execute_line_core(&*self.session, command)
            .map_err(|error| AppError::Ex(error.to_string()))?;
        if let Some(pending) = self.ex.borrow_mut().take_pending_edit_mode() {
            Self::apply_pending_edit_mode(&self.session, &self.mode, pending)
                .map_err(|error| AppError::Api(error.to_string()))?;
        }
        if let ExecOutcome::Quit(code) = outcome {
            self.exiting = true;
            self.exit_code = code;
        }
        Ok(())
    }

    fn fire_vim_enter(&mut self) -> Result<(), AppError> {
        let plan = self.session.with_editor_mut(|editor| {
            editor
                .autocmds_mut()
                .plan(Event::VimEnter, AutocmdContext::default())
        });
        ox_api::execute_firing_plan(&self.session, plan)
            .map_err(|error| AppError::Api(error.to_string()))
    }

    fn dispatch(
        &mut self,
        channel: ChannelId,
        method: &OxStr,
        params: &[Object],
    ) -> Result<(Object, BTreeMap<u64, Vec<u8>>), ApiError> {
        let name = method.to_string_lossy();
        let caller = self.session.enter_rpc_call(channel);
        let result = match name.as_ref() {
            "nvim_get_api_info" => self.dispatch_api_info(channel, params),
            "nvim_call_atomic" => self.dispatch_call_atomic(channel, params),
            "nvim_input" => self.dispatch_input(params),
            "nvim_exec_lua" | "nvim_execute_lua" => self.dispatch_lua(params),
            "nvim_command" => self.dispatch_command(params),
            "nvim_cmd" => self.dispatch_nvim_cmd(params),
            "nvim_ui_attach" => self.ui_attach(channel, params),
            "nvim_ui_detach" => self.ui_detach(channel, params),
            "nvim_ui_try_resize" => self.ui_resize(channel, params),
            _ => {
                let Some((_, dispatch)) = self.registry.get(&name) else {
                    return Err(ApiError::exception(Registry::invalid_method_message(
                        name.as_ref(),
                    )));
                };
                dispatch(&self.session, params)
            }
        };
        drop(caller);
        let result = result?;
        if let Some(code) = self.ex.borrow_mut().take_quit() {
            self.exiting = true;
            self.exit_code = code;
        }
        let redraws = if name == "nvim_ui_attach"
            || name == "nvim_ui_try_resize"
            || method_is_mutating(&name)
        {
            self.redraw()?
        } else {
            BTreeMap::new()
        };
        Ok((result, redraws))
    }

    fn dispatch_api_info(
        &mut self,
        channel: ChannelId,
        params: &[Object],
    ) -> Result<Object, ApiError> {
        let Some((_, dispatch)) = self.registry.get("nvim_get_api_info") else {
            return Err(ApiError::exception("nvim_get_api_info is not registered"));
        };
        let mut result = dispatch(&self.session, params)?;
        let Object::Array(info) = &mut result else {
            return Err(ApiError::exception("invalid API metadata response"));
        };
        let Some(id) = info.first_mut() else {
            return Err(ApiError::exception("invalid API metadata response"));
        };
        let channel_id = i64::try_from(channel.get()).map_err(|_| {
            ApiError::exception("channel ID exceeds signed MessagePack integer range")
        })?;
        *id = Object::Integer(channel_id);
        Ok(result)
    }

    fn dispatch_call_atomic(
        &mut self,
        channel: ChannelId,
        params: &[Object],
    ) -> Result<Object, ApiError> {
        let [Object::Array(calls)] = params else {
            return Err(ApiError::validation(
                "nvim_call_atomic expects one Array argument",
            ));
        };
        let mut results = Vec::with_capacity(calls.len());
        for (index, call) in calls.iter().enumerate() {
            let (name, args) = ox_api::decode_atomic_call(call)?;
            let name = name.to_string_lossy();
            let result = match name.as_ref() {
                "nvim_get_api_info" => self.dispatch_api_info(channel, args),
                "nvim_call_atomic" => self.dispatch_call_atomic(channel, args),
                _ => match self.registry.get(&name) {
                    Some((_, dispatch)) => {
                        let caller = self.session.enter_rpc_call(channel);
                        let result = dispatch(&self.session, args);
                        drop(caller);
                        result
                    }
                    None => Err(ApiError::exception(Registry::invalid_method_message(
                        name.as_ref(),
                    ))),
                },
            };
            match result {
                Ok(value) => results.push(value),
                Err(error) => {
                    return Ok(Object::Array(vec![
                        Object::Array(results),
                        Object::Array(vec![
                            Object::Integer(i64::try_from(index).unwrap_or(i64::MAX)),
                            Object::Integer(error.error_type()),
                            Object::String(OxStr::from(error.message())),
                        ]),
                    ]));
                }
            }
        }
        Ok(Object::Array(vec![Object::Array(results), Object::Nil]))
    }

    fn dispatch_input(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::String(input)] = params else {
            return Err(ApiError::validation(
                "nvim_input expects one String argument",
            ));
        };
        let count = i64::try_from(input.as_bytes().len())
            .map_err(|_| ApiError::exception("Input length exceeds Integer range"))?;
        let Some((_, replace)) = self.registry.get("nvim_replace_termcodes") else {
            return Err(ApiError::exception(
                "nvim_replace_termcodes is not registered",
            ));
        };
        let replaced = replace(
            &self.session,
            &[
                Object::String(input.clone()),
                Object::Boolean(false),
                Object::Boolean(true),
                Object::Boolean(true),
            ],
        )?;
        let Object::String(encoded) = replaced else {
            return Err(ApiError::exception(
                "nvim_replace_termcodes returned a non-string",
            ));
        };
        let keys = Keys::from_encoded(encoded.as_bytes().to_vec())
            .map_err(|error| ApiError::exception(error.to_string()))?;
        self.session.with_editor_mut(|editor| {
            editor
                .typeahead_mut()
                .append(&keys, TypeaheadFlags::default());
        });
        Ok(Object::Integer(count))
    }

    fn dispatch_lua(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::String(code), Object::Array(args)] = params else {
            return Err(ApiError::validation(
                "nvim_exec_lua expects (String, Array)",
            ));
        };
        let code = std::str::from_utf8(code.as_bytes())
            .map_err(|_| ApiError::validation("Lua source must be valid UTF-8"))?;
        self.lua
            .borrow_mut()
            .exec(code, args.clone())
            .map_err(|error| ApiError::exception(nvim_exec_lua_error_text(error.to_string())))
    }

    fn dispatch_command(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::String(command)] = params else {
            return Err(ApiError::validation(
                "nvim_command expects one String argument",
            ));
        };
        let command = std::str::from_utf8(command.as_bytes())
            .map_err(|_| ApiError::validation("Ex command must be valid UTF-8"))?;
        let outcome = self
            .ex
            .borrow_mut()
            .execute_line(&*self.session, command)
            .map_err(|error| map_api_exec_error(ApiOperation::Command, error))?;
        if let Some(pending) = self.ex.borrow_mut().take_pending_edit_mode() {
            Self::apply_pending_edit_mode(&self.session, &self.mode, pending)?;
        }
        if let ExecOutcome::Quit(code) = outcome {
            self.exiting = true;
            self.exit_code = code;
        }
        Ok(Object::Nil)
    }

    fn dispatch_nvim_cmd(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let (cmd, opts) = nvim_cmd_args(params)?;
        let mut ex = self.ex.borrow_mut();
        let mut executor = ExApiExecutor {
            executor: &mut ex,
            outcome: ExecOutcome::Completed,
        };
        let result = ox_api::execute_nvim_cmd(&self.session, cmd, opts, &mut executor)?;
        if let Some(pending) = executor.executor.take_pending_edit_mode() {
            Self::apply_pending_edit_mode(&self.session, &self.mode, pending)?;
        }
        if let ExecOutcome::Quit(code) = executor.outcome {
            self.exiting = true;
            self.exit_code = code;
        }
        Ok(Object::String(result))
    }
    fn apply_pending_edit_mode(
        session: &ApiSession,
        machine: &RefCell<ModeMachine>,
        pending: PendingEditMode,
    ) -> Result<(), ApiError> {
        match pending {
            PendingEditMode::Insert => machine.borrow_mut().enter_insert(),
            PendingEditMode::Append => {
                session.with_editor_mut(|editor| {
                    machine
                        .borrow_mut()
                        .enter_append(editor)
                        .map_err(|error| ApiError::exception(error.to_string()))
                })?;
            }
            PendingEditMode::Replace => machine.borrow_mut().enter_replace(),
            PendingEditMode::StopInsert => machine.borrow_mut().stop_insert(),
        }
        Ok(())
    }

    fn resize_current_tabpage(&mut self, width: usize, height: usize) -> Result<(), ApiError> {
        let geometry = Geometry::new(0, 0, width, height)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        self.session.with_editor_mut(|editor| {
            editor
                .resize_tabpage(TabHandle::CURRENT, geometry)
                .map_err(|error| ApiError::exception(error.to_string()))
        })
    }

    fn ui_attach(&mut self, channel: ChannelId, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::Integer(width), Object::Integer(height), raw_options] = params else {
            return Err(ApiError::validation(
                "nvim_ui_attach expects (Integer, Integer, Dict)",
            ));
        };
        let mut options = match raw_options {
            Object::Dict(options) => options.clone(),
            Object::Array(values) if values.is_empty() => Dict(Vec::new()),
            _ => {
                return Err(ApiError::validation(
                    "nvim_ui_attach expects (Integer, Integer, Dict)",
                ));
            }
        };
        let width = positive_dimension(*width, "width")?;
        let height = positive_dimension(*height, "height")?;
        // RGB is the historical default protocol request.  ox-ui implements
        // the modern linegrid protocol only, so RGB implies that supported
        // representation rather than falling back to a legacy cell protocol.
        if matches!(
            options.get(&OxStr::from("rgb")),
            Some(Object::Boolean(true))
        ) && options.get(&OxStr::from("ext_linegrid")).is_none()
        {
            options
                .0
                .push((OxStr::from("ext_linegrid"), Object::Boolean(true)));
        }
        self.session
            .with_render_state(|ui_channels, _, _| {
                ui_channels.attach(channel.get(), width, height, UiOptions::from_dict(&options))
            })
            .map_err(|error| ApiError::exception(error.to_string()))?;
        if let Err(error) = self.resize_current_tabpage(width, height) {
            let _ = self
                .session
                .with_render_state(|ui_channels, _, _| ui_channels.detach(channel.get()));
            return Err(error);
        }
        self.sync_ui_active();
        Ok(Object::Nil)
    }

    fn ui_detach(&mut self, channel: ChannelId, params: &[Object]) -> Result<Object, ApiError> {
        if !params.is_empty() {
            return Err(ApiError::validation("nvim_ui_detach expects no arguments"));
        }
        self.session.with_render_state(|ui_channels, _, _| {
            ui_channels
                .detach(channel.get())
                .map_err(|error| ApiError::exception(error.to_string()))
        })?;
        self.emitter.detach(channel.get());
        self.sync_ui_active();
        Ok(Object::Nil)
    }

    /// Mirrors `ui_active()` into the message sink: `msg_use_printf`
    /// (`message.c` line 3013) stops printing as soon as a UI can display the
    /// text, and starts again when the last one detaches.
    fn sync_ui_active(&mut self) {
        let attached = self
            .session
            .with_render_state(|ui_channels, _, _| ui_channels.iter().next().is_some());
        self.session
            .with_editor_mut(|editor| editor.message_routing.ui_attached = attached);
    }

    fn ui_resize(&mut self, channel: ChannelId, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::Integer(width), Object::Integer(height)] = params else {
            return Err(ApiError::validation(
                "nvim_ui_try_resize expects (Integer, Integer)",
            ));
        };
        let width = positive_dimension(*width, "width")?;
        let height = positive_dimension(*height, "height")?;
        self.resize_current_tabpage(width, height)?;
        self.session.with_render_state(|ui_channels, _, _| {
            ui_channels
                .try_resize(channel.get(), width, height)
                .map_err(|error| ApiError::exception(error.to_string()))
        })?;
        Ok(Object::Nil)
    }

    fn redraw(&mut self) -> Result<BTreeMap<u64, Vec<u8>>, ApiError> {
        self.sync_chrome()?;
        self.publish_messages()
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let (width, height) = self.session.with_render_state(|ui_channels, _, _| {
            ui_channels.iter().map(|(_, channel)| channel.size()).fold(
                (1, 1),
                |(max_width, max_height), (width, height)| {
                    (max_width.max(width), max_height.max(height))
                },
            )
        });
        self.session
            .with_editor(|editor| {
                self.session.with_render_state(|_, highlights, _| {
                    self.compositor
                        .refresh_from_editor(editor, width, height, highlights)
                })
            })
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let output = self
            .session
            .with_render_state(|ui_channels, highlights, chrome| {
                self.emitter
                    .redraw(ui_channels, &self.compositor, highlights, chrome)
                    .map_err(|error| ApiError::exception(error.to_string()))
            })?;
        let RedrawOutput(frames, semantic) = output;
        // vim.ui_attach callbacks (upstream ui_add_cb via the event loop):
        // queued at emission, invoked here with no editor or render-state
        // borrow held.
        if ox_lua::has_attached_callbacks() {
            for event in &semantic {
                ox_lua::enqueue_ui_event(&event.name.to_string_lossy(), event.args.clone());
            }
            self.deliver_ui_event_callbacks()?;
        }
        Ok(frames)
    }

    /// Invokes queued UI-event callbacks on the Lua host. The editor and
    /// render-state borrows must be released: callbacks may call any API.
    fn deliver_ui_event_callbacks(&mut self) -> Result<(), ApiError> {
        let lua = self.lua.borrow().lua().clone();
        ox_lua::deliver_pending_ui_events(&lua)
            .map_err(|error| ApiError::exception(error.to_string()))
    }

    /// Mirrors the input mode onto the command-line chrome: shows the escaped
    /// cmdline (with control characters rendered as `^X` in `SpecialKey`) while
    /// the mode is Cmdline, and hides level 1 otherwise.
    fn sync_cmdline_chrome(&mut self, mode: &Mode) -> Result<(), ApiError> {
        match mode {
            Mode::Cmdline(state) => {
                let first_char = match state.kind {
                    CmdlineKind::Search(ox_editor::SearchDirection::Forward) => "/",
                    CmdlineKind::Search(ox_editor::SearchDirection::Backward) => "?",
                    CmdlineKind::Ex => ":",
                };
                let special_hl_id = self.session.with_render_state(|_, highlights, _| {
                    match highlights.group_id(&OxStr::from("SpecialKey")) {
                        Some(id) => Ok(id),
                        None => highlights
                            .define_group(
                                "SpecialKey",
                                Highlight {
                                    rgb: HlAttrs {
                                        foreground: Some(0x00_00_ff),
                                        ..HlAttrs::default()
                                    },
                                    ..Highlight::default()
                                },
                            )
                            .map_err(|error| ApiError::exception(error.to_string())),
                    }
                })?;
                let mut content = Vec::new();
                let mut plain = String::new();
                for character in state.text.chars() {
                    if character.is_ascii_control() {
                        if !plain.is_empty() {
                            content.push(ContentChunk::new(
                                0,
                                OxStr(std::mem::take(&mut plain).into_bytes()),
                            ));
                        }
                        let visible = if character == '\u{7f}' {
                            '?'
                        } else {
                            char::from_u32(u32::from(character) ^ 0x40).unwrap_or('?')
                        };
                        let mut escaped = String::with_capacity(2);
                        escaped.push('^');
                        escaped.push(visible);
                        content.push(ContentChunk::new(
                            special_hl_id,
                            OxStr(escaped.into_bytes()),
                        ));
                    } else {
                        plain.push(character);
                    }
                }
                if !plain.is_empty() {
                    content.push(ContentChunk::new(0, OxStr(plain.into_bytes())));
                }
                let position = content
                    .iter()
                    .map(|chunk| chunk.text.as_bytes().len())
                    .sum();
                self.session.with_render_state(|_, _, chrome| {
                    chrome.show_cmdline(UiCmdlineState {
                        content,
                        position,
                        first_char: OxStr::from(first_char),
                        prompt: OxStr::from(""),
                        indent: 0,
                        level: 1,
                        hl_id: 0,
                    });
                });
            }
            _ => self
                .session
                .with_render_state(|_, _, chrome| chrome.hide_cmdline(1, false)),
        }
        Ok(())
    }

    fn sync_chrome(&mut self) -> Result<(), ApiError> {
        let mode = self.mode.borrow().mode().clone();
        self.sync_cmdline_chrome(&mode)?;
        // Showmode: emit `-- INSERT --`, `-- REPLACE --`, `-- VISUAL --` etc.
        // mirroring Neovim's `showmode()` (`drawscreen.c:901`) gated by
        // `p_smd`. The highlight is `ModeMsg` (HLF_CM), defaulting to bold.
        let showmode_text: Option<&str> = match &mode {
            Mode::Insert(_) => Some("-- INSERT --"),
            Mode::Replace(_) => Some("-- REPLACE --"),
            Mode::Visual(state) => Some(match state.kind {
                VisualKind::Character => "-- VISUAL --",
                VisualKind::Line => "-- VISUAL LINE --",
                VisualKind::Block => "-- VISUAL BLOCK --",
            }),
            _ => None,
        };
        let mode_msg_id = self.session.with_render_state(|_, highlights, _| {
            match highlights.group_id(&OxStr::from("ModeMsg")) {
                Some(id) => Ok(id),
                None => highlights
                    .define_group(
                        "ModeMsg",
                        Highlight {
                            rgb: HlAttrs {
                                bold: true,
                                ..HlAttrs::default()
                            },
                            cterm: HlAttrs {
                                bold: true,
                                ..HlAttrs::default()
                            },
                            cterm_explicit: true,
                            ..Highlight::default()
                        },
                    )
                    .map_err(|error| ApiError::exception(error.to_string())),
            }
        })?;
        let showmode_content = match showmode_text {
            Some(text) => vec![ContentChunk::new(mode_msg_id, OxStr::from(text))],
            None => Vec::new(),
        };
        self.session.with_render_state(|_, _, chrome| {
            chrome.set_showmode(showmode_content);
        });
        Ok(())
    }

    /// Sends every newly retained message where the editor sink decided it
    /// goes: an attached UI, stdout, stderr, or nowhere.
    fn publish_messages(&mut self) -> Result<(), AppError> {
        let pending: Vec<(ox_editor::Message, MessageDestination)> =
            self.session.with_editor(|editor| {
                let from = self.rendered_messages;
                editor.messages()[from..]
                    .iter()
                    .cloned()
                    .zip(editor.message_destinations()[from..].iter().copied())
                    .collect()
            });
        self.rendered_messages += pending.len();
        for (message, destination) in &pending {
            if *destination == MessageDestination::Ui {
                self.show_in_chrome(message);
            } else {
                self.printf
                    .write(*destination, message)
                    .map_err(AppError::Io)?;
            }
        }
        Ok(())
    }

    /// Runs the idempotent editor exit sequence and publishes its messages.
    fn run_exit(&mut self) -> Result<(), AppError> {
        self.ex
            .borrow_mut()
            .run_exit_sequence(&*self.session)
            .map_err(|error| AppError::Ex(error.to_string()))?;
        self.publish_messages()
    }

    fn show_in_chrome(&mut self, message: &ox_editor::Message) {
        let text = match &message.content {
            Object::String(text) => text.clone(),
            value => OxStr::from(format!("{value:?}").as_bytes()),
        };
        self.session.with_render_state(|_, _, chrome| {
            chrome.show_message(MessageState {
                kind: OxStr::from(if message.kind == MessageKind::Error {
                    "emsg"
                } else {
                    "echo"
                }),
                content: vec![ContentChunk::new(0, text)],
                replace_last: false,
                history: message.history,
                append: false,
                id: Object::Nil,
                trigger: OxStr::from(""),
            });
        });
    }

    /// One turn of `state_enter`'s input handling (`state.c:34-106`).
    ///
    /// Everything a consumed key produces — a finished `:` command line, a
    /// mapping's Ex-command or `<expr>` right-hand side — is run by
    /// `ExExecutor::run_typeahead`, the same entry point `:normal` and
    /// `feedkeys()` use, so a mapping cannot behave differently depending on
    /// how its left-hand side arrived.
    fn drive_input(&mut self) -> Result<(), ApiError> {
        loop {
            self.mode.borrow_mut().set_no_more_input(false);
            // The host can still receive keys: a pending mapping parks
            // instead of timing out (`vgetorpeek`'s interactive wait).
            let result = self
                .ex
                .borrow_mut()
                .run_typeahead(&*self.session, &self.mode);
            self.mode.borrow_mut().set_no_more_input(true);
            let outcome = result.map_err(|error| ApiError::exception(error.to_string()))?;
            let repeats = self.mode.borrow_mut().take_paste_repeats();
            let (outcome, repeats) = (outcome, repeats);
            if let ExecOutcome::Quit(code) = outcome {
                self.exiting = true;
                self.exit_code = code;
            }
            if repeats.is_empty() {
                return Ok(());
            }
            for data in repeats {
                ox_api::nvim_paste(&self.session, OxStr(data), false, -1)?;
            }
        }
    }

    /// Drains queued Lua work for this state; see [`drain_lua_work_queue`].
    fn drain_lua_work(&mut self) -> bool {
        drain_lua_work_queue(&self.lua_work, &self.session)
    }

    /// Release the ephemeral Lua references owned by one reply payload.
    ///
    /// Upstream frees each `kObjectTypeLuaRef` while packing it
    /// (`msgpack_rpc/packer.c`), making the encoded reply the reference's
    /// final consumer, so a long-running session does not grow the
    /// `ox-lua.refs` registry table without bound.  Only freshly allocated
    /// result references may pass through here; references stored in the
    /// editor (autocmd callbacks, variables, keymaps) are aliased and must
    /// be released by their owners instead.
    fn free_reply_refs(&self, object: &Object) {
        let lua = self.lua.borrow();
        free_object_refs(lua.lua(), object);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "RPC request and notification states preserve reply and redraw ordering"
    )]
    fn process_message(
        &mut self,
        channel: ChannelId,
        message: Message,
    ) -> Result<Vec<(u64, Vec<u8>)>, AppError> {
        let mut writes = Vec::new();
        match message {
            Message::Request {
                msgid,
                method,
                params,
            } => {
                let is_input = matches!(
                    method.as_bytes(),
                    b"nvim_input" | b"nvim_feedkeys" | b"nvim_paste"
                );
                let is_ui_attach = method.as_bytes() == b"nvim_ui_attach";
                let owns_result_refs = allocates_result_refs(method.as_bytes());
                let dispatched = self.dispatch(channel, &method, &params);
                let (mut result, mut redraws) = match dispatched {
                    Ok((result, redraws)) => (Ok(result), redraws),
                    Err(error) => (Err(error), BTreeMap::new()),
                };
                if result.is_ok() && is_input {
                    match self.drive_input() {
                        Ok(()) => {
                            redraws = self
                                .redraw()
                                .map_err(|error| AppError::Api(error.to_string()))?;
                        }
                        Err(error) => {
                            let message = error.message().to_owned();
                            self.session.with_editor_mut(|editor| {
                                editor.push_message(ox_editor::Message {
                                    kind: MessageKind::Error,
                                    content: Object::String(OxStr::from(message.as_str())),
                                    history: true,
                                    leading_newline: true,
                                });
                            });
                            redraws = self
                                .redraw()
                                .map_err(|error| AppError::Api(error.to_string()))?;
                            result = Err(error);
                        }
                    }
                }
                let response = Message::Response { msgid, result };
                let encoded = response.encode_bytes();
                if owns_result_refs
                    && let Message::Response {
                        result: Ok(value), ..
                    } = &response
                {
                    self.free_reply_refs(value);
                }
                let response = (channel.get(), encoded);
                if is_ui_attach {
                    writes.extend(redraws);
                    writes.push(response);
                } else {
                    writes.push(response);
                    writes.extend(redraws);
                }
            }
            Message::Notification { method, params } => {
                let is_input = matches!(
                    method.as_bytes(),
                    b"nvim_input" | b"nvim_feedkeys" | b"nvim_paste"
                );
                let owns_result_refs = allocates_result_refs(method.as_bytes());
                match self.dispatch(channel, &method, &params) {
                    Ok((value, mut redraws)) => {
                        // No reply is encoded for a fire-and-forget call, so
                        // the ephemeral references in its result are released
                        // here instead.
                        if owns_result_refs {
                            self.free_reply_refs(&value);
                        }
                        if is_input {
                            match self.drive_input() {
                                Ok(()) => {
                                    redraws = self
                                        .redraw()
                                        .map_err(|error| AppError::Api(error.to_string()))?;
                                }
                                Err(error) => {
                                    let message = error.message().to_owned();
                                    self.session.with_editor_mut(|editor| {
                                        editor.push_message(ox_editor::Message {
                                            kind: MessageKind::Error,
                                            content: Object::String(OxStr::from(message.as_str())),
                                            history: true,
                                            leading_newline: true,
                                        });
                                    });
                                    redraws = self
                                        .redraw()
                                        .map_err(|error| AppError::Api(error.to_string()))?;
                                    writes.push((channel.get(), ox_rpc::nvim_error_event(&error)));
                                    writes.extend(redraws);
                                    let _ = self.drain_lua_work();
                                    return Ok(writes);
                                }
                            }
                        }
                        writes.extend(redraws);
                    }
                    Err(error) => writes.push((channel.get(), ox_rpc::nvim_error_event(&error))),
                }
            }
            Message::Response { .. } => {}
        }
        let _ = self.drain_lua_work();
        Ok(writes)
    }

    fn should_exit(&self) -> bool {
        self.exiting
    }

    /// Process exit code requested so far (`:cquit`, else 0).
    fn exit_code(&self) -> i64 {
        self.exit_code
    }
}

/// Drains queued Lua work (`vim.schedule` callbacks, deferred channel
/// sends) until the queue empties. Runs on the RPC turn and on the
/// background tick's borrow-free phase - upstream services this queue
/// from the main loop (`loop_put`/`process_events`, event/loop.c), not
/// from message arrival, so an idle session still makes progress.
///
/// A failing work item reports through the message system and the drain
/// continues (upstream `nlua_error`, executor.c:526-544).
///
/// Returns whether at least one work item ran.
fn drain_lua_work_queue(queue: &Rc<RefCell<VecDeque<Work>>>, session: &Rc<ApiSession>) -> bool {
    let mut ran = false;
    loop {
        let work = queue.borrow_mut().pop_front();
        let Some(work) = work else { return ran };
        ran = true;
        if let Err(error) = work() {
            let message = error.to_string();
            session.with_editor_mut(|editor| {
                ox_editor::excmd_exec::push_text_message(editor, message, true, true);
            });
        }
    }
}
fn positive_dimension(value: i64, name: &str) -> Result<usize, ApiError> {
    usize::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| ApiError::validation(format!("{name} must be positive")))
}

/// Methods whose dispatch allocates fresh Lua references inside the result.
///
/// `nvim_exec_lua`/`nvim_execute_lua` convert the chunk's first return value
/// (`nlua_exec` parity), so every `Object::LuaRef` in their reply was created
/// by that dispatch and is aliased nowhere else.  Registry results may borrow
/// references stored in the editor (autocmd callbacks, variables, keymaps),
/// which stay owned by their stores and must never be released here.
fn allocates_result_refs(method: &[u8]) -> bool {
    method == b"nvim_exec_lua" || method == b"nvim_execute_lua"
}

/// Collect every Lua reference id stored in `object`, recursing through
/// arrays and dictionaries like upstream `api_luarefs_free_object`.
fn collect_object_refs(object: &Object, out: &mut Vec<i32>) {
    match object {
        Object::LuaRef(reference) => out.push(*reference),
        Object::Array(items) => {
            for item in items {
                collect_object_refs(item, out);
            }
        }
        Object::Dict(Dict(entries)) => {
            for (_, value) in entries {
                collect_object_refs(value, out);
            }
        }
        _ => {}
    }
}

/// Release every Lua reference id collected from `object`.
///
/// Releasing an already-released slot is a no-op, so duplicate ids in one
/// payload are safe; a failed release only leaves the slot pinned and cannot
/// corrupt the registry.
fn free_object_refs(lua: &Lua, object: &Object) {
    let mut references = Vec::new();
    collect_object_refs(object, &mut references);
    for reference in references {
        let _ = free_lua_ref(lua, reference);
    }
}

fn method_is_mutating(method: &str) -> bool {
    method.starts_with("nvim_set_")
        || method.starts_with("nvim_buf_set_")
        || method.starts_with("nvim_win_set_")
        || method.starts_with("nvim_tabpage_set_")
        || method.starts_with("nvim_del_")
        || method.starts_with("nvim_create_")
        || method.starts_with("nvim_open_")
        || method.starts_with("nvim_close_")
        || matches!(
            method,
            "nvim_command"
                | "nvim_exec_lua"
                | "nvim_execute_lua"
                | "nvim_input"
                | "nvim_input_mouse"
                | "nvim_feedkeys"
                | "nvim_paste"
                | "nvim_put"
        )
}

/// Serve channel 1 over stdin/stdout until the peer closes its write side.
/// Returns the process exit code requested by `:cquit` (0 otherwise).
pub fn run_stdio(cli: &Cli, timer: &mut StartupTimer) -> Result<i64, AppError> {
    let state = Rc::new(RefCell::new(AppState::new(cli, timer)?));
    if state.borrow().should_exit() {
        state.borrow_mut().run_exit()?;
        return Ok(state.borrow().exit_code());
    }

    if !cli.embed {
        let mut decoder = IncrementalDecoder::new();
        let mut input = io::stdin().lock();
        let mut output = io::stdout().lock();
        let mut bytes = [0_u8; 8192];
        loop {
            let count = input.read(&mut bytes).map_err(AppError::Io)?;
            if count == 0 {
                break;
            }
            let messages = decoder
                .feed(&bytes[..count])
                .map_err(|error| AppError::Server(error.to_string()))?;
            for message in messages {
                for (channel, bytes) in state.borrow_mut().process_message(CHAN_STDIO, message)? {
                    if channel == CHAN_STDIO.get() {
                        output.write_all(&bytes).map_err(AppError::Io)?;
                    }
                }
            }
            output.flush().map_err(AppError::Io)?;
            if state.borrow().should_exit() {
                break;
            }
        }
        state.borrow_mut().run_exit()?;
        return Ok(state.borrow().exit_code());
    }

    #[cfg(unix)]
    {
        let accept_uv = Rc::new(RefCell::new(
            UvLoop::new().map_err(|error| AppError::Server(error.to_string()))?,
        ));
        let runtime = Rc::new(RefCell::new(NetworkRuntime::new(
            state.clone(),
            Rc::clone(&accept_uv),
        )));
        let listen_server = ListenServer::new(&runtime)?;
        state
            .borrow()
            .ex
            .borrow_mut()
            .set_server_host(Box::new(listen_server.clone()));
        let mut uv_loop = UvLoop::new().map_err(|error| AppError::Server(error.to_string()))?;
        let stdio_poll = bind_stdio(&mut uv_loop, &runtime)?;
        let timer =
            ox_uv::Timer::new(&mut uv_loop).map_err(|error| AppError::Server(error.to_string()))?;
        let callback_runtime = runtime.clone();
        timer
            .start(&mut uv_loop, 1, 10, move |uv_loop, _| {
                callback_runtime.borrow_mut().poll_background(uv_loop)
            })
            .map_err(|error| AppError::Server(error.to_string()))?;
        let pump_timer =
            ox_uv::Timer::new(&mut uv_loop).map_err(|error| AppError::Server(error.to_string()))?;
        let pump_server = listen_server.clone();
        pump_timer
            .start(&mut uv_loop, 1, 10, move |_, _| {
                pump_server.pump();
                Ok(())
            })
            .map_err(|error| AppError::Server(error.to_string()))?;
        let run_result = uv_loop
            .run(RunMode::Default)
            .map_err(|error| AppError::Server(error.to_string()));
        let poll_close = stdio_poll
            .close(&mut uv_loop)
            .map_err(|error| AppError::Server(error.to_string()));
        let timer_close = timer
            .close(&mut uv_loop)
            .map_err(|error| AppError::Server(error.to_string()));
        let pump_close = pump_timer
            .close(&mut uv_loop)
            .map_err(|error| AppError::Server(error.to_string()));
        run_result?;
        poll_close?;
        timer_close?;
        pump_close?;
        listen_server.close_all();
        if let Some(error) = runtime.borrow_mut().error.take() {
            return Err(AppError::Server(error));
        }
    }

    #[cfg(not(unix))]
    {
        return Err(AppError::Server(
            "--embed is unsupported on this platform".into(),
        ));
    }

    state.borrow_mut().run_exit()?;
    Ok(state.borrow().exit_code())
}

/// Serve RPC peers accepted from a TCP address or Unix-domain pipe.
/// Returns the process exit code requested by `:cquit` (0 otherwise).
/// A listen value without `:`, `/` or `\\` is a NAME: it joins the runtime
/// directory as `<dir>/<name>.<pid>.<counter>` (`server_address_new`,
/// #8519), with the uid-owned-directory checks `ensure_listen_directory`
/// applies; anything else binds verbatim.
fn expand_listen_address(address: &str) -> Result<String, AppError> {
    static LISTEN_NAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if address.contains([':', '/', '\\']) {
        return Ok(address.to_owned());
    }
    let counter = LISTEN_NAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("{}.{}.{counter}", address, std::process::id());
    let directory = std::path::PathBuf::from(
        ox_editor::stdpath(ox_editor::StdPath::Run)
            .first()
            .map_or("/tmp", String::as_str),
    );
    let directory = ensure_listen_directory(&directory)?;
    Ok(directory.join(name).to_string_lossy().into_owned())
}

pub fn run_listener(cli: &Cli, address: &str, timer: &mut StartupTimer) -> Result<i64, AppError> {
    let state = Rc::new(RefCell::new(AppState::new(cli, timer)?));
    // main.c getout(): a startup command that quits ends the process before
    // the event loop starts, mirroring `run_stdio`.
    if state.borrow().should_exit() {
        state.borrow_mut().run_exit()?;
        return Ok(state.borrow().exit_code());
    }
    let accept_uv = Rc::new(RefCell::new(
        UvLoop::new().map_err(|error| AppError::Server(error.to_string()))?,
    ));
    let runtime = Rc::new(RefCell::new(NetworkRuntime::new(
        state.clone(),
        Rc::clone(&accept_uv),
    )));
    let listen_server = ListenServer::new(&runtime)?;
    state
        .borrow()
        .ex
        .borrow_mut()
        .set_server_host(Box::new(listen_server.clone()));
    let mut uv_loop = UvLoop::new().map_err(|error| AppError::Server(error.to_string()))?;
    // Upstream `server_start`: a listen value without ':' or '/' is a NAME,
    // not a path — it is appended to a generated per-process address
    // (`server_address_new`: `<stdpath run>/<name>.<pid>.<counter>`), so
    // same-named listeners in one process tree never collide (#8519).
    let address = expand_listen_address(address)?;
    // The startup listener binds through the same `ServerHost` the builtins
    // use, so `serverlist()`/`serverstop()` see it (upstream keeps it in the
    // same `watchers` array, `server.c:203-204`).
    let mut server = listen_server.clone();
    let servername = server
        .start(&address)
        .map_err(|error| AppError::Server(format!("Failed to start server: {error}")))?;
    let state = runtime.borrow().state.clone();
    state.borrow().session.with_editor_mut(|editor| {
        editor.vvars_mut().insert(
            OxStr::from("servername"),
            Object::String(OxStr::from(servername.as_str())),
        );
    });
    #[cfg(unix)]
    let stdio_poll = cli
        .embed
        .then(|| bind_stdio(&mut uv_loop, &runtime))
        .transpose()?;
    #[cfg(not(unix))]
    if cli.embed {
        return Err(AppError::Server(
            "--embed with --listen is unsupported on this platform".into(),
        ));
    }

    let background_timer =
        ox_uv::Timer::new(&mut uv_loop).map_err(|error| AppError::Server(error.to_string()))?;
    let callback_runtime = runtime.clone();
    background_timer
        .start(&mut uv_loop, 1, 10, move |uv_loop, _| {
            callback_runtime.borrow_mut().poll_background(uv_loop)
        })
        .map_err(|error| AppError::Server(error.to_string()))?;
    // The accept pump: every listener — the startup server included — lives
    // on the accept loop, so this timer is what accepts and serves peers.
    let pump_timer =
        ox_uv::Timer::new(&mut uv_loop).map_err(|error| AppError::Server(error.to_string()))?;
    let pump_server = listen_server.clone();
    let pump_state = runtime.borrow().state.clone();
    pump_timer
        .start(&mut uv_loop, 1, 10, move |uv_loop, _| {
            pump_server.pump();
            // A peer's `qa!` lands on the accept loop; only this timer sees
            // it from the main loop, so the exit decision must stop the main
            // loop here or the run never ends (upstream: the main loop's own
            // event processing performs the exit).
            if pump_state.borrow().should_exit() {
                uv_loop.stop();
            }
            Ok(())
        })
        .map_err(|error| AppError::Server(error.to_string()))?;
    let run_result = uv_loop
        .run(RunMode::Default)
        .map_err(|error| AppError::Server(error.to_string()));
    let timer_close_result = background_timer
        .close(&mut uv_loop)
        .map_err(|error| AppError::Server(error.to_string()));
    let pump_close_result = pump_timer
        .close(&mut uv_loop)
        .map_err(|error| AppError::Server(error.to_string()));
    #[cfg(unix)]
    let stdio_close_result = stdio_poll
        .map(|poll| {
            poll.close(&mut uv_loop)
                .map_err(|error| AppError::Server(error.to_string()))
        })
        .transpose();
    run_result?;
    timer_close_result?;
    pump_close_result?;
    #[cfg(unix)]
    stdio_close_result?;
    if let Some(error) = runtime.borrow_mut().error.take() {
        return Err(AppError::Server(error));
    }
    listen_server.close_all();
    state.borrow_mut().run_exit()?;
    Ok(state.borrow().exit_code())
}

#[cfg(unix)]
fn bind_stdio(
    uv_loop: &mut UvLoop,
    runtime: &Rc<RefCell<NetworkRuntime>>,
) -> Result<Poll, AppError> {
    let mut decoder = IncrementalDecoder::new();
    let callback_runtime = runtime.clone();
    let callback = move |uv_loop: &mut UvLoop, _id: HandleId, events: PollEvents| {
        if !events.readable() && !events.disconnect() {
            return;
        }
        let result = (|| -> Result<(), AppError> {
            let mut input = io::stdin().lock();
            let mut output = io::stdout().lock();
            loop {
                let mut bytes = vec![0; 64 * 1024].into_boxed_slice();
                match input.read(&mut bytes) {
                    Ok(0) => {
                        uv_loop.stop();
                        break;
                    }
                    Ok(count) => {
                        let messages = decoder
                            .feed(&bytes[..count])
                            .map_err(|error| AppError::Server(error.to_string()))?;
                        for message in messages {
                            let state = callback_runtime.borrow().state.clone();
                            for (channel, bytes) in
                                state.borrow_mut().process_message(CHAN_STDIO, message)?
                            {
                                if channel == CHAN_STDIO.get() {
                                    output.write_all(&bytes).map_err(AppError::Io)?;
                                }
                            }
                            if state.borrow().should_exit() {
                                uv_loop.stop();
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(AppError::Io(error)),
                }
            }
            output.flush().map_err(AppError::Io)
        })();
        if let Err(error) = result {
            callback_runtime.borrow_mut().error = Some(error.to_string());
            uv_loop.stop();
        }
    };
    let mut poll = Poll::new(uv_loop, io::stdin(), callback)
        .map_err(|error| AppError::Server(error.to_string()))?;
    poll.poll_start(uv_loop, "rd")
        .map_err(|error| AppError::Server(error.to_string()))?;
    Ok(poll)
}

/// Current effective uid, through the audited `ox-sys` boundary: the old
/// `/proc/self` read answered 0 (root) wherever `/proc` is absent.
#[cfg(unix)]
fn user_id() -> u32 {
    ox_sys::current_euid()
}

/// Checks whether `directory` is a uid-owned 0700 directory, the same
/// validation upstream applies to `/tmp/nvim.<user>` (os/fileio.c
/// :3340-3363).
#[cfg(unix)]
fn listen_directory_is_private(directory: &std::path::Path) -> bool {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let Ok(metadata) = std::fs::metadata(directory) else {
        return false;
    };
    let mode = metadata.permissions().mode() & 0o777;
    metadata.is_dir() && user_id() == metadata.uid() && mode == 0o700
}

/// Creates (or accepts) `directory` as a uid-owned 0700 dir, mirroring
/// upstream's tempdir validation (`os_mkdir(tmp, 0700)`; valid only while
/// owned by this uid with mode exactly 0700, os/fileio.c:3340-3363).
#[cfg(unix)]
fn make_private_listen_directory(directory: &std::path::Path) -> Result<(), AppError> {
    use std::os::unix::fs::DirBuilderExt as _;
    if listen_directory_is_private(directory) {
        return Ok(());
    }
    if directory.exists() {
        return Err(AppError::Server(format!(
            "refusing listen directory {}: not a uid-owned 0700 directory",
            directory.display()
        )));
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(directory)
        .map_err(|error| {
            AppError::Server(format!("cannot create private listen directory: {error}"))
        })?;
    if !listen_directory_is_private(directory) {
        return Err(AppError::Server(format!(
            "refusing listen directory {}: not a uid-owned 0700 directory",
            directory.display()
        )));
    }
    Ok(())
}

/// Creates the run directory a generated listen name lands in (`os_mkdir_
/// recurse` upstream). 0o700: the directory is the only gate on an RPC
/// socket that grants full editor control, so it must not be
/// world-traversable. `DirBuilder::create` on an existing path leaves its
/// permissions alone.
fn make_listen_directory(directory: &std::path::Path) -> Result<(), AppError> {
    #[cfg(unix)]
    use std::os::unix::fs::DirBuilderExt as _;
    #[cfg(unix)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(directory)
        .map_err(|error| AppError::Server(format!("cannot create listen directory: {error}")))
}

/// Returns a validated directory for a generated listen socket. On Unix this
/// is `directory` when it is a uid-owned 0700 directory; if it does not
/// exist it is created with 0700 and re-checked. If the candidate is the
/// platform temp dir, or if it exists but is not private, fall back to a
/// uid-qualified `oxvim.<uid>` directory under the temp dir and validate
/// Upstream validates the fallback tempdir (`/tmp/nvim.<user>`)
/// (`os/fileio.c:3340-3363`) but uses an explicit `$XDG_RUNTIME_DIR`
/// unvalidated (`msgpack_rpc/server.c:126` via `os/stdpaths.c:182-186`),
/// so the XDG check is defense-in-depth.
fn ensure_listen_directory(directory: &std::path::Path) -> Result<std::path::PathBuf, AppError> {
    #[cfg(unix)]
    {
        let temp_dir = std::env::temp_dir();
        let fallback = || temp_dir.join(format!("oxvim.{}", user_id()));
        let candidate = if directory == temp_dir {
            fallback()
        } else {
            directory.to_path_buf()
        };
        if listen_directory_is_private(&candidate) {
            return Ok(candidate);
        }
        if !candidate.exists() {
            make_listen_directory(&candidate)?;
            if listen_directory_is_private(&candidate) {
                return Ok(candidate);
            }
        }
        let fallback = fallback();
        make_private_listen_directory(&fallback)?;
        Ok(fallback)
    }
    #[cfg(not(unix))]
    {
        make_listen_directory(directory)?;
        Ok(directory.to_path_buf())
    }
}

/// One listening socket owned by the accept loop: the effective bound
/// address paired with its uv handle.
struct ListenEntry {
    address: String,
    listener: Listener,
}

/// A `ServerHost` request the accept loop could not run synchronously,
/// because the caller dispatched from a peer the loop itself serves. The
/// next pump drains it.
enum PendingListen {
    Start(String),
    Stop(String),
}

/// The dispatch closure every listener handle shares, `Rc`-shared across
/// bind attempts (`Tcp::bind`/`Pipe::bind` consume their callback by
/// value).
type SharedNetCallback = Rc<RefCell<Box<dyn FnMut(&mut UvLoop, HandleId, NetEvent) + 'static>>>;

/// The `ox_editor::ServerHost` the `serverstart()`/`serverstop()`/
/// `serverlist()` builtins resolve through. Upstream keeps every watcher in
/// the `watchers` array of `msgpack_rpc/server.c` and binds on the main
/// loop; this port's builtin dispatch never holds `&mut UvLoop`, so every
/// listener — the `--listen` startup server included — binds on a private
/// accept loop instead.
///
/// Binding itself is synchronous (`Tcp::bind`/`Pipe::bind` create their
/// sockets before attaching to the loop), so `serverstart()` resolves a
/// random TCP port and reports bind failures at the call site exactly as
/// upstream does. Only accepts, reads and writes wait for the 10 ms pump on
/// the main loop, and accepted peers adopt the shared [`NetworkRuntime`]
/// machinery — `accept`, `remove_peer`, the msgpack decoder, channel
/// registration — so RPC over a `serverstart()`-created socket behaves like
/// RPC over `--listen` (`connection_cb` -> `channel_from_connection`,
/// `server.c:274-282`).
#[derive(Clone)]
struct ListenServer {
    runtime: Rc<RefCell<NetworkRuntime>>,
    uv: Rc<RefCell<UvLoop>>,
    listeners: Rc<RefCell<Vec<ListenEntry>>>,
    pending: Rc<RefCell<Vec<PendingListen>>>,
}

impl ListenServer {
    fn new(runtime: &Rc<RefCell<NetworkRuntime>>) -> Result<Self, AppError> {
        Ok(Self {
            runtime: Rc::clone(runtime),
            uv: Rc::new(RefCell::new(
                UvLoop::new().map_err(|error| AppError::Server(error.to_string()))?,
            )),
            listeners: Rc::new(RefCell::new(Vec::new())),
            pending: Rc::new(RefCell::new(Vec::new())),
        })
    }

    /// The accept pump: one non-blocking turn of the accept loop, then any
    /// requests queued while the loop was busy. Driven by a timer on the
    /// main loop, whose callbacks serialize every borrow of `uv`.
    fn pump(&self) {
        if let Ok(mut uv) = self.uv.try_borrow_mut() {
            let _ = uv.run(RunMode::NoWait);
        }
        for op in std::mem::take(&mut *self.pending.borrow_mut()) {
            match op {
                PendingListen::Start(address) => {
                    let mut server = self.clone();
                    if let Err(error) = server.start(&address) {
                        let session = self.runtime.borrow().state.borrow().session.clone();
                        report_server_error(&session, &format!("Failed to start server: {error}"));
                    }
                }
                PendingListen::Stop(address) => self.close_entry(&address),
            }
        }
    }

    /// Removes `address` from the registry and closes its handle on the
    /// accept loop (`socket_watcher_close` + swap-remove, `server.c:241-248`).
    fn close_entry(&self, address: &str) {
        let entry = {
            let mut listeners = self.listeners.borrow_mut();
            listeners
                .iter()
                .position(|entry| entry.address == address)
                .map(|index| listeners.remove(index))
        };
        if let Some(entry) = entry
            && let Ok(mut uv) = self.uv.try_borrow_mut()
        {
            let _ = entry.listener.close(&mut uv);
        }
    }

    /// The dispatch closure every listener handle shares: accepted peers
    /// land in the same [`NetworkRuntime`] as `--listen` peers.
    fn shared_callback(&self) -> SharedNetCallback {
        let runtime = Rc::clone(&self.runtime);
        Rc::new(RefCell::new(Box::new(move |uv_loop, id, event| {
            handle_network_event(&runtime, uv_loop, id, event);
        })))
    }

    /// Closes every listener on the accept loop; process shutdown.
    fn close_all(&self) {
        let entries = std::mem::take(&mut *self.listeners.borrow_mut());
        if let Ok(mut uv) = self.uv.try_borrow_mut() {
            for entry in entries {
                let _ = entry.listener.close(&mut uv);
            }
            // Flush deferred closes so bound pipe paths unlink before exit.
            let _ = uv.run(RunMode::NoWait);
        }
    }
}

impl ServerHost for ListenServer {
    fn start(&mut self, address: &str) -> Result<String, String> {
        if address.is_empty() {
            // `server_start` (`server.c:168-171`): an empty address is a
            // validation failure, reported through the `result > 0` branch
            // of `f_serverstart`.
            return Err("Unknown system error".into());
        }
        if self
            .listeners
            .borrow()
            .iter()
            .any(|entry| entry.address == address)
        {
            // Already listening on this address (`server.c:184-193`) ->
            // result 2, also the `result > 0` branch.
            return Err("Unknown system error".into());
        }
        let Ok(mut uv) = self.uv.try_borrow_mut() else {
            // Re-entered from a peer this loop serves: the bind waits for
            // the next pump, and the requested address is returned
            // unresolved — an ephemeral-port TCP endpoint reports its port
            // only once the pump has bound it.
            self.pending
                .borrow_mut()
                .push(PendingListen::Start(address.to_owned()));
            return Ok(address.to_owned());
        };
        let callback = self.shared_callback();
        let (bound, listener) = match tcp_endpoint(address) {
            Some((host, port)) => start_tcp(&mut uv, host, port, &callback)?,
            None => start_pipe(&mut uv, address, &callback)?,
        };
        self.listeners.borrow_mut().push(ListenEntry {
            address: bound.clone(),
            listener,
        });
        Ok(bound)
    }

    fn stop(&mut self, address: &str) -> bool {
        let found = self
            .listeners
            .borrow()
            .iter()
            .any(|entry| entry.address == address);
        if !found {
            return false;
        }
        if self.uv.try_borrow_mut().is_err() {
            // Busy accept loop: drop the entry now (the registry stays
            // truthful for `serverlist()`), defer the uv close.
            self.pending
                .borrow_mut()
                .push(PendingListen::Stop(address.to_owned()));
            return true;
        }
        self.close_entry(address);
        true
    }

    fn list(&self) -> Vec<String> {
        self.listeners
            .borrow()
            .iter()
            .map(|entry| entry.address.clone())
            .collect()
    }
}

/// Splits a TCP endpoint at its last colon (`socket_address_tcp_host_end`,
/// `event/socket.c:29-43`): a leading colon is not a host/port split, and a
/// Windows drive-letter path (`X:/...`) is a pipe path, not TCP.
#[must_use]
fn tcp_endpoint(address: &str) -> Option<(&str, &str)> {
    let bytes = address.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        return None;
    }
    let index = address.rfind(':')?;
    (index > 0).then_some((&address[..index], &address[index + 1..]))
}

/// Parses the port half of a TCP endpoint (`try_getdigits`,
/// `event/socket.c:55-58`): empty means "assign a random port"; anything
/// but digits in range is upstream's `UV_EINVAL`, "invalid argument".
fn parse_tcp_port(port: &str) -> Result<u16, String> {
    if port.is_empty() {
        return Ok(0);
    }
    port.parse::<u16>()
        .map_err(|_| "invalid argument".to_owned())
}

/// Maps a bind/listen failure to the `%s` suffix of upstream's
/// `Failed to start server: %s` (`eval/funcs.c:6243`), using the
/// `uv_strerror` texts the gated specs assert.
fn bind_error_text(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        std::io::ErrorKind::AddrInUse => "address already in use".into(),
        std::io::ErrorKind::InvalidInput => "invalid argument".into(),
        _ => error.to_string(),
    }
}

/// [`bind_error_text`] for network handle failures.
fn net_error_text(error: &ox_uv::NetError) -> String {
    match error {
        ox_uv::NetError::Io(io) => bind_error_text(io),
        _ => error.to_string(),
    }
}

/// A per-bind-call view of the shared listener callback.
fn shared_view(
    callback: &SharedNetCallback,
) -> impl FnMut(&mut UvLoop, HandleId, NetEvent) + 'static {
    let callback = Rc::clone(callback);
    move |uv_loop, id, event| (callback.borrow_mut())(uv_loop, id, event)
}

/// Resolves `host:port` and binds the first candidate that listens, the way
/// `socket_watcher_init` + `socket_watcher_start` do
/// (`event/socket.c:52-79,142-171`). The returned address keeps the
/// caller's host text and appends the bound port — a `0`/empty port binds
/// an ephemeral port and the resolved number is what `serverstart()`
/// returns (`snprintf(watcher->addr ...)`, `event/socket.c:158-170`).
fn start_tcp(
    uv_loop: &mut UvLoop,
    host: &str,
    port: &str,
    callback: &SharedNetCallback,
) -> Result<(String, Listener), String> {
    let parsed = parse_tcp_port(port)?;
    let resolved = dns::getaddrinfo(
        Some(host),
        if port.is_empty() { None } else { Some(port) },
        dns::AddrInfoHints::default(),
    )
    .map_err(|error| match error.name {
        "EAI_NONAME" => "name or service not known".into(),
        _ => error.message,
    })?;
    let mut last = String::from("Unknown system error");
    for candidate in resolved {
        let address = SocketAddr::new(candidate.address, parsed.max(candidate.port));
        let mut listener = match Tcp::bind(uv_loop, address, shared_view(callback)) {
            Ok(listener) => listener,
            Err(error) => {
                last = net_error_text(&error);
                continue;
            }
        };
        if let Err(error) = listener.listen(uv_loop, 128) {
            last = net_error_text(&error);
            continue;
        }
        let bound = match listener.local_addr() {
            Ok(bound) => bound,
            Err(error) => {
                last = net_error_text(&error);
                continue;
            }
        };
        let bound_port = bound.port();
        return Ok((format!("{host}:{bound_port}"), Listener::Tcp(listener)));
    }
    Err(last)
}

/// Binds a unix-socket listener at `address`, applying upstream's stale-file
/// recovery (#36581, `event/socket.c:215-247`): a path that exists is probed
/// with a synchronous connect — a live listener fails the start, a dead
/// file is removed and the bind retried. The socket is restricted to its
/// owner before the first accept can happen.
#[cfg(unix)]
fn start_pipe(
    uv_loop: &mut UvLoop,
    address: &str,
    callback: &SharedNetCallback,
) -> Result<(String, Listener), String> {
    let path = Path::new(address);
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(path) {
            // `socket_alive` (`event/socket.c:101-134`): something answers,
            // so a live server owns the address.
            Ok(_probe) => return Err("address already in use".into()),
            // Dead socket file: remove it and bind afresh.
            Err(_) => {
                std::fs::remove_file(path).map_err(|error| bind_error_text(&error))?;
            }
        }
    }
    let mut listener =
        Pipe::bind(uv_loop, path, shared_view(callback)).map_err(|error| net_error_text(&error))?;
    listener
        .listen(uv_loop, 128)
        .map_err(|error| net_error_text(&error))?;
    restrict_pipe_permissions(path)?;
    Ok((address.to_owned(), Listener::Pipe(listener)))
}

/// The socket grants full RPC control; bind's default mode is 0777&~umask,
/// so restrict it to the owner (carried over from the startup `bind_pipe`).
#[cfg(unix)]
fn restrict_pipe_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path)
        .map_err(|error| format!("listen socket vanished: {error}"))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot restrict listen socket: {error}"))?;
    }
    Ok(())
}

/// Unix-domain `serverstart` addresses are unsupported off unix.
#[cfg(not(unix))]
fn start_pipe(
    _uv_loop: &mut UvLoop,
    _address: &str,
    _callback: &SharedNetCallback,
) -> Result<(String, Listener), String> {
    Err("Unix-domain server addresses are unsupported on this platform".into())
}

/// A deferred start failed at pump time; the loop keeps running and the
/// user sees why (`emsg`-style, as `report_job_callback_error` does).
fn report_server_error(session: &ApiSession, message: &str) {
    session.with_editor_mut(|editor| {
        ox_editor::excmd_exec::push_text_message(editor, message.to_owned(), true, true);
    });
}

enum Listener {
    Tcp(Tcp),
    #[cfg(unix)]
    Pipe(Pipe),
}

impl Listener {
    fn close(&self, uv_loop: &mut UvLoop) -> Result<(), ox_uv::Error> {
        match self {
            Self::Tcp(listener) => listener.close(uv_loop),
            #[cfg(unix)]
            Self::Pipe(listener) => listener.close(uv_loop),
        }
    }
}

enum Stream {
    Tcp(Tcp),
    #[cfg(unix)]
    Pipe(Pipe),
}

impl Stream {
    fn read_start(&mut self, uv_loop: &mut UvLoop) -> Result<(), String> {
        match self {
            Self::Tcp(stream) => stream.read_start(uv_loop),
            #[cfg(unix)]
            Self::Pipe(stream) => stream.read_start(uv_loop),
        }
        .map_err(|error| error.to_string())
    }

    fn write(&mut self, uv_loop: &mut UvLoop, bytes: Vec<u8>) -> Result<(), String> {
        match self {
            Self::Tcp(stream) => stream.write(uv_loop, bytes),
            #[cfg(unix)]
            Self::Pipe(stream) => stream.write(uv_loop, bytes),
        }
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    fn close(&self, uv_loop: &mut UvLoop) -> Result<(), String> {
        match self {
            Self::Tcp(stream) => stream.close(uv_loop),
            #[cfg(unix)]
            Self::Pipe(stream) => stream.close(uv_loop),
        }
        .map_err(|error| error.to_string())
    }
}

struct Peer {
    channel: ChannelId,
    decoder: IncrementalDecoder,
}

struct NetworkRuntime {
    state: Rc<RefCell<AppState>>,
    /// The accept loop every listening socket and peer stream lives on; the
    /// background tick writes to peers through it (see `poll_background`).
    accept_uv: Rc<RefCell<UvLoop>>,
    peers: HashMap<HandleId, Peer>,
    streams: HashMap<HandleId, Stream>,
    error: Option<String>,
    /// Set when an accept-loop error must end the process; the next
    /// `poll_background` stops the main loop.
    shutdown: bool,
}

impl NetworkRuntime {
    fn new(state: Rc<RefCell<AppState>>, accept_uv: Rc<RefCell<UvLoop>>) -> Self {
        Self {
            state,
            accept_uv,
            peers: HashMap::new(),
            streams: HashMap::new(),
            error: None,
            shutdown: false,
        }
    }
    /// The 10 ms background tick.
    ///
    /// Upstream delivers job callbacks on the main loop (`process_events`
    /// → `channel_write` → `invoke_callback`, event/loop.c), so a
    /// fire-and-forget `jobstart`'s `on_exit` fires between input batches.
    /// The tick is this server's main-loop turn: it flushes terminal PTY
    /// output, delivers deferred job events with no executor [`RefCell`]
    /// borrow live so callbacks re-enter the primary executor, then drains
    /// queued Lua work (`vim.schedule`, deferred channel sends) - upstream
    /// services that queue from the main loop, not from message arrival,
    /// so an idle session still makes progress.
    ///
    /// `system()`/`wait()` keep their re-defer semantics untouched.
    ///
    /// # Errors
    ///
    /// Returns the drain, redraw, or stream write failure.
    fn poll_background(&mut self, uv_loop: &mut UvLoop) -> Result<(), ox_uv::CallbackError> {
        let session = self.state.borrow().session.clone();
        let ex = self.state.borrow().ex.clone();
        let changed = ex
            .borrow_mut()
            .flush_pty_output(&*session)
            .map_err(ox_uv::CallbackError::new)?;
        let delivered = match deliver_deferred_job_events(&session, &ex) {
            Ok(delivered) => delivered,
            Err(error) => {
                report_job_callback_error(&session, &error);
                false
            }
        };
        // A jobwait flush that no Lua boundary claimed (Vimscript caller)
        // is served here; the events are already delivered, so only the
        // marker is taken.
        let _ = ex.borrow_mut().take_lua_flush_pending();
        let worked = self.state.borrow_mut().drain_lua_work();
        if !delivered && !changed && !worked {
            return Ok(());
        }
        let writes = self
            .state
            .borrow_mut()
            .redraw()
            .map_err(|error| ox_uv::CallbackError::new(error.to_string()))?;
        // Peers live on the accept loop, so their writes go through it, not
        // the main-loop `uv_loop` this tick received. The pump timer and
        // this tick are both main-loop callbacks, so the borrow is free.
        let mut accept_uv = self.accept_uv.borrow_mut();
        for (channel, bytes) in writes {
            if channel == CHAN_STDIO.get() {
                let mut output = io::stdout().lock();
                output
                    .write_all(&bytes)
                    .and_then(|()| output.flush())
                    .map_err(|error| ox_uv::CallbackError::new(error.to_string()))?;
                continue;
            }
            let target = self
                .peers
                .iter()
                .find_map(|(id, peer)| (peer.channel.get() == channel).then_some(*id));
            if let Some(target) = target
                && let Some(stream) = self.streams.get_mut(&target)
            {
                stream
                    .write(&mut accept_uv, bytes)
                    .map_err(|error| ox_uv::CallbackError::new(error.clone()))?;
            }
        }
        if self.shutdown {
            uv_loop.stop();
        }
        Ok(())
    }

    fn accept(&mut self, uv_loop: &mut UvLoop, mut stream: Stream) -> Result<(), String> {
        let id = match &stream {
            Stream::Tcp(stream) => stream.id(),
            #[cfg(unix)]
            Stream::Pipe(stream) => stream.id(),
        };
        if stream.read_start(uv_loop).is_err() {
            let _ = stream.close(uv_loop);
            return Ok(());
        }
        let channel = ChannelId::new(
            self.state
                .borrow_mut()
                .session
                .with_editor_mut(|editor| editor.allocate_channel_id()),
        );
        self.peers.insert(
            id,
            Peer {
                channel,
                decoder: IncrementalDecoder::new(),
            },
        );
        self.streams.insert(id, stream);
        let registration = {
            let state = self.state.borrow_mut();
            register_channel(&state.session, ChannelInfo::socket_rpc(channel))
        };
        if let Err(error) = registration {
            self.remove_peer(uv_loop, id);
            return Err(error.message().to_owned());
        }
        Ok(())
    }

    fn read(&mut self, uv_loop: &mut UvLoop, id: HandleId, bytes: &[u8]) -> Result<(), String> {
        let (channel, messages) = {
            let peer = self
                .peers
                .get_mut(&id)
                .ok_or_else(|| "read from unknown RPC peer".to_owned())?;
            let messages = peer
                .decoder
                .feed(bytes)
                .map_err(|error| error.to_string())?;
            (peer.channel, messages)
        };
        for message in messages {
            let writes = self
                .state
                .borrow_mut()
                .process_message(channel, message)
                .map_err(|error| error.to_string())?;
            for (target, bytes) in writes {
                let target_id = self
                    .peers
                    .iter()
                    .find_map(|(id, peer)| (peer.channel.get() == target).then_some(*id));
                if let Some(target_id) = target_id
                    && let Some(stream) = self.streams.get_mut(&target_id)
                    && stream.write(uv_loop, bytes).is_err()
                {
                    self.remove_peer(uv_loop, target_id);
                }
            }
            if self.state.borrow().should_exit() {
                uv_loop.stop();
            }
        }
        Ok(())
    }

    fn remove_peer(&mut self, uv_loop: &mut UvLoop, id: HandleId) {
        let channel = self.peers.get(&id).map(|peer| peer.channel);
        self.peers.remove(&id);
        if let Some(stream) = self.streams.remove(&id) {
            let _ = stream.close(uv_loop);
        }
        // Transport detached first; channel metadata removal then makes the
        // peer disappear from `nvim_list_chans()`. Stdio is never a peer.
        if let Some(channel) = channel {
            let _ = close_channel(&self.state.borrow_mut().session, channel);
        }
    }
}

fn handle_network_event(
    runtime: &Rc<RefCell<NetworkRuntime>>,
    uv_loop: &mut UvLoop,
    id: HandleId,
    event: NetEvent,
) {
    let result = match event {
        NetEvent::AcceptedTcp(stream) => runtime.borrow_mut().accept(uv_loop, Stream::Tcp(*stream)),
        #[cfg(unix)]
        NetEvent::AcceptedPipe(stream) => {
            runtime.borrow_mut().accept(uv_loop, Stream::Pipe(*stream))
        }
        NetEvent::Read(bytes) => runtime.borrow_mut().read(uv_loop, id, &bytes),
        NetEvent::Eof => {
            runtime.borrow_mut().remove_peer(uv_loop, id);
            Ok(())
        }
        NetEvent::Error(error) => Err(error.to_string()),
        NetEvent::WriteComplete { result, .. } | NetEvent::ShutdownComplete(result) => {
            result.map_err(|error| error.to_string())
        }
        NetEvent::Connected(result) => result.map_err(|error| error.to_string()),
        NetEvent::Datagram { .. } => Ok(()),
    };
    if let Err(error) = result {
        let mut runtime = runtime.borrow_mut();
        if runtime.peers.contains_key(&id) {
            runtime.remove_peer(uv_loop, id);
        } else {
            runtime.error = Some(error);
            // `uv_loop` here is the accept loop; stopping it would only
            // silence the pump. Flag the background tick to stop the main
            // loop, which ends the process like the old direct stop did.
            runtime.shutdown = true;
        }
    }
}

fn live_mode_builtin(
    session: &ApiSession,
    name: &OxStr,
    args: &[Typval],
) -> Result<Option<Typval>, ApiError> {
    let value = match name.as_bytes() {
        b"mode" => {
            let (mode, _) = ox_api::current_mode_name(session)?;
            Typval::String(OxStr::from(mode))
        }
        b"getcmdtype" => Typval::String(OxStr::from(ox_api::current_cmdline_type(session)?)),
        b"getcmdline" => Typval::String(OxStr(ox_api::current_cmdline_text(session)?.into_bytes())),
        b"histget" => {
            let index = match args {
                [Typval::String(kind)] if kind.as_bytes() == b":" => Some(-1),
                [Typval::String(kind), Typval::Number(index)] if kind.as_bytes() == b":" => {
                    isize::try_from(*index).ok()
                }
                _ => None,
            };
            let entry = index
                .map(|index| ox_api::command_history(session, index))
                .transpose()?
                .flatten()
                .unwrap_or_default();
            Typval::String(OxStr(entry.into_bytes()))
        }
        b"reg_recording" => Typval::String(match ox_api::recording_register(session)? {
            Some(name) => OxStr::from(name.to_string().as_str()),
            None => OxStr::from(""),
        }),
        _ => return Ok(None),
    };
    Ok(Some(value))
}

struct EditorBuiltins {
    session: Rc<ApiSession>,
    ex: Rc<RefCell<ExExecutor>>,
}

impl EditorBuiltins {
    /// Serve `name` through the primary executor and the live editor.
    ///
    /// This is the tier `vim.fn` reaches from a Lua config file, an RPC
    /// request, a scheduled callback or an autocommand -- everywhere a plugin
    /// runs. Lua re-entered from *inside* Ex execution (`:lua`,
    /// `lua <<EOF` in an init.vim) does not come through here:
    /// `with_scoped_editor_api` rebinds `vim.call`/`vim.fn` over the live
    /// editor and the primary/nested executor pair for the duration of the
    /// chunk, so no scratch editor or fresh executor is ever consulted.
    fn dispatch(&self, name: &OxStr, args: Vec<Typval>) -> Result<Typval, ExecError> {
        // Reentrant builtin calls borrow the session afresh, matching upstream reentry parity.
        let value = live_mode_builtin(&self.session, name, &args);
        if let Some(value) = value.map_err(|error| ExecError::Editor(error.to_string()))? {
            return Ok(value);
        }
        let Ok(mut ex) = self.ex.try_borrow_mut() else {
            return Err(ExecError::Editor(
                "no free Ex executor for a Vimscript builtin call".into(),
            ));
        };
        ex.call_builtin(&*self.session, name, args)
    }
}

impl BuiltinHost for EditorBuiltins {
    /// One route for every name, because `vim.fn.X()` and `:echo X()` have to
    /// answer the same thing.
    ///
    /// This replaces a three-branch dispatch that sent six job names to the Ex
    /// executor, `getline`/`setline` to a buffer seam, and *everything else* to
    /// `Builtins::without_regex()` -- a stateless table with no editor, no file
    /// IO and no regex engine. 24 builtins that work in Vimscript answered
    /// `E117` from Lua and every regex builtin answered `E54: regular-
    /// expression engine is not installed`, so the same function gave two
    /// answers depending on which language called it.
    fn call(&self, name: &OxStr, args: Vec<Typval>) -> Result<Typval, String> {
        self.dispatch(name, args).map_err(|error| error.to_string())
    }

    fn is_fast(&self, name: &OxStr) -> bool {
        ox_eval::builtins::is_fast_builtin(name)
    }
}

/// Lua variable access backed by the editor's canonical API dictionaries.
pub(crate) struct EditorVariables {
    pub(crate) session: Rc<ApiSession>,
}

impl VariableHost for EditorVariables {
    fn get_var(
        &self,
        scope: VariableScope,
        handle: i64,
        name: &OxStr,
    ) -> Result<Option<Object>, String> {
        self.session.with_editor(|editor| {
            let variables = variables(editor, scope, handle)?;
            Ok(variables.get(name).cloned())
        })
    }

    fn set_var(
        &self,
        scope: VariableScope,
        handle: i64,
        name: OxStr,
        value: Option<Object>,
    ) -> Result<(), String> {
        if scope == VariableScope::Vim && !vim_variable_is_writable(name.as_bytes()) {
            return Err(format!(
                "E46: Cannot change read-only variable \"{}\"",
                name.to_string_lossy()
            ));
        }
        self.session.with_editor_mut(|editor| {
            let variables = variables_mut(editor, scope, handle)?;
            if let Some(value) = value {
                variables.insert(name, value);
            } else {
                let index = variables.iter().position(|(key, _)| key == &name);
                if let Some(index) = index {
                    variables.0.remove(index);
                }
            }
            Ok(())
        })
    }
}

fn variables(editor: &Editor, scope: VariableScope, handle: i64) -> Result<&Dict, String> {
    match scope {
        VariableScope::Global => Ok(editor.gvars()),
        VariableScope::Vim => Ok(editor.vvars()),
        VariableScope::Buffer => editor
            .buffer(BufHandle::try_from(handle).map_err(|error| error.to_string())?)
            .map(ox_editor::BufferState::variables)
            .map_err(|error| error.to_string()),
        VariableScope::Window => editor
            .window_variables(WinHandle::try_from(handle).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string()),
        VariableScope::Tabpage => editor
            .tabpage_variables(TabHandle::try_from(handle).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string()),
    }
}

fn variables_mut(
    editor: &mut Editor,
    scope: VariableScope,
    handle: i64,
) -> Result<&mut Dict, String> {
    match scope {
        VariableScope::Global => Ok(editor.gvars_mut()),
        VariableScope::Vim => Ok(editor.vvars_mut()),
        VariableScope::Buffer => editor
            .buffer_mut(BufHandle::try_from(handle).map_err(|error| error.to_string())?)
            .map(ox_editor::BufferState::variables_mut)
            .map_err(|error| error.to_string()),
        VariableScope::Window => editor
            .window_variables_mut(WinHandle::try_from(handle).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string()),
        VariableScope::Tabpage => editor
            .tabpage_variables_mut(TabHandle::try_from(handle).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string()),
    }
}

/// The Lua/Ex host clone shared by every executor surface: the Ex executor's
/// `LuaExec` host, the API-level `LuaExecutor` slots, and the UI redraw
/// provider path. All instances hold `Lua` clones backed by the same mlua
/// registry, so a raw reference has one registry identity while executor
/// objects remain independently borrowable.
#[derive(Clone)]
struct ServerLuaExec {
    session: Rc<ApiSession>,
    lua: Lua,
    registry: Rc<Registry>,
    ex: Rc<RefCell<ExExecutor>>,
    nested_ex: Rc<RefCell<ExExecutor>>,
    event_loop: EventLoopPump,
}

impl LuaExec for ServerLuaExec {
    fn execute_chunk(&mut self, code: &str, args: Vec<Object>) -> Result<Object, LuaExecError> {
        exec_api_chunk(
            &self.lua,
            &self.registry,
            &self.ex,
            &self.nested_ex,
            &self.session,
            code,
            &args,
        )
    }

    fn execute_file(&mut self, path: &Path) -> Result<(), LuaExecError> {
        let lua = &self.lua;
        with_scoped_editor_api(
            lua,
            &self.registry,
            &self.ex,
            &self.nested_ex,
            &self.session,
            || {
                let loadfile: Function = lua
                    .globals()
                    .get("loadfile")
                    .map_err(|error| LuaExecError::Load(error.to_string()))?;
                let arguments = MultiValue::from_vec(vec![Value::String(
                    lua.create_string(path.to_string_lossy().as_bytes())
                        .map_err(|error| LuaExecError::Conversion(error.to_string()))?,
                )]);
                let mut loaded = call_with_traceback(lua, &loadfile, arguments)
                    .map_err(|error| LuaExecError::Load(error.to_string()))?;
                match (loaded.pop_front(), loaded.pop_front()) {
                    (Some(Value::Function(chunk)), _) => {
                        call_with_traceback(lua, &chunk, MultiValue::new())
                            .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
                        Ok(())
                    }
                    (Some(Value::Nil), Some(Value::String(message))) => {
                        Err(LuaExecError::Load(message.to_string_lossy()))
                    }
                    (Some(value), _) => {
                        Err(LuaExecError::Load(format!("loadfile returned {value:?}")))
                    }
                    (None, _) => Err(LuaExecError::Load("loadfile returned no values".to_owned())),
                }
            },
        )
    }

    fn invoke_callback(
        &mut self,
        reference: usize,
        args: Vec<Object>,
    ) -> Result<Object, LuaExecError> {
        let reference = i32::try_from(reference).map_err(|_| {
            LuaExecError::Conversion("Lua callback reference is out of range".to_owned())
        })?;
        let lua = &self.lua;
        // The scoped bindings let callbacks re-enter the live editor and are
        // restored even when the callback fails.
        with_scoped_editor_api(
            lua,
            &self.registry,
            &self.ex,
            &self.nested_ex,
            &self.session,
            || {
                let value = object_to_lua(lua, &Object::LuaRef(reference))
                    .map_err(|error| LuaExecError::Conversion(error.to_string()))?;
                let Value::Function(function) = value else {
                    return Err(LuaExecError::Conversion(
                        "Lua callback reference is not a function".to_owned(),
                    ));
                };
                let args = args
                    .iter()
                    .map(|argument| {
                        object_to_lua(lua, argument)
                            .map_err(|error| LuaExecError::Conversion(error.to_string()))
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into();
                let mut values = call_with_traceback(lua, &function, args)
                    .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
                let Some(value) = values.pop_front() else {
                    return Ok(Object::Nil);
                };
                lua_to_object(lua, &value)
                    .map_err(|error| LuaExecError::Conversion(error.to_string()))
            },
        )
    }

    fn free_callback(&mut self, reference: usize) -> Result<(), LuaExecError> {
        let reference = i32::try_from(reference).map_err(|_| {
            LuaExecError::Conversion("Lua callback reference is out of range".to_owned())
        })?;
        free_lua_ref(&self.lua, reference)
            .map_err(|error| LuaExecError::Conversion(error.to_string()))
    }

    fn discard_result(&mut self, result: Object) {
        free_object_refs(&self.lua, &result);
    }

    fn eval_expression(
        &mut self,
        expression: &str,
        arg: Option<&Typval>,
    ) -> Result<Typval, LuaExecError> {
        let lua = &self.lua;
        with_scoped_editor_api(
            lua,
            &self.registry,
            &self.ex,
            &self.nested_ex,
            &self.session,
            || {
                // lua/executor.c nlua_call_luaeval wraps the expression exactly
                // like `local _A=select(1,...) return (<expr>)` named "luaeval()".
                let chunk = format!("local _A=select(1,...) return ({expression})");
                let function = lua
                    .load(chunk.as_bytes())
                    .set_name("luaeval()")
                    .into_function()
                    .map_err(|error| LuaExecError::Load(error.to_string()))?;
                let argument = match arg {
                    Some(value) => typval_to_lua(lua, value)
                        .map_err(|error| LuaExecError::Conversion(error.to_string()))?,
                    // A missing second argument reaches Lua as nil, mirroring
                    // PUSH_ALL_TYPVALS lowering VAR_UNKNOWN to lua_pushnil.
                    None => Value::Nil,
                };
                let mut results =
                    call_with_traceback(lua, &function, MultiValue::from_vec(vec![argument]))
                        .map_err(|error| LuaExecError::Runtime(lua_runtime_error_text(error)))?;
                let value = results.pop_front().unwrap_or(Value::Nil);
                lua_to_typval(lua, &value)
                    .map_err(|error| LuaExecError::Conversion(error.to_string()))
            },
        )
    }

    fn run_event_turn(&mut self) -> Result<(), LuaExecError> {
        // The turn runs under the scoped bindings so a check callback's
        // `vim.api` access dispatches through the session this loop is
        // already executing with.
        with_scoped_editor_api(
            &self.lua,
            &self.registry,
            &self.ex,
            &self.nested_ex,
            &self.session,
            || {
                self.event_loop
                    .run_once()
                    .map_err(|error| LuaExecError::Runtime(error.to_string()))
            },
        )
    }
}

/// Runs one `nvim_exec_lua`-shaped chunk against the live editor under the
/// scoped API bindings. Shared by the Ex executor's `LuaExec` host and the
/// API-level [`LuaExecutor`], so nested `nvim_exec_lua` observes the same
/// editor and bindings whichever surface it arrives through.
fn exec_api_chunk(
    lua: &Lua,
    registry: &Registry,
    ex: &Rc<RefCell<ExExecutor>>,
    nested_ex: &Rc<RefCell<ExExecutor>>,
    session: &ApiSession,
    code: &str,
    args: &[Object],
) -> Result<Object, LuaExecError> {
    with_scoped_editor_api(lua, registry, ex, nested_ex, session, || {
        let function = lua
            .load(code)
            .set_name("<nvim>")
            .into_function()
            .map_err(|error| LuaExecError::Load(error.to_string()))?;
        let lua_args = args
            .iter()
            .map(|arg| {
                object_to_lua(lua, arg).map_err(|error| LuaExecError::Conversion(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into();
        let mut results = call_with_traceback(lua, &function, lua_args)
            .map_err(|error| LuaExecError::Runtime(lua_runtime_error_text(error)))?;
        results.pop_front().map_or(Ok(Object::Nil), |value| {
            lua_to_object(lua, &value).map_err(|error| LuaExecError::Conversion(error.to_string()))
        })
    })
}

fn nvim_exec_lua_error_text(message: String) -> String {
    message
        .split_once("[string \"<nvim>\"]:")
        .and_then(|(_, rest)| rest.split_once(": ").map(|(_, detail)| detail.to_owned()))
        .unwrap_or(message)
}

fn lua_runtime_error_text(error: mlua::Error) -> String {
    match error {
        mlua::Error::RuntimeError(message) => message,
        error => error.to_string(),
    }
}

/// Flattens [`LuaExecError`] for hosts that report plain strings.
fn lua_exec_error_text(error: LuaExecError) -> String {
    match error {
        LuaExecError::Load(message)
        | LuaExecError::Runtime(message)
        | LuaExecError::Conversion(message) => message,
    }
}

type ExExecutorPair = (Rc<RefCell<ExExecutor>>, Rc<RefCell<ExExecutor>>);

fn fresh_executors(
    lua: &Lua,
    registry: &Rc<Registry>,
    session: &Rc<ApiSession>,
    source: &Rc<RefCell<ExExecutor>>,
    channel_ids: &ChannelIds,
    event_loop: &EventLoopPump,
) -> Option<ExExecutorPair> {
    let mut primary = ExExecutor::new();
    let mut nested = ExExecutor::new();
    {
        let source = source.try_borrow().ok()?;
        primary.share_user_commands_from(&source);
        nested.share_user_commands_from(&source);
        primary.share_user_functions_from(&source);
        nested.share_user_functions_from(&source);
        primary.share_runtime_roots_from(&source);
        nested.share_runtime_roots_from(&source);
    }
    primary.set_channel_ids(channel_ids.clone());
    nested.set_channel_ids(channel_ids.clone());
    let primary = Rc::new(RefCell::new(primary));
    let nested = Rc::new(RefCell::new(nested));
    let callback_host = || {
        Rc::new(RefCell::new(ServerLuaExec {
            session: session.clone(),
            lua: lua.clone(),
            registry: registry.clone(),
            ex: primary.clone(),
            nested_ex: nested.clone(),
            event_loop: event_loop.clone(),
        }))
    };
    primary.borrow_mut().set_lua_exec(callback_host());
    nested.borrow_mut().set_lua_exec(callback_host());
    Some((primary, nested))
}

/// `nvim_exec_lua` host installed for nested calls reaching the API registry
/// (Vimscript `nvim_exec_lua()`, `luaeval("vim.api.nvim_exec_lua(...)")`,
/// `:call nvim_exec_lua(...)`), running the chunk on the live editor.
#[derive(Clone)]
struct ApiLuaExecutor {
    session: Rc<ApiSession>,
    lua: Lua,
    registry: Rc<Registry>,
    ex: Rc<RefCell<ExExecutor>>,
    nested_ex: Rc<RefCell<ExExecutor>>,
    channel_ids: ChannelIds,
    event_loop: EventLoopPump,
}

impl LuaExecutor for ApiLuaExecutor {
    fn exec(
        &mut self,
        session: &ApiSession,
        code: &str,
        args: Vec<Object>,
    ) -> Result<Object, String> {
        exec_api_chunk(
            &self.lua,
            &self.registry,
            &self.ex,
            &self.nested_ex,
            session,
            code,
            &args,
        )
        .map_err(lua_exec_error_text)
    }

    fn invoke_callback(
        &mut self,
        session: &ApiSession,
        reference: usize,
        args: Vec<Object>,
    ) -> Result<Object, String> {
        let reference = i32::try_from(reference)
            .map_err(|_| "Lua callback reference is out of range".to_owned())?;
        let (lua, registry, ex, nested_ex) = (&self.lua, &self.registry, &self.ex, &self.nested_ex);
        with_scoped_editor_api(lua, registry, ex, nested_ex, session, || {
            let value = object_to_lua(lua, &Object::LuaRef(reference))
                .map_err(|error| LuaExecError::Conversion(error.to_string()))?;
            let Value::Function(function) = value else {
                return Err(LuaExecError::Conversion(
                    "Lua callback reference is not a function".into(),
                ));
            };
            let args = args
                .iter()
                .map(|argument| {
                    object_to_lua(lua, argument)
                        .map_err(|error| LuaExecError::Conversion(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?
                .into();
            let mut values = call_with_traceback(lua, &function, args)
                .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
            let Some(value) = values.pop_front() else {
                return Ok(Object::Nil);
            };
            lua_to_object(lua, &value).map_err(|error| LuaExecError::Conversion(error.to_string()))
        })
        .map_err(lua_exec_error_text)
    }

    fn call_ref(
        &mut self,
        session: &ApiSession,
        reference: usize,
        args: Vec<Object>,
    ) -> Result<Vec<Object>, String> {
        let reference = i32::try_from(reference)
            .map_err(|_| "Lua callback reference is out of range".to_owned())?;
        let (lua, registry, ex, nested_ex) = (&self.lua, &self.registry, &self.ex, &self.nested_ex);
        with_scoped_editor_api(lua, registry, ex, nested_ex, session, || {
            let value = object_to_lua(lua, &Object::LuaRef(reference))
                .map_err(|error| LuaExecError::Conversion(error.to_string()))?;
            let Value::Function(function) = value else {
                return Err(LuaExecError::Conversion(
                    "Lua callback reference is not a function".into(),
                ));
            };
            let args = args
                .iter()
                .map(|argument| {
                    object_to_lua(lua, argument)
                        .map_err(|error| LuaExecError::Conversion(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?
                .into();
            let values = call_with_traceback(lua, &function, args)
                .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
            let mut converted = Vec::new();
            for value in values {
                match lua_to_object_ref(lua, &value) {
                    Ok(object) => converted.push(object),
                    Err(error) => {
                        // None of the refs converted so far has an owner yet.
                        free_object_refs(lua, &Object::Array(converted));
                        return Err(LuaExecError::Conversion(error.to_string()));
                    }
                }
            }
            Ok(converted)
        })
        .map_err(lua_exec_error_text)
    }

    fn free_callback(&mut self, reference: usize) -> Result<(), String> {
        let reference = i32::try_from(reference)
            .map_err(|_| "Lua callback reference is out of range".to_owned())?;
        free_lua_ref(&self.lua, reference).map_err(|error| error.to_string())
    }

    fn fork(&self) -> Option<Box<dyn LuaExecutor>> {
        let (ex, nested_ex) = fresh_executors(
            &self.lua,
            &self.registry,
            &self.session,
            &self.ex,
            &self.channel_ids,
            &self.event_loop,
        )?;
        Some(Box::new(Self {
            session: self.session.clone(),
            lua: self.lua.clone(),
            registry: self.registry.clone(),
            ex,
            nested_ex,
            channel_ids: self.channel_ids.clone(),
            event_loop: self.event_loop.clone(),
        }))
    }
}

/// Autocmd host installed for API-planned firing (`nvim_exec_autocmds` and
/// every command path that fires autocmds through the planner).
///
/// Ex-string actions run on the outermost free executor against the editor
/// the caller is already executing with, so the callback observes the live
/// editor; Lua callbacks run under the scoped bindings, so re-entrant API
/// and builtin calls stay on that same editor.
#[derive(Clone)]
struct ServerAutocmdHost {
    session: Rc<ApiSession>,
    ex: Rc<RefCell<ExExecutor>>,
    nested_ex: Rc<RefCell<ExExecutor>>,
    lua: Lua,
    registry: Rc<Registry>,
    channel_ids: ChannelIds,
    event_loop: EventLoopPump,
}

impl ServerAutocmdHost {
    fn execute_vimscript(
        &self,
        action: &AutocmdAction,
        execute: impl FnOnce(&mut ExExecutor) -> Result<ExecOutcome, ExecError>,
    ) -> Result<AutocmdExecution, String> {
        if let Ok(mut ex) = self.ex.try_borrow_mut() {
            return execute(&mut ex)
                .map(|_| AutocmdExecution::Keep)
                .map_err(|error| format_autocmd_exec_error(action, &error));
        }
        let Ok(mut nested) = self.nested_ex.try_borrow_mut() else {
            return Err("no free Ex executor for an autocmd action".into());
        };
        execute(&mut nested)
            .map(|_| AutocmdExecution::Keep)
            .map_err(|error| format_autocmd_exec_error(action, &error))
    }
}

impl AutocmdExecutor for ServerAutocmdHost {
    fn execute(&mut self, _action: &AutocmdAction) -> Result<AutocmdExecution, String> {
        Err("no live editor is available for autocmd execution".into())
    }

    fn execute_with_session(
        &mut self,
        session: &ApiSession,
        action: &AutocmdAction,
    ) -> Result<AutocmdExecution, String> {
        match &action.kind {
            AutocmdKind::ExString(source) => self.execute_vimscript(action, |executor| {
                executor.execute_autocmd_command(session, action, source)
            }),
            AutocmdKind::VimscriptFunction(name) => self.execute_vimscript(action, |executor| {
                executor.execute_autocmd_function(session, action, name)
            }),
            AutocmdKind::LuaCallback(reference) => {
                let reference = i32::try_from(*reference)
                    .map_err(|_| "autocmd Lua reference is out of range".to_owned())?;
                let args = action.callback_args().map_err(|error| error.to_string())?;
                let (lua, registry, ex, nested_ex) =
                    (&self.lua, &self.registry, &self.ex, &self.nested_ex);
                with_scoped_editor_api(lua, registry, ex, nested_ex, session, || {
                    let value = object_to_lua(lua, &Object::LuaRef(reference))
                        .map_err(|error| LuaExecError::Conversion(error.to_string()))?;
                    let Value::Function(function) = value else {
                        return Err(LuaExecError::Conversion(
                            "autocmd Lua reference is not a function".into(),
                        ));
                    };
                    let args = args
                        .iter()
                        .map(|argument| {
                            object_to_lua(lua, argument)
                                .map_err(|error| LuaExecError::Conversion(error.to_string()))
                        })
                        .collect::<Result<Vec<_>, _>>()?
                        .into();
                    let mut values = call_with_traceback(lua, &function, args)
                        .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
                    let outcome = match values.pop_front() {
                        None | Some(Value::Nil | Value::Boolean(false)) => AutocmdExecution::Keep,
                        Some(_) => AutocmdExecution::Delete,
                    };
                    Ok(outcome)
                })
                .map_err(lua_exec_error_text)
            }
        }
    }

    fn release_callback(&mut self, reference: u64) -> Result<(), String> {
        let reference = i32::try_from(reference)
            .map_err(|_| "autocmd Lua reference is out of range".to_owned())?;
        free_lua_ref(&self.lua, reference).map_err(|error| error.to_string())
    }

    fn fork(&self) -> Option<Box<dyn AutocmdExecutor>> {
        let (ex, nested_ex) = fresh_executors(
            &self.lua,
            &self.registry,
            &self.session,
            &self.ex,
            &self.channel_ids,
            &self.event_loop,
        )?;
        Some(Box::new(Self {
            session: self.session.clone(),
            ex,
            nested_ex,
            lua: self.lua.clone(),
            registry: self.registry.clone(),
            channel_ids: self.channel_ids.clone(),
            event_loop: self.event_loop.clone(),
        }))
    }
}

/// Formats a Vimscript autocmd execution failure with the event and
/// definition pattern that produced the action, matching upstream's
/// `do_autocmd` error label. For `ExecError::Vim` the raw exception
/// message is used (without the throwpoint suffix that
/// `ExecError::Display` appends), so a user `:throw 'foo'` becomes
/// `WinLeave Autocommands for "*": foo`. Other execution errors keep
/// their existing `Display` text under the same source prefix.
fn format_autocmd_exec_error(action: &AutocmdAction, error: &ExecError) -> String {
    let message = match &error {
        ExecError::Vim(exception) => exception.message(),
        _ => error.to_string(),
    };
    format!(
        "{} Autocommands for \"{}\": {}",
        action.event.as_str(),
        action.pattern,
        message
    )
}

#[derive(Clone, Copy)]
enum ApiOperation {
    Command,
    Exec2,
    Eval,
    CallFunction,
}

fn api_throwpoint(operation: ApiOperation, raw: &str) -> Option<String> {
    match operation {
        ApiOperation::Command => None,
        ApiOperation::Exec2 => {
            let line = raw
                .strip_prefix("script <nvim>[")?
                .strip_suffix(']')?
                .parse::<usize>()
                .ok()?;
            Some(format!("nvim_exec2(), line {line}"))
        }
        ApiOperation::Eval | ApiOperation::CallFunction => {
            let functions = raw
                .split_once("..script ")
                .map_or(raw, |(functions, _)| functions)
                .strip_prefix("function ")?;
            let (frames, line) = functions.strip_suffix(']')?.rsplit_once('[')?;
            let line = line.parse::<usize>().ok()?;
            Some(format!(
                "function {}, line {line}",
                frames.replace("..function ", "..")
            ))
        }
    }
}

fn map_api_exec_error(operation: ApiOperation, error: ExecError) -> ApiError {
    let message = match error {
        ExecError::Vim(exception) => match api_throwpoint(operation, &exception.throwpoint) {
            Some(throwpoint) => format!("{throwpoint}: {}", exception.message()),
            None => exception.message(),
        },
        ExecError::Eval(error) => format!("Vim:{}: {}", error.code, error.message),
        other => other.to_string(),
    };
    ApiError::exception(message)
}

/// Maps an `ExecError` from `define_user_command` to the correct `ApiError`
/// class: a "command already exists" failure is a validation error (wire
/// type 1), everything else is an exception (wire type 0).
fn map_define_error(error: &ExecError) -> ApiError {
    let message = error.to_string();
    if message.contains("already exists") {
        ApiError::validation(message)
    } else {
        ApiError::exception(message)
    }
}

/// Maps an `ExecError` from `parse_cmdline` to bare error text. The API
/// handler (`nvim_parse_cmd`) adds the `"Parsing command-line: "` prefix
/// itself, so the trait method must return the unprefixed message (with
/// E-code) to avoid double-prefixing. `nvim_command`/`nvim_exec2` parse
/// failures use the same bare text, matching upstream.
fn map_parse_error(error: ExecError) -> ApiError {
    match error {
        ExecError::Parse(parse_error) => ApiError::exception(format!(
            "{}: {}",
            parse_error.code.as_str(),
            parse_error.message
        )),
        other => ApiError::exception(other.to_string()),
    }
}

#[derive(Clone)]
struct ServerCommandHost {
    session: Rc<ApiSession>,
    ex: Rc<RefCell<ExExecutor>>,
    nested_ex: Rc<RefCell<ExExecutor>>,
    lua: Lua,
    registry: Rc<Registry>,
    channel_ids: ChannelIds,
    event_loop: EventLoopPump,
}

impl CommandExecutor for ServerCommandHost {
    fn execute(
        &mut self,
        session: &ApiSession,
        commands: &[ox_api::ExCommand],
    ) -> Result<(), ApiError> {
        // Reentrant `nvim_exec2`/`nvim_command` (Vimscript calling the API
        // while a command already runs) executes on the nested executor
        // instead of panicking on the outer borrow. The guard drops at the
        // arm boundary so a pending `jobwait` Lua flush delivers with no
        // borrow live, like every other entry.
        let (result, owner) = if let Ok(mut guard) = self.ex.try_borrow_mut() {
            let result = guard
                .execute_commands(session, commands)
                .map_err(|error| map_api_exec_error(ApiOperation::Command, error))
                .map(|_| ());
            (result, self.ex.clone())
        } else if let Ok(mut guard) = self.nested_ex.try_borrow_mut() {
            let result = guard
                .execute_commands(session, commands)
                .map_err(|error| map_api_exec_error(ApiOperation::Command, error))
                .map(|_| ());
            (result, self.nested_ex.clone())
        } else {
            return Err(ApiError::exception(
                "no free Ex executor for a nested command",
            ));
        };
        deliver_pending_lua_flush(session, &owner);
        result
    }

    fn execute_command(&mut self, session: &ApiSession, command: &str) -> Result<(), ApiError> {
        let (result, owner) = if let Ok(mut guard) = self.ex.try_borrow_mut() {
            let result = guard
                .execute_line(session, command)
                .map_err(|error| map_api_exec_error(ApiOperation::Command, error))
                .map(|_| ());
            (result, self.ex.clone())
        } else if let Ok(mut guard) = self.nested_ex.try_borrow_mut() {
            let result = guard
                .execute_line(session, command)
                .map_err(|error| map_api_exec_error(ApiOperation::Command, error))
                .map(|_| ());
            (result, self.nested_ex.clone())
        } else {
            return Err(ApiError::exception(
                "no free Ex executor for a nested command",
            ));
        };
        deliver_pending_lua_flush(session, &owner);
        result
    }

    fn execute_script(&mut self, session: &ApiSession, source: &str) -> Result<(), ApiError> {
        let (result, owner) = if let Ok(mut guard) = self.ex.try_borrow_mut() {
            let result = guard
                .execute_script(session, "<nvim>", source)
                .map_err(|error| map_api_exec_error(ApiOperation::Exec2, error))
                .map(|_| ());
            (result, self.ex.clone())
        } else if let Ok(mut guard) = self.nested_ex.try_borrow_mut() {
            let result = guard
                .execute_script(session, "<nvim>", source)
                .map_err(|error| map_api_exec_error(ApiOperation::Exec2, error))
                .map(|_| ());
            (result, self.nested_ex.clone())
        } else {
            return Err(ApiError::exception(
                "no free Ex executor for a nested command",
            ));
        };
        deliver_pending_lua_flush(session, &owner);
        result
    }

    fn define_user_command(
        &mut self,
        session: &ApiSession,
        buffer: Option<BufHandle>,
        command: UserCommand,
        force: bool,
    ) -> Result<(), ApiError> {
        if let Ok(mut ex) = self.ex.try_borrow_mut() {
            return session
                .with_editor_mut(|editor| ex.define_user_command(editor, buffer, command, force))
                .map_err(|error| map_define_error(&error));
        }
        let Ok(mut nested) = self.nested_ex.try_borrow_mut() else {
            return Err(ApiError::exception(
                "no free Ex executor for a nested command",
            ));
        };
        session
            .with_editor_mut(|editor| nested.define_user_command(editor, buffer, command, force))
            .map_err(|error| map_define_error(&error))
    }

    fn delete_user_command(
        &mut self,
        session: &ApiSession,
        buffer: Option<BufHandle>,
        name: &str,
    ) -> Result<(), ApiError> {
        if let Ok(mut ex) = self.ex.try_borrow_mut() {
            return session
                .with_editor_mut(|editor| ex.delete_user_command(editor, buffer, name))
                .map_err(|error| ApiError::exception(error.to_string()));
        }
        let Ok(mut nested) = self.nested_ex.try_borrow_mut() else {
            return Err(ApiError::exception(
                "no free Ex executor for a nested command",
            ));
        };
        session
            .with_editor_mut(|editor| nested.delete_user_command(editor, buffer, name))
            .map_err(|error| ApiError::exception(error.to_string()))
    }

    fn list_user_commands(
        &mut self,
        _session: &ApiSession,
        buffer: Option<BufHandle>,
    ) -> Result<Vec<UserCommand>, ApiError> {
        if let Ok(ex) = self.ex.try_borrow() {
            return Ok(ex.list_user_commands(buffer));
        }
        let Ok(nested) = self.nested_ex.try_borrow() else {
            return Err(ApiError::exception(
                "no free Ex executor for a nested command",
            ));
        };
        Ok(nested.list_user_commands(buffer))
    }

    fn parse_cmdline(
        &mut self,
        session: &ApiSession,
        line: &str,
    ) -> Result<Vec<ox_api::ExCommand>, ApiError> {
        if let Ok(ex) = self.ex.try_borrow() {
            return session
                .with_editor(|editor| ex.parse_commands(editor, line))
                .map_err(map_parse_error);
        }
        let Ok(nested) = self.nested_ex.try_borrow() else {
            return Err(ApiError::exception(
                "no free Ex executor for a nested command",
            ));
        };
        session
            .with_editor(|editor| nested.parse_commands(editor, line))
            .map_err(map_parse_error)
    }

    fn remove_buffer(&mut self, buffer: BufHandle) -> Result<(), ApiError> {
        if let Ok(mut ex) = self.ex.try_borrow_mut() {
            ex.remove_buffer(buffer);
            return Ok(());
        }
        let Ok(mut nested) = self.nested_ex.try_borrow_mut() else {
            return Err(ApiError::exception(
                "no free Ex executor for a nested command",
            ));
        };
        nested.remove_buffer(buffer);
        Ok(())
    }

    fn evaluate(&mut self, session: &ApiSession, expression: &str) -> Result<Typval, ApiError> {
        let (result, owner) = if let Ok(mut guard) = self.ex.try_borrow_mut() {
            let result = guard
                .evaluate_expression(session, expression)
                .map_err(|error| map_api_exec_error(ApiOperation::Eval, error));
            (result, self.ex.clone())
        } else if let Ok(mut guard) = self.nested_ex.try_borrow_mut() {
            let result = guard
                .evaluate_expression(session, expression)
                .map_err(|error| map_api_exec_error(ApiOperation::Eval, error));
            (result, self.nested_ex.clone())
        } else {
            return Err(ApiError::exception(
                "no free Ex executor for Vimscript expression evaluation",
            ));
        };
        deliver_pending_lua_flush(session, &owner);
        result
    }

    fn call_builtin(
        &mut self,
        session: &ApiSession,
        name: &OxStr,
        args: Vec<Typval>,
    ) -> Result<Typval, ApiError> {
        if let Some(value) = live_mode_builtin(session, name, &args)? {
            return Ok(value);
        }
        // Same tiering as `execute`: the primary executor when it is free,
        // the nested one when a running command re-enters the API. The
        // guard drops at the arm boundary so the pending `jobwait` flush
        // delivers with no borrow live.
        let (result, owner) = if let Ok(mut guard) = self.ex.try_borrow_mut() {
            let result = guard
                .call_builtin(session, name, args)
                .map_err(|error| map_api_exec_error(ApiOperation::CallFunction, error));
            (result, self.ex.clone())
        } else if let Ok(mut guard) = self.nested_ex.try_borrow_mut() {
            let result = guard
                .call_builtin(session, name, args)
                .map_err(|error| map_api_exec_error(ApiOperation::CallFunction, error));
            (result, self.nested_ex.clone())
        } else {
            return Err(ApiError::exception(
                "no free Ex executor for a Vimscript builtin call",
            ));
        };
        deliver_pending_lua_flush(session, &owner);
        result
    }

    fn change_directory(&mut self, session: &ApiSession, path: &str) -> Result<(), ApiError> {
        if let Ok(mut ex) = self.ex.try_borrow_mut() {
            return ex
                .change_directory(session, path)
                .map_err(|error| map_api_exec_error(ApiOperation::Command, error));
        }
        let Ok(mut nested) = self.nested_ex.try_borrow_mut() else {
            return Err(ApiError::exception(
                "no free Ex executor for a nested command",
            ));
        };
        nested
            .change_directory(session, path)
            .map_err(|error| map_api_exec_error(ApiOperation::Command, error))
    }

    fn fork(&self) -> Option<Box<dyn CommandExecutor>> {
        let (ex, nested_ex) = fresh_executors(
            &self.lua,
            &self.registry,
            &self.session,
            &self.ex,
            &self.channel_ids,
            &self.event_loop,
        )?;
        Some(Box::new(Self {
            session: self.session.clone(),
            ex,
            nested_ex,
            lua: self.lua.clone(),
            registry: self.registry.clone(),
            channel_ids: self.channel_ids.clone(),
            event_loop: self.event_loop.clone(),
        }))
    }
}

struct ExApiExecutor<'a> {
    executor: &'a mut ExExecutor,
    outcome: ExecOutcome,
}

impl CommandExecutor for ExApiExecutor<'_> {
    fn execute(
        &mut self,
        session: &ApiSession,
        commands: &[ox_api::ExCommand],
    ) -> Result<(), ApiError> {
        self.outcome = self
            .executor
            .execute_commands(session, commands)
            .map_err(|error| map_api_exec_error(ApiOperation::Command, error))?;
        Ok(())
    }

    fn define_user_command(
        &mut self,
        session: &ApiSession,
        buffer: Option<BufHandle>,
        command: UserCommand,
        force: bool,
    ) -> Result<(), ApiError> {
        session
            .with_editor_mut(|editor| {
                self.executor
                    .define_user_command(editor, buffer, command, force)
            })
            .map_err(|error| map_define_error(&error))
    }

    fn delete_user_command(
        &mut self,
        session: &ApiSession,
        buffer: Option<BufHandle>,
        name: &str,
    ) -> Result<(), ApiError> {
        session
            .with_editor_mut(|editor| self.executor.delete_user_command(editor, buffer, name))
            .map_err(|error| ApiError::exception(error.to_string()))
    }

    fn list_user_commands(
        &mut self,
        _session: &ApiSession,
        buffer: Option<BufHandle>,
    ) -> Result<Vec<UserCommand>, ApiError> {
        Ok(self.executor.list_user_commands(buffer))
    }

    fn parse_cmdline(
        &mut self,
        session: &ApiSession,
        line: &str,
    ) -> Result<Vec<ox_api::ExCommand>, ApiError> {
        session
            .with_editor(|editor| self.executor.parse_commands(editor, line))
            .map_err(map_parse_error)
    }

    fn remove_buffer(&mut self, buffer: BufHandle) -> Result<(), ApiError> {
        self.executor.remove_buffer(buffer);
        Ok(())
    }

    fn evaluate(&mut self, session: &ApiSession, expression: &str) -> Result<Typval, ApiError> {
        self.executor
            .evaluate_expression(session, expression)
            .map_err(|error| map_api_exec_error(ApiOperation::Eval, error))
    }

    fn call_builtin(
        &mut self,
        session: &ApiSession,
        name: &OxStr,
        args: Vec<Typval>,
    ) -> Result<Typval, ApiError> {
        if let Some(value) = live_mode_builtin(session, name, &args)? {
            return Ok(value);
        }
        self.executor
            .call_builtin(session, name, args)
            .map_err(|error| map_api_exec_error(ApiOperation::CallFunction, error))
    }
    fn change_directory(&mut self, session: &ApiSession, path: &str) -> Result<(), ApiError> {
        self.executor
            .change_directory(session, path)
            .map_err(|error| map_api_exec_error(ApiOperation::Command, error))
    }
}

/// `(false, message)` result for the scoped-call Lua shim
/// ([`error_shim`]): the shim re-raises the message as a *string* Lua
/// error, so `pcall` never observes an mlua `WrappedFailure` userdata --
/// upstream raises plain strings (`nlua_error`), and the `exec_lua` harness
/// rejects userdata with "cannot be serialized over RPC".
fn scoped_failure(lua: &Lua, error: impl std::fmt::Display) -> mlua::Result<(bool, Value)> {
    Ok((false, Value::String(lua.create_string(error.to_string())?)))
}

/// Success half of the scoped-call shim contract: convert one builtin result
/// Typval, reporting conversion failures through [`scoped_failure`].
fn scoped_typval(lua: &Lua, value: &Typval) -> mlua::Result<(bool, Value)> {
    match typval_to_lua(lua, value) {
        Ok(value) => Ok((true, value)),
        Err(error) => scoped_failure(lua, error),
    }
}

/// [`scoped_failure`] for natives returning [`MultiValue`]: the flag rides
/// as the first value, so one shim serves `(bool, Value)` and multi-value
/// natives alike.
fn scoped_failure_multi(lua: &Lua, error: impl std::fmt::Display) -> mlua::Result<MultiValue> {
    Ok(MultiValue::from_vec(vec![
        Value::Boolean(false),
        Value::String(lua.create_string(error.to_string())?),
    ]))
}

/// One Vimscript builtin call from a scoped Lua chunk: the primary executor
/// when it is free, otherwise the nested one, always against the live editor
/// this Ex frame is executing with -- the same tiering `EditorBuiltins` uses
/// outside Ex execution.
///
/// Temporary Lua function references belong to this call. Release them after
/// the builtin returns, including conversion and execution failures.
///
/// `jobstart` is the exception: on success its callback references transfer
/// to the job (upstream's job-owned callback refs), so freeing them here
/// would leave the `on_exit/on_stdout` closures dangling and deliver the wrong
/// callback. This leaves a known, bounded leak: those refs are not freed when
/// the job is dropped until a future lifetime pass is added.
fn dispatch_scoped_builtin(
    lua: &Lua,
    session: &ApiSession,
    ex: &Rc<RefCell<ExExecutor>>,
    nested_ex: &Rc<RefCell<ExExecutor>>,
    name: &[u8],
    args: &[Value],
) -> mlua::Result<(bool, Value)> {
    let name = OxStr(name.to_vec());
    let mut converted = Vec::with_capacity(args.len());
    let mut references = Vec::new();
    for value in args {
        match lua_to_typval(lua, value) {
            Ok(value) => {
                collect_typval_refs(&value, &mut references);
                converted.push(value);
            }
            Err(error) => {
                free_typval_refs(lua, &references);
                return scoped_failure(lua, error);
            }
        }
    }

    match live_mode_builtin(session, &name, &converted) {
        Ok(Some(value)) => {
            free_typval_refs(lua, &references);
            return scoped_typval(lua, &value);
        }
        Ok(None) => {}
        Err(error) => {
            free_typval_refs(lua, &references);
            return scoped_failure(lua, error);
        }
    }
    let (result, owner) = match ex.try_borrow_mut() {
        Ok(mut guard) => {
            let result = guard.call_builtin(session, &name, converted);
            (result, Rc::clone(ex))
        }
        Err(_) => match nested_ex.try_borrow_mut() {
            Ok(mut guard) => {
                let result = guard.call_builtin(session, &name, converted);
                (result, Rc::clone(nested_ex))
            }
            Err(_) => (
                Err(ExecError::Editor(
                    "no free Ex executor for a Vimscript builtin call".into(),
                )),
                Rc::clone(ex),
            ),
        },
    };
    let is_jobstart = name.as_bytes() == b"jobstart";
    // A failed spawn returns `Ok(-1)`: no job exists to own the callback
    // references, so they free here with every other non-transferring
    // result (the successful-spawn transfer is the only retention).
    let failed_spawn = is_jobstart && matches!(&result, Ok(Typval::Number(id)) if *id <= 0);
    if !is_jobstart || result.is_err() || failed_spawn {
        free_typval_refs(lua, &references);
    }
    let result = match result {
        Ok(value) => value,
        Err(error) => return scoped_failure(lua, error),
    };
    // A `jobwait` flush re-deferred Lua callbacks that upstream delivers
    // before the builtin returns (multiqueue_process_events,
    // funcs.c:3668/3721); the executor borrow is released here, so this is
    // the first boundary that can run them.
    deliver_pending_lua_flush(session, &owner);
    scoped_typval(lua, &result)
}

/// Delivers a pending `jobwait` Lua flush at a borrow-free boundary, looping
/// while delivered callbacks re-mark the manager. Delivery is skipped while
/// the Lua host is borrowed - a Lua chunk holds it across its own execution,
/// so a `jobwait` called from inside the chunk leaves the marker for the
/// tick; reentrant `vim.fn` calls made by a callback being delivered meet
/// the same busy host and defer the same way, which bounds the recursion.
/// Callback failure reports through the message system, the way the tick
/// driver treats it.
fn deliver_pending_lua_flush(session: &ApiSession, owner: &Rc<RefCell<ExExecutor>>) {
    let host = owner.borrow().lua_host();
    if let Some(host) = host
        && host.try_borrow_mut().is_err()
    {
        return;
    }
    while owner.borrow_mut().take_lua_flush_pending() {
        if let Err(error) = deliver_deferred_job_events(session, owner) {
            report_job_callback_error(session, &error);
            break;
        }
    }
}

/// One scoped `nvim_cmd` from Lua re-entered from Vimscript: the primary
/// executor when it is free, otherwise the nested one -- the same tiering
/// `ServerCommandHost` uses for reentrant commands. Fails only when both are
/// already borrowed by an enclosing Ex frame.
fn dispatch_scoped_nvim_cmd(
    session: &ApiSession,
    ex: &Rc<RefCell<ExExecutor>>,
    nested_ex: &Rc<RefCell<ExExecutor>>,
    cmd: &Dict,
    opts: &Dict,
) -> Result<OxStr, mlua::Error> {
    let mut ex = match ex.try_borrow_mut() {
        Ok(ex) => ex,
        Err(_) => nested_ex
            .try_borrow_mut()
            .map_err(|_| mlua::Error::runtime("no free Ex executor for a nested nvim_cmd"))?,
    };
    let mut executor = ExApiExecutor {
        executor: &mut ex,
        outcome: ExecOutcome::Completed,
    };
    ox_api::execute_nvim_cmd(session, cmd, opts, &mut executor).map_err(mlua::Error::external)
}

/// Rebinds the Lua surface over the caller's live session, so Lua re-entered
/// from Vimscript observes the same state as the enclosing dispatch instead
/// of a scratch copy: `vim.api` dispatch, `vim._getvar`/`vim._setvar`,
/// `vim.call` and `vim.fn` all run through `session` (editor access stays
/// statement-scoped inside each binding), and nested `nvim_cmd` and Vimscript
/// builtins fall to `nested_ex` (the nested half of the primary/nested
/// executor pair) once `ex` is borrowed by the enclosing command. Every
/// original binding is restored when `run` returns.
#[expect(
    clippy::too_many_lines,
    reason = "scoped Lua rebinding and restoration form one lifetime-sensitive transaction"
)]
fn with_scoped_editor_api<T>(
    lua: &Lua,
    registry: &Registry,
    ex: &Rc<RefCell<ExExecutor>>,
    nested_ex: &Rc<RefCell<ExExecutor>>,
    session: &ApiSession,
    run: impl FnOnce() -> Result<T, LuaExecError>,
) -> Result<T, LuaExecError> {
    let vim: Table = lua
        .globals()
        .get("vim")
        .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let api: Table = vim
        .get("api")
        .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let variable_lookup_binding: Value = vim
        .get("_getvar")
        .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let variable_assignment_binding: Value = vim
        .get("_setvar")
        .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let original_call: Value = vim
        .get("call")
        .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let original_fn: Value = vim
        .get("fn")
        .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let originals = registry
        .iter()
        .map(|(metadata, _)| {
            api.get::<Value>(metadata.name)
                .map(|value| (metadata.name, value))
        })
        .collect::<mlua::Result<Vec<_>>>()
        .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    // Scoped natives signal failure as `(false, message)`; this Lua shim
    // re-raises the message as a string error, so `pcall` in chunks and the
    // exec_lua harness never observe an mlua `WrappedFailure` userdata.
    let shim: Function =
        error_shim(lua).map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    // Shared by reference so every scope closure copies the borrow instead
    // of the first closure moving the shim away from the rest.
    let shim = &shim;
    // The caller's real session drives every scoped binding: scope closures
    // accept the non-'static `&ApiSession` borrow, and no throwaway session
    // is ever constructed here.
    let result = lua.scope(|scope| {
        vim.set(
            "_getvar",
            shim.call::<Function>(scope.create_function_mut(
                move |lua, (scope, handle, name): (mlua::LuaString, i64, mlua::LuaString)| {
                    let Ok(scope) = parse_variable_scope(&scope) else {
                        return scoped_failure(lua, "unknown variable scope");
                    };
                    let name = OxStr(name.as_bytes().to_vec());
                    session.with_editor(|editor| {
                        let values = match variables(editor, scope, handle) {
                            Ok(values) => values,
                            Err(error) => return scoped_failure(lua, error),
                        };
                        match values.get(&name) {
                            Some(value) => match object_to_lua(lua, value) {
                                Ok(value) => Ok((true, value)),
                                Err(error) => scoped_failure(lua, error),
                            },
                            None => Ok((true, Value::Nil)),
                        }
                    })
                },
            )?)?,
        )?;
        vim.set(
            "_setvar",
            shim.call::<Function>(scope.create_function_mut(
                move |lua,
                      (scope, handle, name, value): (
                    mlua::LuaString,
                    i64,
                    mlua::LuaString,
                    Value,
                )| {
                    let Ok(scope) = parse_variable_scope(&scope) else {
                        return scoped_failure(lua, "unknown variable scope");
                    };
                    let name = OxStr(name.as_bytes().to_vec());
                    if scope == VariableScope::Vim && !vim_variable_is_writable(name.as_bytes()) {
                        return scoped_failure(
                            lua,
                            format!(
                                "E46: Cannot change read-only variable \"{}\"",
                                name.to_string_lossy()
                            ),
                        );
                    }
                    let value = if value.is_nil() {
                        None
                    } else {
                        match lua_to_object(lua, &value) {
                            Ok(value) => Some(value),
                            Err(error) => return scoped_failure(lua, error),
                        }
                    };
                    session.with_editor_mut(|editor| {
                        let variables = match variables_mut(editor, scope, handle) {
                            Ok(variables) => variables,
                            Err(error) => return scoped_failure(lua, error),
                        };
                        if let Some(value) = value {
                            variables.insert(name, value);
                        } else {
                            let index = variables.iter().position(|(key, _)| key == &name);
                            if let Some(index) = index {
                                variables.0.remove(index);
                            }
                        }
                        Ok((true, Value::Nil))
                    })
                },
            )?)?,
        )?;
        // Lua→Vimscript reentry runs on the live editor and the
        // primary/nested executor pair, mirroring `EditorBuiltins`' tiering
        // Ex code can never be running inside a fast callback.
        let call_ex = ex.clone();
        let call_nested = nested_ex.clone();
        vim.set(
            "call",
            shim.call::<Function>(scope.create_function_mut(
                move |lua, (name, args): (mlua::LuaString, Variadic<Value>)| {
                    dispatch_scoped_builtin(
                        lua,
                        session,
                        &call_ex,
                        &call_nested,
                        &name.as_bytes(),
                        args.as_slice(),
                    )
                },
            )?)?,
        )?;
        let fn_ex = ex.clone();
        let fn_nested = nested_ex.clone();
        let fn_table = lua.create_table()?;
        let fn_metatable = lua.create_table()?;
        fn_metatable.set(
            "__index",
            scope.create_function_mut(move |_lua, (_table, name): (Table, mlua::LuaString)| {
                let ex = fn_ex.clone();
                let nested_ex = fn_nested.clone();
                let name = name.as_bytes().to_vec();
                let native = scope.create_function_mut(move |lua, args: Variadic<Value>| {
                    dispatch_scoped_builtin(lua, session, &ex, &nested_ex, &name, args.as_slice())
                })?;
                shim.call::<Function>(native)
            })?,
        )?;
        fn_table.set_metatable(Some(fn_metatable))?;
        vim.set("fn", fn_table)?;
        for (metadata, dispatch) in registry.iter() {
            if metadata.name == "nvim_cmd" {
                let cmd_ex = ex.clone();
                let nested_ex = nested_ex.clone();
                api.set(
                    metadata.name,
                    shim.call::<Function>(scope.create_function_mut(
                        move |lua, args: Variadic<Value>| {
                            let mut converted = Vec::with_capacity(args.len());
                            for value in args.iter() {
                                match lua_to_object(lua, value) {
                                    Ok(value) => converted.push(value),
                                    Err(error) => return scoped_failure_multi(lua, error),
                                }
                            }
                            let (cmd, opts) = match nvim_cmd_args(&converted) {
                                Ok(args) => args,
                                Err(error) => return scoped_failure_multi(lua, error),
                            };
                            let result = match dispatch_scoped_nvim_cmd(
                                session, &cmd_ex, &nested_ex, cmd, opts,
                            ) {
                                Ok(result) => result,
                                Err(error) => return scoped_failure_multi(lua, error),
                            };
                            match object_to_lua(lua, &Object::String(result)) {
                                Ok(value) => {
                                    Ok(MultiValue::from_vec(vec![Value::Boolean(true), value]))
                                }
                                Err(error) => scoped_failure_multi(lua, error),
                            }
                        },
                    )?)?,
                )?;
                continue;
            }
            let params = metadata.params;
            api.set(
                metadata.name,
                shim.call::<Function>(scope.create_function_mut(
                    move |lua, args: Variadic<Value>| {
                        let mut converted = Vec::with_capacity(args.len());
                        for value in args.iter() {
                            match lua_to_object(lua, value) {
                                Ok(value) => converted.push(value),
                                Err(error) => return scoped_failure_multi(lua, error),
                            }
                        }
                        while converted.len() < params.len() {
                            let (_, kind, optional) = params[converted.len()];
                            if !optional || kind != ox_api::TypeRef::Dict {
                                break;
                            }
                            converted.push(Object::Dict(Dict(Vec::new())));
                        }
                        let result = match dispatch(session, &converted) {
                            Ok(result) => result,
                            Err(error) => return scoped_failure_multi(lua, error),
                        };
                        // upstream nlua_api_call: nvim_buf_call/nvim_win_call
                        // return the callback's whole retstack, so an
                        // Object::Array result is the multi-value marker and
                        // expands; every other function keeps single-value
                        // returns untouched.
                        if matches!(metadata.name, "nvim_buf_call" | "nvim_win_call") {
                            let items: &[Object] = match &result {
                                Object::Array(items) => items,
                                _ => std::slice::from_ref(&result),
                            };
                            let mut values = Vec::with_capacity(items.len());
                            let mut failure = None;
                            for value in items {
                                match object_to_lua(lua, value) {
                                    Ok(value) => values.push(value),
                                    Err(error) => {
                                        failure = Some(error.to_string());
                                        break;
                                    }
                                }
                            }
                            free_object_refs(lua, &result);
                            if let Some(error) = failure {
                                return scoped_failure_multi(lua, error);
                            }
                            values.insert(0, Value::Boolean(true));
                            return Ok(MultiValue::from_vec(values));
                        }
                        let value = match object_to_lua(lua, &result) {
                            Ok(value) => value,
                            Err(error) => return scoped_failure_multi(lua, error),
                        };
                        Ok(MultiValue::from_vec(vec![Value::Boolean(true), value]))
                    },
                )?)?,
            )?;
        }
        Ok(run())
    });
    let run_result = match result {
        Ok(run_result) => run_result,
        Err(error) => Err(LuaExecError::Runtime(error.to_string())),
    };
    let mut restore_error = None;
    let mut record_restore = |result: mlua::Result<()>| {
        if let Err(error) = result
            && restore_error.is_none()
        {
            restore_error = Some(LuaExecError::Runtime(error.to_string()));
        }
    };
    for (name, value) in originals {
        record_restore(api.set(name, value));
    }
    record_restore(vim.set("call", original_call));
    record_restore(vim.set("fn", original_fn));
    record_restore(vim.set("_getvar", variable_lookup_binding));
    record_restore(vim.set("_setvar", variable_assignment_binding));
    match (run_result, restore_error) {
        (Err(error), _) | (Ok(_), Some(error)) => Err(error),
        (Ok(value), None) => Ok(value),
    }
}

fn parse_variable_scope(scope: &mlua::LuaString) -> Result<VariableScope, String> {
    match scope.as_bytes().as_ref() {
        b"g" => Ok(VariableScope::Global),
        b"b" => Ok(VariableScope::Buffer),
        b"w" => Ok(VariableScope::Window),
        b"t" => Ok(VariableScope::Tabpage),
        b"v" => Ok(VariableScope::Vim),
        _ => Err("unknown variable scope".into()),
    }
}

fn nvim_cmd_args(params: &[Object]) -> Result<(&Dict, &Dict), ApiError> {
    static EMPTY: std::sync::OnceLock<Dict> = std::sync::OnceLock::new();
    let empty = EMPTY.get_or_init(|| Dict(Vec::new()));
    match params {
        [Object::Dict(cmd)] => Ok((cmd, empty)),
        [Object::Dict(cmd), Object::Dict(opts)] => Ok((cmd, opts)),
        [Object::Dict(cmd), Object::Array(opts)] if opts.is_empty() => Ok((cmd, empty)),
        _ => Err(ApiError::validation(
            "nvim_cmd expects (Dict, optional Dict)",
        )),
    }
}

struct LuaScheduler {
    queue: Rc<RefCell<VecDeque<Work>>>,
}

impl Scheduler for LuaScheduler {
    fn schedule_deferred(&self, work: Work) -> Result<(), String> {
        self.queue.borrow_mut().push_back(work);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_types::Funcref;

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires deferred send setup and execution to succeed"
    )]
    fn job_channel_sink_defers_send_on_reentrant_borrow() {
        let ex = Rc::new(RefCell::new(ox_editor::ExExecutor::new()));
        let queue = Rc::new(RefCell::new(VecDeque::new()));
        let mut sink = JobChannelSink {
            session: Rc::new(ApiSession::new(Rc::new(RefCell::new(Editor::new())))),
            ex: ex.clone(),
            queue: queue.clone(),
            deferred: Rc::new(Cell::new(0)),
        };

        let outer = ex.borrow_mut();
        // A send while `ex` is already borrowed must not panic; it should
        // queue the work for the scheduler instead.
        ox_api::ChannelSink::send(&mut sink, 7, b"hello\n").unwrap();

        assert_eq!(queue.borrow().len(), 1, "reentrant send must be queued");
        assert_eq!(
            sink.deferred.get(),
            1,
            "deferred counter must track queued send"
        );

        drop(outer);
        let work = queue.borrow_mut().pop_front().unwrap();
        work().unwrap();
    }

    #[test]
    fn job_channel_sink_failure_reports_message() {
        let session = Rc::new(ApiSession::new(Rc::new(RefCell::new(Editor::new()))));
        // A write failure (EPIPE-class) routes into the message system;
        // job_send reports Ok(false) for unknown channels, so drive the
        // reporting helper with the failure shape directly.
        report_job_send(&session, Err("broken pipe".to_owned()), 99);
        let reported = session.with_editor(|editor| {
            editor
                .messages()
                .iter()
                .any(|m| format!("{:?}", m.content).contains("channel 99 write failed"))
        });
        assert!(reported, "failed job-channel write must report a message");
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires a non-existent channel send to succeed"
    )]
    fn job_channel_sink_missing_channel_reports_message() {
        let session = Rc::new(ApiSession::new(Rc::new(RefCell::new(Editor::new()))));
        let ex = Rc::new(RefCell::new(ox_editor::ExExecutor::new()));
        let queue = Rc::new(RefCell::new(VecDeque::new()));
        let deferred = Rc::new(Cell::new(0));
        let mut sink = JobChannelSink {
            session: session.clone(),
            ex,
            queue,
            deferred,
        };
        // A channel id with no running job: job_send returns Ok(false), which
        // must surface through the message system.
        ox_api::ChannelSink::send(&mut sink, 12345, b"stale send\n").unwrap();
        let reported = session.with_editor(|editor| {
            editor
                .messages()
                .iter()
                .any(|m| format!("{:?}", m.content).contains("channel 12345 write failed"))
        });
        assert!(reported, "missing job-channel write must report a message");
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires editor and Lua dispatch setup to succeed"
    )]
    fn lua_autocmd_once_runs_on_live_editor() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let (_, dispatch) = core.registry.get("nvim_exec_lua").unwrap();
        let result = dispatch(
            &core.session,
            &[
                Object::String(OxStr::from(
                    r#"
                    local count = 0
                    vim.api.nvim_create_autocmd("FileType", {
                      pattern = "*",
                      callback = function() count = count + 1 end,
                      once = true,
                    })
                    vim.cmd "set filetype=txt"
                    vim.cmd "set filetype=python"
                    return count
                    "#,
                )),
                Object::Array(Vec::new()),
            ],
        )
        .unwrap();
        assert_eq!(result, Object::Integer(1));
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires editor and Lua dispatch setup to succeed"
    )]
    fn failing_lua_autocmd_keeps_api_mutations() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let (_, dispatch) = core.registry.get("nvim_exec_lua").unwrap();
        let result = dispatch(
            &core.session,
            &[
                Object::String(OxStr::from(
                    r#"
                    vim.api.nvim_create_autocmd("FileType", {
                      pattern = "*",
                      callback = function()
                        vim.api.nvim_set_var("failed_autocmd_api_works", true)
                        error("expected autocmd failure")
                      end,
                    })
                    local ok = pcall(vim.cmd, "set filetype=txt")
                    return { ok, vim.api.nvim_get_var("failed_autocmd_api_works") }
                    "#,
                )),
                Object::Array(Vec::new()),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Object::Array(vec![Object::Boolean(false), Object::Boolean(true)])
        );
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires editor and Lua dispatch setup to succeed"
    )]
    fn ui_attach_validates_and_registers() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let (_, dispatch) = core.registry.get("nvim_exec_lua").unwrap();
        let run = |chunk: &str| {
            dispatch(
                &core.session,
                &[
                    Object::String(OxStr::from(chunk)),
                    Object::Array(Vec::new()),
                ],
            )
        };
        // nlua_ui_attach validation (executor.c:807-862): bad ns, bad opts
        // key, no-true-widget, and the working attach/detach pair.
        assert!(run("vim.ui_attach(999, {ext_popupmenu=true}, function() end)").is_err());
        assert!(
            run(
                "vim.ui_attach(vim.api.nvim_create_namespace 'x', {ext_bogus=true}, function() end)"
            )
            .is_err()
        );
        assert!(
            run("vim.ui_attach(vim.api.nvim_create_namespace 'x', {}, function() end)").is_err()
        );
        assert_eq!(
            run("local ns = vim.api.nvim_create_namespace 'probe' vim.ui_attach(ns, {ext_messages=true}, function() end) return 'attached'").unwrap(),
            Object::String(OxStr::from("attached"))
        );
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires editor and Lua dispatch setup to succeed"
    )]
    fn scoped_builtin_errors_reach_pcall_as_strings() {
        // The exec_lua harness (testnvim/exec_lua.lua:44) rejects *userdata*
        // error values ("cannot be serialized over RPC"), so every scoped
        // `vim.fn`/`vim.api` failure must surface as a plain string the way
        // upstream `nlua_error` raises them - never an mlua WrappedFailure.
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let (_, dispatch) = core.registry.get("nvim_exec_lua").unwrap();
        let result = dispatch(
            &core.session,
            &[
                Object::String(OxStr::from(
                    r"
                    local fn_ok, fn_err = pcall(vim.fn.nosuchvimfunction, 1)
                    local api_ok, api_err = pcall(vim.api.nvim_buf_get_lines, 9999, 0, -1, false)
                    local function shape(ok, err)
                      if ok then return { true, type(err) } end
                      return { false, type(err), #tostring(err) > 0 }
                    end
                    return { shape(fn_ok, fn_err), shape(api_ok, api_err) }
                    ",
                )),
                Object::Array(Vec::new()),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Object::Array(vec![
                // `nosuchvimfunction` fails builtin dispatch: pcall must see
                // a *string*, never an mlua WrappedFailure userdata.
                Object::Array(vec![
                    Object::Boolean(false),
                    Object::String(OxStr::from("string")),
                    Object::Boolean(true),
                ]),
                // An invalid buffer handle fails the API dispatch tier: same
                // string-error contract for `vim.api` bindings.
                Object::Array(vec![
                    Object::Boolean(false),
                    Object::String(OxStr::from("string")),
                    Object::Boolean(true),
                ]),
            ])
        );
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires editor and Lua dispatch setup to succeed"
    )]
    fn shared_user_command_registry_visible_from_both_executors() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        // Define a user command through the API path
        // (CommandExecutor::define_user_command) then execute it through
        // vim.cmd (CommandExecutor::parse_cmdline + execute). The shared
        // registry installed by share_user_commands_from makes the
        // API-defined command visible to the Ex parser.
        let (_, dispatch) = core.registry.get("nvim_exec_lua").unwrap();
        let result = dispatch(
            &core.session,
            &[
                Object::String(OxStr::from(
                    r#"
                    vim.api.nvim_create_user_command("TestShared", "let g:shared_cmd_ran = 1", {})
                    vim.cmd "TestShared"
                    return vim.g.shared_cmd_ran
                    "#,
                )),
                Object::Array(Vec::new()),
            ],
        )
        .unwrap();
        assert_eq!(result, Object::Integer(1));
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires editor and Lua dispatch setup to succeed"
    )]
    fn lua_callback_user_command_can_call_vim_api() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        // A Lua-callback user command must be able to call vim.api against
        // the live editor. invoke_callback wraps the callback in
        // with_scoped_editor_api so vim.api dispatches against the editor
        // the Ex frame is executing with, not stale or unbound state.
        let (_, dispatch) = core.registry.get("nvim_exec_lua").unwrap();
        let result = dispatch(
            &core.session,
            &[
                Object::String(OxStr::from(
                    r#"
                    vim.api.nvim_create_user_command("TestCallback", function()
                        vim.api.nvim_set_var("callback_api_works", true)
                        assert(vim.api.nvim_get_var("callback_api_works"))
                    end, {})
                    vim.api.nvim_create_user_command("TestFailingCallback", function()
                        vim.api.nvim_set_var("failed_callback_api_works", true)
                        error("expected callback failure")
                    end, {})
                    vim.cmd "TestCallback"
                    local ok = pcall(vim.cmd, "TestFailingCallback")
                    return {
                      vim.api.nvim_get_var("callback_api_works"),
                      ok,
                      vim.api.nvim_get_var("failed_callback_api_works"),
                    }
                    "#,
                )),
                Object::Array(Vec::new()),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Object::Array(vec![
                Object::Boolean(true),
                Object::Boolean(false),
                Object::Boolean(true),
            ])
        );
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the fixture, embedded core, and forked calls must succeed"
    )]
    fn forked_executors_keep_runtime_roots() {
        let root = std::env::temp_dir().join(format!("oxvim-fork-rtp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("autoload")).unwrap();
        std::fs::write(
            root.join("autoload/mylib.vim"),
            "function! mylib#Greet()\n  let g:greeted = 1\nendfunction\n",
        )
        .unwrap();

        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        core.ex
            .borrow_mut()
            .scripts_mut()
            .add_runtime_root(root.clone());

        let channel_ids = core.session.with_editor(Editor::channel_ids);
        let host = core.lua.borrow();
        let lua = host.lua().clone();
        let event_loop = host.event_loop_pump();
        let (primary, nested) = fresh_executors(
            &lua,
            &core.registry,
            &core.session,
            &core.ex,
            &channel_ids,
            &event_loop,
        )
        .unwrap();
        drop(host);

        for fork in [primary, nested] {
            let mut executor = fork.borrow_mut();
            executor
                .execute_line(&*core.session, "let g:greeted = 0")
                .unwrap();
            executor
                .execute_line(&*core.session, "call mylib#Greet()")
                .unwrap();
            executor
                .execute_line(
                    &*core.session,
                    "if g:greeted != 1 | throw 'autoload body did not run' | endif",
                )
                .unwrap();
        }
        let _ = std::fs::remove_dir_all(&root);
    }
    #[cfg(unix)]
    #[test]
    #[expect(clippy::unwrap_used, reason = "temp-file fixture must succeed")]
    fn user_id_matches_owner_of_new_files() {
        // The old `/proc/self` read answered 0 (root) wherever `/proc` is
        // absent; the euid must own files this process creates.
        let path = std::env::temp_dir().join(format!("oxvim-uid-probe-{}.tmp", std::process::id()));
        std::fs::write(&path, b"probe").unwrap();
        let owned = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&path).unwrap());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(user_id(), owned);
    }

    #[cfg(unix)]
    #[test]
    #[expect(clippy::unwrap_used, reason = "temp-directory fixture must succeed")]
    fn listen_directory_falls_back_from_untrusted_xdg_runtime() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!("oxvim-listen-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::DirBuilder::new()
            .recursive(true)
            .create(&root)
            .unwrap();

        // A world-writable XDG_RUNTIME_DIR candidate must be rejected in
        // favor of the validated /tmp/oxvim.<uid> fallback.
        let xdg_bad = root.join("xdg_bad");
        std::fs::create_dir(&xdg_bad).unwrap();
        std::fs::set_permissions(&xdg_bad, std::fs::Permissions::from_mode(0o777)).unwrap();
        let chosen = ensure_listen_directory(&xdg_bad).unwrap();
        let expected = std::env::temp_dir().join(format!("oxvim.{}", user_id()));
        assert_eq!(
            chosen, expected,
            "loose XDG runtime dir must fall back to private temp dir"
        );

        // A missing, valid XDG candidate is created with 0700 and used.
        let xdg_good = root.join("xdg_good");
        let chosen = ensure_listen_directory(&xdg_good).unwrap();
        assert_eq!(
            chosen, xdg_good,
            "valid XDG runtime dir should be created and used"
        );
        let metadata = std::fs::metadata(&xdg_good).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            0o700,
            "created XDG runtime dir must be 0700"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir(&expected);
    }

    // A fire-and-forget job's on_exit reaches its handler on the tick path:
    // flush_pty_output re-defers the drained events, then the borrow-free
    // deliver_deferred_job_events delivers them with no executor borrow
    // live (upstream delivers job callbacks on the main loop, event/loop.c).
    // Before this fix the exit event was re-deferred forever and on_exit
    // never ran.
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the fixture, script, and tick calls must succeed"
    )]
    fn tick_delivers_fire_and_forget_on_exit_exactly_once() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        core.ex
            .borrow_mut()
            .execute_script(
                &*core.session,
                "<test>",
                "
                let s:logger = {'exits': []}
                function! s:logger.on_exit(id, status, event)
                    let g:exit_status = a:status
                    call add(self.exits, a:status)
                endfunction
                let g:logger = s:logger
                call jobstart(['sh', '-c', 'exit 7'], s:logger)
            ",
            )
            .unwrap();
        // Tick turns until the exit event has been delivered.
        let mut delivered = false;
        for _ in 0..500 {
            let _changed = core
                .ex
                .borrow_mut()
                .flush_pty_output(&*core.session)
                .unwrap();
            if deliver_deferred_job_events(&core.session, &core.ex).unwrap() {
                delivered = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(delivered, "the tick must deliver the deferred on_exit");
        let status = core
            .ex
            .borrow_mut()
            .evaluate_expression(&*core.session, "g:exit_status")
            .unwrap();
        assert_eq!(status, Typval::Number(7), "on_exit must carry the code");
        let exits = core
            .ex
            .borrow_mut()
            .evaluate_expression(&*core.session, "len(g:logger.exits)")
            .unwrap();
        assert_eq!(exits, Typval::Number(1), "on_exit must run exactly once");
        // A second tick finds nothing: the event is not re-delivered.
        let _changed = core
            .ex
            .borrow_mut()
            .flush_pty_output(&*core.session)
            .unwrap();
        let again = deliver_deferred_job_events(&core.session, &core.ex).unwrap();
        assert!(!again, "a delivered on_exit must not re-fire");
    }

    // An on_exit handler that starts another job must not deadlock the tick:
    // the delivery runs with no editor borrow live, so the nested jobstart
    // re-enters the executor cleanly, and the nested job's own on_exit fires
    // on a later turn (upstream: callbacks run on the main loop and may
    // start jobs).
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the fixture, script, and tick calls must succeed"
    )]
    fn on_exit_reentry_starts_a_job_without_deadlocking() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        core.ex
            .borrow_mut()
            .execute_script(
                &*core.session,
                "<test>",
                "
                let s:logger = {'exits': []}
                function! s:logger.on_exit(id, status, event)
                    call add(self.exits, a:status)
                    if len(self.exits) < 2
                        call jobstart(['sh', '-c', 'exit 0'], self)
                    endif
                endfunction
                let g:logger = s:logger
                call jobstart(['sh', '-c', 'exit 0'], s:logger)
            ",
            )
            .unwrap();
        // Tick until both the original and the nested on_exit ran. A
        // manager-clobber (the nested job's fresh manager dropped under the
        // restore) or a reentry deadlock strands this at 1.
        let mut nested_exit_ran = false;
        for _ in 0..1000 {
            let _flushed = core
                .ex
                .borrow_mut()
                .flush_pty_output(&*core.session)
                .unwrap();
            let _delivered = deliver_deferred_job_events(&core.session, &core.ex).unwrap();
            let exits = core
                .ex
                .borrow_mut()
                .evaluate_expression(&*core.session, "len(g:logger.exits)")
                .unwrap();
            if exits == Typval::Number(2) {
                nested_exit_ran = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            nested_exit_ran,
            "the nested jobstart's on_exit never ran (reentry failed)"
        );
    }
    // A Lua on_exit handler that starts a nested job must run on the primary
    // executor: the tick drops the ex RefCell before each callback, so the
    // nested jobstart does not fall back to the nested manager. Both on_exit
    // callbacks fire exactly once and no events remain deferred.
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the fixture, script, and tick calls must succeed"
    )]
    fn lua_on_exit_starts_nested_job_and_both_exit_exactly_once() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let lua = core.ex.borrow().lua_host().unwrap();
        lua.borrow_mut()
            .execute_chunk(
                "
                vim.g.exit_count = 0
                local exits = {}
                local function on_exit(id, status, event)
                  table.insert(exits, status)
                  vim.g.exit_count = vim.g.exit_count + 1
                  if #exits < 2 then
                    vim.fn.jobstart({'sh', '-c', 'exit 0'}, {on_exit = on_exit})
                  end
                end
                vim.fn.jobstart({'sh', '-c', 'exit 0'}, {on_exit = on_exit})
                ",
                Vec::new(),
            )
            .unwrap();
        let mut exit_count = Typval::Number(0);
        for _ in 0..500 {
            let _ = core
                .ex
                .borrow_mut()
                .flush_pty_output(&*core.session)
                .unwrap();
            let _ = deliver_deferred_job_events(&core.session, &core.ex).unwrap();
            exit_count = core
                .ex
                .borrow_mut()
                .evaluate_expression(&*core.session, "g:exit_count")
                .unwrap();
            if exit_count == Typval::Number(2) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            exit_count,
            Typval::Number(2),
            "both on_exit callbacks must fire exactly once"
        );
        assert!(
            core.ex.borrow_mut().take_deferred_job_events().is_empty(),
            "no deferred job events remain after both exits delivered"
        );
    }

    // A Lua on_exit handler that calls chansend on a still-running primary
    // job must reach the primary manager, not the nested fallback, so the
    // return value is the number of bytes written (not 0).
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the fixture, script, and tick calls must succeed"
    )]
    fn lua_on_exit_chansend_reaches_primary_job() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let lua = core.ex.borrow().lua_host().unwrap();
        lua.borrow_mut()
            .execute_chunk(
                r#"
                vim.g.send_ok = 0
                local primary_id = vim.fn.jobstart({'cat'})
                local function on_exit(id, status, event)
                  local ok = vim.fn.chansend(primary_id, "done\n")
                  vim.g.send_ok = ok
                  vim.fn.jobstop(primary_id)
                end
                vim.fn.jobstart({'sh', '-c', 'exit 0'}, {on_exit = on_exit})
                "#,
                Vec::new(),
            )
            .unwrap();
        let mut send_ok = Typval::Number(0);
        for _ in 0..500 {
            let _ = core
                .ex
                .borrow_mut()
                .flush_pty_output(&*core.session)
                .unwrap();
            let _ = deliver_deferred_job_events(&core.session, &core.ex).unwrap();
            send_ok = core
                .ex
                .borrow_mut()
                .evaluate_expression(&*core.session, "g:send_ok")
                .unwrap();
            if send_ok != Typval::Number(0) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_ne!(
            send_ok,
            Typval::Number(0),
            "chansend on the primary job must return non-zero"
        );
    }

    // The jobwait flush delivers a Lua on_exit under the enclosing
    // `call_builtin` borrow, so its re-entry was routed to the nested
    // executor's never-pumped job manager and the nested job stranded.
    // The flush now re-defers Lua-registered events; the tick delivers
    // them with no borrow live, the re-entrant jobstart lands on the
    // primary executor, and both exits fire exactly once.
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the fixture, script, and tick calls must succeed"
    )]
    fn lua_on_exit_via_jobwait_nested_jobstart_does_not_strand() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let lua = core.ex.borrow().lua_host().unwrap();
        lua.borrow_mut()
            .execute_chunk(
                "
                vim.g.nested_exited = 0
                local exits = 0
                local function on_exit(id, status, event)
                  exits = exits + 1
                  if exits == 1 then
                    vim.fn.jobstart({'sh', '-c', 'exit 0'}, {on_exit = on_exit})
                  else
                    vim.g.nested_exited = 1
                  end
                end
                local id = vim.fn.jobstart({'sh', '-c', 'exit 0'}, {on_exit = on_exit})
                vim.fn.jobwait({id}, 2000)
                ",
                Vec::new(),
            )
            .unwrap();
        let mut nested_exited = Typval::Number(0);
        for _ in 0..500 {
            let _ = core
                .ex
                .borrow_mut()
                .flush_pty_output(&*core.session)
                .unwrap();
            let _ = deliver_deferred_job_events(&core.session, &core.ex).unwrap();
            nested_exited = core
                .ex
                .borrow_mut()
                .evaluate_expression(&*core.session, "g:nested_exited")
                .unwrap();
            if nested_exited == Typval::Number(1) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            nested_exited,
            Typval::Number(1),
            "the nested job's on_exit must fire: the re-entrant jobstart \
             must land on the primary executor, not the nested fallback"
        );
        assert!(
            core.ex.borrow_mut().take_deferred_job_events().is_empty(),
            "no deferred job events remain after both exits delivered"
        );
    }

    // A Lua on_stdout produced while a chansend drains its pty must reach
    // its handler on the borrow-free tick path: chansend only writes
    // (upstream `f_chansend`, funcs.c:649-694), so its poll re-defers what
    // it sweeps and the drain the caller drives afterwards delivers it.
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the fixture, script, and tick calls must succeed"
    )]
    fn lua_on_stdout_via_chansend_is_delivered_by_the_next_drain() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let lua = core.ex.borrow().lua_host().unwrap();
        lua.borrow_mut()
            .execute_chunk(
                r#"
                vim.g.stdout_seen = 0
                vim.g.chan_ok = 0
                local function on_stdout(id, data, event)
                  vim.g.stdout_seen = 1
                end
                local id = vim.fn.jobstart({'cat'}, {on_stdout = on_stdout})
                vim.g.chan_ok = vim.fn.chansend(id, "ping\n") > 0
                "#,
                Vec::new(),
            )
            .unwrap();
        let chan_ok = core
            .ex
            .borrow_mut()
            .evaluate_expression(&*core.session, "g:chan_ok")
            .unwrap();
        assert_eq!(
            chan_ok,
            Typval::Bool(true),
            "chansend to the primary job must write its bytes"
        );
        let mut stdout_seen = Typval::Number(0);
        for _ in 0..500 {
            let _ = core
                .ex
                .borrow_mut()
                .flush_pty_output(&*core.session)
                .unwrap();
            let _ = deliver_deferred_job_events(&core.session, &core.ex).unwrap();
            stdout_seen = core
                .ex
                .borrow_mut()
                .evaluate_expression(&*core.session, "g:stdout_seen")
                .unwrap();
            if stdout_seen == Typval::Number(1) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            stdout_seen,
            Typval::Number(1),
            "the Lua on_stdout swept by chansend's poll must be delivered \
             by the borrow-free drain, not dropped"
        );
    }

    // The tick driver keeps the hostless Lua event with the requeued tail:
    // a later host can still serve it (`invoke_job_events`' contract for
    // the flush paths; the old delivery dropped the offending event).
    #[test]
    #[expect(clippy::unwrap_used, reason = "the fixture must build")]
    fn hostless_lua_event_requeues_through_the_tick_driver() {
        let session = Rc::new(ApiSession::new(Rc::new(RefCell::new(Editor::new()))));
        let ex = Rc::new(RefCell::new(ExExecutor::new()));
        // A job manager must exist for the deferred queue; jobstart
        // installs one. The executor still has no Lua host.
        ex.borrow_mut()
            .execute_script(
                &*session,
                "<test>",
                "let g:probe = jobstart(['sh', '-c', 'exit 0'])",
            )
            .unwrap();
        let Typval::Dict(receiver) = Typval::dict(Vec::new()) else {
            unreachable!("Typval::dict builds a dict")
        };
        let event = JobEvent {
            callback: Typval::Funcref(Funcref {
                name: OxStr::from("probe"),
                args: Vec::new(),
                dict: None,
                registry: Some(42),
            }),
            receiver,
            args: Vec::new(),
        };
        ex.borrow_mut().defer_job_events(vec![event]);
        let error = deliver_deferred_job_events(&session, &ex).unwrap_err();
        assert!(error.contains("E5108"), "{error}");
        let requeued = ex.borrow_mut().take_deferred_job_events();
        assert_eq!(
            requeued.len(),
            1,
            "the hostless event must requeue, not drop"
        );
        assert!(
            matches!(&requeued[0].callback, Typval::Funcref(f) if f.registry == Some(42)),
            "the requeued event is the Lua-registered one"
        );
        // Redelivery of the requeued batch is stable: same report, same
        // requeue, no duplication.
        ex.borrow_mut().defer_job_events(requeued);
        assert!(deliver_deferred_job_events(&session, &ex).is_err());
        assert_eq!(ex.borrow_mut().take_deferred_job_events().len(), 1);
    }

    // The tick's borrow-free phase services the Lua work queue, so an idle
    // session (nothing arriving on RPC) still runs vim.schedule callbacks
    // and deferred channel sends - upstream's main-loop queue, not
    // message-arrival-driven.
    #[test]
    #[expect(clippy::unwrap_used, reason = "the fixture must build")]
    fn lua_work_queue_drains_without_an_rpc_message() {
        let core = build_embedded_core(Editor::new(), true).unwrap();
        let ran = Rc::new(std::cell::Cell::new(false));
        let flag = ran.clone();
        let order: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        let first = order.clone();
        let last = order.clone();
        core.lua_work
            .borrow_mut()
            .push_back(Box::new(move || -> Result<(), mlua::Error> {
                first.borrow_mut().push("failing");
                Err(mlua::Error::RuntimeError(
                    "scheduled work failed".to_owned(),
                ))
            }));
        core.lua_work
            .borrow_mut()
            .push_back(Box::new(move || -> Result<(), mlua::Error> {
                last.borrow_mut().push("ok");
                flag.set(true);
                Ok(())
            }));
        assert!(drain_lua_work_queue(&core.lua_work, &core.session));
        assert!(ran.get(), "the queued work item must run");
        assert_eq!(*order.borrow(), vec!["failing", "ok"]);
        assert!(
            core.lua_work.borrow().is_empty(),
            "the drain must empty the queue"
        );
        assert!(
            !drain_lua_work_queue(&core.lua_work, &core.session),
            "an empty queue reports no work"
        );
    }

    // Upstream runs a job's callbacks before `jobwait` returns
    // (`multiqueue_process_events`, funcs.c:3668/3721), so the first
    // borrow-free boundary after the builtin must deliver the Lua flush:
    // here an API `nvim_call_function` turn with the Lua host free. Pre-
    // hoist the flush waited for the 10 ms tick and this read zero.
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the fixture, chunk, and dispatch must succeed"
    )]
    fn lua_on_exit_flushed_by_jobwait_is_delivered_before_the_call_returns() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let (_, exec) = core.registry.get("nvim_exec_lua").unwrap();
        exec(
            &core.session,
            &[
                Object::String(OxStr::from(
                    r"
                    vim.g.flushed = 0
                    local function on_exit()
                      vim.g.flushed = 1
                    end
                    vim.g.id = vim.fn.jobstart({'sh', '-c', 'exit 0'}, {on_exit = on_exit})
                    ",
                )),
                Object::Array(Vec::new()),
            ],
        )
        .unwrap();
        let Typval::Number(id) = core
            .ex
            .borrow_mut()
            .evaluate_expression(&*core.session, "g:id")
            .unwrap()
        else {
            unreachable!("jobstart returns a channel id");
        };
        let (_, call) = core.registry.get("nvim_call_function").unwrap();
        call(
            &core.session,
            &[
                Object::String(OxStr::from("jobwait")),
                Object::Array(vec![
                    Object::Array(vec![Object::Integer(id)]),
                    Object::Integer(2000),
                ]),
            ],
        )
        .unwrap();
        assert_eq!(
            core.ex
                .borrow_mut()
                .evaluate_expression(&*core.session, "g:flushed")
                .unwrap(),
            Typval::Number(1),
            "the on_exit flushed by jobwait must run before the API call \
             returns, with no tick in between"
        );
    }

    // A TabLeave handler may close the destination tab's current window;
    // the enter events must then bind to the window promoted as the tab's
    // current (`enter_tabpage` reads `tp_curwin` after the switch,
    // window.c:4767), not to the buffer snapshotted before any handler ran.
    #[test]
    #[expect(clippy::unwrap_used, reason = "the fixture and chunk must succeed")]
    fn tab_enters_bind_to_the_promoted_window_after_a_leave_handler_closes_it() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let (_, exec) = core.registry.get("nvim_exec_lua").unwrap();
        exec(
            &core.session,
            &[
                Object::String(OxStr::from(
                    r#"
                    vim.g.entered = -1
                    local function step(n, f)
                      local ok, err = pcall(f)
                      if not ok then
                        error('step ' .. n .. ': ' .. tostring(err))
                      end
                    end
                    local t1 = vim.api.nvim_get_current_tabpage()
                    step(1, function() vim.cmd('tabnew') end)
                    local t2 = vim.api.nvim_get_current_tabpage()
                    vim.g.w2 = vim.api.nvim_get_current_win()
                    local promoted
                    step(2, function() promoted = vim.api.nvim_create_buf(false, true) end)
                    vim.g.promoted = promoted
                    step(3, function()
                      vim.api.nvim_open_win(promoted, false, {split = 'right'})
                    end)
                    step(4, function() vim.api.nvim_set_current_tabpage(t1) end)
                    step(5, function()
                      vim.api.nvim_create_autocmd('TabLeave',
                        {command = 'call nvim_win_close(g:w2, v:true)'})
                    end)
                    step(6, function()
                      vim.api.nvim_create_autocmd('TabEnter',
                        {command = 'let g:entered = str2nr(expand("<abuf>"))'})
                    end)
                    step(7, function() vim.api.nvim_set_current_tabpage(t2) end)
                    "#,
                )),
                Object::Array(Vec::new()),
            ],
        )
        .unwrap();
        let entered = core
            .ex
            .borrow_mut()
            .evaluate_expression(&*core.session, "g:entered")
            .unwrap();
        let promoted = core
            .ex
            .borrow_mut()
            .evaluate_expression(&*core.session, "g:promoted")
            .unwrap();
        assert_eq!(
            entered, promoted,
            "TabEnter must bind to the promoted window's buffer, not the \
             closed window's snapshot"
        );
    }

    // The CommandExecutor boundary (nvim_command/nvim_exec2/evaluate) now
    // delivers a pending jobwait Lua flush like the call_builtin boundary
    // does: a `:call jobwait(...)` from the API returns only after the Lua
    // on_exit ran (funcs.c:3668/3721).
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the fixture, chunk, and command must succeed"
    )]
    fn lua_on_exit_flushed_by_vimscript_jobwait_delivers_before_the_command_returns() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let core = build_embedded_core(editor, true).unwrap();
        let (_, exec) = core.registry.get("nvim_exec_lua").unwrap();
        exec(
            &core.session,
            &[
                Object::String(OxStr::from(
                    r"
                    vim.g.flushed = 0
                    local function on_exit()
                      vim.g.flushed = 1
                    end
                    vim.g.id = vim.fn.jobstart({'sh', '-c', 'exit 0'}, {on_exit = on_exit})
                    ",
                )),
                Object::Array(Vec::new()),
            ],
        )
        .unwrap();
        let (_, command) = core.registry.get("nvim_command").unwrap();
        command(
            &core.session,
            &[Object::String(OxStr::from("call jobwait([g:id], 2000)"))],
        )
        .unwrap();
        assert_eq!(
            core.ex
                .borrow_mut()
                .evaluate_expression(&*core.session, "g:flushed")
                .unwrap(),
            Typval::Number(1),
            "the on_exit flushed by a Vimscript jobwait must run before the \\
             command boundary returns, with no tick in between"
        );
    }

    /// A `ListenServer` over a real `AppState` runtime, with its own accept
    /// loop. Binds are synchronous, so start/stop/list run without ever
    /// turning the loop; `close_all` flushes the deferred unlinks.
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires editor and listener setup to succeed"
    )]
    fn listen_server() -> ListenServer {
        let cli = Cli::default();
        let state = Rc::new(RefCell::new(
            AppState::new(&cli, &mut StartupTimer::start()).unwrap(),
        ));
        let accept_uv = Rc::new(RefCell::new(UvLoop::new().unwrap()));
        let runtime = Rc::new(RefCell::new(NetworkRuntime::new(
            state,
            Rc::clone(&accept_uv),
        )));
        ListenServer::new(&runtime).unwrap()
    }

    /// A unique pipe path under the temp dir; tests never share names.
    fn pipe_path(label: &str) -> String {
        std::env::temp_dir()
            .join(format!("oxvim-w2b-{}.{}.sock", std::process::id(), label))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires listener setup to succeed"
    )]
    fn listen_server_pipe_start_stop_and_list_track_the_registry() {
        let mut server = listen_server();
        let path = pipe_path("registry");
        let bound = server.start(&path).unwrap();
        assert_eq!(bound, path);
        assert_eq!(server.list(), vec![path.clone()]);
        assert!(server.stop(&path), "stopping a live listener returns true");
        assert!(server.list().is_empty());
        assert!(!server.stop(&path), "stopping twice returns false");
        server.close_all();
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires listener setup to succeed"
    )]
    fn listen_server_rejects_empty_and_duplicate_addresses() {
        let mut server = listen_server();
        assert_eq!(
            server.start("").unwrap_err(),
            "Unknown system error",
            "empty address is the result > 0 branch (`server.c:168-171`)"
        );
        let path = pipe_path("duplicate");
        server.start(&path).unwrap();
        assert_eq!(
            server.start(&path).unwrap_err(),
            "Unknown system error",
            "duplicate watcher is the result 2 branch (`server.c:184-193`)"
        );
        server.close_all();
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires listener setup to succeed"
    )]
    fn listen_server_recovers_a_stale_pipe_file() {
        // First listener leaves a dead socket file behind (no unlink flush);
        // the probe-connect recovery (#36581) must remove and rebind it.
        let path = pipe_path("stale");
        {
            let mut first = listen_server();
            first.start(&path).unwrap();
            first.close_all();
        }
        let mut second = listen_server();
        assert_eq!(second.start(&path).unwrap(), path);
        assert_eq!(second.list(), vec![path]);
        second.close_all();
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the test requires listener setup to succeed"
    )]
    #[expect(clippy::expect_used, reason = "port parse failures must fail the test")]
    fn listen_server_tcp_ephemeral_port_reports_the_bound_address() {
        let mut server = listen_server();
        let bound = server.start("127.0.0.1:0").unwrap();
        let (_host, port) = bound.rsplit_once(':').expect("host:port form");
        let port: u16 = port.parse().expect("numeric port");
        assert!(port > 0, "ephemeral port must resolve to a real bind");
        assert_eq!(server.list(), vec![bound]);
        server.close_all();
    }
}
