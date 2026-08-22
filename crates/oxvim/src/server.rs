//! Embedded stdio RPC server.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use ox_api::Registry;
use ox_editor::{Editor, ExExecutor, Geometry};
use ox_eval::{BufferHost, BuiltinHost as EvalBuiltinHost, Builtins, Scope, call_buffer_builtin, is_buffer_builtin};
use ox_loop::{Event, Loop};
use ox_lua::{BuiltinHost, LuaHost, RuntimeRoot, Scheduler, Work};
use ox_rpc::{CHAN_STDIO, ChannelId, IncrementalDecoder, Message};
use ox_types::{ApiError, Object, OxStr, Typval};

use crate::AppError;
use crate::runtime::runtime_root;

/// All mutable state owned by the embedded server.
pub struct AppState {
    event_loop: Loop,
    editor: Rc<RefCell<Editor>>,
    lua: LuaHost,
    registry: Registry,
    ex: ExExecutor,
    channel: ChannelId,
    lua_work: Rc<RefCell<VecDeque<Work>>>,
}

impl AppState {
    /// Build a server with one current empty buffer and window.
    pub fn new() -> Result<Self, AppError> {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).map_err(|error| AppError::Editor(error.to_string()))?;
        editor
            .create_tabpage(
                buffer,
                Geometry::new(0, 0, 80, 24).map_err(|error| AppError::Editor(error.to_string()))?,
            )
            .map_err(|error| AppError::Editor(error.to_string()))?;
        let editor = Rc::new(RefCell::new(editor));
        let lua_work = Rc::new(RefCell::new(VecDeque::new()));
        let lua = LuaHost::new(
            RuntimeRoot::new(runtime_root()?),
            Rc::new(EditorBuiltins { editor: editor.clone() }),
            Rc::new(LuaScheduler { queue: lua_work.clone() }),
        )
        .map_err(|error| AppError::Lua(error.to_string()))?;
        Ok(Self {
            event_loop: Loop::new().map_err(|error| AppError::Server(error.to_string()))?,
            editor,
            lua,
            registry: ox_api::core().map_err(|error| AppError::Api(error.to_string()))?,
            ex: ExExecutor::new(),
            channel: CHAN_STDIO,
            lua_work,
        })
    }

    fn is_fast(&self, method: &str) -> bool {
        self.registry.get(method).is_some_and(|(metadata, _)| metadata.fast)
    }

    fn dispatch(&mut self, method: &OxStr, params: &[Object]) -> Result<Object, ApiError> {
        let name = method.to_string_lossy();
        match name.as_ref() {
            "nvim_get_api_info" => self.dispatch_api_info(params),
            "nvim_exec_lua" | "nvim_execute_lua" => self.dispatch_lua(params),
            "nvim_command" => self.dispatch_command(params),
            "nvim_ui_attach" => Err(ApiError::exception("nvim_ui_attach is not yet wired")),
            _ => {
                let Some((_, dispatch)) = self.registry.get(&name) else {
                    return Err(ApiError::exception(format!("Invalid method: {name}")));
                };
                dispatch(&mut self.editor.borrow_mut(), params)
            }
        }
    }

    fn dispatch_api_info(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let Some((_, dispatch)) = self.registry.get("nvim_get_api_info") else {
            return Err(ApiError::exception("nvim_get_api_info is not registered"));
        };
        let mut result = dispatch(&mut self.editor.borrow_mut(), params)?;
        let Object::Array(info) = &mut result else {
            return Err(ApiError::exception("invalid API metadata response"));
        };
        let Some(channel) = info.first_mut() else {
            return Err(ApiError::exception("invalid API metadata response"));
        };
        *channel = Object::Integer(self.channel.get() as i64);
        Ok(result)
    }

    fn dispatch_lua(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::String(code), Object::Array(args)] = params else {
            return Err(ApiError::validation("nvim_exec_lua expects (String, Array)"));
        };
        let code = std::str::from_utf8(code.as_bytes())
            .map_err(|_| ApiError::validation("Lua source must be valid UTF-8"))?;
        self.lua.exec(code, args.clone()).map_err(|error| ApiError::exception(error.to_string()))
    }

    fn dispatch_command(&mut self, params: &[Object]) -> Result<Object, ApiError> {
        let [Object::String(command)] = params else {
            return Err(ApiError::validation("nvim_command expects one String argument"));
        };
        let command = std::str::from_utf8(command.as_bytes())
            .map_err(|_| ApiError::validation("Ex command must be valid UTF-8"))?;
        self.ex
            .execute_line(&mut self.editor.borrow_mut(), command)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        Ok(Object::Nil)
    }

    fn drain_lua_work(&mut self) -> Result<(), AppError> {
        loop {
            let work = self.lua_work.borrow_mut().pop_front();
            let Some(work) = work else { return Ok(()) };
            work().map_err(|error| AppError::Lua(error.to_string()))?;
        }
    }
}

