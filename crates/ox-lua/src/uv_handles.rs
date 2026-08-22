//! Stream, process, DNS, work, and isolated-thread `vim.uv` bindings.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use mlua::{AnyUserData, Function, Lua, LuaString, MultiValue, Table, UserData, UserDataMethods, Value, Variadic};
use ox_uv::dns::{self, AddrInfoHints};
use ox_uv::net::{NetEvent, Tcp, Udp};
#[cfg(unix)]
use ox_uv::net::{Pipe, Tty, TtyMode};
use ox_uv::process::{self, Process, ProcessPipe, SpawnOptions, StdioConfig};
use ox_uv::thread;
use ox_uv::{Async, CallbackError, Check, Handle, HandleId, Idle, Prepare, RunMode, Signal, UvLoop};

#[cfg(unix)]
const SIGNALS: &[(&str, i32)] = &[
    ("sighup", signal_hook::consts::SIGHUP),
    ("sigint", signal_hook::consts::SIGINT),
    ("sigquit", signal_hook::consts::SIGQUIT),
    ("sigill", signal_hook::consts::SIGILL),
    ("sigtrap", signal_hook::consts::SIGTRAP),
    ("sigabrt", signal_hook::consts::SIGABRT),
    ("sigbus", signal_hook::consts::SIGBUS),
    ("sigfpe", signal_hook::consts::SIGFPE),
    ("sigkill", signal_hook::consts::SIGKILL),
    ("sigusr1", signal_hook::consts::SIGUSR1),
    ("sigsegv", signal_hook::consts::SIGSEGV),
    ("sigusr2", signal_hook::consts::SIGUSR2),
    ("sigpipe", signal_hook::consts::SIGPIPE),
    ("sigalrm", signal_hook::consts::SIGALRM),
    ("sigterm", signal_hook::consts::SIGTERM),
    ("sigchld", signal_hook::consts::SIGCHLD),
    ("sigcont", signal_hook::consts::SIGCONT),
    ("sigstop", signal_hook::consts::SIGSTOP),
    ("sigtstp", signal_hook::consts::SIGTSTP),
    ("sigttin", signal_hook::consts::SIGTTIN),
    ("sigttou", signal_hook::consts::SIGTTOU),
    ("sigurg", signal_hook::consts::SIGURG),
    ("sigxcpu", signal_hook::consts::SIGXCPU),
    ("sigxfsz", signal_hook::consts::SIGXFSZ),
    ("sigvtalrm", signal_hook::consts::SIGVTALRM),
    ("sigprof", signal_hook::consts::SIGPROF),
    ("sigwinch", signal_hook::consts::SIGWINCH),
    ("sigio", signal_hook::consts::SIGIO),
    ("sigsys", signal_hook::consts::SIGSYS),
    ("sigiot", signal_hook::consts::SIGABRT),
    ("sigpoll", signal_hook::consts::SIGIO),
];

fn signal_number(value: Value) -> mlua::Result<i32> {
    match value {
        Value::Integer(number) => i32::try_from(number)
            .map_err(|_| mlua::Error::runtime(format!("invalid signal number: {number}"))),
        Value::String(name) => {
            let name = name.to_str()?;
            #[cfg(unix)]
            if let Some((_, number)) = SIGNALS.iter().find(|(candidate, _)| name == *candidate) {
                return Ok(*number);
            }
            Err(mlua::Error::runtime(format!("invalid signal name: {name}")))
        }
        _ => Err(mlua::Error::runtime("signal must be a string or integer")),
    }
}

fn signal_name(lua: &Lua, number: i32) -> mlua::Result<Value> {
    #[cfg(unix)]
    if let Some((name, _)) = SIGNALS.iter().find(|(_, candidate)| *candidate == number) {
        return Ok(Value::String(lua.create_string(*name)?));
    }
    Ok(Value::Integer(i64::from(number)))
}

use crate::vim::{call_with_traceback, FastCallbackState, Scheduler};

type DeferredOperation = Box<dyn FnOnce(&mut UvLoop)>;

#[derive(Clone)]
struct LoopAccess {
    uv_loop: Rc<RefCell<UvLoop>>,
    in_callback: Rc<Cell<bool>>,
    deferred: Rc<RefCell<VecDeque<DeferredOperation>>>,
}

impl LoopAccess {
    fn apply(&self, operation: DeferredOperation) -> mlua::Result<()> {
        if self.in_callback.get() {
            self.deferred.borrow_mut().push_back(operation);
            return Ok(());
        }
        operation(&mut self.uv_loop.borrow_mut());
        Ok(())
    }

    fn callback<R>(&self, uv_loop: &mut UvLoop, callback: impl FnOnce() -> R) -> R {
        let nested = self.in_callback.replace(true);
        let result = callback();
        self.in_callback.set(nested);
        if !nested {
            loop {
                let operation = self.deferred.borrow_mut().pop_front();
                let Some(operation) = operation else { break };
                self.in_callback.set(true);
                operation(uv_loop);
                self.in_callback.set(false);
            }
        }
        result
    }
}

fn invoke(lua: &Lua, fast: &FastCallbackState, callback: &Function, args: MultiValue) {
    let _guard = fast.enter();
    if let Err(error) = call_with_traceback(lua, callback, args) { eprintln!("vim.uv callback error: {error}"); }
}

fn error_args(lua: &Lua, result: Result<(), impl ToString>) -> mlua::Result<MultiValue> {
    let mut args = MultiValue::new();
    match result {
        Ok(()) => args.push_back(Value::Nil),
        Err(error) => args.push_back(Value::String(lua.create_string(error.to_string())?)),
    }
    Ok(args)
}

fn address_table(lua: &Lua, address: SocketAddr) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("ip", address.ip().to_string())?;
    table.set("port", address.port())?;
    table.set("family", if address.is_ipv4() { "inet" } else { "inet6" })?;
    Ok(table)
}

fn socket_addr(host: &str, port: u16) -> mlua::Result<SocketAddr> {
    let ip = host.parse::<IpAddr>().map_err(mlua::Error::external)?;
    Ok(SocketAddr::new(ip, port))
}

#[derive(Default)]
struct StreamCallbacks {
    listen: Option<Function>,
    connect: Option<Function>,
    read: Option<Function>,
    writes: HashMap<u64, Function>,
    /// Pipe-only: callback of the write being queued; a synchronously flushed
    /// completion claims it during `ProcessPipe::write` before its id is known.
    pending_write: Option<Function>,
    /// Pipe-only: set while a `ProcessPipe::write` call holds the pipe borrow.
    /// Every completion it delivers — this write or an earlier buffered one
    /// flushed by it — parks instead of invoking Lua under the borrow.
    write_in_flight: bool,
    /// Pipe-only: (callback, result) parked in completion order while a write
    /// call is in flight; delivered in order once the pipe borrow is released.
    parked_writes: VecDeque<(Function, Result<(), String>)>,
    shutdown: Option<Function>,
    accepted_tcp: VecDeque<Tcp>,
    #[cfg(unix)]
    accepted_pipe: VecDeque<Pipe>,
}

