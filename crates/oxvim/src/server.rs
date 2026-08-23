//! Embedded stdio and listening RPC servers.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;

use mlua::{Function, Lua, MultiValue, Table, Value, Variadic};
use ox_api::{CommandExecutor, Registry};
use ox_editor::{
    vim_variable_is_writable, AutocmdContext, AutocmdKind, CmdlineKind, Editor, Event, ExExecutor,
    ExecError, ExecOutcome, Geometry, LuaExec, LuaExecError, MessageDestination, MessageKind,
    Mode, ModeMachine, Keys,
    OptionValue, TypeaheadFlags,
};
use ox_lua::{
    ApiDispatchContext, BuiltinHost, LuaHost, RuntimeRoot as LuaRuntimeRoot, Scheduler, VariableHost, VariableScope, Work, bind_api,
    bind_variables, call_with_traceback, free_lua_ref, lua_to_object, lua_to_typval, object_to_lua,
    typval_to_lua,
};
use ox_rpc::{CHAN_STDIO, ChannelId, IncrementalDecoder, Message};
use ox_types::{ApiError, BufHandle, Dict, Object, OxStr, TabHandle, Typval, WinHandle};
use ox_ui::{
    ChromeState, CmdlineState as UiCmdlineState, Compositor, ContentChunk, Emitter, HlState,
    MessageState, UiChannels, UiOptions,
};
use ox_uv::{Handle, HandleId, NetEvent, RunMode, Tcp, UvLoop};
#[cfg(unix)]
use ox_uv::{Poll, PollEvents};
#[cfg(unix)]
use ox_uv::net::Pipe;

use crate::AppError;
use crate::cli::{Cli, UserConfig};
use crate::runtime::{apply_startup_options, open_startup_buffers, runtime_root};
use crate::messages::PrintfSink;
use crate::startuptime::StartupTimer;

#[derive(Default)]
struct TerminalChannelSink {
    output: BTreeMap<u64, Vec<u8>>,
}

impl ox_api::ChannelSink for TerminalChannelSink {
    fn send(&mut self, channel: u64, bytes: &[u8]) -> Result<(), String> {
        self.output.entry(channel).or_default().extend_from_slice(bytes);
        Ok(())
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
        let Ok(entries) = fs::read_dir(dir) else { return };
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
    let extension = |path: &Path, wanted: &str| {
        path.extension().is_some_and(|value| value == wanted)
    };
    let mut ordered: Vec<_> = found.iter().filter(|path| extension(path, "vim")).cloned().collect();
    ordered.extend(found.iter().filter(|path| extension(path, "lua")).cloned());
    ordered
}

/// All mutable state shared by every RPC transport.
pub struct AppState {
    editor: Rc<RefCell<Editor>>,
    lua: Rc<RefCell<LuaHost>>,
    registry: Rc<Registry>,
    ex: Rc<RefCell<ExExecutor>>,
    mode: ModeMachine,
    exiting: bool,
    /// Process exit code requested by `:cquit` (0 for plain quits).
    exit_code: i64,
    rendered_messages: usize,
    /// Stdout/stderr message output for the modes with no attached UI.
    printf: PrintfSink,
    lua_work: Rc<RefCell<VecDeque<Work>>>,
    ui_channels: UiChannels,
    emitter: Emitter,
    highlights: HlState,
    chrome: ChromeState,
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
                Geometry::new(0, 0, 80, 24)
                    .map_err(|error| AppError::Editor(error.to_string()))?,
            )
            .map_err(|error| AppError::Editor(error.to_string()))?;
        editor.vvars_mut().insert(
            OxStr::from("servername"),
            Object::String(OxStr::from("")),
        );
        apply_startup_options(&mut editor, cli)?;
        // option.c set_init_default for 'runtimepath'/'packpath': the
        // runtimepath_default layout over the resolved runtime tree,
        // before any user startup command runs.
        let runtime_path = runtime_root()?;
        let default_rtp = ox_editor::default_runtimepath(cli.clean, &runtime_path);
        editor
            .options_mut()
            .set_global("runtimepath", OptionValue::String(default_rtp.clone()))
            .map_err(|error| AppError::Editor(error.to_string()))?;
        editor
            .options_mut()
            .set_global("packpath", OptionValue::String(default_rtp.clone()))
            .map_err(|error| AppError::Editor(error.to_string()))?;
        let editor = Rc::new(RefCell::new(editor));
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
        let channel_ids = editor.borrow().channel_ids();
        ex.borrow_mut().set_channel_ids(channel_ids.clone());
        nested_ex.borrow_mut().set_channel_ids(channel_ids);
        let mut lua = LuaHost::new(
            LuaRuntimeRoot::new(runtime_path),
            Rc::new(EditorBuiltins {
                editor: editor.clone(),
                ex: ex.clone(),
                nested_ex: nested_ex.clone(),
                nested_editor: Rc::new(RefCell::new(Editor::new())),
            }),
            Rc::new(LuaScheduler { queue: lua_work.clone() }),
        )
        .map_err(|error| AppError::Lua(error.to_string()))?;
        bind_api(
            lua.lua(),
            &registry,
            ApiDispatchContext::new(editor.clone()),
            lua.fast_callbacks(),
        )
        .map_err(|error| AppError::Lua(error.to_string()))?;
        bind_variables(lua.lua(), Rc::new(EditorVariables { editor: editor.clone() }))
            .map_err(|error| AppError::Lua(error.to_string()))?;
        ox_api::set_channel_sink(&editor.borrow(), Box::new(TerminalChannelSink::default()));

