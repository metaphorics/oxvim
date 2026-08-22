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
    AutocmdContext, AutocmdKind, CmdlineKind, Editor, Event, ExExecutor, ExecOutcome, Geometry,
    LuaExec, LuaExecError, MappingAction, MessageKind, Mode, ModeMachine, Keys, TypeaheadFlags,
};
use ox_eval::{
    BufferHost, BuiltinHost as EvalBuiltinHost, Builtins, Scope, call_buffer_builtin,
    is_buffer_builtin,
};
use ox_lua::{
    ApiDispatchContext, BuiltinHost, LuaHost, RuntimeRoot, Scheduler, VariableHost, VariableScope, Work, bind_api,
    bind_variables, call_with_traceback, lua_to_object, object_to_lua,
};
use ox_rpc::{
    CHAN_STDIO, ChannelId, ChannelIdAllocator, IncrementalDecoder, Message,
};
use ox_types::{ApiError, BufHandle, Dict, Object, OxStr, TabHandle, Typval, WinHandle};
use ox_ui::{
    ChromeState, CmdlineState as UiCmdlineState, Compositor, ContentChunk, Emitter, HlState,
    MessageState, UiChannels, UiOptions,
};
use ox_uv::{Handle, HandleId, NetEvent, RunMode, Tcp, UvLoop};
#[cfg(unix)]
use ox_uv::net::Pipe;

use crate::AppError;
use crate::cli::{Cli, UserConfig};
use crate::runtime::runtime_root;

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

/// All mutable state shared by every RPC transport.
pub struct AppState {
    editor: Rc<RefCell<Editor>>,
    lua: Rc<RefCell<LuaHost>>,
    registry: Rc<Registry>,
    ex: ExExecutor,
    mode: ModeMachine,
    exiting: bool,
    rendered_messages: usize,
    lua_work: Rc<RefCell<VecDeque<Work>>>,
    ui_channels: UiChannels,
    emitter: Emitter,
    highlights: HlState,
    chrome: ChromeState,
}