struct TcpContext {
    lua: Lua,
    fast: FastCallbackState,
    access: LoopAccess,
    routes: RefCell<HashMap<HandleId, Rc<RefCell<StreamCallbacks>>>>,
}

impl TcpContext {
    fn event(self: &Rc<Self>, uv_loop: &mut UvLoop, id: HandleId, event: NetEvent) {
        let callbacks = self.routes.borrow().get(&id).cloned();
        let Some(callbacks) = callbacks else { return };
        self.access.callback(uv_loop, || match event {
            NetEvent::AcceptedTcp(child) => {
                let callback = { let mut state = callbacks.borrow_mut(); state.accepted_tcp.push_back(*child); state.listen.clone() };
                if let Some(callback) = callback { let mut args = MultiValue::new(); args.push_back(Value::Nil); invoke(&self.lua, &self.fast, &callback, args); }
            }
            #[cfg(unix)]
            NetEvent::AcceptedPipe(child) => {
                let callback = { let mut state = callbacks.borrow_mut(); state.accepted_pipe.push_back(*child); state.listen.clone() };
                if let Some(callback) = callback { let mut args = MultiValue::new(); args.push_back(Value::Nil); invoke(&self.lua, &self.fast, &callback, args); }
            }
            NetEvent::Connected(result) => {
                let callback = callbacks.borrow_mut().connect.take();
                if let Some(callback) = callback { if let Ok(args) = error_args(&self.lua, result) { invoke(&self.lua, &self.fast, &callback, args); } }
            }
            NetEvent::Read(bytes) => {
                let callback = callbacks.borrow().read.clone();
                if let Some(callback) = callback { if let Ok(string) = self.lua.create_string(bytes) { let mut args = MultiValue::new(); args.push_back(Value::Nil); args.push_back(Value::String(string)); invoke(&self.lua, &self.fast, &callback, args); } }
            }
            NetEvent::Eof => {
                let callback = callbacks.borrow().read.clone();
                if let Some(callback) = callback { let mut args = MultiValue::new(); args.push_back(Value::Nil); args.push_back(Value::Nil); invoke(&self.lua, &self.fast, &callback, args); }
            }
            NetEvent::WriteComplete { id, result } => {
                let callback = callbacks.borrow_mut().writes.remove(&id.get());
                if let Some(callback) = callback { if let Ok(args) = error_args(&self.lua, result) { invoke(&self.lua, &self.fast, &callback, args); } }
            }
            NetEvent::ShutdownComplete(result) => {
                let callback = callbacks.borrow_mut().shutdown.take();
                if let Some(callback) = callback { if let Ok(args) = error_args(&self.lua, result) { invoke(&self.lua, &self.fast, &callback, args); } }
            }
            NetEvent::Error(error) => {
                let callback = { let state = callbacks.borrow(); state.read.clone().or_else(|| state.connect.clone()).or_else(|| state.listen.clone()) };
                if let Some(callback) = callback { let mut args = MultiValue::new(); if let Ok(message) = self.lua.create_string(error.to_string()) { args.push_back(Value::String(message)); args.push_back(Value::Nil); invoke(&self.lua, &self.fast, &callback, args); } }
            }
            NetEvent::Datagram { .. } => {}
        });
    }
}

#[derive(Clone)]
struct LuaTcp {
    inner: Rc<RefCell<Option<Tcp>>>,
    callbacks: Rc<RefCell<StreamCallbacks>>,
    context: Rc<TcpContext>,
}

impl UserData for LuaTcp {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("bind", |_, this, (host, port): (String, u16)| {
            let address = socket_addr(&host, port)?;
            let context = this.context.clone();
            let context_for_callback = context.clone();
            let tcp = Tcp::bind(&mut context.access.uv_loop.borrow_mut(), address, move |loop_, id, event| context_for_callback.event(loop_, id, event)).map_err(mlua::Error::external)?;
            context.routes.borrow_mut().insert(tcp.id(), this.callbacks.clone());
            *this.inner.borrow_mut() = Some(tcp);
            Ok(true)
        });
        methods.add_method("connect", |_, this, (host, port, callback): (String, u16, Function)| {
            this.callbacks.borrow_mut().connect = Some(callback);
            let address = socket_addr(&host, port)?;
            let inner = this.inner.clone(); let callbacks = this.callbacks.clone(); let context = this.context.clone();
            let access = context.access.clone();
            let context_for_callback = context.clone();
            access.apply(Box::new(move |uv_loop| match Tcp::connect(uv_loop, address, move |loop_, id, event| context_for_callback.event(loop_, id, event)) {
                Ok(tcp) => { context.routes.borrow_mut().insert(tcp.id(), callbacks); *inner.borrow_mut() = Some(tcp); }
                Err(error) => { if let Some(callback) = callbacks.borrow_mut().connect.take() { if let Ok(args) = error_args(&context.lua, Err(error)) { invoke(&context.lua, &context.fast, &callback, args); } } }
            }))?;
            Ok(true)
        });
        methods.add_method("listen", |_, this, (backlog, callback): (u32, Function)| {
            this.callbacks.borrow_mut().listen = Some(callback);
            let inner = this.inner.clone();
            this.context.access.apply(Box::new(move |uv_loop| { if let Some(tcp) = inner.borrow_mut().as_mut() { let _ = tcp.listen(uv_loop, backlog); } }))?;
            Ok(true)
        });
        methods.add_method("accept", |_, this, peer: AnyUserData| {
            let peer = peer.borrow_mut::<LuaTcp>()?;
            let child = this.callbacks.borrow_mut().accepted_tcp.pop_front().ok_or_else(|| mlua::Error::runtime("no pending TCP connection"))?;
            this.context.routes.borrow_mut().insert(child.id(), peer.callbacks.clone());
            *peer.inner.borrow_mut() = Some(child);
            Ok(true)
        });
        add_tcp_stream_methods(methods);
        methods.add_method("getsockname", |lua, this, ()| {
            let inner = this.inner.borrow(); let tcp = inner.as_ref().ok_or_else(|| mlua::Error::runtime("TCP handle is not initialized"))?;
            address_table(lua, tcp.local_addr().map_err(mlua::Error::external)?)
        });
        methods.add_method("getpeername", |lua, this, ()| {
            let inner = this.inner.borrow(); let tcp = inner.as_ref().ok_or_else(|| mlua::Error::runtime("TCP handle is not initialized"))?;
            address_table(lua, tcp.peer_addr().map_err(mlua::Error::external)?)
        });
        methods.add_method("nodelay", |_, this, enable: bool| { let inner = this.inner.borrow(); inner.as_ref().ok_or_else(|| mlua::Error::runtime("TCP handle is not initialized"))?.nodelay(enable).map_err(mlua::Error::external)?; Ok(true) });
        methods.add_method("is_closing", |_, this, ()| Ok(this.inner.borrow().as_ref().is_none_or(|tcp| tcp.is_closing(&this.context.access.uv_loop.borrow()))));
        methods.add_method("close", |_, this, callback: Option<Function>| {
            let inner = this.inner.clone(); let context = this.context.clone();
            let access = context.access.clone();
            access.apply(Box::new(move |uv_loop| if let Some(tcp) = inner.borrow_mut().take() { let id = tcp.id(); let _ = tcp.close(uv_loop); context.routes.borrow_mut().remove(&id); if let Some(callback) = callback { invoke(&context.lua, &context.fast, &callback, MultiValue::new()); } }))?;
            Ok(())
        });
    }
}