/// Serve channel 1 over stdin/stdout until the peer closes its write side.
pub fn run_stdio() -> Result<(), AppError> {
    let mut state = AppState::new()?;
    let mut decoder = IncrementalDecoder::new();
    let deferred = Arc::new(Mutex::new(VecDeque::new()));
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
            let fast = match &message {
                Message::Request { method, .. } | Message::Notification { method, .. } => {
                    state.is_fast(&method.to_string_lossy())
                }
                Message::Response { .. } => true,
            };
            if fast {
                dispatch_message(&mut state, message, &mut output)?;
            } else {
                let ready = deferred.clone();
                let root = state.event_loop.root();
                state
                    .event_loop
                    .events()
                    .put(root, Event::callback(move || {
                        ready.lock().expect("deferred RPC queue poisoned").push_back(message);
                    }))
                    .map_err(|error| AppError::Server(error.to_string()))?;
            }
        }
        let root = state.event_loop.root();
        for event in state
            .event_loop
            .events()
            .process_events(root)
            .map_err(|error| AppError::Server(error.to_string()))?
        {
            event.dispatch();
        }
        loop {
            let message = deferred
                .lock()
                .map_err(|_| AppError::Server("deferred RPC queue poisoned".to_owned()))?
                .pop_front();
            let Some(message) = message else { break };
            dispatch_message(&mut state, message, &mut output)?;
        }
        state.drain_lua_work()?;
        output.flush().map_err(AppError::Io)?;
    }
    Ok(())
}

fn dispatch_message(state: &mut AppState, message: Message, output: &mut impl Write) -> Result<(), AppError> {
    match message {
        Message::Request { msgid, method, params } => {
            let response = Message::Response { msgid, result: state.dispatch(&method, &params) };
            output.write_all(&response.encode_bytes()).map_err(AppError::Io)
        }
        Message::Notification { method, params } => {
            if let Err(error) = state.dispatch(&method, &params) {
                output.write_all(&ox_rpc::nvim_error_event(&error)).map_err(AppError::Io)?;
            }
            Ok(())
        }
        Message::Response { .. } => Ok(()),
    }
}

struct EditorBuiltins {
    editor: Rc<RefCell<Editor>>,
}

impl BuiltinHost for EditorBuiltins {
    fn call(&self, name: &OxStr, args: Vec<Typval>) -> Result<Typval, String> {
        let name_text = name.to_string_lossy();
        if is_buffer_builtin(&name_text) {
            return call_buffer_builtin(&mut CurrentBuffer(&mut self.editor.borrow_mut()), &name_text, args)
                .map_err(|error| error.to_string());
        }
        let mut builtins = Builtins::without_regex();
        builtins.call(name, args, &mut Scope::new()).map_err(|error| error.to_string())
    }
}

struct CurrentBuffer<'a>(&'a mut Editor);

impl BufferHost for CurrentBuffer<'_> {
    fn line_count(&self) -> ox_eval::Result<usize> {
        let buffer = self.0.current_buffer().ok_or_else(|| ox_eval::EvalError::new("E86", 0, "Buffer not found"))?;
        self.0.buffer(buffer).and_then(|state| state.text().map_err(Into::into))
            .map(|text| text.line_count())
            .map_err(|error: ox_editor::EditorError| ox_eval::EvalError::new("E86", 0, error.to_string()))
    }

    fn get_line(&self, lnum: usize) -> ox_eval::Result<Option<OxStr>> {
        let buffer = self.0.current_buffer().ok_or_else(|| ox_eval::EvalError::new("E86", 0, "Buffer not found"))?;
        let state = self.0.buffer(buffer).map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))?;
        let text = state.text().map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))?;
        if lnum == 0 || lnum > text.line_count() { return Ok(None); }
        text.line(lnum).map(|line| Some(OxStr(line))).map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))
    }

    fn replace_line(&mut self, lnum: usize, text: &OxStr) -> ox_eval::Result<()> {
        let buffer = self.0.current_buffer().ok_or_else(|| ox_eval::EvalError::new("E86", 0, "Buffer not found"))?;
        let state = self.0.buffer_mut(buffer).map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))?;
        state.replace_lines(lnum, lnum, &[text.as_bytes().to_vec()], ox_text::Position { lnum, col: 0 }, ox_text::Position { lnum, col: 0 }, 0)
            .map(|_| ()).map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))
    }

    fn append_line(&mut self, text: &OxStr) -> ox_eval::Result<()> {
        let count = self.line_count()?;
        let buffer = self.0.current_buffer().ok_or_else(|| ox_eval::EvalError::new("E86", 0, "Buffer not found"))?;
        let state = self.0.buffer_mut(buffer).map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))?;
        state.append_lines(count, &[text.as_bytes().to_vec()], ox_text::Position { lnum: count, col: 0 }, 0)
            .map(|_| ()).map_err(|error| ox_eval::EvalError::new("E86", 0, error.to_string()))
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