impl AppState {
    /// Build one editor/Lua/API instance and execute process startup.
    pub fn new(cli: &Cli) -> Result<Self, AppError> {
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
        let registry = Rc::new(ox_api::core().map_err(|error| AppError::Api(error.to_string()))?);
        let lua_work = Rc::new(RefCell::new(VecDeque::new()));
        let mut lua = LuaHost::new(
            RuntimeRoot::new(runtime_root()?),
            Rc::new(EditorBuiltins { editor: editor.clone() }),
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
        let lua = Rc::new(RefCell::new(lua));
        let mut ex = ExExecutor::new();
        ex.set_lua_exec(Rc::new(RefCell::new(ServerLuaExec {
            lua: lua.clone(),
            registry: registry.clone(),
        })));

        let mut state = Self {
            editor,
            lua,
            registry,
            ex,
            mode: ModeMachine::default(),
            exiting: false,
            rendered_messages: 0,
            lua_work,
            ui_channels: UiChannels::new(),
            emitter: Emitter::new(),
            highlights: HlState::new(),
            chrome: ChromeState::new(),
        };
        state.run_startup(cli)?;
        Ok(state)
    }

    fn run_startup(&mut self, cli: &Cli) -> Result<(), AppError> {
        for command in &cli.pre_commands {
            self.execute_ex(command)?;
        }

        // No user-config discovery contract is exported yet.  Explicit files
        // are real and deterministic; NONE/NORC/--clean intentionally source
        // nothing rather than guessing platform paths.
        if !cli.clean && let UserConfig::File(path) = &cli.user_config {
            if Path::new(path).extension().is_some_and(|extension| extension == "lua") {
                self.lua
                    .borrow_mut()
                    .exec_file(Path::new(path))
                    .map_err(|error| AppError::Lua(error.to_string()))?;
            } else {
                let source = fs::read_to_string(path).map_err(AppError::Io)?;
                self.ex
                    .execute_script(&mut self.editor.borrow_mut(), path, &source)
                    .map_err(|error| AppError::Ex(error.to_string()))?;
            }
        }

        for command in &cli.commands {
            self.execute_ex(command)?;
        }
        self.fire_vim_enter()
    }

    fn execute_ex(&mut self, command: &str) -> Result<(), AppError> {
        self.ex
            .execute_line(&mut self.editor.borrow_mut(), command)
            .map_err(|error| AppError::Ex(error.to_string()))?;
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
            .execute_line(&mut self.editor.borrow_mut(), command)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        if outcome == ExecOutcome::Quit {
            self.exiting = true;
        }
        Ok(Object::Nil)
    }

    fn dispatch_nvim_cmd(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let (cmd, opts) = nvim_cmd_args(params)?;
        let mut executor = ExApiExecutor { executor: &mut self.ex, outcome: ExecOutcome::Completed };
        let result = ox_api::execute_nvim_cmd(&mut self.editor.borrow_mut(), cmd, opts, &mut executor)?;
        if executor.outcome == ExecOutcome::Quit {
            self.exiting = true;
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
        Ok(Object::Nil)
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

        let editor = self.editor.borrow();
        for message in &editor.messages()[self.rendered_messages..] {
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
        self.rendered_messages = editor.messages().len();
    }

    fn drive_input(&mut self) -> Result<(), ApiError> {
        loop {
            let ready = self.mode.run_once(&mut self.editor.borrow_mut())
                .map_err(|error| ApiError::exception(error.to_string()))?;
            if !ready { break; }

            if let Some(command) = self.mode.take_ex_command() {
                let outcome = self.ex.execute_line(&mut self.editor.borrow_mut(), &command)
                    .map_err(|error| ApiError::exception(error.to_string()))?;
                if outcome == ExecOutcome::Quit { self.exiting = true; }
            }
            if let Some(action) = self.mode.take_mapping_action() {
                let outcome = match action {
                    MappingAction::ExCommands(commands) => self.ex.execute_commands(&mut self.editor.borrow_mut(), &commands)
                        .map_err(|error| ApiError::exception(error.to_string()))?,
                    MappingAction::Expr(id) | MappingAction::Callback(id) => {
                        return Err(ApiError::exception(format!("mapping callback {id} has no registered host evaluator")));
                    }
                    MappingAction::Keys(_) | MappingAction::Nop => ExecOutcome::Completed,
                };
                if outcome == ExecOutcome::Quit { self.exiting = true; }
            }
            if self.exiting { break; }
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
                            writes.extend(redraws);
                            self.drain_lua_work()?;
                            return Ok(writes);
                        }
                    }
                }
                let response = (channel.get(), Message::Response { msgid, result }.encode_bytes());
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
                match self.dispatch(channel, &method, &params) {
                    Ok((_, mut redraws)) => {
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
}

fn positive_dimension(value: i64, name: &str) -> Result<usize, ApiError> {
    usize::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| ApiError::validation(format!("{name} must be positive")))
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
pub fn run_stdio(cli: &Cli) -> Result<(), AppError> {
    let mut state = AppState::new(cli)?;
    let mut decoder = IncrementalDecoder::new();
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut bytes = [0_u8; 8192];

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
    Ok(())
}

/// Serve RPC peers accepted from a TCP address or Unix-domain pipe.
pub fn run_listener(cli: &Cli, address: &str) -> Result<(), AppError> {
    let state = Rc::new(RefCell::new(AppState::new(cli)?));
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

    uv_loop
        .run(RunMode::Default)
        .map_err(|error| AppError::Server(error.to_string()))?;
    if let Some(error) = runtime.borrow_mut().error.take() {
        return Err(AppError::Server(error));
    }
    Ok(())
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
    allocator: ChannelIdAllocator,
    peers: HashMap<HandleId, Peer>,
    streams: HashMap<HandleId, Stream>,
    error: Option<String>,
}

impl NetworkRuntime {
    fn new(state: Rc<RefCell<AppState>>) -> Self {
        Self {
            state,
            allocator: ChannelIdAllocator::new(),
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
        self.peers.insert(id, Peer {
            channel: self.allocator.alloc(),
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
}

impl BuiltinHost for EditorBuiltins {
    fn call(&self, name: &OxStr, args: Vec<Typval>) -> Result<Typval, String> {
        let name_text = name.to_string_lossy();
        if is_buffer_builtin(&name_text) {
            return call_buffer_builtin(
                &mut CurrentBuffer(&mut self.editor.borrow_mut()),
                &name_text,
                args,
            )
            .map_err(|error| error.to_string());
        }
        let mut builtins = Builtins::without_regex();
        builtins
            .call(name, args, &mut Scope::new())
            .map_err(|error| error.to_string())
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
        if scope == VariableScope::Vim {
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

struct CurrentBuffer<'a>(&'a mut Editor);

impl BufferHost for CurrentBuffer<'_> {
    fn line_count(&self) -> ox_eval::Result<usize> {
        let buffer = self
            .0
            .current_buffer()
            .ok_or_else(|| ox_eval::EvalError::new("E86", 0, "Buffer not found"))?;
        self.0
            .buffer(buffer)
            .and_then(|state| state.text().map_err(Into::into))
            .map(|text| text.line_count())
            .map_err(|error: ox_editor::EditorError| {
                ox_eval::EvalError::new("E86", 0, error.to_string())
            })
    }

    fn get_line(&self, lnum: usize) -> ox_eval::Result<Option<OxStr>> {
        let buffer = self
            .0
            .current_buffer()
            .ok_or_else(|| ox_eval::EvalError::new("E86", 0, "Buffer not found"))?;
        let state = self
            .0
            .buffer(buffer)
            .map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))?;
        let text = state
            .text()
            .map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))?;
        if lnum == 0 || lnum > text.line_count() { return Ok(None); }
        text.line(lnum)
            .map(|line| Some(OxStr(line)))
            .map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))
    }

    fn replace_line(&mut self, lnum: usize, text: &OxStr) -> ox_eval::Result<()> {
        let buffer = self
            .0
            .current_buffer()
            .ok_or_else(|| ox_eval::EvalError::new("E86", 0, "Buffer not found"))?;
        let state = self
            .0
            .buffer_mut(buffer)
            .map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))?;
        state
            .replace_lines(
                lnum,
                lnum,
                &[text.as_bytes().to_vec()],
                ox_text::Position { lnum, col: 0 },
                ox_text::Position { lnum, col: 0 },
                0,
            )
            .map(|_| ())
            .map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))
    }

    fn append_line(&mut self, text: &OxStr) -> ox_eval::Result<()> {
        let count = self.line_count()?;
        let buffer = self
            .0
            .current_buffer()
            .ok_or_else(|| ox_eval::EvalError::new("E86", 0, "Buffer not found"))?;
        let state = self
            .0
            .buffer_mut(buffer)
            .map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))?;
        state
            .append_lines(
                count,
                &[text.as_bytes().to_vec()],
                ox_text::Position { lnum: count, col: 0 },
                0,
            )
            .map(|_| ())
            .map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))
    }
}

struct ServerLuaExec {
    lua: Rc<RefCell<LuaHost>>,
    registry: Rc<Registry>,
}

impl LuaExec for ServerLuaExec {
    fn execute_chunk(
        &mut self,
        editor: &mut Editor,
        code: &str,
        args: Vec<Object>,
    ) -> Result<Object, LuaExecError> {
        let host = self.lua.borrow();
        let lua = host.lua();
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
                lua_to_object(lua, &value).map_err(|error| LuaExecError::Conversion(error.to_string()))
            })
        })
    }

    fn execute_file(&mut self, editor: &mut Editor, path: &Path) -> Result<(), LuaExecError> {
        let host = self.lua.borrow();
        let lua = host.lua();
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
                if scope == VariableScope::Vim {
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
                    if metadata.name == "nvim_get_option_value" && args.len() == 1 {
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