fn add_tcp_stream_methods<M: UserDataMethods<LuaTcp>>(methods: &mut M) {
    methods.add_method("read_start", |_, this, callback: Function| {
        this.callbacks.borrow_mut().read = Some(callback);
        let inner = this.inner.clone(); this.context.access.apply(Box::new(move |uv_loop| if let Some(tcp) = inner.borrow_mut().as_mut() { let _ = tcp.read_start(uv_loop); }))?; Ok(true)
    });
    methods.add_method("read_stop", |_, this, ()| { this.callbacks.borrow_mut().read = None; let inner = this.inner.clone(); this.context.access.apply(Box::new(move |uv_loop| if let Some(tcp) = inner.borrow_mut().as_mut() { let _ = tcp.read_stop(uv_loop); }))?; Ok(true) });
    methods.add_method("write", |_, this, (bytes, callback): (LuaString, Option<Function>)| {
        let data = bytes.as_bytes().to_vec(); let inner = this.inner.clone(); let callbacks = this.callbacks.clone();
        this.context.access.apply(Box::new(move |uv_loop| if let Some(tcp) = inner.borrow_mut().as_mut() { if let Ok(id) = tcp.write(uv_loop, data) { if let Some(callback) = callback { callbacks.borrow_mut().writes.insert(id.get(), callback); } } }))?; Ok(true)
    });
    methods.add_method("shutdown", |_, this, callback: Option<Function>| { this.callbacks.borrow_mut().shutdown = callback; let inner = this.inner.clone(); this.context.access.apply(Box::new(move |uv_loop| if let Some(tcp) = inner.borrow_mut().as_mut() { let _ = tcp.shutdown(uv_loop); }))?; Ok(true) });
}

#[cfg(unix)]
#[derive(Clone)]
struct LuaProcessPipe {
    inner: Rc<RefCell<Option<ProcessPipe>>>,
    callbacks: Rc<RefCell<StreamCallbacks>>,
    lua: Lua,
    fast: FastCallbackState,
    access: LoopAccess,
}

#[cfg(unix)]
impl LuaProcessPipe {
    fn install_endpoint(&self, mut pipe: ProcessPipe) {
        let callbacks = self.callbacks.clone(); let lua = self.lua.clone(); let fast = self.fast.clone(); let access = self.access.clone();
        pipe.set_callback(move |uv_loop, _, event| access.callback(uv_loop, || match event {
            NetEvent::Read(bytes) => if let Some(callback) = callbacks.borrow().read.clone() { if let Ok(string) = lua.create_string(bytes) { let mut args = MultiValue::new(); args.push_back(Value::Nil); args.push_back(Value::String(string)); invoke(&lua, &fast, &callback, args); } },
            NetEvent::Eof => if let Some(callback) = callbacks.borrow().read.clone() { let mut args = MultiValue::new(); args.push_back(Value::Nil); args.push_back(Value::Nil); invoke(&lua, &fast, &callback, args); },
            // Release the borrow before invoking: the Lua write callback may
            // call back into this pipe (shutdown/close/write).
            NetEvent::WriteComplete { id, result } => {
                let result = result.map_err(|error| error.to_string());
                let mut state = callbacks.borrow_mut();
                let callback = state.writes.remove(&id.get()).or_else(|| state.pending_write.take());
                match callback {
                    // A completion delivered inside `ProcessPipe::write` — for
                    // the write being queued or an earlier buffered one it just
                    // flushed: park it; the pipe borrow is still held.
                    Some(callback) if state.write_in_flight => { state.parked_writes.push_back((callback, result)); }
                    Some(callback) => { drop(state); if let Ok(args) = error_args(&lua, result) { invoke(&lua, &fast, &callback, args); } }
                    None => {}
                }
            },
            _ => {}
        }));
        *self.inner.borrow_mut() = Some(pipe);
    }
}

#[cfg(unix)]
impl UserData for LuaProcessPipe {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("read_start", |_, this, callback: Function| { this.callbacks.borrow_mut().read = Some(callback); let inner = this.inner.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(pipe) = inner.borrow_mut().as_mut() { let _ = pipe.read_start_current(uv_loop); }))?; Ok(true) });
        methods.add_method("read_stop", |_, this, ()| { this.callbacks.borrow_mut().read = None; let inner = this.inner.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(pipe) = inner.borrow_mut().as_mut() { let _ = pipe.read_stop(uv_loop); }))?; Ok(true) });
        methods.add_method("write", |_, this, (bytes, callback): (LuaString, Option<Function>)| {
            let data = bytes.as_bytes().to_vec();
            let inner = this.inner.clone();
            let callbacks = this.callbacks.clone();
            let lua = this.lua.clone();
            let fast = this.fast.clone();
            let access = this.access.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(callback) = callback { callbacks.borrow_mut().pending_write = Some(callback); }
                // `ProcessPipe::write` flushes synchronously and delivers
                // WriteComplete events inside this call — for the write being
                // queued and for any earlier buffered writes it drains. Park
                // them all; keep Lua out until the pipe borrow is released.
                let queued = {
                    callbacks.borrow_mut().write_in_flight = true;
                    let queued = {
                        let mut inner = inner.borrow_mut();
                        match inner.as_mut() {
                            Some(pipe) => pipe.write(uv_loop, data),
                            None => Err(ox_uv::net::NetError::Closed),
                        }
                    };
                    callbacks.borrow_mut().write_in_flight = false;
                    queued
                };
                let claimed = callbacks.borrow_mut().pending_write.take();
                match (queued, claimed) {
                    // An outstanding remainder completes on a later loop turn
                    // under this write id, like TCP/TTY.
                    (Ok(id), Some(callback)) => { callbacks.borrow_mut().writes.insert(id.get(), callback); }
                    // The queued write never reached the pipe: report it here.
                    (Err(error), Some(callback)) => { callbacks.borrow_mut().parked_writes.push_back((callback, Err(error.to_string()))); }
                    _ => {}
                }
                let parked: Vec<_> = callbacks.borrow_mut().parked_writes.drain(..).collect();
                if !parked.is_empty() {
                    access.callback(uv_loop, move || {
                        for (callback, result) in parked {
                            if let Ok(args) = error_args(&lua, result) { invoke(&lua, &fast, &callback, args); }
                        }
                    });
                }
            }))?;
            Ok(true)
        });
        methods.add_method("shutdown", |_, this, callback: Option<Function>| { this.callbacks.borrow_mut().shutdown = callback; let inner = this.inner.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(pipe) = inner.borrow_mut().as_mut() { let _ = pipe.shutdown(uv_loop); }))?; Ok(true) });
        methods.add_method("close", |_, this, callback: Option<Function>| { let inner = this.inner.clone(); let lua = this.lua.clone(); let fast = this.fast.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(pipe) = inner.borrow_mut().take() { let _ = pipe.close(uv_loop); if let Some(callback) = callback { invoke(&lua, &fast, &callback, MultiValue::new()); } }))?; Ok(()) });
        methods.add_method("is_closing", |_, this, ()| Ok(this.inner.borrow().as_ref().is_none_or(|pipe| pipe.is_closing(&this.access.uv_loop.borrow()))));
    }
}