        // Load the reachable embedded core prelude before user-controlled Ex startup commands.
        lua.exec("require('vim._core.shared')", Vec::new())
            .map_err(|error| AppError::Lua(error.to_string()))?;
        let callback_lua = lua.lua().clone();
        let lua = Rc::new(RefCell::new(lua));
        let callback_host = || Rc::new(RefCell::new(ServerLuaExec {
            lua: callback_lua.clone(),
            registry: registry.clone(),
        }));
        ex.borrow_mut().set_lua_exec(callback_host());
        nested_ex.borrow_mut().set_lua_exec(callback_host());

        let mut state = Self {
            editor,
            lua,
            registry,
            ex,
            mode: ModeMachine::default(),
            exiting: false,
            exit_code: 0,
            rendered_messages: 0,
            printf: PrintfSink::default(),
            lua_work,
            ui_channels: UiChannels::new(),
            emitter: Emitter::new(),
            highlights: HlState::new(),
            chrome: ChromeState::new(),
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
            self.editor
                .borrow_mut()
                .arglist_mut()
                .set(cli.files.iter().map(|file| OxStr::from(file.as_str())).collect());
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
            self.load_plugins()?;
        }
        timer.mark("loading plugins");

        // main.c create_windows()/edit_buffers(): the requested window or
        // tab-page layout is built first, then every positional file becomes
        // a named buffer loaded from disk (upstream also names a buffer when
        // the file does not exist yet) and fills one window in argv order.
        if self.exiting {
            return Ok(());
        }
        open_startup_buffers(&mut self.editor.borrow_mut(), cli)?;
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
            .execute_script(&mut self.editor.borrow_mut(), &name, &source)
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
                    self.editor.borrow_mut().push_message(ox_editor::Message {
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
        let Some(value) = std::env::var_os(name) else { return Ok(false) };
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
    fn load_plugins(&mut self) -> Result<(), AppError> {
        let rtp = match self.editor.borrow().options().get_global("runtimepath") {
            Ok(OptionValue::String(value)) => value.clone(),
            _ => return Ok(()),
        };
        let (after, plain): (Vec<&str>, Vec<&str>) = rtp
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .partition(|entry| Path::new(entry).file_name().is_some_and(|name| name == "after"));
        for entry in plain.into_iter().chain(after) {
            for script in plugin_scripts(Path::new(entry)) {
                // `source_callback_vim_lua` (runtime.c:371-396) discards
                // `do_source`'s result and sources the next file, so an error
                // inside one plugin ends that plugin and nothing else. One
                // broken plugin must not be able to stop startup -- with the
                // error propagated instead, `runtime/plugin/gzip.vim` took the
                // whole editor down on every plain startup.
                if let Err(error) = self.source_config_file(&script) {
                    self.editor.borrow_mut().push_message(ox_editor::Message {
                        kind: MessageKind::Error,
                        content: Object::String(OxStr::from(
                            format!("{}: {error}", script.display()).as_str(),
                        )),
                        history: true,
                    });
                }
                if self.exiting {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn execute_ex(&mut self, command: &str) -> Result<(), AppError> {
        let outcome = self
            .ex
            .borrow_mut()
            .execute_line(&mut self.editor.borrow_mut(), command)
            .map_err(|error| AppError::Ex(error.to_string()))?;
        if let ExecOutcome::Quit(code) = outcome {
            self.exiting = true;
            self.exit_code = code;
        }
        Ok(())
    }

    fn fire_vim_enter(&mut self) -> Result<(), AppError> {
        let plan = self
            .editor
            .borrow_mut()
            .autocmds_mut()
            .plan(Event::VimEnter, AutocmdContext::default());
        for action in plan.ready {
            if action.once {
                self.editor.borrow_mut().autocmds_mut().consume_once(action.id);
            }
            match action.kind {
                AutocmdKind::ExString(command) => self.execute_ex(&command)?,
                AutocmdKind::LuaCallback(reference) => {
                    let lua = self.lua.borrow();
                    let reference = i32::try_from(reference)
                        .map_err(|_| AppError::Lua("autocmd Lua reference is out of range".into()))?;
                    let value = object_to_lua(lua.lua(), &Object::LuaRef(reference))
                        .map_err(|error| AppError::Lua(error.to_string()))?;
                    let Value::Function(function) = value else {
                        return Err(AppError::Lua("autocmd Lua reference is not a function".into()));
                    };
                    call_with_traceback(lua.lua(), &function, MultiValue::new())
                        .map_err(|error| AppError::Lua(error.to_string()))?;
                }
            }
        }
        Ok(())
    }

    fn dispatch(
        &mut self,
        channel: ChannelId,
        method: &OxStr,
        params: &[Object],
    ) -> Result<(Object, BTreeMap<u64, Vec<u8>>), ApiError> {
        let name = method.to_string_lossy();
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
                    return Err(ApiError::exception(format!("Invalid method: {name}")));
                };
                dispatch(&mut self.editor.borrow_mut(), params)
            }
        }?;
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
        let mut result = dispatch(&mut self.editor.borrow_mut(), params)?;
        let Object::Array(info) = &mut result else {
            return Err(ApiError::exception("invalid API metadata response"));
        };
        let Some(id) = info.first_mut() else {
            return Err(ApiError::exception("invalid API metadata response"));
        };
        *id = Object::Integer(channel.get() as i64);
        Ok(result)
    }

    fn dispatch_call_atomic(
        &mut self,
        channel: ChannelId,
        params: &[Object],
    ) -> Result<Object, ApiError> {
        let [Object::Array(calls)] = params else {
            return Err(ApiError::validation("nvim_call_atomic expects one Array argument"));
        };
        let mut results = Vec::with_capacity(calls.len());
        for (index, call) in calls.iter().enumerate() {
            let Object::Array(call) = call else {
                return Err(ApiError::validation("each call must be an Array"));
            };
            let (Some(Object::String(name)), Some(Object::Array(args))) = (call.first(), call.get(1)) else {
                return Err(ApiError::validation("each call must contain name and arguments"));
            };
            let name = name.to_string_lossy();
            let result = match name.as_ref() {
                "nvim_get_api_info" => self.dispatch_api_info(channel, args),
                "nvim_call_atomic" => self.dispatch_call_atomic(channel, args),
                _ => match self.registry.get(&name) {
                    Some((_, dispatch)) => dispatch(&mut self.editor.borrow_mut(), args),
                    None => Err(ApiError::validation(format!("Invalid method: {name}"))),
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
            return Err(ApiError::validation("nvim_input expects one String argument"));
        };
        let count = i64::try_from(input.as_bytes().len())
            .map_err(|_| ApiError::exception("Input length exceeds Integer range"))?;
        let Some((_, replace)) = self.registry.get("nvim_replace_termcodes") else {
            return Err(ApiError::exception("nvim_replace_termcodes is not registered"));
        };
        let replaced = replace(
            &mut self.editor.borrow_mut(),
            &[
                Object::String(input.clone()),
                Object::Boolean(false),
                Object::Boolean(true),
                Object::Boolean(true),
            ],
        )?;
        let Object::String(encoded) = replaced else {
            return Err(ApiError::exception("nvim_replace_termcodes returned a non-string"));
        };
        let keys = Keys::from_encoded(encoded.as_bytes().to_vec())
            .map_err(|error| ApiError::exception(error.to_string()))?;
        self.editor.borrow_mut().typeahead_mut().append(&keys, TypeaheadFlags::default());
        Ok(Object::Integer(count))
    }

    fn dispatch_lua(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::String(code), Object::Array(args)] = params else {
            return Err(ApiError::validation("nvim_exec_lua expects (String, Array)"));
        };
        let code = std::str::from_utf8(code.as_bytes())
            .map_err(|_| ApiError::validation("Lua source must be valid UTF-8"))?;
        self.lua
            .borrow_mut()
            .exec(code, args.clone())
            .map_err(|error| ApiError::exception(error.to_string()))
    }

    fn dispatch_command(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::String(command)] = params else {
            return Err(ApiError::validation("nvim_command expects one String argument"));
        };
        let command = std::str::from_utf8(command.as_bytes())
            .map_err(|_| ApiError::validation("Ex command must be valid UTF-8"))?;
        let outcome = self.ex
            .borrow_mut()
            .execute_line(&mut self.editor.borrow_mut(), command)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        if let ExecOutcome::Quit(code) = outcome {
            self.exiting = true;
            self.exit_code = code;
        }
        Ok(Object::Nil)
    }

    fn dispatch_nvim_cmd(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let (cmd, opts) = nvim_cmd_args(params)?;
        let mut ex = self.ex.borrow_mut();
        let mut executor = ExApiExecutor { executor: &mut ex, outcome: ExecOutcome::Completed };
        let result = ox_api::execute_nvim_cmd(&mut self.editor.borrow_mut(), cmd, opts, &mut executor)?;
        if let ExecOutcome::Quit(code) = executor.outcome {
            self.exiting = true;
            self.exit_code = code;
        }
        Ok(Object::String(result))
    }

    fn ui_attach(&mut self, channel: ChannelId, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::Integer(width), Object::Integer(height), Object::Dict(options)] = params else {
            return Err(ApiError::validation("nvim_ui_attach expects (Integer, Integer, Dict)"));
        };
        let width = positive_dimension(*width, "width")?;
        let height = positive_dimension(*height, "height")?;
        let mut options = options.clone();
        // RGB is the historical default protocol request.  ox-ui implements
        // the modern linegrid protocol only, so RGB implies that supported
        // representation rather than falling back to a legacy cell protocol.
        if matches!(options.get(&OxStr::from("rgb")), Some(Object::Boolean(true)))
            && options.get(&OxStr::from("ext_linegrid")).is_none()
        {
            options.0.push((OxStr::from("ext_linegrid"), Object::Boolean(true)));
        }
        self.ui_channels
            .attach(channel.get(), width, height, UiOptions::from_dict(&options))
            .map_err(|error| ApiError::exception(error.to_string()))?;
        self.sync_ui_active();
        Ok(Object::Nil)
    }

    fn ui_detach(&mut self, channel: ChannelId, params: &[Object]) -> Result<Object, ApiError> {
        if !params.is_empty() {
            return Err(ApiError::validation("nvim_ui_detach expects no arguments"));
        }
        self.ui_channels
            .detach(channel.get())
            .map_err(|error| ApiError::exception(error.to_string()))?;
        self.emitter.detach(channel.get());
        self.sync_ui_active();
        Ok(Object::Nil)
    }

    /// Mirrors `ui_active()` into the message sink: `msg_use_printf`
    /// (`message.c` line 3013) stops printing as soon as a UI can display the
    /// text, and starts again when the last one detaches.
    fn sync_ui_active(&mut self) {
        let attached = self.ui_channels.iter().next().is_some();
        self.editor.borrow_mut().message_routing.ui_attached = attached;
    }

    fn ui_resize(&mut self, channel: ChannelId, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::Integer(width), Object::Integer(height)] = params else {
            return Err(ApiError::validation("nvim_ui_try_resize expects (Integer, Integer)"));
        };
        self.ui_channels
            .try_resize(
                channel.get(),
                positive_dimension(*width, "width")?,
                positive_dimension(*height, "height")?,
            )
            .map_err(|error| ApiError::exception(error.to_string()))?;
        Ok(Object::Nil)
    }

    fn redraw(&mut self) -> Result<BTreeMap<u64, Vec<u8>>, ApiError> {
        self.sync_chrome();
        self.publish_messages()
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let (width, height) = self
            .ui_channels
            .iter()
            .map(|(_, channel)| channel.size())
            .fold((1, 1), |(max_width, max_height), (width, height)| {
                (max_width.max(width), max_height.max(height))
            });
        let compositor = Compositor::from_editor(&self.editor.borrow(), width, height)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        self.emitter
            .redraw(
                &mut self.ui_channels,
                &compositor,
                &mut self.highlights,
                &mut self.chrome,
            )
            .map_err(|error| ApiError::exception(error.to_string()))
    }

    fn sync_chrome(&mut self) {
        match self.mode.mode() {
            Mode::Cmdline(state) => {
                let first_char = match state.kind {
                    CmdlineKind::Search(ox_editor::SearchDirection::Forward) => "/",
                    CmdlineKind::Search(ox_editor::SearchDirection::Backward) => "?",
                    CmdlineKind::Ex => ":",
                };
                self.chrome.show_cmdline(UiCmdlineState {
                    content: vec![ContentChunk::new(0, state.text.as_str())],
                    position: state.text.len(),
                    first_char: OxStr::from(first_char),
                    prompt: OxStr::from(""),
                    indent: 0,
                    level: 1,
                    hl_id: 0,
                });
            }
            _ => self.chrome.hide_cmdline(1, false),
        }
    }

    /// Sends every newly retained message where the editor sink decided it
    /// goes: an attached UI, stdout, stderr, or nowhere.
    fn publish_messages(&mut self) -> Result<(), AppError> {
        let pending: Vec<(ox_editor::Message, MessageDestination)> = {
            let editor = self.editor.borrow();
            let from = self.rendered_messages;
            editor.messages()[from..]
                .iter()
                .cloned()
                .zip(editor.message_destinations()[from..].iter().copied())
                .collect()
        };
        self.rendered_messages += pending.len();
        for (message, destination) in &pending {
            if *destination == MessageDestination::Ui {
                self.show_in_chrome(message);
            } else {
                self.printf.write(*destination, message).map_err(AppError::Io)?;
            }
        }
        Ok(())
    }

    /// `getout` (`main.c`:753) for an exit this host decided on rather than a
    /// command: the peer closed its write side, or the loop was stopped. The
    /// executor's own sequence is idempotent, so an exit a `:quit` already
    /// carried through fires nothing a second time. Anything the handlers emit
    /// is published, as upstream's exit messages are.
    fn run_exit(&mut self) -> Result<(), AppError> {
        {
            let mut editor = self.editor.borrow_mut();
            self.ex
                .borrow_mut()
                .run_exit_sequence(&mut editor)
                .map_err(|error| AppError::Ex(error.to_string()))?;
        }
        self.publish_messages()
    }

    fn show_in_chrome(&mut self, message: &ox_editor::Message) {
        let text = match &message.content {
            Object::String(text) => text.clone(),
            value => OxStr::from(format!("{value:?}").as_bytes()),
        };
        self.chrome.show_message(MessageState {
            kind: OxStr::from(if message.kind == MessageKind::Error { "emsg" } else { "echo" }),
            content: vec![ContentChunk::new(0, text)],
            replace_last: false,
            history: message.history,
            append: false,
            id: Object::Nil,
            trigger: OxStr::from(""),
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
        let outcome = self
            .ex
            .borrow_mut()
            .run_typeahead(&mut self.editor.borrow_mut(), &mut self.mode)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        if let ExecOutcome::Quit(code) = outcome {
            self.exiting = true;
            self.exit_code = code;
        }
        Ok(())
    }

    fn drain_lua_work(&mut self) -> Result<(), AppError> {
        loop {
            let work = self.lua_work.borrow_mut().pop_front();
            let Some(work) = work else { return Ok(()) };
            work().map_err(|error| AppError::Lua(error.to_string()))?;
        }
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

    fn process_message(
        &mut self,
        channel: ChannelId,
        message: Message,
    ) -> Result<Vec<(u64, Vec<u8>)>, AppError> {
        let mut writes = Vec::new();
        match message {
            Message::Request { msgid, method, params } => {
                let is_input = method.as_bytes() == b"nvim_input" || method.as_bytes() == b"nvim_feedkeys";
                let is_ui_attach = method.as_bytes() == b"nvim_ui_attach";
                let owns_result_refs = allocates_result_refs(method.as_bytes());
                let dispatched = self.dispatch(channel, &method, &params);
                let (result, mut redraws) = match dispatched {
                    Ok((result, redraws)) => (Ok(result), redraws),
                    Err(error) => (Err(error), BTreeMap::new()),
                };
                if result.is_ok() && is_input {
                    match self.drive_input() {
                        Ok(()) => redraws = self.redraw().map_err(|error| AppError::Api(error.to_string()))?,
                        Err(error) => {
                            let message = error.message().to_owned();
                            self.editor.borrow_mut().push_message(ox_editor::Message {
                                kind: MessageKind::Error,
                                content: Object::String(OxStr::from(message.as_str())),
                                history: true,
                            });
                            redraws = self.redraw().map_err(|error| AppError::Api(error.to_string()))?;
                            writes.push((channel.get(), Message::Response { msgid, result: Err(error) }.encode_bytes()));
                            if owns_result_refs
                                && let Ok(value) = &result
                            {
                                self.free_reply_refs(value);
                            }
                            writes.extend(redraws);
                            self.drain_lua_work()?;
                            return Ok(writes);
                        }
                    }
                }
                let response = Message::Response { msgid, result };
                let encoded = response.encode_bytes();
                if owns_result_refs
                    && let Message::Response { result: Ok(value), .. } = &response
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
                let is_input = method.as_bytes() == b"nvim_input" || method.as_bytes() == b"nvim_feedkeys";
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
                                Ok(()) => redraws = self.redraw().map_err(|error| AppError::Api(error.to_string()))?,
                                Err(error) => {
                                    let message = error.message().to_owned();
                                    self.editor.borrow_mut().push_message(ox_editor::Message {
                                        kind: MessageKind::Error,
                                        content: Object::String(OxStr::from(message.as_str())),
                                        history: true,
                                    });
                                    redraws = self.redraw().map_err(|error| AppError::Api(error.to_string()))?;
                                    writes.push((channel.get(), ox_rpc::nvim_error_event(&error)));
                                    writes.extend(redraws);
                                    self.drain_lua_work()?;
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
        self.drain_lua_work()?;
        Ok(writes)
    }

    fn should_exit(&self) -> bool { self.exiting }

    /// Process exit code requested so far (`:cquit`, else 0).
    fn exit_code(&self) -> i64 { self.exit_code }
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
    let mut state = AppState::new(cli, timer)?;
    let mut decoder = IncrementalDecoder::new();
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut bytes = [0_u8; 8192];

    // main.c getout(): a startup command that quits ends the process before
    // the input loop starts, so `--headless -c 'echo x' -c 'qall!'` never
    // waits for a peer to close stdin.
    if state.should_exit() {
        state.run_exit()?;
        return Ok(state.exit_code());
    }

    loop {
        let count = input.read(&mut bytes).map_err(AppError::Io)?;
        if count == 0 { break; }
        let messages = decoder
            .feed(&bytes[..count])
            .map_err(|error| AppError::Server(error.to_string()))?;
        for message in messages {
            for (channel, bytes) in state.process_message(CHAN_STDIO, message)? {
                if channel == CHAN_STDIO.get() {
                    output.write_all(&bytes).map_err(AppError::Io)?;
                }
            }
        }
        output.flush().map_err(AppError::Io)?;
        if state.should_exit() { break; }
    }
    state.run_exit()?;
    Ok(state.exit_code())
}

/// Serve RPC peers accepted from a TCP address or Unix-domain pipe.
/// Returns the process exit code requested by `:cquit` (0 otherwise).
pub fn run_listener(cli: &Cli, address: &str, timer: &mut StartupTimer) -> Result<i64, AppError> {
    let state = Rc::new(RefCell::new(AppState::new(cli, timer)?));
    let runtime = Rc::new(RefCell::new(NetworkRuntime::new(state)));
    let mut uv_loop = UvLoop::new().map_err(|error| AppError::Server(error.to_string()))?;
    let callback_runtime = runtime.clone();
    let callback = move |uv_loop: &mut UvLoop, id: HandleId, event: NetEvent| {
        handle_network_event(&callback_runtime, uv_loop, id, event);
    };

    let listener = if let Ok(socket) = address.parse::<SocketAddr>() {
        let mut listener = Tcp::bind(&mut uv_loop, socket, callback)
            .map_err(|error| AppError::Server(error.to_string()))?;
        listener
            .listen(&mut uv_loop, 128)
            .map_err(|error| AppError::Server(error.to_string()))?;
        Listener::Tcp(listener)
    } else {
        bind_pipe(&mut uv_loop, address, callback)?
    };
    let servername = listener.servername()?;
    let state = runtime.borrow().state.clone();
    state.borrow().editor.borrow_mut().vvars_mut().insert(
        OxStr::from("servername"),
        Object::String(OxStr::from(servername.as_str())),
    );
    #[cfg(unix)]
    let stdio_poll = cli.embed.then(|| bind_stdio(&mut uv_loop, runtime.clone())).transpose()?;
    #[cfg(not(unix))]
    if cli.embed {
        return Err(AppError::Server("--embed with --listen is unsupported on this platform".into()));
    }

    let run_result = uv_loop
        .run(RunMode::Default)
        .map_err(|error| AppError::Server(error.to_string()));
    #[cfg(unix)]
    let stdio_close_result = stdio_poll
        .map(|poll| poll.close(&mut uv_loop).map_err(|error| AppError::Server(error.to_string())))
        .transpose();
    let close_result = listener
        .close(&mut uv_loop)
        .map_err(|error| AppError::Server(error.to_string()));
    run_result?;
    #[cfg(unix)]
    stdio_close_result?;
    if let Some(error) = runtime.borrow_mut().error.take() {
        return Err(AppError::Server(error));
    }
    close_result?;
    state.borrow_mut().run_exit()?;
    Ok(state.borrow().exit_code())
}

#[cfg(unix)]
fn bind_stdio(uv_loop: &mut UvLoop, runtime: Rc<RefCell<NetworkRuntime>>) -> Result<Poll, AppError> {
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
                let mut bytes = [0; 64 * 1024];
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
                            for (channel, bytes) in state.borrow_mut().process_message(CHAN_STDIO, message)? {
                                if channel == CHAN_STDIO.get() {
                                    output.write_all(&bytes).map_err(AppError::Io)?;
                                }
                            }
                            if state.borrow().should_exit() {
                                uv_loop.stop();
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
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

#[allow(dead_code)]
enum Listener {
    Tcp(Tcp),
    #[cfg(unix)]
    Pipe(Pipe),
}

impl Listener {
    fn servername(&self) -> Result<String, AppError> {
        match self {
            Self::Tcp(listener) => listener
                .local_addr()
                .map(|address| address.to_string())
                .map_err(|error| AppError::Server(error.to_string())),
            #[cfg(unix)]
            Self::Pipe(listener) => listener
                .local_name()
                .map_err(|error| AppError::Server(error.to_string()))?
                .map(|path| path.to_string_lossy().into_owned())
                .ok_or_else(|| AppError::Server("bound pipe has no local name".into())),
        }
    }

    fn close(&self, uv_loop: &mut UvLoop) -> Result<(), ox_uv::Error> {
        match self {
            Self::Tcp(listener) => listener.close(uv_loop),
            #[cfg(unix)]
            Self::Pipe(listener) => listener.close(uv_loop),
        }
    }
}

#[cfg(unix)]
fn bind_pipe<F>(uv_loop: &mut UvLoop, address: &str, callback: F) -> Result<Listener, AppError>
where
    F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static,
{
    let mut listener = Pipe::bind(uv_loop, address, callback)
        .map_err(|error| AppError::Server(error.to_string()))?;
    listener
        .listen(uv_loop, 128)
        .map_err(|error| AppError::Server(error.to_string()))?;
    Ok(Listener::Pipe(listener))
}

#[cfg(not(unix))]
fn bind_pipe<F>(_uv_loop: &mut UvLoop, _address: &str, _callback: F) -> Result<Listener, AppError>
where
    F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static,
{
    Err(AppError::Server(
        "Unix-domain --listen addresses are unsupported on this platform".into(),
    ))
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
    peers: HashMap<HandleId, Peer>,
    streams: HashMap<HandleId, Stream>,
    error: Option<String>,
}

impl NetworkRuntime {
    fn new(state: Rc<RefCell<AppState>>) -> Self {
        Self {
            state,
            peers: HashMap::new(),
            streams: HashMap::new(),
            error: None,
        }
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
            self.state.borrow_mut().editor.borrow_mut().allocate_channel_id(),
        );
        self.peers.insert(id, Peer {
            channel,
            decoder: IncrementalDecoder::new(),
        });
        self.streams.insert(id, stream);
        Ok(())
    }

    fn read(&mut self, uv_loop: &mut UvLoop, id: HandleId, bytes: &[u8]) -> Result<(), String> {
        let (channel, messages) = {
            let peer = self.peers.get_mut(&id).ok_or_else(|| "read from unknown RPC peer".to_owned())?;
            let messages = peer.decoder.feed(bytes).map_err(|error| error.to_string())?;
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
            if self.state.borrow().should_exit() { uv_loop.stop(); }
        }
        Ok(())
    }

    fn remove_peer(&mut self, uv_loop: &mut UvLoop, id: HandleId) {
        self.peers.remove(&id);
        if let Some(stream) = self.streams.remove(&id) {
            let _ = stream.close(uv_loop);
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
        NetEvent::AcceptedPipe(stream) => runtime.borrow_mut().accept(uv_loop, Stream::Pipe(*stream)),
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
            uv_loop.stop();
        }
    }
}

struct EditorBuiltins {
    editor: Rc<RefCell<Editor>>,
    ex: Rc<RefCell<ExExecutor>>,
    nested_ex: Rc<RefCell<ExExecutor>>,
    nested_editor: Rc<RefCell<Editor>>,
}

impl EditorBuiltins {
    /// Serve `name` through the outermost executor and editor that are free.
    ///
    /// The first tier is the real pair, and it is the tier `vim.fn` reaches
    /// from a Lua config file, an RPC request, a scheduled callback or an
    /// autocommand -- everywhere a plugin runs. The inner tiers exist because
    /// Lua can also be reached from *inside* Ex execution (`:lua`, `lua <<EOF`
    /// in an init.vim), which already holds both; a nested executor over a
    /// scratch editor still answers every builtin that needs no editor state,
    /// and the alternative here was a `RefCell already borrowed` panic.
    fn dispatch(&self, name: &OxStr, args: Vec<Typval>) -> Result<Typval, ExecError> {
        if let Ok(mut ex) = self.ex.try_borrow_mut() {
            if let Ok(mut editor) = self.editor.try_borrow_mut() {
                return ex.call_builtin(&mut editor, name, args);
            }
        }
        if let Ok(mut ex) = self.nested_ex.try_borrow_mut() {
            if let Ok(mut editor) = self.nested_editor.try_borrow_mut() {
                return ex.call_builtin(&mut editor, name, args);
            }
        }
        ExExecutor::new().call_builtin(&mut Editor::new(), name, args)
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
}

/// Lua variable access backed by the editor's canonical API dictionaries.
pub(crate) struct EditorVariables {
    pub(crate) editor: Rc<RefCell<Editor>>,
}

impl VariableHost for EditorVariables {
    fn get_var(
        &self,
        scope: VariableScope,
        handle: i64,
        name: &OxStr,
    ) -> Result<Option<Object>, String> {
        let editor = self.editor.borrow();
        let variables = variables(&editor, scope, handle)?;
        Ok(variables.get(name).cloned())
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
        let mut editor = self.editor.borrow_mut();
        let variables = variables_mut(&mut editor, scope, handle)?;
        if let Some(value) = value {
            variables.insert(name, value);
        } else {
            let index = variables.iter().position(|(key, _)| key == &name);
            if let Some(index) = index {
                variables.0.remove(index);
            }
        }
        Ok(())
    }
}

fn variables(editor: &Editor, scope: VariableScope, handle: i64) -> Result<&Dict, String> {
    match scope {
        VariableScope::Global => Ok(editor.gvars()),
        VariableScope::Vim => Ok(editor.vvars()),
        VariableScope::Buffer => editor
            .buffer(BufHandle::try_from(handle).map_err(|error| error.to_string())?)
            .map(|buffer| buffer.variables())
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
            .map(|buffer| buffer.variables_mut())
            .map_err(|error| error.to_string()),
        VariableScope::Window => editor
            .window_variables_mut(WinHandle::try_from(handle).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string()),
        VariableScope::Tabpage => editor
            .tabpage_variables_mut(TabHandle::try_from(handle).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string()),
    }
}

struct ServerLuaExec {
    lua: Lua,
    registry: Rc<Registry>,
}

impl LuaExec for ServerLuaExec {
    fn execute_chunk(
        &mut self,
        editor: &mut Editor,
        code: &str,
        args: Vec<Object>,
    ) -> Result<Object, LuaExecError> {
        let lua = &self.lua;
        with_scoped_editor_api(lua, &self.registry, editor, || {
            let function = lua
                .load(code)
                .set_name("<nvim>")
                .into_function()
                .map_err(|error| LuaExecError::Load(error.to_string()))?;
            let lua_args = args
                .iter()
                .map(|arg| object_to_lua(lua, arg).map_err(|error| LuaExecError::Conversion(error.to_string())))
                .collect::<Result<Vec<_>, _>>()?
                .into();
            let mut results = call_with_traceback(lua, &function, lua_args)
                .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
            results.pop_front().map_or(Ok(Object::Nil), |value| {
                let object = lua_to_object(lua, &value).map_err(|error| LuaExecError::Conversion(error.to_string()))?;
                // The Ex executor discards non-scalar Lua results (`:lua`
                // prints inside Lua through `vim._print`, and `:luado` only
                // consumes strings and numbers — upstream passes kRetNilBool
                // here), so the converted references are released at once
                // instead of leaking one registry slot per call.
                free_object_refs(lua, &object);
                Ok(object)
            })
        })
    }

    fn execute_file(&mut self, editor: &mut Editor, path: &Path) -> Result<(), LuaExecError> {
        let lua = &self.lua;
        with_scoped_editor_api(lua, &self.registry, editor, || {
            let loadfile: Function = lua.globals().get("loadfile")
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
                (Some(value), _) => Err(LuaExecError::Load(format!("loadfile returned {value:?}"))),
                (None, _) => Err(LuaExecError::Load("loadfile returned no values".to_owned())),
            }
        })
    }

    fn invoke_callback(
        &mut self,
        _editor: &mut Editor,
        reference: usize,
        args: Vec<Object>,
    ) -> Result<(), LuaExecError> {
        let reference = i32::try_from(reference)
            .map_err(|_| LuaExecError::Conversion("Lua callback reference is out of range".to_owned()))?;
        let lua = &self.lua;
        let value = object_to_lua(lua, &Object::LuaRef(reference))
            .map_err(|error| LuaExecError::Conversion(error.to_string()))?;
        let Value::Function(function) = value else {
            return Err(LuaExecError::Conversion("Lua callback reference is not a function".to_owned()));
        };
        let args = args.iter()
            .map(|argument| object_to_lua(lua, argument).map_err(|error| LuaExecError::Conversion(error.to_string())))
            .collect::<Result<Vec<_>, _>>()?
            .into();
        call_with_traceback(lua, &function, args)
            .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
        Ok(())
    }

    fn eval_expression(
        &mut self,
        editor: &mut Editor,
        expression: &str,
        arg: Option<&Typval>,
    ) -> Result<Typval, LuaExecError> {
        let lua = &self.lua;
        with_scoped_editor_api(lua, &self.registry, editor, || {
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
            let mut results = call_with_traceback(lua, &function, MultiValue::from_vec(vec![argument]))
                .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
            let value = results.pop_front().unwrap_or(Value::Nil);
            lua_to_typval(lua, &value)
                .map_err(|error| LuaExecError::Conversion(error.to_string()))
        })
    }
}

struct ExApiExecutor<'a> {
    executor: &'a mut ExExecutor,
    outcome: ExecOutcome,
}

impl CommandExecutor for ExApiExecutor<'_> {
    fn execute(&mut self, editor: &mut Editor, commands: &[ox_api::ExCommand]) -> Result<(), ApiError> {
        self.outcome = self.executor
            .execute_commands(editor, commands)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        Ok(())
    }
}

fn with_scoped_editor_api<T>(
    lua: &Lua,
    registry: &Registry,
    editor: &mut Editor,
    run: impl FnOnce() -> Result<T, LuaExecError>,
) -> Result<T, LuaExecError> {
    let vim: Table = lua.globals().get("vim").map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let api: Table = vim.get("api").map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let original_getvar: Value = vim.get("_getvar").map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let original_setvar: Value = vim.get("_setvar").map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let originals = registry
        .iter()
        .map(|(metadata, _)| api.get::<Value>(metadata.name).map(|value| (metadata.name, value)))
        .collect::<mlua::Result<Vec<_>>>()
        .map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    let editor = Rc::new(RefCell::new(editor));
    let result = lua.scope(|scope| {
        let get_editor = editor.clone();
        vim.set(
            "_getvar",
            scope.create_function_mut(move |lua, (scope, handle, name): (mlua::LuaString, i64, mlua::LuaString)| {
                let scope = parse_variable_scope(&scope)?;
                let name = OxStr(name.as_bytes().to_vec());
                let editor = get_editor.borrow();
                match variables(&editor, scope, handle).map_err(mlua::Error::runtime)?.get(&name) {
                    Some(value) => object_to_lua(lua, value).map_err(mlua::Error::external),
                    None => Ok(Value::Nil),
                }
            })?,
        )?;
        let set_editor = editor.clone();
        vim.set(
            "_setvar",
            scope.create_function_mut(move |lua, (scope, handle, name, value): (mlua::LuaString, i64, mlua::LuaString, Value)| {
                let scope = parse_variable_scope(&scope)?;
                let name = OxStr(name.as_bytes().to_vec());
                if scope == VariableScope::Vim && !vim_variable_is_writable(name.as_bytes()) {
                    return Err(mlua::Error::runtime(format!("E46: Cannot change read-only variable \"{}\"", name.to_string_lossy())));
                }
                let value = if value.is_nil() {
                    None
                } else {
                    Some(lua_to_object(lua, &value).map_err(mlua::Error::external)?)
                };
                let mut editor = set_editor.borrow_mut();
                let variables = variables_mut(&mut editor, scope, handle).map_err(mlua::Error::runtime)?;
                if let Some(value) = value {
                    variables.insert(name, value);
                } else {
                    let index = variables.iter().position(|(key, _)| key == &name);
                    if let Some(index) = index {
                        variables.0.remove(index);
                    }
                }
                Ok(())
            })?,
        )?;
        for (metadata, dispatch) in registry.iter() {
            let editor = editor.clone();
            if metadata.name == "nvim_cmd" {
                api.set(
                    metadata.name,
                    scope.create_function_mut(move |lua, args: Variadic<Value>| {
                        let args = args
                            .iter()
                            .map(|value| lua_to_object(lua, value).map_err(mlua::Error::external))
                            .collect::<Result<Vec<_>, _>>()?;
                        let (cmd, opts) = nvim_cmd_args(&args).map_err(mlua::Error::external)?;
                        let mut nested = ExExecutor::new();
                        let mut executor = ExApiExecutor {
                            executor: &mut nested,
                            outcome: ExecOutcome::Completed,
                        };
                        let result = ox_api::execute_nvim_cmd(
                            &mut editor.borrow_mut(),
                            cmd,
                            opts,
                            &mut executor,
                        )
                        .map_err(mlua::Error::external)?;
                        object_to_lua(lua, &Object::String(result)).map_err(mlua::Error::external)
                    })?,
                )?;
                continue;
            }
            api.set(
                metadata.name,
                scope.create_function_mut(move |lua, args: Variadic<Value>| {
                    let mut args = args
                        .iter()
                        .map(|value| lua_to_object(lua, value).map_err(mlua::Error::external))
                        .collect::<Result<Vec<_>, _>>()?;
                    if (metadata.name == "nvim_get_option_value" && args.len() == 1)
                        || (metadata.name == "nvim_set_option_value" && args.len() == 2)
                    {
                        args.push(Object::Dict(Dict(Vec::new())));
                    }
                    let result = dispatch(&mut editor.borrow_mut(), &args).map_err(mlua::Error::external)?;
                    object_to_lua(lua, &result).map_err(mlua::Error::external)
                })?,
            )?;
        }
        Ok(run())
    });
    for (name, value) in originals {
        api.set(name, value).map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    }
    vim.set("_getvar", original_getvar).map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    vim.set("_setvar", original_setvar).map_err(|error| LuaExecError::Runtime(error.to_string()))?;
    result.map_err(|error| LuaExecError::Runtime(error.to_string()))?
}

fn parse_variable_scope(scope: &mlua::LuaString) -> mlua::Result<VariableScope> {
    match scope.as_bytes().as_ref() {
        b"g" => Ok(VariableScope::Global),
        b"b" => Ok(VariableScope::Buffer),
        b"w" => Ok(VariableScope::Window),
        b"t" => Ok(VariableScope::Tabpage),
        b"v" => Ok(VariableScope::Vim),
        _ => Err(mlua::Error::runtime("unknown variable scope")),
    }
}

fn nvim_cmd_args(params: &[Object]) -> Result<(&Dict, &Dict), ApiError> {
    static EMPTY: std::sync::OnceLock<Dict> = std::sync::OnceLock::new();
    let empty = EMPTY.get_or_init(|| Dict(Vec::new()));
    match params {
        [Object::Dict(cmd)] => Ok((cmd, empty)),
        [Object::Dict(cmd), Object::Dict(opts)] => Ok((cmd, opts)),
        [Object::Dict(cmd), Object::Array(opts)] if opts.is_empty() => Ok((cmd, empty)),
        _ => Err(ApiError::validation("nvim_cmd expects (Dict, optional Dict)")),
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