#[derive(Clone)]
struct LuaProcess { inner: Rc<RefCell<Option<Process>>>, access: LoopAccess }
impl UserData for LuaProcess {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get_pid", |_, this, ()| Ok(this.inner.borrow().as_ref().map(Process::pid)));
        methods.add_method("kill", |_, this, signal: Option<i32>| { this.inner.borrow().as_ref().ok_or_else(|| mlua::Error::runtime("process is closed"))?.kill(signal).map_err(mlua::Error::external)?; Ok(true) });
        methods.add_method("close", |_, this, ()| { let inner = this.inner.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(process) = inner.borrow_mut().take() { let _ = process.close(uv_loop); }))?; Ok(()) });
        methods.add_method("is_closing", |_, this, ()| Ok(this.inner.borrow().as_ref().is_none_or(|process| process.is_closing(&this.access.uv_loop.borrow()))));
    }
}

#[derive(Clone)]
enum ThreadArg { Nil, Bool(bool), Integer(i64), Number(f64), String(Vec<u8>) }
fn thread_arg(value: Value) -> mlua::Result<ThreadArg> { match value { Value::Nil => Ok(ThreadArg::Nil), Value::Boolean(v) => Ok(ThreadArg::Bool(v)), Value::Integer(v) => Ok(ThreadArg::Integer(v)), Value::Number(v) => Ok(ThreadArg::Number(v)), Value::String(v) => Ok(ThreadArg::String(v.as_bytes().to_vec())), other => Err(mlua::Error::runtime(format!("unsupported thread argument {}", other.type_name()))) } }

fn push_thread_arg(lua: &Lua, values: &mut MultiValue, argument: ThreadArg) -> Result<(), String> {
    values.push_back(match argument {
        ThreadArg::Nil => Value::Nil,
        ThreadArg::Bool(value) => Value::Boolean(value),
        ThreadArg::Integer(value) => Value::Integer(value),
        ThreadArg::Number(value) => Value::Number(value),
        ThreadArg::String(value) => Value::String(lua.create_string(value).map_err(|error| error.to_string())?),
    });
    Ok(())
}

fn run_isolated(chunk: &[u8], arguments: Vec<ThreadArg>) -> Result<Vec<ThreadArg>, String> {
    let child = Lua::new();
    let function = child.load(chunk).into_function().map_err(|error| error.to_string())?;
    let mut values = MultiValue::new();
    for argument in arguments { push_thread_arg(&child, &mut values, argument)?; }
    let returned = function.call::<MultiValue>(values).map_err(|error| error.to_string())?;
    returned.into_iter().map(|value| thread_arg(value).map_err(|error| error.to_string())).collect()
}

#[derive(Default)]
struct WorkCompletion { results: Mutex<VecDeque<Result<Vec<ThreadArg>, String>>> }
struct PendingWork { completion: Arc<WorkCompletion>, callback: Function }
struct LuaWork {
    work: ox_uv::work::Work<ox_uv::UvLoopPoster>,
}
impl UserData for LuaWork {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("queue", |_, this, args: Variadic<Value>| {
            let arguments = args.into_iter().map(thread_arg).collect::<mlua::Result<Vec<_>>>()?;
            this.work.queue(Box::new(arguments)).map_err(mlua::Error::external)?;
            Ok(true)
        });
    }
}
struct LuaThread { inner: RefCell<Option<thread::Thread<Result<(), String>>>> }
impl UserData for LuaThread {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("join", |_, this, ()| { let mut thread = this.inner.borrow_mut(); let thread = thread.as_mut().ok_or_else(|| mlua::Error::runtime("thread is closed"))?; thread.join().map_err(mlua::Error::external)?.map_err(mlua::Error::runtime)?; Ok(true) });
        methods.add_method_mut("detach", |_, this, ()| { this.inner.borrow_mut().as_mut().ok_or_else(|| mlua::Error::runtime("thread is closed"))?.detach().map_err(mlua::Error::external)?; Ok(true) });
    }
}

#[derive(Default)]
struct ProcessCompletion { result: Mutex<Option<Result<(i64, i32), String>>> }
struct PendingProcess { completion: Arc<ProcessCompletion>, callback: Function }

#[derive(Clone, Copy)]
enum PhaseHandle { Idle(Idle), Prepare(Prepare), Check(Check) }
impl PhaseHandle {
    fn start(&self, uv_loop: &mut UvLoop, mut callback: impl FnMut(&mut UvLoop) -> Result<(), CallbackError> + 'static) -> ox_uv::Result<()> {
        match self {
            Self::Idle(handle) => handle.start(uv_loop, move |loop_, _| callback(loop_)),
            Self::Prepare(handle) => handle.start(uv_loop, move |loop_, _| callback(loop_)),
            Self::Check(handle) => handle.start(uv_loop, move |loop_, _| callback(loop_)),
        }
    }
    fn stop(&self, uv_loop: &mut UvLoop) -> ox_uv::Result<()> { match self { Self::Idle(handle) => handle.stop(uv_loop), Self::Prepare(handle) => handle.stop(uv_loop), Self::Check(handle) => handle.stop(uv_loop) } }
    fn close(&self, uv_loop: &mut UvLoop) -> ox_uv::Result<()> { match self { Self::Idle(handle) => handle.close(uv_loop), Self::Prepare(handle) => handle.close(uv_loop), Self::Check(handle) => handle.close(uv_loop) } }
    fn active(&self, uv_loop: &UvLoop) -> bool { match self { Self::Idle(handle) => handle.is_active(uv_loop), Self::Prepare(handle) => handle.is_active(uv_loop), Self::Check(handle) => handle.is_active(uv_loop) } }
}
struct LuaPhase { handle: PhaseHandle, access: LoopAccess, lua: Lua, fast: FastCallbackState }
impl UserData for LuaPhase {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("start", |_, this, callback: Function| { let handle = this.handle; let access = this.access.clone(); let event_access = access.clone(); let lua = this.lua.clone(); let fast = this.fast.clone(); access.apply(Box::new(move |uv_loop| { let _ = handle.start(uv_loop, move |loop_| { event_access.callback(loop_, || invoke(&lua, &fast, &callback, MultiValue::new())); Ok(()) }); }))?; Ok(true) });
        methods.add_method("stop", |_, this, ()| { let handle = this.handle; this.access.apply(Box::new(move |uv_loop| { let _ = handle.stop(uv_loop); }))?; Ok(true) });
        methods.add_method("is_active", |_, this, ()| Ok(this.handle.active(&this.access.uv_loop.borrow())));
        methods.add_method("close", |_, this, ()| { let handle = this.handle; this.access.apply(Box::new(move |uv_loop| { let _ = handle.close(uv_loop); }))?; Ok(()) });
    }
}

struct LuaAsync { handle: Async, access: LoopAccess }
impl UserData for LuaAsync {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("send", |_, this, ()| { this.handle.send(&this.access.uv_loop.borrow()).map_err(mlua::Error::external)?; Ok(true) });
        methods.add_method("close", |_, this, ()| { let handle = this.handle; this.access.apply(Box::new(move |uv_loop| { let _ = handle.close(uv_loop); }))?; Ok(()) });
    }
}

struct LuaSignal { handle: Signal, access: LoopAccess, lua: Lua, fast: FastCallbackState }

impl LuaSignal {
    fn start(&self, signum: Value, callback: Function, oneshot: bool) -> mlua::Result<i32> {
        let signum = signal_number(signum)?;
        let handle = self.handle;
        let access = self.access.clone();
        let event_access = access.clone();
        let lua = self.lua.clone();
        let fast = self.fast.clone();
        access.apply(Box::new(move |uv_loop| {
            let event_lua = lua.clone();
            let event_fast = fast.clone();
            let event_callback = move |loop_: &mut UvLoop, _: HandleId, delivered: i32| {
                event_access.callback(loop_, || {
                    let mut args = MultiValue::new();
                    args.push_back(signal_name(&event_lua, delivered).unwrap_or(Value::Integer(i64::from(delivered))));
                    invoke(&event_lua, &event_fast, &callback, args);
                });
                Ok(())
            };
            let _ = if oneshot {
                handle.start_oneshot(uv_loop, signum, event_callback)
            } else {
                handle.start(uv_loop, signum, event_callback)
            };
        }))?;
        Ok(0)
    }

    fn stop(&self) -> mlua::Result<i32> {
        let handle = self.handle;
        self.access.apply(Box::new(move |uv_loop| { let _ = handle.stop(uv_loop); }))?;
        Ok(0)
    }
}

impl UserData for LuaSignal {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("start", |_, this, (signum, callback): (Value, Function)| this.start(signum, callback, false));
        methods.add_method("start_oneshot", |_, this, (signum, callback): (Value, Function)| this.start(signum, callback, true));
        methods.add_method("stop", |_, this, ()| this.stop());
        methods.add_method("is_closing", |_, this, ()| Ok(this.handle.is_closing(&this.access.uv_loop.borrow())));
        methods.add_method("close", |_, this, ()| { let handle=this.handle; this.access.apply(Box::new(move |uv_loop| { let _=handle.close(uv_loop); }))?; Ok(()) });
    }
}

fn install_aux(lua: &Lua, uv: &Table, access: &LoopAccess, fast: &FastCallbackState) -> mlua::Result<()> {
    for (name, kind) in [("new_idle", 0_u8), ("new_prepare", 1), ("new_check", 2)] {
        let access = access.clone(); let lua_for_handle=lua.clone(); let fast=fast.clone();
        uv.set(name, lua.create_function(move |lua, ()| { let handle = match kind { 0 => PhaseHandle::Idle(Idle::new(&mut access.uv_loop.borrow_mut()).map_err(mlua::Error::external)?), 1 => PhaseHandle::Prepare(Prepare::new(&mut access.uv_loop.borrow_mut()).map_err(mlua::Error::external)?), _ => PhaseHandle::Check(Check::new(&mut access.uv_loop.borrow_mut()).map_err(mlua::Error::external)?) }; lua.create_userdata(LuaPhase { handle, access: access.clone(), lua: lua_for_handle.clone(), fast: fast.clone() }) })?)?;
    }
    let async_access=access.clone(); let async_lua=lua.clone(); let async_fast=fast.clone();
    uv.set("new_async", lua.create_function(move |lua, callback: Function| { let event_access=async_access.clone(); let event_lua=async_lua.clone(); let event_fast=async_fast.clone(); let handle=Async::new(&mut async_access.uv_loop.borrow_mut(), move |loop_,_| { event_access.callback(loop_, || invoke(&event_lua,&event_fast,&callback,MultiValue::new())); Ok(()) }).map_err(mlua::Error::external)?; lua.create_userdata(LuaAsync { handle, access: async_access.clone() }) })?)?;
    let signal_access=access.clone(); let signal_lua=lua.clone(); let signal_fast=fast.clone();
    uv.set("new_signal", lua.create_function(move |lua, ()| { let handle=Signal::new(&mut signal_access.uv_loop.borrow_mut()).map_err(mlua::Error::external)?; lua.create_userdata(LuaSignal { handle, access:signal_access.clone(), lua:signal_lua.clone(), fast:signal_fast.clone() }) })?)?;
    uv.set("signal_start", lua.create_function(|_, (signal, signum, callback): (AnyUserData, Value, Function)| signal.borrow::<LuaSignal>()?.start(signum, callback, false))?)?;
    uv.set("signal_start_oneshot", lua.create_function(|_, (signal, signum, callback): (AnyUserData, Value, Function)| signal.borrow::<LuaSignal>()?.start(signum, callback, true))?)?;
    uv.set("signal_stop", lua.create_function(|_, signal: AnyUserData| signal.borrow::<LuaSignal>()?.stop())?)?;
    Ok(())
}

pub(crate) fn install(lua: &Lua, uv: &Table, uv_loop: Rc<RefCell<UvLoop>>, scheduler: Rc<dyn Scheduler>, fast: FastCallbackState) -> mlua::Result<()> {
    let access = LoopAccess { uv_loop: uv_loop.clone(), in_callback: Rc::new(Cell::new(false)), deferred: Rc::new(RefCell::new(VecDeque::new())) };
    let tcp_context = Rc::new(TcpContext { lua: lua.clone(), fast: fast.clone(), access: access.clone(), routes: RefCell::new(HashMap::new()) });
    let pending_processes: Rc<RefCell<Vec<PendingProcess>>> = Rc::new(RefCell::new(Vec::new()));
    let pending_works: Rc<RefCell<Vec<PendingWork>>> = Rc::new(RefCell::new(Vec::new()));

    let run_loop = uv_loop.clone(); let run_pending = pending_processes.clone(); let run_works = pending_works.clone(); let run_scheduler = scheduler.clone(); let run_lua = lua.clone(); let run_fast = fast.clone();
    uv.set("run", lua.create_function(move |_, mode: Option<String>| {
        let mode = match mode.as_deref().unwrap_or("default") { "default" => RunMode::Default, "once" => RunMode::Once, "nowait" => RunMode::NoWait, other => return Err(mlua::Error::runtime(format!("invalid run mode: {other}"))) };
        let alive = run_loop.borrow_mut().run(mode).map_err(mlua::Error::external)?;
        let mut pending = run_pending.borrow_mut();
        let mut index = 0;
        while index < pending.len() {
            let result = pending[index].completion.result.lock().map_err(|_| mlua::Error::runtime("process completion lock poisoned"))?.take();
            if let Some(result) = result {
                let process = pending.remove(index); let callback = process.callback; let lua = run_lua.clone(); let fast = run_fast.clone();
                let (code, signal) = result.map_err(mlua::Error::runtime)?;
                run_scheduler.schedule_deferred(Box::new(move || { let mut args = MultiValue::new(); args.push_back(Value::Integer(code)); args.push_back(Value::Integer(i64::from(signal))); let _guard = fast.enter(); call_with_traceback(&lua, &callback, args).map(|_| ()) })).map_err(mlua::Error::runtime)?;
            } else { index += 1; }
        }
        for pending in run_works.borrow().iter() {
            loop {
                let result = pending.completion.results.lock().map_err(|_| mlua::Error::runtime("work completion lock poisoned"))?.pop_front();
                let Some(result) = result else { break };
                let callback = pending.callback.clone(); let lua = run_lua.clone(); let fast = run_fast.clone();
                run_scheduler.schedule_deferred(Box::new(move || {
                    let mut args = MultiValue::new();
                    match result {
                        Ok(values) => { args.push_back(Value::Nil); for value in values { push_thread_arg(&lua, &mut args, value).map_err(mlua::Error::runtime)?; } }
                        Err(error) => args.push_back(Value::String(lua.create_string(error)?)),
                    }
                    let _guard = fast.enter(); call_with_traceback(&lua, &callback, args).map(|_| ())
                })).map_err(mlua::Error::runtime)?;
            }
        }
        Ok(alive)
    })?)?;

    let new_tcp_context = tcp_context.clone();
    uv.set("new_tcp", lua.create_function(move |lua, ()| lua.create_userdata(LuaTcp { inner: Rc::new(RefCell::new(None)), callbacks: Rc::new(RefCell::new(StreamCallbacks::default())), context: new_tcp_context.clone() }))?)?;

    #[cfg(unix)] {
        let pipe_access = access.clone(); let pipe_fast = fast.clone(); let pipe_lua = lua.clone();
        uv.set("new_pipe", lua.create_function(move |lua, _ipc: Option<bool>| lua.create_userdata(LuaProcessPipe { inner: Rc::new(RefCell::new(None)), callbacks: Rc::new(RefCell::new(StreamCallbacks::default())), lua: pipe_lua.clone(), fast: pipe_fast.clone(), access: pipe_access.clone() }))?)?;

        let spawn_loop = uv_loop.clone(); let spawn_pending = pending_processes.clone(); let spawn_access = access.clone();
        uv.set("spawn", lua.create_function(move |lua, (program, options, callback): (String, Table, Function)| {
            let mut spawn_options = SpawnOptions::new(program);
            if let Ok(args) = options.get::<Table>("args") { spawn_options.args = args.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()?.into_iter().map(OsString::from).collect(); }
            spawn_options.cwd = options.get::<Option<String>>("cwd")?.map(PathBuf::from);
            spawn_options.detached = options.get::<Option<bool>>("detached")?.unwrap_or(false);
            spawn_options.uid = options.get::<Option<u32>>("uid")?;
            spawn_options.gid = options.get::<Option<u32>>("gid")?;
            let stdio = options.get::<Option<Table>>("stdio")?;
            let mut pipe_targets: [Option<LuaProcessPipe>; 3] = [None, None, None];
            if let Some(stdio) = stdio {
                for index in 0..3 { match stdio.raw_get::<Value>(index + 1)? { Value::UserData(userdata) => { let pipe = userdata.borrow::<LuaProcessPipe>()?.clone(); spawn_options.stdio[index] = StdioConfig::CreatePipe; pipe_targets[index] = Some(pipe); }, Value::Nil => spawn_options.stdio[index] = StdioConfig::Ignore, _ => return Err(mlua::Error::runtime("stdio entries must be pipe handles or nil")) } }
            }
            let completion = Arc::new(ProcessCompletion::default()); let waiter = completion.clone();
            let spawned = process::spawn(&mut spawn_loop.borrow_mut(), spawn_options, move |_, result| { let value = result.map(|exit| (exit.code, exit.signal)).map_err(|error| error.to_string()); if let Ok(mut slot) = waiter.result.lock() { *slot = Some(value); } }).map_err(mlua::Error::external)?;
            let mut pipes = spawned.pipes;
            if let (Some(target), Some(pipe)) = (&pipe_targets[0], pipes.stdin.take()) { target.install_endpoint(pipe); }
            if let (Some(target), Some(pipe)) = (&pipe_targets[1], pipes.stdout.take()) { target.install_endpoint(pipe); }
            if let (Some(target), Some(pipe)) = (&pipe_targets[2], pipes.stderr.take()) { target.install_endpoint(pipe); }
            let process = LuaProcess { inner: Rc::new(RefCell::new(Some(spawned.process))), access: spawn_access.clone() };
            let pid = process.inner.borrow().as_ref().map(Process::pid).unwrap_or_default();
            spawn_pending.borrow_mut().push(PendingProcess { completion, callback });
            Ok((lua.create_userdata(process)?, pid))
        })?)?;
    }

    uv.set("new_thread", lua.create_function(move |lua, (function, args): (Function, Variadic<Value>)| {
        let chunk = function.dump(false); let arguments = args.into_iter().map(thread_arg).collect::<mlua::Result<Vec<_>>>()?;
        let thread = thread::new_thread(None, move |_| {
            run_isolated(&chunk, arguments).map(|_| ())
        }).map_err(mlua::Error::external)?;
        lua.create_userdata(LuaThread { inner: RefCell::new(Some(thread)) })
    })?)?;

    let work_pending = pending_works.clone(); let work_loop = uv_loop.clone();
    uv.set("new_work", lua.create_function(move |lua, (work_function, after_function): (Function, Function)| {
        let chunk = work_function.dump(false); let completion = Arc::new(WorkCompletion::default()); let after_completion = completion.clone();
        let pool = ox_uv::pool::Pool::new(); let poster = work_loop.borrow().completion_poster();
        let work = ox_uv::work::new_work(pool, poster, move |data| {
            let arguments = data.downcast::<Vec<ThreadArg>>().map(|data| *data).unwrap_or_default();
            Box::new(run_isolated(&chunk, arguments)) as ox_uv::work::WorkData
        }, move |_, result| {
            let value = result.map_err(|error| error.to_string()).and_then(|data| data.downcast::<Result<Vec<ThreadArg>, String>>().map(|data| *data).unwrap_or_else(|_| Err("invalid work result".into())));
            if let Ok(mut results) = after_completion.results.lock() { results.push_back(value); }
        });
        work_pending.borrow_mut().push(PendingWork { completion, callback: after_function });
        lua.create_userdata(LuaWork { work })
    })?)?;

    let addrinfo_fast = fast.clone();
    uv.set("getaddrinfo", lua.create_function(move |lua, (host, service, callback): (Option<String>, Option<String>, Option<Function>)| {
        let result = dns::getaddrinfo(host.as_deref(), service.as_deref(), AddrInfoHints::default());
        let values = match result { Ok(entries) => { let table = lua.create_table()?; for (index, entry) in entries.into_iter().enumerate() { let item = address_table(lua, SocketAddr::new(entry.address, entry.port))?; item.set("socktype", format!("{:?}", entry.socket_type).to_lowercase())?; table.raw_set(index + 1, item)?; } Value::Table(table) }, Err(error) => { if let Some(callback) = callback { let mut args = MultiValue::new(); args.push_back(Value::String(lua.create_string(error.to_string())?)); invoke(lua, &addrinfo_fast, &callback, args); return Ok(Value::Nil); } return Err(mlua::Error::external(error)); } };
        if let Some(callback) = callback { let mut args = MultiValue::new(); args.push_back(Value::Nil); args.push_back(values); invoke(lua, &addrinfo_fast, &callback, args); Ok(Value::Nil) } else { Ok(values) }
    })?)?;
    let nameinfo_fast = fast.clone();
    uv.set("getnameinfo", lua.create_function(move |lua, (host, port, callback): (String, u16, Option<Function>)| { let info = dns::getnameinfo(socket_addr(&host, port)?).map_err(mlua::Error::external)?; if let Some(callback) = callback { let mut args = MultiValue::new(); args.push_back(Value::Nil); args.push_back(Value::String(lua.create_string(info.host)?)); args.push_back(Value::String(lua.create_string(info.service)?)); invoke(lua, &nameinfo_fast, &callback, args); Ok(Value::Nil) } else { let table = lua.create_table()?; table.set("host", info.host)?; table.set("service", info.service)?; Ok(Value::Table(table)) } })?)?;

    install_aux(lua, uv, &access, &fast)?;
    install_udp_tty(lua, uv, access, fast)?;
    Ok(())
}

#[derive(Default)]
struct UdpCallbacks { recv: Option<Function>, sends: HashMap<u64, Function> }
#[derive(Clone)]
struct LuaUdp { inner: Rc<RefCell<Option<Udp>>>, callbacks: Rc<RefCell<UdpCallbacks>>, lua: Lua, fast: FastCallbackState, access: LoopAccess }
impl UserData for LuaUdp {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("bind", |_, this, (host, port): (String, u16)| {
            let address = socket_addr(&host, port)?; let callbacks = this.callbacks.clone(); let lua = this.lua.clone(); let fast = this.fast.clone(); let access = this.access.clone();
            let event_access = access.clone();
            let udp = Udp::bind(&mut access.uv_loop.borrow_mut(), address, move |loop_, _, event| event_access.callback(loop_, || match event {
                NetEvent::Datagram { data, from } => { let callback = callbacks.borrow().recv.clone(); if let Some(callback) = callback { if let (Ok(data), Ok(address)) = (lua.create_string(data), address_table(&lua, from)) { let mut args = MultiValue::new(); args.push_back(Value::Nil); args.push_back(Value::String(data)); args.push_back(Value::Table(address)); invoke(&lua, &fast, &callback, args); } } }
                NetEvent::WriteComplete { id, result } => { let callback = callbacks.borrow_mut().sends.remove(&id.get()); if let Some(callback) = callback { if let Ok(args) = error_args(&lua, result) { invoke(&lua, &fast, &callback, args); } } }
                NetEvent::Error(error) => { let callback = callbacks.borrow().recv.clone(); if let Some(callback) = callback { let mut args = MultiValue::new(); if let Ok(message) = lua.create_string(error.to_string()) { args.push_back(Value::String(message)); args.push_back(Value::Nil); invoke(&lua, &fast, &callback, args); } } }
                _ => {}
            })).map_err(mlua::Error::external)?;
            *this.inner.borrow_mut() = Some(udp); Ok(true)
        });
        methods.add_method("connect", |_, this, (host, port): (String, u16)| { let address = socket_addr(&host, port)?; this.inner.borrow().as_ref().ok_or_else(|| mlua::Error::runtime("UDP handle is not initialized"))?.connect(address).map_err(mlua::Error::external)?; Ok(true) });
        methods.add_method("recv_start", |_, this, callback: Function| { this.callbacks.borrow_mut().recv = Some(callback); let inner = this.inner.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(udp) = inner.borrow_mut().as_mut() { let _ = udp.recv_start(uv_loop); }))?; Ok(true) });
        methods.add_method("recv_stop", |_, this, ()| { this.callbacks.borrow_mut().recv = None; let inner = this.inner.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(udp) = inner.borrow_mut().as_mut() { let _ = udp.recv_stop(uv_loop); }))?; Ok(true) });
        methods.add_method("send", |_, this, (bytes, host, port, callback): (LuaString, Option<String>, Option<u16>, Option<Function>)| { let target = match (host, port) { (Some(host), Some(port)) => Some(socket_addr(&host, port)?), (None, None) => None, _ => return Err(mlua::Error::runtime("UDP target needs host and port")) }; let data = bytes.as_bytes().to_vec(); let inner = this.inner.clone(); let callbacks = this.callbacks.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(udp) = inner.borrow_mut().as_mut() { if let Ok(id) = udp.send(uv_loop, data, target) { if let Some(callback) = callback { callbacks.borrow_mut().sends.insert(id.get(), callback); } } }))?; Ok(true) });
        methods.add_method("getsockname", |lua, this, ()| { let inner = this.inner.borrow(); address_table(lua, inner.as_ref().ok_or_else(|| mlua::Error::runtime("UDP handle is not initialized"))?.local_addr().map_err(mlua::Error::external)?) });
        methods.add_method("getpeername", |lua, this, ()| { let inner = this.inner.borrow(); address_table(lua, inner.as_ref().ok_or_else(|| mlua::Error::runtime("UDP handle is not initialized"))?.peer_addr().map_err(mlua::Error::external)?) });
        methods.add_method("set_broadcast", |_, this, enable: bool| { this.inner.borrow().as_ref().ok_or_else(|| mlua::Error::runtime("UDP handle is not initialized"))?.set_broadcast(enable).map_err(mlua::Error::external)?; Ok(true) });
        methods.add_method("set_ttl", |_, this, ttl: u32| { this.inner.borrow().as_ref().ok_or_else(|| mlua::Error::runtime("UDP handle is not initialized"))?.set_ttl(ttl).map_err(mlua::Error::external)?; Ok(true) });
        methods.add_method("close", |_, this, ()| { let inner = this.inner.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(udp) = inner.borrow_mut().take() { let _ = udp.close(uv_loop); }))?; Ok(()) });
    }
}

fn install_udp_tty(lua: &Lua, uv: &Table, access: LoopAccess, fast: FastCallbackState) -> mlua::Result<()> {
    let udp_access = access.clone(); let udp_lua = lua.clone(); let udp_fast = fast.clone();
    uv.set("new_udp", lua.create_function(move |lua, ()| lua.create_userdata(LuaUdp { inner: Rc::new(RefCell::new(None)), callbacks: Rc::new(RefCell::new(UdpCallbacks::default())), lua: udp_lua.clone(), fast: udp_fast.clone(), access: udp_access.clone() }))?)?;
    #[cfg(unix)] {
        let tty_access = access.clone(); let tty_lua = lua.clone(); let tty_fast = fast.clone();
        uv.set("new_tty", lua.create_function(move |lua, (fd, readable): (i32, bool)| {
            let path = format!("/proc/self/fd/{fd}"); let file = OpenOptions::new().read(readable).write(!readable).open(path).map_err(mlua::Error::external)?;
            let callbacks = Rc::new(RefCell::new(StreamCallbacks::default())); let event_callbacks = callbacks.clone(); let event_lua = tty_lua.clone(); let event_fast = tty_fast.clone(); let event_access = tty_access.clone();
            let tty = Tty::open(&mut tty_access.uv_loop.borrow_mut(), file, readable, move |loop_, _, event| event_access.callback(loop_, || {
                match event {
                    NetEvent::Read(bytes) => { let callback = event_callbacks.borrow().read.clone(); if let Some(callback) = callback { if let Ok(bytes) = event_lua.create_string(bytes) { let mut args = MultiValue::new(); args.push_back(Value::Nil); args.push_back(Value::String(bytes)); invoke(&event_lua, &event_fast, &callback, args); } } }
                    NetEvent::Eof => { let callback = event_callbacks.borrow().read.clone(); if let Some(callback) = callback { let mut args = MultiValue::new(); args.push_back(Value::Nil); args.push_back(Value::Nil); invoke(&event_lua, &event_fast, &callback, args); } }
                    NetEvent::WriteComplete { id, result } => { let callback = event_callbacks.borrow_mut().writes.remove(&id.get()); if let Some(callback) = callback { if let Ok(args) = error_args(&event_lua, result) { invoke(&event_lua, &event_fast, &callback, args); } } }
                    _ => {}
                }
            })).map_err(mlua::Error::external)?;
            lua.create_userdata(LuaTty { inner: Rc::new(RefCell::new(Some(tty))), callbacks, access: tty_access.clone() })
        })?)?;
    }
    Ok(())
}

#[cfg(unix)]
struct LuaTty { inner: Rc<RefCell<Option<Tty>>>, callbacks: Rc<RefCell<StreamCallbacks>>, access: LoopAccess }
#[cfg(unix)]
impl UserData for LuaTty {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("read_start", |_, this, callback: Function| { this.callbacks.borrow_mut().read = Some(callback); let inner = this.inner.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(tty) = inner.borrow_mut().as_mut() { let _ = tty.read_start(uv_loop); }))?; Ok(true) });
        methods.add_method("read_stop", |_, this, ()| { this.callbacks.borrow_mut().read = None; let inner = this.inner.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(tty) = inner.borrow_mut().as_mut() { let _ = tty.read_stop(uv_loop); }))?; Ok(true) });
        methods.add_method("write", |_, this, (bytes, callback): (LuaString, Option<Function>)| { let data = bytes.as_bytes().to_vec(); let inner = this.inner.clone(); let callbacks = this.callbacks.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(tty) = inner.borrow_mut().as_mut() { if let Ok(id) = tty.write(uv_loop, data) { if let Some(callback) = callback { callbacks.borrow_mut().writes.insert(id.get(), callback); } } }))?; Ok(true) });
        methods.add_method("get_winsize", |_, this, ()| this.inner.borrow().as_ref().ok_or_else(|| mlua::Error::runtime("TTY is closed"))?.get_winsize().map_err(mlua::Error::external));
        methods.add_method("set_mode", |_, this, mode: String| { let mode = match mode.as_str() { "normal" => TtyMode::Normal, "raw" => TtyMode::Raw, "io" => TtyMode::Cbreak, _ => return Err(mlua::Error::runtime("invalid TTY mode")) }; this.inner.borrow().as_ref().ok_or_else(|| mlua::Error::runtime("TTY is closed"))?.set_mode(mode).map_err(mlua::Error::external)?; Ok(true) });
        methods.add_method("close", |_, this, ()| { let inner = this.inner.clone(); this.access.apply(Box::new(move |uv_loop| if let Some(tty) = inner.borrow_mut().take() { let _ = tty.close(uv_loop); }))?; Ok(()) });
    }
}
